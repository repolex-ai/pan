//! Pan's HTTP surface — one fast API, Swagger-documented from commit one.
//!
//! The Swagger/OpenAPI doc IS the interface spec (self-documenting, live,
//! verifiable against the running server; no prose API doc to drift). Two
//! capability groups: CRUD (media) and Query (SPARQL, SPARQL+vector).
//!
//! REWRITE_CLEAN from Pool's serve.rs: the store operations carried forward,
//! the queue/render/soul-routing cruft left behind. Mode 1 has ONE store —
//! there is no `?soul=`, no registry, no home fallback, by construction.

use axum::body::Bytes;
use axum::extract::{Path as AxPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

use crate::detectors::Detectors;
use crate::{Facts, Pan, SearchHit};

pub struct AppState {
    pub pan: Pan,
    pub detectors: Detectors,
}

type SharedState = Arc<AppState>;

// ── DTOs ────────────────────────────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct HealthResponse {
    pub ok: bool,
    /// Identity of this store (one value, deliberately boring).
    pub storage_id: String,
    pub version: String,
}

#[derive(Serialize, ToSchema)]
pub struct InfoResponse {
    pub storage_id: String,
    pub version: String,
    pub root: String,
    pub storage_root: String,
    pub default_index: String,
    /// Configured detector roles (endpoints are config, not disclosed).
    pub detectors: Vec<String>,
    pub indexes: HashMap<String, IndexInfo>,
}

#[derive(Serialize, ToSchema)]
pub struct IndexInfo {
    pub dim: usize,
    pub count: usize,
}

#[derive(Serialize, ToSchema)]
pub struct MediaCreated {
    /// The assigned identity — short, random, new on EVERY put (two puts of
    /// the same bytes are two different media objects).
    pub pan_id: String,
    /// The full subject IRI minted for this object,
    /// e.g. `https://repolex.ai/ontology/pan/image/<panId>`.
    pub subject: String,
    pub blob_path: String,
    pub created_at: String,
    /// True when the embed detector ran and the vector was indexed.
    pub embedded: bool,
    /// Present when an embed was attempted and failed (media is stored regardless).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embed_error: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct FactsBody {
    /// Predicate → value(s). Predicates are full IRIs, `prefix:local` short
    /// forms, or bare names (expanded via the default prefix). Unresolvable
    /// predicates fail the whole request — loud, never a silent drop.
    pub facts: HashMap<String, serde_json::Value>,
}

#[derive(Serialize, ToSchema)]
pub struct FactsResponse {
    pub pan_id: String,
    /// Full-IRI predicate → values.
    pub facts: HashMap<String, Vec<String>>,
}

#[derive(Deserialize, ToSchema)]
pub struct VectorBody {
    /// 1-D, L2-normalized embedding. Dim must match the index (fixed by its
    /// first insert).
    pub vector: Vec<f32>,
}

#[derive(Serialize, ToSchema)]
pub struct VectorAdded {
    pub added: bool,
    pub index: String,
}

#[derive(Deserialize, ToSchema)]
pub struct QueryBody {
    /// SPARQL. Store prefixes (pan: + pan.yml extras + rdf/rdfs/owl/xsd) are
    /// pre-declared; results are W3C `application/sparql-results+json`.
    pub query: String,
}

#[derive(Deserialize, ToSchema)]
pub struct SearchBody {
    /// SPARQL graph pattern gating the candidate set. `?s` is the media
    /// subject and `?id` is pre-bound via `?s pan:panId ?id`. Empty/absent =
    /// no graph gate (pure kNN).
    #[serde(default)]
    pub r#where: String,
    /// The query embedding (dim must match the index).
    pub vector: Vec<f32>,
    /// Max hits. Default 10.
    pub k: Option<usize>,
    /// Index name. Default: the store's configured index_id.
    pub index: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct SearchResponse {
    pub hits: Vec<Hit>,
}

#[derive(Serialize, ToSchema)]
pub struct Hit {
    pub pan_id: String,
    /// Cosine similarity, 1.0 = identical direction.
    pub score: f32,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: String,
}

// ── Error mapping ───────────────────────────────────────────────────────────

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}

fn map_err(e: anyhow::Error) -> ApiError {
    let msg = format!("{e:#}");
    if msg.contains("not found") {
        ApiError(StatusCode::NOT_FOUND, msg)
    } else if msg.contains("unknown prefix")
        || msg.contains("invalid")
        || msg.contains("SPARQL error")
        || msg.contains("does not match index")
        || msg.contains("no such index")
        || msg.contains("index name")
        || msg.contains("empty vector")
        || msg.contains("search where-clause")
    {
        // 4xx = the CALLER's mistake; the message is about their input, safe to
        // return verbatim so they can fix it.
        ApiError(StatusCode::BAD_REQUEST, msg)
    } else {
        // 5xx = OUR internals (fs paths, store internals). Log the detail
        // server-side; hand the client a generic message so absolute paths and
        // storage layout never leak over the wire.
        tracing::error!("internal error: {msg}");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
    }
}

// ── Handlers ────────────────────────────────────────────────────────────────

#[utoipa::path(get, path = "/health", tag = "meta",
    responses((status = 200, body = HealthResponse)))]
async fn health(State(st): State<SharedState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        storage_id: st.pan.cfg.storage_id.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

#[utoipa::path(get, path = "/info", tag = "meta",
    responses((status = 200, body = InfoResponse)))]
async fn info(State(st): State<SharedState>) -> Json<InfoResponse> {
    let indexes = st
        .pan
        .index_stats()
        .into_iter()
        .map(|(name, s)| (name, IndexInfo { dim: s.dim, count: s.count }))
        .collect();
    Json(InfoResponse {
        storage_id: st.pan.cfg.storage_id.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        root: st.pan.layout.root.display().to_string(),
        storage_root: st.pan.layout.storage_root.display().to_string(),
        default_index: st.pan.cfg.index_id.clone(),
        detectors: {
            let mut d: Vec<String> = st.pan.cfg.detectors.keys().cloned().collect();
            d.sort();
            d
        },
        indexes,
    })
}

#[utoipa::path(post, path = "/media", tag = "media",
    request_body(content = [u8], content_type = "application/octet-stream",
        description = "Raw media bytes (v1: images). Content-Type header is recorded as pan:mediaType."),
    responses(
        (status = 201, body = MediaCreated),
        (status = 400, body = ErrorBody)))]
async fn put_media(
    State(st): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<MediaCreated>), ApiError> {
    if body.is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "empty body".into()));
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|s| *s != "application/octet-stream")
        .map(String::from);

    let res = st
        .pan
        .put(&body, content_type.as_deref(), Facts::new())
        .map_err(map_err)?;

    // Embed best-effort: a configured detector that fails never fails the
    // store — the media is in, the miss is reported.
    let mut embedded = false;
    let mut embed_error = None;
    let media_type = content_type.unwrap_or_else(|| "image/png".to_string());
    match st.detectors.embed(&body, &media_type).await {
        Ok(Some(vec)) => match st.pan.add_vector(&res.pan_id, &st.pan.cfg.index_id, &vec) {
            Ok(_) => {
                embedded = true;
                st.pan.flush().map_err(map_err)?;
            }
            Err(e) => embed_error = Some(format!("{e:#}")),
        },
        Ok(None) => {} // not configured — stage skipped
        Err(e) => embed_error = Some(format!("{e:#}")),
    }

    Ok((
        StatusCode::CREATED,
        Json(MediaCreated {
            pan_id: res.pan_id,
            subject: res.subject,
            blob_path: res.blob_path,
            created_at: res.created_at,
            embedded,
            embed_error,
        }),
    ))
}

#[utoipa::path(get, path = "/media/{pan_id}", tag = "media",
    params(("pan_id" = String, Path, description = "The assigned panId")),
    responses(
        (status = 200, description = "The media bytes", content_type = "application/octet-stream"),
        (status = 404, body = ErrorBody)))]
async fn get_media(
    State(st): State<SharedState>,
    AxPath(pan_id): AxPath<String>,
) -> Result<Response, ApiError> {
    let (bytes, facts) = st.pan.get(&pan_id).map_err(map_err)?;
    let media_type = facts
        .iter()
        .find(|(p, _)| p == &format!("{}mediaType", crate::PAN_NS))
        .and_then(|(_, v)| v.first().cloned())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    Ok(([(header::CONTENT_TYPE, media_type)], bytes).into_response())
}

#[utoipa::path(delete, path = "/media/{pan_id}", tag = "media",
    params(("pan_id" = String, Path)),
    responses((status = 204), (status = 404, body = ErrorBody)))]
async fn delete_media(
    State(st): State<SharedState>,
    AxPath(pan_id): AxPath<String>,
) -> Result<StatusCode, ApiError> {
    st.pan.delete(&pan_id).map_err(map_err)?;
    st.pan.flush().map_err(map_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get, path = "/media/{pan_id}/facts", tag = "media",
    params(("pan_id" = String, Path)),
    responses((status = 200, body = FactsResponse), (status = 404, body = ErrorBody)))]
async fn get_facts(
    State(st): State<SharedState>,
    AxPath(pan_id): AxPath<String>,
) -> Result<Json<FactsResponse>, ApiError> {
    let facts = st.pan.facts_for(&pan_id).map_err(map_err)?;
    if facts.is_empty() {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("panId not found: {pan_id}")));
    }
    Ok(Json(FactsResponse {
        pan_id,
        facts: facts.into_iter().collect(),
    }))
}

#[utoipa::path(put, path = "/media/{pan_id}/facts", tag = "media",
    params(("pan_id" = String, Path)),
    request_body = FactsBody,
    responses(
        (status = 200, body = FactsResponse),
        (status = 400, body = ErrorBody, description = "Unresolvable predicate — nothing written"),
        (status = 404, body = ErrorBody)))]
async fn put_facts(
    State(st): State<SharedState>,
    AxPath(pan_id): AxPath<String>,
    Json(body): Json<FactsBody>,
) -> Result<Json<FactsResponse>, ApiError> {
    let mut facts = Facts::new();
    for (pred, val) in body.facts {
        match val {
            serde_json::Value::Array(items) => {
                for item in items {
                    facts.insert(&pred, json_scalar(&item)?);
                }
            }
            other => facts.insert(&pred, json_scalar(&other)?),
        }
    }
    st.pan.describe(&pan_id, facts).map_err(map_err)?;
    let out = st.pan.facts_for(&pan_id).map_err(map_err)?;
    Ok(Json(FactsResponse {
        pan_id,
        facts: out.into_iter().collect(),
    }))
}

fn json_scalar(v: &serde_json::Value) -> Result<String, ApiError> {
    match v {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        serde_json::Value::Bool(b) => Ok(b.to_string()),
        other => Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("fact values must be scalars or arrays of scalars, got: {other}"),
        )),
    }
}

#[utoipa::path(put, path = "/media/{pan_id}/vectors/{index}", tag = "vectors",
    params(("pan_id" = String, Path), ("index" = String, Path, description = "Index name (one index per embedder)")),
    request_body = VectorBody,
    responses((status = 200, body = VectorAdded), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn put_vector(
    State(st): State<SharedState>,
    AxPath((pan_id, index)): AxPath<(String, String)>,
    Json(body): Json<VectorBody>,
) -> Result<Json<VectorAdded>, ApiError> {
    if st.pan.facts_for(&pan_id).map_err(map_err)?.is_empty() {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("panId not found: {pan_id}")));
    }
    if body.vector.is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "empty vector".into()));
    }
    let added = st.pan.add_vector(&pan_id, &index, &body.vector).map_err(map_err)?;
    st.pan.flush().map_err(map_err)?;
    Ok(Json(VectorAdded { added, index }))
}

#[utoipa::path(post, path = "/query", tag = "query",
    request_body = QueryBody,
    responses(
        (status = 200, description = "W3C application/sparql-results+json (SELECT/ASK) or N-Triples (CONSTRUCT/DESCRIBE)"),
        (status = 400, body = ErrorBody)))]
async fn query(
    State(st): State<SharedState>,
    Json(body): Json<QueryBody>,
) -> Result<Response, ApiError> {
    use oxigraph::sparql::results::{QueryResultsFormat, QueryResultsSerializer};
    let results = st.pan.query(&body.query).map_err(map_err)?;
    let ser = QueryResultsSerializer::from_format(QueryResultsFormat::Json);
    let internal = |e: String| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e);
    match results {
        crate::QueryResults::Solutions(solutions) => {
            let mut w = ser
                .serialize_solutions_to_writer(Vec::new(), solutions.variables().to_vec())
                .map_err(|e| internal(format!("serialize results: {e}")))?;
            for sol in solutions {
                let sol = sol.map_err(|e| internal(format!("read solution: {e}")))?;
                w.serialize(&sol)
                    .map_err(|e| internal(format!("serialize solution: {e}")))?;
            }
            let buf = w
                .finish()
                .map_err(|e| internal(format!("finish results: {e}")))?;
            Ok((
                [(header::CONTENT_TYPE, "application/sparql-results+json")],
                buf,
            )
                .into_response())
        }
        crate::QueryResults::Boolean(b) => {
            let buf = ser
                .serialize_boolean_to_writer(Vec::new(), b)
                .map_err(|e| internal(format!("serialize boolean: {e}")))?;
            Ok((
                [(header::CONTENT_TYPE, "application/sparql-results+json")],
                buf,
            )
                .into_response())
        }
        crate::QueryResults::Graph(triples) => {
            let mut w = oxigraph::io::RdfSerializer::from_format(oxigraph::io::RdfFormat::NTriples)
                .for_writer(Vec::new());
            for t in triples {
                let t = t.map_err(|e| internal(format!("read triple: {e}")))?;
                w.serialize_triple(t.as_ref())
                    .map_err(|e| internal(format!("serialize triple: {e}")))?;
            }
            let buf = w.finish().map_err(|e| internal(format!("finish graph: {e}")))?;
            Ok(([(header::CONTENT_TYPE, "application/n-triples")], buf).into_response())
        }
    }
}

#[utoipa::path(post, path = "/search", tag = "query",
    request_body = SearchBody,
    responses((status = 200, body = SearchResponse), (status = 400, body = ErrorBody)))]
async fn search(
    State(st): State<SharedState>,
    Json(body): Json<SearchBody>,
) -> Result<Json<SearchResponse>, ApiError> {
    let index = body.index.unwrap_or_else(|| st.pan.cfg.index_id.clone());
    let k = body.k.unwrap_or(10);
    let hits: Vec<SearchHit> = st
        .pan
        .search(&body.r#where, &body.vector, k, &index)
        .map_err(map_err)?;
    Ok(Json(SearchResponse {
        hits: hits
            .into_iter()
            .map(|h| Hit { pan_id: h.pan_id, score: h.score })
            .collect(),
    }))
}

// ── Router + OpenAPI ────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Pan",
        description = "A standalone media store that speaks git-lex: stores media, describes it with a graph, searches by graph pattern AND vector similarity. This document IS the interface spec.",
    ),
    paths(health, info, put_media, get_media, delete_media, get_facts, put_facts, put_vector, query, search),
    components(schemas(
        HealthResponse, InfoResponse, IndexInfo, MediaCreated, FactsBody, FactsResponse,
        VectorBody, VectorAdded, QueryBody, SearchBody, SearchResponse, Hit, ErrorBody
    )),
    tags(
        (name = "meta", description = "Store identity + status"),
        (name = "media", description = "CRUD — media objects (assigned panId) + facts"),
        (name = "vectors", description = "Attach embeddings (the Iris two-call flow: put, then vector)"),
        (name = "query", description = "SPARQL and SPARQL+vector fusion")
    )
)]
pub struct ApiDoc;

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/info", get(info))
        .route("/media", post(put_media))
        .route("/media/{pan_id}", get(get_media).delete(delete_media))
        .route("/media/{pan_id}/facts", get(get_facts).put(put_facts))
        .route("/media/{pan_id}/vectors/{index}", put(put_vector))
        .route("/query", post(query))
        .route("/search", post(search))
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// Open the store and serve until ctrl-c.
pub async fn serve(root: &std::path::Path, bind: &str, port: u16) -> anyhow::Result<()> {
    let pan = Pan::open(root)?;
    let detectors = Detectors::new(pan.cfg.detectors.clone());
    tracing::info!(
        storage_id = %pan.cfg.storage_id,
        root = %pan.layout.root.display(),
        "pan store open"
    );
    let state: SharedState = Arc::new(AppState { pan, detectors });
    let app = router(state.clone());

    let addr = format!("{bind}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("pan serving on http://{addr} (swagger at /swagger-ui)");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    state.pan.flush()?;
    Ok(())
}
