//! pand's HTTP surface — Swagger-documented; the OpenAPI doc IS the spec.
//!
//! Every media call is a GRAPH call: nothing here reads the filesystem
//! except through the store, and `GET /media/{id}` serves bytes at the path
//! the graph declares. A store is named by its id (six characters: the start
//! of the soul's genesis SHA, or
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
use crate::{bare_id, bracket_iri, MediaState, PAN_NS};

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
    pub pid: u32,
    /// Since this process started.
    pub images_stored: u64,
    /// Calls made to model endpoints since this process started.
    pub model_calls: u64,
    /// Per store: images and how many of them have each derived record.
    pub counts: Vec<StoreCountsOut>,
    /// Per stage: calls in flight allowed right now / the configured ceiling.
    pub windows: HashMap<String, String>,
}

#[derive(Serialize, ToSchema)]
pub struct StoreCountsOut {
    pub store: String,
    pub images: u64,
    pub thumbnails: u64,
    pub captions: u64,
    pub embeddings: u64,
    pub poses: u64,
    pub regions: u64,
    pub depths: u64,
}

/// The receipt for one stored file.
#[derive(Serialize, ToSchema)]
pub struct Delivered {
    /// `<pan/Image/k7m2p9x4>` — the identity, bracket form.
    pub id: String,
    pub iri: String,
    pub store: String,
    pub media_path: String,
    pub created_date: String,
    pub thumbnail: bool,
    /// Statements read from the XMP the file arrived with and loaded into
    /// the graph. A producer that wrote a block into the image can assert
    /// this is non-zero.
    pub statements: usize,
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
    pub created_date: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enrichment_complete_date: Option<String>,
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

/// One imageset: its three facts and, when asked for by id, its members.
#[derive(Serialize, ToSchema)]
pub struct ImageSetResponse {
    /// `<pan/ImageSet/abcd2345>` — the identity, bracket form.
    pub id: String,
    pub iri: String,
    pub store: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_date: String,
    /// The media whose `pan:relatedToId` names this set, bracket form.
    /// Omitted from the list; present on one set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<Vec<String>>,
}

#[derive(Deserialize, ToSchema)]
pub struct ImageSetCreateBody {
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct ImageSetMemberBody {
    /// `<pan/Image/x>`, the full IRI, or the bare id.
    pub media: String,
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
        || msg.contains("XMP")
        || msg.contains("ambiguous")
        || msg.contains("not a property pan.ttl declares on an image")
        || msg.contains("nothing to set")
        || msg.contains("nothing to unset")
        || msg.contains("imageset file")
        || msg.contains("not a readable image")
        || msg.contains("pan:mediaCreatedDate")
        || msg.contains("could not be made")
    {
        ApiError(StatusCode::BAD_REQUEST, msg)
    } else {
        tracing::error!("internal error: {msg}");
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error".to_string(),
        )
    }
}

fn media_iri_bracket(iri: &str) -> String {
    bracket_iri(iri)
}

// ── Handlers ────────────────────────────────────────────────────────────────

#[utoipa::path(get, path = "/health", tag = "meta", responses((status = 200, body = HealthResponse)))]
async fn health(State(d): State<Shared>) -> Json<HealthResponse> {
    let counts: Vec<StoreCountsOut> = {
        let stores = d.stores.clone();
        tokio::task::spawn_blocking(move || {
            stores
                .iter()
                .map(|s| {
                    let c = s.pan.counts().unwrap_or_default();
                    StoreCountsOut {
                        store: s.entry.id.clone(),
                        images: c.images,
                        thumbnails: c.thumbnails,
                        captions: c.captions,
                        embeddings: c.embeddings,
                        poses: c.poses,
                        regions: c.regions,
                        depths: c.depths,
                    }
                })
                .collect()
        })
        .await
        .unwrap_or_default()
    };
    let windows: HashMap<String, String> = d
        .funnels
        .iter()
        .filter(|(k, _)| d.cfg.models.get(*k).map(|m| m.enabled).unwrap_or(false))
        .map(|(k, l)| (k.clone(), format!("{}/{}", l.window(), l.ceiling())))
        .collect();
    Json(HealthResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_secs: d.started.elapsed().as_secs(),
        stores: d.stores.iter().map(|s| s.entry.id.clone()).collect(),
        default: d.default_id.clone(),
        stages: d
            .cfg
            .models
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    if v.enabled {
                        v.model.clone()
                    } else {
                        format!("{} (off)", v.model)
                    },
                )
            })
            .collect(),
        pid: std::process::id(),
        images_stored: d
            .counters
            .images_stored
            .load(std::sync::atomic::Ordering::Relaxed),
        model_calls: d
            .counters
            .model_calls
            .load(std::sync::atomic::Ordering::Relaxed),
        counts,
        windows,
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
                    .map(|(n, st)| {
                        (
                            n,
                            IndexInfo {
                                dim: st.dim,
                                count: st.count,
                            },
                        )
                    })
                    .collect(),
            })
            .collect(),
    )
}

/// Store a media file in the default store. The request body IS the file:
/// raw bytes, media type in `Content-Type`. Nothing else is sent — whatever
/// XMP the file carries is its metadata (a producer writes its block into the
/// image before handing it over; Rob, 2026-09-04). Pan reads that XMP into
/// the graph, appends its own block, and never touches the rest.
#[utoipa::path(post, path = "/media", tag = "media",
    request_body(content = Vec<u8>, content_type = "image/png", description = "The media file, raw bytes. Content-Type names the media type."),
    responses((status = 201, body = Delivered), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn deliver(
    State(d): State<Shared>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<Delivered>), ApiError> {
    ingest(&d, None, &headers, body).await
}

/// Store a media file in one named store (six characters of a soul's genesis
/// SHA, or a bare
/// store id). Same body as `POST /media`.
#[utoipa::path(post, path = "/stores/{id}/media", tag = "media", params(("id" = String, Path, description = "store id")),
    request_body(content = Vec<u8>, content_type = "image/png", description = "The media file, raw bytes. Content-Type names the media type."),
    responses((status = 201, body = Delivered), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn deliver_to(
    State(d): State<Shared>,
    AxPath(store_id): AxPath<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<Delivered>), ApiError> {
    ingest(&d, Some(&store_id), &headers, body).await
}

async fn ingest(
    d: &Daemon,
    store_id: Option<&str>,
    headers: &axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<Delivered>), ApiError> {
    let store = d.store_for(store_id).map_err(map_err)?;
    if body.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "empty body: the request body must be the media file's bytes".into(),
        ));
    }
    // Media type: the Content-Type header, parameters stripped. A PNG is
    // recognisable without one; anything else must say what it is.
    let declared = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .filter(|s| !s.is_empty() && s != "application/octet-stream");
    let content_type = match declared {
        Some(ct) => ct,
        None if crate::xmp::is_png(&body) => "image/png".to_string(),
        None => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "Content-Type required: the bytes are not a PNG and no media type was given".into(),
            ))
        }
    };
    // Pan stores media. A JSON or form body here is a caller still speaking
    // Pool's wire; say so instead of filing its JSON as an image.
    if !(content_type.starts_with("image/")
        || content_type.starts_with("video/")
        || content_type.starts_with("audio/"))
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("Content-Type {content_type} is not media. The request body must be the file itself (raw bytes, Content-Type image/png etc.); metadata travels inside the file's XMP."),
        ));
    }
    let s = store.clone();
    let bytes = body.to_vec();
    let res = tokio::task::spawn_blocking(move || s.pan.put(&bytes, Some(&content_type)))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(map_err)?;
    d.counters
        .images_stored
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(store = %store.entry.id, id = %res.id, statements = res.statements, "stored");
    Ok((
        StatusCode::CREATED,
        Json(Delivered {
            id: media_iri_bracket(&res.iri),
            iri: res.iri,
            store: store.entry.id.clone(),
            media_path: res.media_path,
            created_date: res.created_date,
            thumbnail: res.thumbnail,
            statements: res.statements,
        }),
    ))
}

#[utoipa::path(post, path = "/media/{id}/set", tag = "media", params(("id" = String, Path)),
    request_body(content = HashMap<String, serde_json::Value>, description = "One JSON object keyed by property local name; only the person-settable fields of pan.ttl (rating, isPicked, isRejected)"),
    responses((status = 200, body = FactsResponse), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn set_fields(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
    Json(body): Json<HashMap<String, serde_json::Value>>,
) -> Result<Json<FactsResponse>, ApiError> {
    let (store, id) = locate(&d, &given)?;
    let mut fields: Vec<(String, serde_json::Value)> = body.into_iter().collect();
    fields.sort_by(|a, b| a.0.cmp(&b.0));
    let s2 = store.clone();
    let id2 = id.clone();
    tokio::task::spawn_blocking(move || s2.pan.set_fields(&id2, &fields))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(map_err)?;
    facts_response(&store, &id).map(Json)
}

#[utoipa::path(post, path = "/media/{id}/unset", tag = "media", params(("id" = String, Path)),
    request_body(content = Vec<String>, description = "The property local names to remove"),
    responses((status = 200, body = FactsResponse), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn unset_fields(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
    Json(body): Json<Vec<String>>,
) -> Result<Json<FactsResponse>, ApiError> {
    let (store, id) = locate(&d, &given)?;
    let s2 = store.clone();
    let id2 = id.clone();
    tokio::task::spawn_blocking(move || s2.pan.unset_fields(&id2, &body))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(map_err)?;
    facts_response(&store, &id).map(Json)
}

fn facts_response(store: &Arc<super::StoreHandle>, id: &str) -> Result<FactsResponse, ApiError> {
    let facts = store.pan.facts_for(id).map_err(map_err)?;
    let iri = store
        .pan
        .subject_for(id)
        .map_err(map_err)?
        .map(|n| n.into_string())
        .unwrap_or_default();
    Ok(FactsResponse {
        id: media_iri_bracket(&iri),
        store: store.entry.id.clone(),
        facts: facts.into_iter().collect(),
    })
}

fn locate(d: &Daemon, given: &str) -> Result<(Arc<super::StoreHandle>, String), ApiError> {
    let id = bare_id(given);
    let store = d
        .locate(&id)
        .map_err(map_err)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("id not found: {given}")))?;
    Ok((store, id))
}

#[utoipa::path(get, path = "/media/{id}", tag = "media", params(("id" = String, Path, description = "<pan/Image/x>, full IRI, or bare id")),
    responses((status = 200, description = "The media bytes"), (status = 404, body = ErrorBody)))]
async fn get_media(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
) -> Result<Response, ApiError> {
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
async fn get_thumbnail(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
) -> Result<Response, ApiError> {
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
    let bytes = tokio::fs::read(&abs).await.map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read thumbnail: {e}"),
        )
    })?;
    Ok(([(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response())
}

#[utoipa::path(delete, path = "/media/{id}", tag = "media", params(("id" = String, Path)),
    responses((status = 204), (status = 404, body = ErrorBody)))]
async fn delete_media(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
) -> Result<StatusCode, ApiError> {
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
async fn get_facts(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
) -> Result<Json<FactsResponse>, ApiError> {
    let (store, id) = locate(&d, &given)?;
    let facts = store.pan.facts_for(&id).map_err(map_err)?;
    let iri = store
        .pan
        .subject_for(&id)
        .map_err(map_err)?
        .map(|n| n.into_string())
        .unwrap_or_default();
    Ok(Json(FactsResponse {
        id: media_iri_bracket(&iri),
        store: store.entry.id.clone(),
        facts: facts.into_iter().collect(),
    }))
}

#[utoipa::path(get, path = "/media/{id}/state", tag = "media", params(("id" = String, Path)),
    responses((status = 200, body = StateResponse), (status = 404, body = ErrorBody)))]
async fn get_state(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
) -> Result<Json<StateResponse>, ApiError> {
    let (store, id) = locate(&d, &given)?;
    let st: MediaState = store
        .pan
        .state_for(&id)
        .map_err(map_err)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("id not found: {given}")))?;
    let present = |link: &str| -> Vec<String> {
        st.enrichment
            .iter()
            .find(|(l, _)| l == link)
            .map(|(_, m)| m.clone())
            .unwrap_or_default()
    };
    let mut stages_out = HashMap::new();
    for stage in [
        stages::STAGE_EMBED,
        stages::STAGE_CAPTION,
        stages::STAGE_POSE,
        stages::STAGE_SEGMENT,
    ] {
        let link = stages::link_for(stage).unwrap_or_default();
        let models = present(link);
        let status = match d.cfg.models.get(stage) {
            None => StageStatus {
                status: if models.is_empty() {
                    "off".into()
                } else {
                    "done".into()
                },
                models,
                error: None,
                terminal: None,
            },
            Some(ep) if !ep.enabled => StageStatus {
                status: if models.iter().any(|m| m == &ep.model) {
                    "done".into()
                } else {
                    "off".into()
                },
                models,
                error: None,
                terminal: None,
            },
            Some(ep) => {
                if models.iter().any(|m| m == &ep.model) {
                    StageStatus {
                        status: "done".into(),
                        models,
                        error: None,
                        terminal: None,
                    }
                } else if let Some(a) = d.last_attempt(&store.entry.id, &id, stage) {
                    StageStatus {
                        status: "holding".into(),
                        models,
                        error: Some(a.error),
                        terminal: Some(a.terminal),
                    }
                } else {
                    StageStatus {
                        status: "pending".into(),
                        models,
                        error: None,
                        terminal: None,
                    }
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
        created_date: st.created_date,
        enrichment_complete_date: st.enrichment_complete_date,
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
    params(("id" = String, Path, description = "store id: six characters, the start of a soul's genesis SHA or of a bare store id")),
    request_body(content = String, content_type = "application/sparql-query"),
    responses((status = 200, description = "W3C sparql-results+json or N-Triples"), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn store_sparql_post(
    State(d): State<Shared>,
    AxPath(id): AxPath<String>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let sparql = if ct.starts_with("application/x-www-form-urlencoded") {
        form_query(&body).ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "form body has no query= field".into(),
            )
        })?
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
    let sparql = q
        .get("query")
        .cloned()
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "missing ?query=".into()))?;
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
    let store = d
        .store(id)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("unknown store: {id}")))?;
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
            let buf = w
                .finish()
                .map_err(|e| internal(format!("finish graph: {e}")))?;
            Ok(([(header::CONTENT_TYPE, "application/n-triples")], buf).into_response())
        }
    }
}

#[utoipa::path(post, path = "/search", tag = "query", request_body = SearchBody,
    responses((status = 200, body = SearchResponse), (status = 400, body = ErrorBody)))]
async fn search(
    State(d): State<Shared>,
    Json(body): Json<SearchBody>,
) -> Result<Json<SearchResponse>, ApiError> {
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
    let hits = store
        .pan
        .search(&body.r#where, &body.vector, k, &index)
        .map_err(map_err)?;
    let mut out = Vec::with_capacity(hits.len());
    for h in hits {
        let iri = store
            .pan
            .subject_for(&h.id)
            .map_err(map_err)?
            .map(|n| n.into_string())
            .unwrap_or(h.id.clone());
        out.push(Hit {
            id: media_iri_bracket(&iri),
            score: h.score,
        });
    }
    Ok(Json(SearchResponse { hits: out }))
}

// ── ImageSets (issue #4; pan.ttl 0.4.2) ─────────────────────────────────────

fn imageset_out(
    store: &Arc<super::StoreHandle>,
    p: crate::ImageSet,
    media: Option<Vec<String>>,
) -> ImageSetResponse {
    ImageSetResponse {
        id: bracket_iri(&p.iri),
        iri: p.iri,
        store: store.entry.id.clone(),
        description: p.description,
        created_date: p.created_date,
        media,
    }
}

#[utoipa::path(get, path = "/imagesets", tag = "imageset",
    responses((status = 200, body = Vec<ImageSetResponse>)))]
async fn imagesets(State(d): State<Shared>) -> Result<Json<Vec<ImageSetResponse>>, ApiError> {
    imagesets_in(&d, None).await
}

#[utoipa::path(get, path = "/stores/{id}/imagesets", tag = "imageset", params(("id" = String, Path, description = "Store id")),
    responses((status = 200, body = Vec<ImageSetResponse>), (status = 404, body = ErrorBody)))]
async fn store_imagesets(
    State(d): State<Shared>,
    AxPath(store_id): AxPath<String>,
) -> Result<Json<Vec<ImageSetResponse>>, ApiError> {
    imagesets_in(&d, Some(&store_id)).await
}

async fn imagesets_in(
    d: &Daemon,
    store_id: Option<&str>,
) -> Result<Json<Vec<ImageSetResponse>>, ApiError> {
    let store = d.store_for(store_id).map_err(map_err)?;
    let sets = store.pan.imageset_list().map_err(map_err)?;
    Ok(Json(
        sets.into_iter()
            .map(|p| imageset_out(&store, p, None))
            .collect(),
    ))
}

#[utoipa::path(post, path = "/imagesets", tag = "imageset",
    request_body(content = ImageSetCreateBody, description = "The set's description; nothing else is on a set"),
    responses((status = 201, body = ImageSetResponse), (status = 400, body = ErrorBody)))]
async fn create_imageset(
    State(d): State<Shared>,
    Json(body): Json<ImageSetCreateBody>,
) -> Result<(StatusCode, Json<ImageSetResponse>), ApiError> {
    create_imageset_in(&d, None, body).await
}

#[utoipa::path(post, path = "/stores/{id}/imagesets", tag = "imageset", params(("id" = String, Path, description = "Store id")),
    request_body(content = ImageSetCreateBody),
    responses((status = 201, body = ImageSetResponse), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn create_store_imageset(
    State(d): State<Shared>,
    AxPath(store_id): AxPath<String>,
    Json(body): Json<ImageSetCreateBody>,
) -> Result<(StatusCode, Json<ImageSetResponse>), ApiError> {
    create_imageset_in(&d, Some(&store_id), body).await
}

async fn create_imageset_in(
    d: &Daemon,
    store_id: Option<&str>,
    body: ImageSetCreateBody,
) -> Result<(StatusCode, Json<ImageSetResponse>), ApiError> {
    let store = d.store_for(store_id).map_err(map_err)?;
    let s2 = store.clone();
    let p =
        tokio::task::spawn_blocking(move || s2.pan.imageset_create(body.description.as_deref()))
            .await
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(map_err)?;
    Ok((
        StatusCode::CREATED,
        Json(imageset_out(&store, p, Some(Vec::new()))),
    ))
}

fn locate_imageset(d: &Daemon, given: &str) -> Result<(Arc<super::StoreHandle>, String), ApiError> {
    let id = bare_id(given);
    let store = d.locate_imageset(&id).map_err(map_err)?.ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            format!("imageset not found: {given}"),
        )
    })?;
    Ok((store, id))
}

fn imageset_response(
    store: &Arc<super::StoreHandle>,
    id: &str,
) -> Result<ImageSetResponse, ApiError> {
    let p = store
        .pan
        .imageset_get(id)
        .map_err(map_err)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("imageset not found: {id}")))?;
    let media = store
        .pan
        .imageset_members(id)
        .map_err(map_err)?
        .iter()
        .map(|iri| bracket_iri(iri))
        .collect();
    Ok(imageset_out(store, p, Some(media)))
}

#[utoipa::path(get, path = "/imagesets/{id}", tag = "imageset", params(("id" = String, Path, description = "<pan/ImageSet/x>, full IRI, or bare id")),
    responses((status = 200, body = ImageSetResponse), (status = 404, body = ErrorBody)))]
async fn get_imageset(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
) -> Result<Json<ImageSetResponse>, ApiError> {
    let (store, id) = locate_imageset(&d, &given)?;
    imageset_response(&store, &id).map(Json)
}

#[utoipa::path(post, path = "/imagesets/{id}/add", tag = "imageset", params(("id" = String, Path)),
    request_body(content = ImageSetMemberBody, description = "The media to put in the set"),
    responses((status = 200, body = ImageSetResponse), (status = 404, body = ErrorBody)))]
async fn imageset_add(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
    Json(body): Json<ImageSetMemberBody>,
) -> Result<Json<ImageSetResponse>, ApiError> {
    imageset_member(&d, &given, &body.media, true).await
}

#[utoipa::path(post, path = "/imagesets/{id}/remove", tag = "imageset", params(("id" = String, Path)),
    request_body(content = ImageSetMemberBody, description = "The media to take out of the set"),
    responses((status = 200, body = ImageSetResponse), (status = 404, body = ErrorBody)))]
async fn imageset_remove(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
    Json(body): Json<ImageSetMemberBody>,
) -> Result<Json<ImageSetResponse>, ApiError> {
    imageset_member(&d, &given, &body.media, false).await
}

async fn imageset_member(
    d: &Daemon,
    given: &str,
    media: &str,
    add: bool,
) -> Result<Json<ImageSetResponse>, ApiError> {
    let (store, set_id) = locate_imageset(d, given)?;
    let media_id = bare_id(media);
    // The image must be in the SAME store as the set: a set never reaches
    // across stores.
    if store.pan.subject_for(&media_id).map_err(map_err)?.is_none() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("id not found in store {}: {media}", store.entry.id),
        ));
    }
    let s2 = store.clone();
    let sid = set_id.clone();
    tokio::task::spawn_blocking(move || {
        if add {
            s2.pan.imageset_add(&sid, &media_id)
        } else {
            s2.pan.imageset_remove(&sid, &media_id)
        }
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(map_err)?;
    imageset_response(&store, &set_id).map(Json)
}

// ── Router ──────────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(
    info(title = "pand", description = "The Pan daemon: every media store on this machine, one door. This document IS the interface spec."),
    paths(health, stores, deliver, deliver_to, get_media, get_thumbnail, delete_media, get_facts, get_state, set_fields, unset_fields, query, search, store_sparql_get, store_sparql_post,
          imagesets, store_imagesets, create_imageset, create_store_imageset, get_imageset, imageset_add, imageset_remove),
    components(schemas(HealthResponse, StoreInfo, IndexInfo, Delivered, FactsResponse, StageStatus, StateResponse, QueryBody, SearchBody, SearchResponse, Hit, ErrorBody,
                       ImageSetResponse, ImageSetCreateBody, ImageSetMemberBody)),
    tags(
        (name = "meta", description = "Daemon + store status"),
        (name = "media", description = "Deliver, read, describe, delete"),
        (name = "imageset", description = "Sets a person curates; membership is pan:relatedToId on the image"),
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
        .route("/media/{id}/set", post(set_fields))
        .route("/media/{id}/unset", post(unset_fields))
        .route("/query", post(query))
        .route("/search", post(search))
        .route("/imagesets", get(imagesets).post(create_imageset))
        .route("/imagesets/{id}", get(get_imageset))
        .route("/imagesets/{id}/add", post(imageset_add))
        .route("/imagesets/{id}/remove", post(imageset_remove))
        .route(
            "/stores/{id}/imagesets",
            get(store_imagesets).post(create_store_imageset),
        )
        .route("/stores/{id}/media", post(deliver_to))
        .route(
            "/stores/{id}/sparql",
            get(store_sparql_get).post(store_sparql_post),
        )
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
    tracing::info!(
        "pand serving on http://{addr} (swagger at /swagger-ui); {} store(s), default {}",
        d.stores.len(),
        d.default_id
    );
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
