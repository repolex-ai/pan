//! pand's HTTP surface — Swagger-documented; the OpenAPI doc IS the spec.
//!
//! Every media call is a GRAPH call: nothing here reads the filesystem
//! except through the store, and `GET /media/{id}` serves bytes at the path
//! the graph declares. A store is named by its id (the soul's genesis SHA, or
//! a bare store's `storage_id`); no name = the configured default; an unknown
//! name = 404, never a fallback.
//!
//! Ids on the wire are the angle-bracket form `<pan/Image/k7m2p9x4>`. Paths
//! accept that form (URL-encoded), the full IRI, or the bare id.

use axum::extract::{Path as AxPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

use super::stages;
use super::Daemon;
use crate::{bare_id, bracket_iri, Facts, MediaState, PAN_NS};

type Shared = Arc<Daemon>;

// ── DTOs ────────────────────────────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct StoreInfo {
    pub id: String,
    pub root: String,
    pub is_default: bool,
    pub indexes: HashMap<String, IndexInfo>,
}

#[derive(Serialize, ToSchema)]
pub struct IndexInfo {
    pub dim: usize,
    pub count: usize,
}

#[derive(Serialize, ToSchema)]
pub struct HealthResponse {
    pub ok: bool,
    pub version: String,
    pub uptime_secs: u64,
    pub stores: Vec<String>,
    pub default: String,
    /// Configured stages (model names; endpoints are config, not disclosed).
    pub stages: HashMap<String, String>,
}

/// The delivery body. This is the shape Horae already sends to Pool
/// (`horae/src/horae/deliver.py`), accepted unchanged; `pan store` sends the
/// same shape. Exactly one of `png_b64` / `bytes_b64` carries the media.
#[derive(Deserialize, ToSchema)]
pub struct DeliveryBody {
    /// Which store: a soul's genesis SHA or a bare store id. Absent = default.
    #[serde(default, alias = "store")]
    pub soul: Option<String>,
    /// Recorded as `copia:momentId` on the image.
    #[serde(default, rename = "momentId")]
    pub moment_id: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    /// Predicate → value(s), named by the producer: full IRIs, `prefix:local`,
    /// or bare names (default prefix). Unresolvable = 400, nothing written.
    #[serde(default)]
    pub fields: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub provenance: Option<serde_json::Value>,
    #[serde(default)]
    pub meta: Option<serde_json::Value>,
    #[serde(default)]
    pub timestamps: Option<serde_json::Value>,
    #[serde(default)]
    pub render: Option<serde_json::Value>,
    #[serde(default)]
    pub png_b64: Option<String>,
    #[serde(default)]
    pub bytes_b64: Option<String>,
    /// MIME type of the bytes. Default: image/png.
    #[serde(default)]
    pub content_type: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct Delivered {
    /// `<pan/Image/k7m2p9x4>` — the identity, bracket form.
    pub id: String,
    pub iri: String,
    pub store: String,
    pub media_path: String,
    pub created_at: String,
    pub thumbnail: bool,
    /// Parts of the delivery pand received but has no declared vocabulary
    /// to record (prompt, provenance, meta, timestamps, render). Reported so
    /// nothing is dropped silently; recording them is an open ruling.
    pub not_recorded: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct FactsResponse {
    pub id: String,
    pub store: String,
    pub facts: HashMap<String, Vec<String>>,
}

#[derive(Serialize, ToSchema)]
pub struct StageStatus {
    /// done | pending | holding | off
    pub status: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
}

#[derive(Serialize, ToSchema)]
pub struct StateResponse {
    pub id: String,
    pub iri: String,
    pub store: String,
    pub media_type: String,
    pub created_at: String,
    pub thumbnail: bool,
    pub stages: HashMap<String, StageStatus>,
}

#[derive(Deserialize, ToSchema)]
pub struct QueryBody {
    #[serde(default, alias = "soul")]
    pub store: Option<String>,
    pub query: String,
}

#[derive(Deserialize, ToSchema)]
pub struct SearchBody {
    #[serde(default, alias = "soul")]
    pub store: Option<String>,
    #[serde(default)]
    pub r#where: String,
    pub vector: Vec<f32>,
    pub k: Option<usize>,
    pub index: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct SearchResponse {
    pub hits: Vec<Hit>,
}

#[derive(Serialize, ToSchema)]
pub struct Hit {
    pub id: String,
    pub score: f32,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: String,
}

// ── Errors ──────────────────────────────────────────────────────────────────

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}

fn map_err(e: anyhow::Error) -> ApiError {
    let msg = format!("{e:#}");
    if msg.contains("not found") || msg.contains("unknown store") {
        ApiError(StatusCode::NOT_FOUND, msg)
    } else if msg.contains("unknown prefix")
        || msg.contains("invalid")
        || msg.contains("SPARQL error")
        || msg.contains("does not match index")
        || msg.contains("no such index")
        || msg.contains("index name")
        || msg.contains("empty")
        || msg.contains("search where-clause")
        || msg.contains("ambiguous")
    {
        ApiError(StatusCode::BAD_REQUEST, msg)
    } else {
        tracing::error!("internal error: {msg}");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
    }
}

fn media_iri_bracket(iri: &str) -> String {
    bracket_iri(iri)
}

// ── Handlers ────────────────────────────────────────────────────────────────

#[utoipa::path(get, path = "/health", tag = "meta", responses((status = 200, body = HealthResponse)))]
async fn health(State(d): State<Shared>) -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_secs: d.started.elapsed().as_secs(),
        stores: d.stores.iter().map(|s| s.entry.id.clone()).collect(),
        default: d.default_id.clone(),
        stages: d.cfg.models.iter().map(|(k, v)| (k.clone(), v.model.clone())).collect(),
    })
}

#[utoipa::path(get, path = "/stores", tag = "meta", responses((status = 200, body = Vec<StoreInfo>)))]
async fn stores(State(d): State<Shared>) -> Json<Vec<StoreInfo>> {
    Json(
        d.stores
            .iter()
            .map(|s| StoreInfo {
                id: s.entry.id.clone(),
                root: s.entry.root.display().to_string(),
                is_default: s.entry.id == d.default_id,
                indexes: s
                    .pan
                    .index_stats()
                    .into_iter()
                    .map(|(n, st)| (n, IndexInfo { dim: st.dim, count: st.count }))
                    .collect(),
            })
            .collect(),
    )
}

#[utoipa::path(post, path = "/media", tag = "media", request_body = DeliveryBody,
    responses((status = 201, body = Delivered), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn deliver(State(d): State<Shared>, Json(body): Json<DeliveryBody>) -> Result<(StatusCode, Json<Delivered>), ApiError> {
    use base64::Engine;
    let store = d.store_for(body.soul.as_deref()).map_err(map_err)?;
    let b64 = body
        .png_b64
        .as_deref()
        .or(body.bytes_b64.as_deref())
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "delivery carries no png_b64 / bytes_b64".into()))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("media is not valid base64: {e}")))?;
    if bytes.is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "empty media".into()));
    }
    let content_type = body.content_type.clone().unwrap_or_else(|| "image/png".to_string());

    let mut facts = Facts::new();
    if let Some(m) = body.moment_id.as_deref().filter(|m| !m.trim().is_empty()) {
        facts.insert("copia:momentId", m.trim());
    }
    for (pred, val) in &body.fields {
        match val {
            serde_json::Value::Array(items) => {
                for item in items {
                    facts.insert(pred.clone(), json_scalar(item)?);
                }
            }
            other => facts.insert(pred.clone(), json_scalar(other)?),
        }
    }
    let mut not_recorded = Vec::new();
    for (name, present) in [
        ("prompt", body.prompt.is_some()),
        ("provenance", body.provenance.is_some()),
        ("meta", body.meta.is_some()),
        ("timestamps", body.timestamps.is_some()),
        ("render", body.render.is_some()),
    ] {
        if present {
            not_recorded.push(name.to_string());
        }
    }

    let s = store.clone();
    let res = tokio::task::spawn_blocking(move || s.pan.put(&bytes, Some(&content_type), facts))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(map_err)?;
    tracing::info!(store = %store.entry.id, id = %res.id, "stored");
    Ok((
        StatusCode::CREATED,
        Json(Delivered {
            id: media_iri_bracket(&res.iri),
            iri: res.iri,
            store: store.entry.id.clone(),
            media_path: res.media_path,
            created_at: res.created_at,
            thumbnail: res.thumbnail,
            not_recorded,
        }),
    ))
}

fn json_scalar(v: &serde_json::Value) -> Result<String, ApiError> {
    match v {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        serde_json::Value::Bool(b) => Ok(b.to_string()),
        other => Err(ApiError(StatusCode::BAD_REQUEST, format!("fact values must be scalars or arrays of scalars, got: {other}"))),
    }
}

fn locate(d: &Daemon, given: &str) -> Result<(Arc<super::StoreHandle>, String), ApiError> {
    let id = bare_id(given);
    let store = d.locate(&id).map_err(map_err)?.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("id not found: {given}")))?;
    Ok((store, id))
}

#[utoipa::path(get, path = "/media/{id}", tag = "media", params(("id" = String, Path, description = "<pan/Image/x>, full IRI, or bare id")),
    responses((status = 200, description = "The media bytes"), (status = 404, body = ErrorBody)))]
async fn get_media(State(d): State<Shared>, AxPath(given): AxPath<String>) -> Result<Response, ApiError> {
    let (store, id) = locate(&d, &given)?;
    let (bytes, facts) = tokio::task::spawn_blocking(move || store.pan.get(&id))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(map_err)?;
    let media_type = facts
        .iter()
        .find(|(p, _)| p == &format!("{PAN_NS}mediaType"))
        .and_then(|(_, v)| v.first().cloned())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    Ok(([(header::CONTENT_TYPE, media_type)], bytes).into_response())
}

#[utoipa::path(get, path = "/media/{id}/thumbnail", tag = "media", params(("id" = String, Path)),
    responses((status = 200, description = "JPEG thumbnail"), (status = 404, body = ErrorBody)))]
async fn get_thumbnail(State(d): State<Shared>, AxPath(given): AxPath<String>) -> Result<Response, ApiError> {
    let (store, id) = locate(&d, &given)?;
    let facts = store.pan.facts_for(&id).map_err(map_err)?;
    let node = facts
        .iter()
        .find(|(p, _)| p == &format!("{PAN_NS}thumbnail"))
        .and_then(|(_, v)| v.first().cloned())
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no thumbnail for {given}")))?;
    let path = store
        .pan
        .node_field(&node, "path")
        .map_err(map_err)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "thumbnail node has no path".into()))?;
    let abs = store.pan.layout.abs(&path);
    let bytes = tokio::fs::read(&abs).await.map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("read thumbnail: {e}")))?;
    Ok(([(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response())
}

#[utoipa::path(delete, path = "/media/{id}", tag = "media", params(("id" = String, Path)),
    responses((status = 204), (status = 404, body = ErrorBody)))]
async fn delete_media(State(d): State<Shared>, AxPath(given): AxPath<String>) -> Result<StatusCode, ApiError> {
    let (store, id) = locate(&d, &given)?;
    tokio::task::spawn_blocking(move || {
        store.pan.delete(&id)?;
        store.pan.flush()
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(map_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get, path = "/media/{id}/facts", tag = "media", params(("id" = String, Path)),
    responses((status = 200, body = FactsResponse), (status = 404, body = ErrorBody)))]
async fn get_facts(State(d): State<Shared>, AxPath(given): AxPath<String>) -> Result<Json<FactsResponse>, ApiError> {
    let (store, id) = locate(&d, &given)?;
    let facts = store.pan.facts_for(&id).map_err(map_err)?;
    let iri = store.pan.subject_for(&id).map_err(map_err)?.map(|n| n.into_string()).unwrap_or_default();
    Ok(Json(FactsResponse {
        id: media_iri_bracket(&iri),
        store: store.entry.id.clone(),
        facts: facts.into_iter().collect(),
    }))
}

#[utoipa::path(get, path = "/media/{id}/state", tag = "media", params(("id" = String, Path)),
    responses((status = 200, body = StateResponse), (status = 404, body = ErrorBody)))]
async fn get_state(State(d): State<Shared>, AxPath(given): AxPath<String>) -> Result<Json<StateResponse>, ApiError> {
    let (store, id) = locate(&d, &given)?;
    let st: MediaState = store
        .pan
        .state_for(&id)
        .map_err(map_err)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("id not found: {given}")))?;
    let present = |link: &str| -> Vec<String> {
        st.enrichment.iter().find(|(l, _)| l == link).map(|(_, m)| m.clone()).unwrap_or_default()
    };
    let mut stages_out = HashMap::new();
    for stage in [stages::STAGE_EMBED, stages::STAGE_CAPTION, stages::STAGE_POSE, stages::STAGE_SAM3] {
        let link = stages::link_for(stage).unwrap_or_default();
        let models = present(link);
        let status = match d.cfg.models.get(stage) {
            None => StageStatus { status: if models.is_empty() { "off".into() } else { "done".into() }, models, error: None, terminal: None },
            Some(ep) => {
                if models.iter().any(|m| m == &ep.model) {
                    StageStatus { status: "done".into(), models, error: None, terminal: None }
                } else if let Some(a) = d.last_attempt(&store.entry.id, &id, stage) {
                    StageStatus { status: "holding".into(), models, error: Some(a.error), terminal: Some(a.terminal) }
                } else {
                    StageStatus { status: "pending".into(), models, error: None, terminal: None }
                }
            }
        };
        stages_out.insert(stage.to_string(), status);
    }
    Ok(Json(StateResponse {
        id: media_iri_bracket(&st.iri),
        iri: st.iri,
        store: store.entry.id.clone(),
        media_type: st.media_type,
        created_at: st.created_at,
        thumbnail: st.thumbnail,
        stages: stages_out,
    }))
}

#[utoipa::path(post, path = "/query", tag = "query", request_body = QueryBody,
    responses((status = 200, description = "W3C sparql-results+json (SELECT/ASK) or N-Triples (CONSTRUCT/DESCRIBE)"), (status = 400, body = ErrorBody)))]
async fn query(State(d): State<Shared>, Json(body): Json<QueryBody>) -> Result<Response, ApiError> {
    let store = d.store_for(body.store.as_deref()).map_err(map_err)?;
    let results = store.pan.query(&body.query).map_err(map_err)?;
    serialize_results(results)
}

/// SPARQL 1.1 Protocol, per store — what git-lex and Syrinx federate to.
/// GET `?query=` · POST `application/sparql-query` (raw) ·
/// POST `application/x-www-form-urlencoded` (`query=`). Results as
/// `application/sparql-results+json` (SELECT/ASK) or N-Triples.
#[utoipa::path(post, path = "/stores/{id}/sparql", tag = "query",
    params(("id" = String, Path, description = "store id: a soul's genesis SHA or a bare store id")),
    request_body(content = String, content_type = "application/sparql-query"),
    responses((status = 200, description = "W3C sparql-results+json or N-Triples"), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn store_sparql_post(
    State(d): State<Shared>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let sparql = if ct.starts_with("application/x-www-form-urlencoded") {
        form_query(&body).ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "form body has no query= field".into()))?
    } else {
        body
    };
    run_sparql(&d, &id, &sparql).await
}

#[utoipa::path(get, path = "/stores/{id}/sparql", tag = "query",
    params(("id" = String, Path), ("query" = String, Query, description = "the SPARQL query")),
    responses((status = 200, description = "W3C sparql-results+json or N-Triples"), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn store_sparql_get(
    State(d): State<Shared>,
    AxPath(id): AxPath<String>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let sparql = q.get("query").cloned().ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "missing ?query=".into()))?;
    run_sparql(&d, &id, &sparql).await
}

fn form_query(body: &str) -> Option<String> {
    for pair in body.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == "query" {
            return urlencoding_decode(v);
        }
    }
    None
}

fn urlencoding_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let h = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(h, 16).ok()?);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

async fn run_sparql(d: &Daemon, id: &str, sparql: &str) -> Result<Response, ApiError> {
    let store = d.store(id).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("unknown store: {id}")))?;
    let results = store.pan.query(sparql).map_err(map_err)?;
    serialize_results(results)
}

fn serialize_results(results: crate::QueryResults) -> Result<Response, ApiError> {
    use oxigraph::sparql::results::{QueryResultsFormat, QueryResultsSerializer};
    let ser = QueryResultsSerializer::from_format(QueryResultsFormat::Json);
    let internal = |e: String| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e);
    match results {
        crate::QueryResults::Solutions(solutions) => {
            let mut w = ser
                .serialize_solutions_to_writer(Vec::new(), solutions.variables().to_vec())
                .map_err(|e| internal(format!("serialize results: {e}")))?;
            for sol in solutions {
                let sol = sol.map_err(|e| internal(format!("read solution: {e}")))?;
                w.serialize(&sol).map_err(|e| internal(format!("serialize solution: {e}")))?;
            }
            let buf = w.finish().map_err(|e| internal(format!("finish results: {e}")))?;
            Ok(([(header::CONTENT_TYPE, "application/sparql-results+json")], buf).into_response())
        }
        crate::QueryResults::Boolean(b) => {
            let buf = ser.serialize_boolean_to_writer(Vec::new(), b).map_err(|e| internal(format!("serialize boolean: {e}")))?;
            Ok(([(header::CONTENT_TYPE, "application/sparql-results+json")], buf).into_response())
        }
        crate::QueryResults::Graph(triples) => {
            let mut w = oxigraph::io::RdfSerializer::from_format(oxigraph::io::RdfFormat::NTriples).for_writer(Vec::new());
            for t in triples {
                let t = t.map_err(|e| internal(format!("read triple: {e}")))?;
                w.serialize_triple(t.as_ref()).map_err(|e| internal(format!("serialize triple: {e}")))?;
            }
            let buf = w.finish().map_err(|e| internal(format!("finish graph: {e}")))?;
            Ok(([(header::CONTENT_TYPE, "application/n-triples")], buf).into_response())
        }
    }
}

#[utoipa::path(post, path = "/search", tag = "query", request_body = SearchBody,
    responses((status = 200, body = SearchResponse), (status = 400, body = ErrorBody)))]
async fn search(State(d): State<Shared>, Json(body): Json<SearchBody>) -> Result<Json<SearchResponse>, ApiError> {
    let store = d.store_for(body.store.as_deref()).map_err(map_err)?;
    // Default index = the embedding model pand is configured with (indexes
    // are named by model); a store's own index_id is the fallback when no
    // embed stage is configured.
    let index = body.index.unwrap_or_else(|| {
        d.cfg
            .models
            .get(stages::STAGE_EMBED)
            .map(|m| m.model.clone())
            .unwrap_or_else(|| store.pan.cfg.index_id.clone())
    });
    let k = body.k.unwrap_or(10);
    let hits = store.pan.search(&body.r#where, &body.vector, k, &index).map_err(map_err)?;
    let mut out = Vec::with_capacity(hits.len());
    for h in hits {
        let iri = store.pan.subject_for(&h.id).map_err(map_err)?.map(|n| n.into_string()).unwrap_or(h.id.clone());
        out.push(Hit { id: media_iri_bracket(&iri), score: h.score });
    }
    Ok(Json(SearchResponse { hits: out }))
}

// ── Router ──────────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(
    info(title = "pand", description = "The Pan daemon: every media store on this machine, one door. This document IS the interface spec."),
    paths(health, stores, deliver, get_media, get_thumbnail, delete_media, get_facts, get_state, query, search, store_sparql_get, store_sparql_post),
    components(schemas(HealthResponse, StoreInfo, IndexInfo, DeliveryBody, Delivered, FactsResponse, StageStatus, StateResponse, QueryBody, SearchBody, SearchResponse, Hit, ErrorBody)),
    tags(
        (name = "meta", description = "Daemon + store status"),
        (name = "media", description = "Deliver, read, describe, delete"),
        (name = "query", description = "SPARQL and SPARQL+vector fusion, per store")
    )
)]
pub struct ApiDoc;

pub fn router(d: Shared) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/stores", get(stores))
        .route("/media", post(deliver))
        .route("/media/{id}", get(get_media).delete(delete_media))
        .route("/media/{id}/thumbnail", get(get_thumbnail))
        .route("/media/{id}/facts", get(get_facts))
        .route("/media/{id}/state", get(get_state))
        .route("/query", post(query))
        .route("/search", post(search))
        .route("/stores/{id}/sparql", get(store_sparql_get).post(store_sparql_post))
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(d)
}

/// Serve + run the stage ladder until ctrl-c. Flushes every store on the way out.
pub async fn serve(d: Shared) -> anyhow::Result<()> {
    let app = router(d.clone());
    let addr = format!("{}:{}", d.cfg.bind, d.cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("pand serving on http://{addr} (swagger at /swagger-ui); {} store(s), default {}", d.stores.len(), d.default_id);
    let ladder = tokio::spawn(stages::run(d.clone()));
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    ladder.abort();
    for s in &d.stores {
        s.pan.flush()?;
    }
    Ok(())
}
