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

use super::calllog::{CallLine, Meter};
use super::client::CallError;
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
    pub faces: u64,
}

/// What `POST /media/{id}/xmp` did.
#[derive(Debug, Serialize, ToSchema)]
pub struct XmpAdded {
    /// `<pan/Image/k7m2p9x4>` — the image, bracket form.
    pub id: String,
    /// Statements the body carried.
    pub statements: usize,
    /// Of those, how many the graph did not already have.
    pub inserted: usize,
    /// rdf:Description elements appended to the file's XMP.
    pub descriptions: usize,
    /// Sets made because the body named them.
    pub imagesets_made: Vec<String>,
    pub videosets_made: Vec<String>,
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
    /// The store to search. Absent: the store holding `like`, else the default.
    #[serde(default, alias = "soul")]
    pub store: Option<String>,
    /// SPARQL group patterns constraining `?s`, the image: run first, then
    /// `?s a pan:Image`; `pan:`, `copia:` and `git-lex:` are declared. Empty:
    /// no graph pass, nearest over the whole index.
    #[serde(default)]
    pub r#where: String,
    /// The query, exactly one of the three: a vector in the index's space …
    #[serde(default)]
    pub vector: Option<Vec<f32>>,
    /// … a stored image, `<pan/Image/x>` or bare id, whose own vector is the
    /// query (it is left out of the hits) …
    #[serde(default)]
    pub like: Option<String>,
    /// … or text, embedded by the configured embed endpoint with no image.
    #[serde(default)]
    pub text: Option<String>,
    /// How many hits; default 10.
    pub k: Option<usize>,
    /// Index (embedding model) name; default the configured embed model.
    pub index: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct SearchResponse {
    pub hits: Vec<Hit>,
}

/// One face near the asked-for face.
#[derive(Serialize, ToSchema)]
pub struct FaceHit {
    /// `<pan/Face/x>` — the face found, bracket form.
    pub face: String,
    /// `<pan/Image/y>` — the image that face is in.
    pub image: String,
    /// Cosine similarity, 1 = the same vector. The model's own rough line
    /// for "same person" is about 0.4.
    pub score: f32,
}

#[derive(Serialize, ToSchema)]
pub struct SimilarFacesResponse {
    /// The face asked about, bracket form.
    pub face: String,
    /// The image it is in.
    pub image: String,
    pub store: String,
    /// The index searched: the face model's name.
    pub index: String,
    pub hits: Vec<FaceHit>,
}

#[derive(Serialize, ToSchema)]
pub struct Hit {
    pub id: String,
    pub score: f32,
}

/// One imageset: its three facts and, when asked for by id, its members.
#[derive(Serialize, ToSchema)]
pub struct ImageSetResponse {
    /// The set class: `ImageSet` or `VideoSet`.
    pub class: String,
    /// `<pan/ImageSet/abcd2345>` — the identity, bracket form.
    pub id: String,
    pub iri: String,
    pub store: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_date: String,
    /// `pan:updatedDate`: when the set's own file was last written. Absent on
    /// a set made before pan.ttl 0.4.20.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_date: Option<String>,
    /// The media whose `pan:relatedToId` names this set, bracket form.
    /// Omitted from the list; present on one set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media: Option<Vec<String>>,
}

/// What `DELETE /imagesets/{id}` destroyed.
#[derive(Serialize, ToSchema)]
pub struct ImageSetDeleted {
    /// `<pan/ImageSet/abcd2345>`, the set that no longer exists.
    pub id: String,
    pub store: String,
    /// The members whose link to the set was removed, bracket form.
    pub unlinked: Vec<String>,
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

/// `GET /stores/{id}/images?since=`: the images whose file was written since
/// an instant, each with its current marks (pan.ttl 0.4.20, goodlux 2026-11-06).
#[derive(Serialize, ToSchema)]
pub struct ChangesResponse {
    /// pand's instant, taken before the read: send it back as the next `since`.
    pub snapshot: String,
    pub store: String,
    pub images: Vec<ChangedImageOut>,
}

#[derive(Serialize, ToSchema)]
pub struct ChangedImageOut {
    /// `<pan/Image/x>`, bracket form.
    pub id: String,
    /// `pan:updatedDate`: when the image's file was last written, by any route.
    pub updated_date: String,
    /// 0 when unrated.
    pub rating: u8,
    /// false when the flag is absent, so a cleared pick is a row.
    pub is_picked: bool,
    pub is_rejected: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct RenameNamespaceBody {
    /// The namespace IRI the facts are under now, e.g. `https://repolex.ai/ontology/kit/copia/`.
    pub from: String,
    /// The namespace IRI they move to, e.g. `https://repolex.ai/ontology/copia/`.
    pub to: String,
}

#[derive(Serialize, ToSchema)]
pub struct RenameNamespaceResponse {
    pub store: String,
    pub from: String,
    pub to: String,
    /// Images whose XMP was rewritten.
    pub images: usize,
    /// Facts moved to the new namespace.
    pub quads: usize,
}

#[derive(Serialize, ToSchema)]
pub struct IndexRebuildResponse {
    pub store: String,
    /// The index name = the model label (`buffalo-l`, `qwen3-vl-embedding-2b`).
    pub index: String,
    pub dim: usize,
    /// Vectors now in the index: one per record in the graph whose `.npy` was on disk.
    pub vectors: usize,
    /// Records in the graph whose `.npy` was not on disk; left out.
    pub missing: usize,
    /// Up to twenty of the missing, by `pan:vectorPath`.
    pub missing_sample: Vec<String>,
    pub seconds: f64,
}

#[derive(Deserialize, ToSchema)]
pub struct RedoBody {
    /// The stage to run again for this image: caption, embed, segment, pose, depth or face.
    pub stage: String,
}

#[derive(Serialize, ToSchema)]
pub struct RedoResponse {
    pub id: String,
    pub store: String,
    pub stage: String,
    /// Files of the old result removed beside the image.
    pub files_removed: usize,
    /// Facts of the old result removed from the graph.
    pub facts_removed: usize,
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
        || msg.contains("namespaces must be given")
        || msg.contains("namespaces are the same")
        || msg.contains("is not a stage reference")
        || msg.contains("nothing to set")
        || msg.contains("nothing to unset")
        || msg.contains("set file")
        || msg.contains("takes images only")
        || msg.contains("takes videos only")
        || msg.contains("not a readable image")
        || msg.contains("pan:mediaCreatedDate")
        || msg.contains("could not be made")
        // Every relatedToId refusal on set/unset is the caller's to fix.
        || msg.contains("relatedToId")
        || msg.contains("Nothing was changed")
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
        let d2 = d.clone();
        tokio::task::spawn_blocking(move || {
            stores
                .iter()
                .map(|s| {
                    // From the cache the refresher keeps (issue #71); only a
                    // store the refresher has not reached yet is counted here.
                    let cached = crate::locked(&d2.counts_cache)
                        .get(&s.entry.id)
                        .map(|(_, c)| c.clone());
                    let c = match cached {
                        Some(c) => c,
                        None => {
                            let c = s.pan.counts().unwrap_or_default();
                            crate::locked(&d2.counts_cache)
                                .insert(s.entry.id.clone(), (std::time::Instant::now(), c.clone()));
                            c
                        }
                    };
                    StoreCountsOut {
                        store: s.entry.id.clone(),
                        images: c.images,
                        thumbnails: c.thumbnails,
                        captions: c.captions,
                        embeddings: c.embeddings,
                        poses: c.poses,
                        regions: c.regions,
                        depths: c.depths,
                        faces: c.faces,
                    }
                })
                .collect()
        })
        .await
        .unwrap_or_default()
    };
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
/// the graph, appends its own block, and never touches the rest. An image
/// (PNG, JPEG, WebP, GIF, TIFF) becomes a pan:Image; an MP4 or MOV
/// (`video/mp4`, `video/quicktime`) a pan:Video, stored as delivered with
/// its XMP as the file's last box (pan.ttl 0.4.22).
#[utoipa::path(post, path = "/media", tag = "media",
    request_body(content = Vec<u8>, content_type = "image/png", description = "The media file, raw bytes: an image, or an MP4/MOV with Content-Type video/mp4 or video/quicktime. Content-Type names the media type."),
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
    request_body(content = Vec<u8>, content_type = "image/png", description = "The media file, raw bytes: an image, or an MP4/MOV with Content-Type video/mp4 or video/quicktime. Content-Type names the media type."),
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
    request_body(content = HashMap<String, serde_json::Value>, description = "One JSON object keyed by property local name: any datatype property pan.ttl declares on an image (rating, isPicked, isRejected, the caption fields, the ingest fields); the value is checked against the declared type and replaces the old one. relatedToId is the one link: its value (a string or a list of strings) is <pan/ImageSet/id>, <pan/Image/id> or <pan/Video/id> in the same store, and it is ADDED to the links already there (a set link follows the ImageSet rules: the set exists, the media is an image). A reference outside Pan, such as <copia/Moment/id>, is refused here; it is written with POST /media/{id}/xmp."),
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
    request_body(content = Vec<String>, description = "The property local names to remove. A link comes off one at a time, named: relatedToId=<pan/ImageSet/id>; bare relatedToId is refused."),
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

/// Add a producer's statements to a stored image: RDF/XML, one or more
/// `<rdf:Description>` elements, exactly what the producer would have
/// written into the file before delivery. They are appended to the file's
/// XMP as written and loaded into the graph with an arriving file's rules.
#[utoipa::path(post, path = "/media/{id}/xmp", tag = "media", params(("id" = String, Path)),
    request_body(content = String, content_type = "application/rdf+xml", description = "One or more rdf:Description elements (or a whole rdf:RDF), the producer's own namespaces declared on them; rdf:about=\"\" is this image. The only pan: fields a producer may write are pan:relatedToId, pan:mediaCreatedDate and pan:renderRequestDate."),
    responses((status = 200, body = XmpAdded), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn add_xmp(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<XmpAdded>, ApiError> {
    let (store, id) = locate(&d, &given)?;
    let declared = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
        .unwrap_or_default();
    if !(declared.is_empty()
        || declared == "application/rdf+xml"
        || declared == "application/xml"
        || declared == "text/xml")
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("Content-Type {declared} is not RDF/XML. The body is the rdf:Description elements themselves, Content-Type application/rdf+xml."),
        ));
    }
    let fragment = String::from_utf8(body.to_vec())
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("XMP to add is not UTF-8: {e}")))?;
    if fragment.trim().is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "empty body: send the rdf:Description elements to add".into(),
        ));
    }
    let s2 = store.clone();
    let id2 = id.clone();
    let added = tokio::task::spawn_blocking(move || s2.pan.add_xmp(&id2, &fragment))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(map_err)?;
    tracing::info!(store = %store.entry.id, id = %id, statements = added.statements, inserted = added.inserted, "xmp added");
    Ok(Json(XmpAdded {
        id: media_iri_bracket(&added.iri),
        statements: added.statements,
        inserted: added.inserted,
        descriptions: added.descriptions,
        imagesets_made: added.imagesets_made,
        videosets_made: added.videosets_made,
    }))
}

#[utoipa::path(post, path = "/media/{id}/redo", tag = "media", params(("id" = String, Path)),
    request_body(content = RedoBody, description = "Forget one stage's result for this image so the stage runs it again"),
    responses((status = 200, body = RedoResponse), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn redo(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
    Json(body): Json<RedoBody>,
) -> Result<Json<RedoResponse>, ApiError> {
    let (store, id) = locate(&d, &given)?;
    let Some(link) = stages::link_for(&body.stage) else {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!(
                "{} is not a stage; one of caption, embed, segment, pose, depth, face",
                body.stage
            ),
        ));
    };
    let s2 = store.clone();
    let id2 = id.clone();
    let r = tokio::task::spawn_blocking(move || s2.pan.forget_enrichment(&id2, link))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(map_err)?;
    Ok(Json(RedoResponse {
        id: bracket_iri(&format!("{}Image/{id}", crate::PAN_MEDIA_NS)),
        store: store.entry.id.clone(),
        stage: body.stage,
        files_removed: r.files_removed,
        facts_removed: r.facts_removed,
    }))
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

#[utoipa::path(get, path = "/media/{id}", tag = "media",
    params(("id" = String, Path, description = "<pan/Image/x>, <pan/Video/x>, full IRI, or bare id"),
           ("Range" = Option<String>, Header, description = "bytes=start-end: one part of the file, answered 206 with Content-Range; how a browser plays a video")),
    responses((status = 200, description = "The media bytes, Accept-Ranges: bytes"), (status = 206, description = "The requested part"),
        (status = 416, description = "The range starts past the end of the file"), (status = 404, body = ErrorBody)))]
/// The file itself, streamed from disk. A `Range` header gets that part of
/// it (RFC 9110 single range), which is what a browser's video player sends
/// to seek; without one, the whole file.
async fn get_media(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    let (store, id) = locate(&d, &given)?;
    let (abs, media_type) = store.pan.media_file(&id).map_err(map_err)?;
    let mut file = tokio::fs::File::open(&abs).await.map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read media {}: {e}", abs.display()),
        )
    })?;
    let len = file
        .metadata()
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("stat media: {e}")))?
        .len();
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|r| byte_range(r, len));
    let mut response_headers = vec![
        (header::CONTENT_TYPE, media_type),
        (header::ACCEPT_RANGES, "bytes".to_string()),
    ];
    let (status, start, count) = match range {
        None => (StatusCode::OK, 0, len),
        Some(Some((start, end))) => {
            response_headers.push((
                header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{len}"),
            ));
            (StatusCode::PARTIAL_CONTENT, start, end - start + 1)
        }
        Some(None) => {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{len}"))],
            )
                .into_response());
        }
    };
    response_headers.push((header::CONTENT_LENGTH, count.to_string()));
    use tokio::io::AsyncSeekExt;
    if start > 0 {
        file.seek(std::io::SeekFrom::Start(start)).await.map_err(|e| {
            ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("seek media: {e}"))
        })?;
    }
    use tokio::io::AsyncReadExt;
    let stream = tokio_util::io::ReaderStream::new(file.take(count));
    let mut resp = axum::body::Body::from_stream(stream).into_response();
    *resp.status_mut() = status;
    for (k, v) in response_headers {
        resp.headers_mut().insert(k, v.parse().expect("header value"));
    }
    Ok(resp)
}

/// One `bytes=` range against a file of `len` bytes: `Some((first, last))`
/// inclusive when it can be satisfied, `None` when it starts past the end.
/// `bytes=a-b`, `bytes=a-` and `bytes=-n` (the last n bytes); anything else
/// is read as "no range".
fn byte_range(header: &str, len: u64) -> Option<(u64, u64)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    if len == 0 {
        return None;
    }
    if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        if n == 0 {
            return None;
        }
        return Some((len.saturating_sub(n), len - 1));
    }
    let start: u64 = a.parse().ok()?;
    if start >= len {
        return None;
    }
    let end: u64 = if b.is_empty() {
        len - 1
    } else {
        b.parse::<u64>().ok()?.min(len - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end))
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

#[utoipa::path(get, path = "/media/{id}/embedding", tag = "media",
    params(("id" = String, Path, description = "<pan/Image/x>, full IRI, or bare id"),
           ("index" = Option<String>, Query, description = "Index (embedding model) name; default the configured embed model")),
    responses((status = 200, description = "The stored vector, the .npy file pand wrote: float32, shape (dim,)"),
              (status = 404, body = ErrorBody)))]
async fn get_embedding(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let (store, id) = locate(&d, &given)?;
    let index = q
        .get("index")
        .cloned()
        .unwrap_or_else(|| default_index(&d, &store));
    let rel = store
        .pan
        .enrichment_file(&id, crate::PanLayout::STAGE_EMBED, &index, "npy")
        .map_err(map_err)?;
    let abs = store.pan.layout.abs(&rel);
    let bytes = tokio::fs::read(&abs).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ApiError(
                StatusCode::NOT_FOUND,
                format!("no embedding for {given} in index {index} yet"),
            )
        } else {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read embedding: {e}"),
            )
        }
    })?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{id}.{index}.npy\""),
            ),
        ],
        bytes,
    )
        .into_response())
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
                status: if models.iter().any(|m| *m == ep.label()) {
                    "done".into()
                } else {
                    "off".into()
                },
                models,
                error: None,
                terminal: None,
            },
            Some(ep) => {
                if models.iter().any(|m| *m == ep.label()) {
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

#[utoipa::path(get, path = "/stores/{id}/images", tag = "query",
    params(("id" = String, Path, description = "store id"),
           ("since" = Option<String>, Query, description = "RFC3339 with a zone; the X-Pan-Snapshot / snapshot of the previous call. Absent: every image carrying pan:updatedDate.")),
    responses((status = 200, body = ChangesResponse, description = "Images whose file was written at or after since, oldest first, with current rating and flags. Cleared marks are rows with the defaults. gzip and zstd when Accept-Encoding asks."), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn store_images_changed(
    State(d): State<Shared>,
    AxPath(store_id): AxPath<String>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let store = d.store_for(Some(&store_id)).map_err(map_err)?;
    let since = q.get("since").cloned();
    let s = store.clone();
    let (snapshot, rows) = tokio::task::spawn_blocking(move || s.pan.updated_since(since.as_deref()))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let body = ChangesResponse {
        snapshot: snapshot.clone(),
        store: store.entry.id.clone(),
        images: rows
            .into_iter()
            .map(|r| ChangedImageOut {
                id: media_iri_bracket(&r.iri),
                updated_date: r.updated_date,
                rating: r.rating,
                is_picked: r.is_picked,
                is_rejected: r.is_rejected,
            })
            .collect(),
    };
    let mut resp = Json(body).into_response();
    resp.headers_mut().insert(
        header::HeaderName::from_static("x-pan-snapshot"),
        header::HeaderValue::from_str(&snapshot)
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
    );
    Ok(resp)
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

/// The index a store searches when none is named: the embedding model pand
/// is configured with (indexes are named by model); the store's own
/// index_id when no embed stage is configured.
fn default_index(d: &Daemon, store: &super::StoreHandle) -> String {
    d.cfg
        .models
        .get(stages::STAGE_EMBED)
        .map(|m| m.label())
        .unwrap_or_else(|| store.pan.cfg.index_id.clone())
}

/// The query vector for a search by text: the configured embed endpoint,
/// sent the text and no image. One line in the call log, stage `search`.
async fn embed_text(d: &Daemon, store: &super::StoreHandle, text: &str) -> Result<Vec<f32>, ApiError> {
    let ep = d
        .cfg
        .models
        .get(stages::STAGE_EMBED)
        .filter(|m| m.enabled)
        .cloned()
        .ok_or_else(|| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "search by text needs an embed endpoint in config.yml, and none is configured".into(),
            )
        })?;
    let meter = Meter::new();
    d.counters
        .model_calls
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let result = d.client.embed_text(&ep.target(), text, &meter).await;
    if let Some(m) = meter.take() {
        let error = result.as_ref().err().map(|e| e.to_string());
        d.calls.record(&CallLine {
            time: CallLine::now(),
            store: &store.entry.id,
            id: "",
            stage: "search",
            model: &ep.model,
            url: &m.url,
            request_bytes: m.request_bytes,
            status: m.status,
            latency_ms: m.latency_ms,
            response_bytes: m.response_bytes,
            outcome: if result.is_ok() { "answered" } else { "failed" },
            error: error.as_deref(),
            finish_reason: None,
            prompt_tokens: None,
            completion_tokens: None,
        });
    }
    match result {
        Ok(v) => Ok(v.vector),
        Err(CallError::Transient(m)) => Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("the embed server did not answer: {m}"),
        )),
        Err(CallError::Terminal(m)) => Err(ApiError(
            StatusCode::BAD_GATEWAY,
            format!("the embed server refused the text: {m}"),
        )),
    }
}

#[utoipa::path(post, path = "/search", tag = "query", request_body = SearchBody,
    responses((status = 200, body = SearchResponse), (status = 400, body = ErrorBody),
              (status = 404, body = ErrorBody), (status = 503, body = ErrorBody)))]
async fn search(
    State(d): State<Shared>,
    Json(body): Json<SearchBody>,
) -> Result<Json<SearchResponse>, ApiError> {
    let given = [body.vector.is_some(), body.like.is_some(), body.text.is_some()]
        .iter()
        .filter(|b| **b)
        .count();
    if given != 1 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "send exactly one of vector, like (a stored image's id) or text".into(),
        ));
    }
    let like_id = body.like.as_deref().map(bare_id);
    let store = match (&body.store, &like_id) {
        (None, Some(id)) => d.locate(id).map_err(map_err)?.ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                format!("id not found: {}", body.like.as_deref().unwrap_or("")),
            )
        })?,
        _ => d.store_for(body.store.as_deref()).map_err(map_err)?,
    };
    let index = body
        .index
        .clone()
        .unwrap_or_else(|| default_index(&d, &store));
    let k = body.k.unwrap_or(10);
    let vector: Vec<f32> = if let Some(v) = body.vector {
        v
    } else if let Some(id) = &like_id {
        let s = store.clone();
        let (id2, ix) = (id.clone(), index.clone());
        tokio::task::spawn_blocking(move || s.pan.stored_vector(&id2, &ix))
            .await
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(map_err)?
            .ok_or_else(|| {
                ApiError(
                    StatusCode::NOT_FOUND,
                    format!(
                        "{} has no vector in index {index} yet",
                        body.like.as_deref().unwrap_or("")
                    ),
                )
            })?
    } else {
        embed_text(&d, &store, body.text.as_deref().unwrap_or("")).await?
    };
    let where_clause = body.r#where;
    let s = store.clone();
    let hits = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Hit>> {
        let hits = s
            .pan
            .search(&where_clause, &vector, k, &index, like_id.as_deref())?;
        let mut out = Vec::with_capacity(hits.len());
        for h in hits {
            let iri = s
                .pan
                .subject_for(&h.id)?
                .map(|n| n.into_string())
                .unwrap_or(h.id.clone());
            out.push(Hit {
                id: media_iri_bracket(&iri),
                score: h.score,
            });
        }
        Ok(out)
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(map_err)?;
    Ok(Json(SearchResponse { hits }))
}

/// Which face index to search when the caller names none: the configured
/// face stage's model label, else the one index that is not the embed index.
fn default_face_index(d: &Daemon) -> Option<String> {
    d.cfg.models.get(stages::STAGE_FACE).map(|m| m.label())
}

#[utoipa::path(get, path = "/faces/{id}/similar", tag = "query",
    params(("id" = String, Path, description = "a Face record: <pan/Face/x> or bare id"),
           ("k" = Option<usize>, Query, description = "how many hits; default 10"),
           ("store" = Option<String>, Query, description = "the store holding the face; default: whichever store has it"),
           ("index" = Option<String>, Query, description = "face index name; default the configured face model")),
    responses((status = 200, body = SimilarFacesResponse), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn similar_faces(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Result<Json<SimilarFacesResponse>, ApiError> {
    let face_id = bare_id(&given);
    let k: usize = match q.get("k") {
        None => 10,
        Some(v) => v.parse().map_err(|_| {
            ApiError(StatusCode::BAD_REQUEST, format!("k must be a number, not {v:?}"))
        })?,
    };
    let Some(index) = q
        .get("index")
        .cloned()
        .or_else(|| default_face_index(&d))
    else {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "no face stage is configured and no index was named".into(),
        ));
    };
    // The store: named, or the one whose graph lists this face.
    let mut store = None;
    let candidates: Vec<Arc<super::StoreHandle>> = match q.get("store") {
        Some(name) => vec![d.store_for(Some(name)).map_err(map_err)?],
        None => d.stores.clone(),
    };
    let mut owner: Option<(String, String)> = None;
    for s in candidates {
        let s2 = s.clone();
        let fid = face_id.clone();
        if let Some(found) = tokio::task::spawn_blocking(move || s2.pan.image_of_face(&fid))
            .await
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(map_err)?
        {
            owner = Some(found);
            store = Some(s);
            break;
        }
    }
    let (Some(store), Some((image_iri, face_iri))) = (store, owner) else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no face {given} in any store"),
        ));
    };
    let s = store.clone();
    let (fid, ix) = (face_id.clone(), index.clone());
    let hits = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Vec<FaceHit>>> {
        let Some(vector) = s.pan.stored_vector(&fid, &ix)? else {
            return Ok(None);
        };
        let found = s.pan.search("", &vector, k, &ix, Some(&fid))?;
        let mut out = Vec::with_capacity(found.len());
        for h in found {
            let Some((img, face)) = s.pan.image_of_face(&h.id)? else {
                continue;
            };
            out.push(FaceHit {
                face: media_iri_bracket(&face),
                image: media_iri_bracket(&img),
                score: h.score,
            });
        }
        Ok(Some(out))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(map_err)?
    .ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            format!("{given} has no vector in index {index}"),
        )
    })?;
    Ok(Json(SimilarFacesResponse {
        face: media_iri_bracket(&face_iri),
        image: media_iri_bracket(&image_iri),
        store: store.entry.id.clone(),
        index,
        hits,
    }))
}

// ── ImageSets (issue #4; pan.ttl 0.4.2) ─────────────────────────────────────

fn imageset_out(
    store: &Arc<super::StoreHandle>,
    p: crate::ImageSet,
    media: Option<Vec<String>>,
) -> ImageSetResponse {
    ImageSetResponse {
        class: p.class.clone(),
        id: bracket_iri(&p.iri),
        iri: p.iri,
        store: store.entry.id.clone(),
        description: p.description,
        created_date: p.created_date,
        updated_date: p.updated_date,
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

#[utoipa::path(get, path = "/stores/{id}/embeddings", tag = "query",
    params(("id" = String, Path, description = "Store id"),
           ("index" = Option<String>, Query, description = "Index (embedding model) name; default the configured embed model"),
           ("since" = Option<String>, Query, description = "RFC3339 with a zone: only images whose embedding record was produced at or after this instant"),
           ("format" = Option<String>, Query, description = "ndjson (default), f32 or f16: the two binary forms are a stream of records [u8 id length][bare id, UTF-8][dim little-endian floats]")),
    responses((status = 200, description = "ndjson: application/x-ndjson, one line per image {\"id\": \"<pan/Image/x>\", \"vector\": [floats]}. f32/f16: application/octet-stream, X-Pan-Dtype float32|float16. All forms: X-Pan-Dim, X-Pan-Count (records to follow), X-Pan-Snapshot (the server's RFC3339 instant the copy was taken; send it back as the next ?since=). gzip and zstd when Accept-Encoding asks."),
              (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn store_embeddings(
    State(d): State<Shared>,
    AxPath(store_id): AxPath<String>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let store = d.store_for(Some(&store_id)).map_err(map_err)?;
    let index = q
        .get("index")
        .cloned()
        .unwrap_or_else(|| default_index(&d, &store));
    let since = q.get("since").cloned();
    let format = q.get("format").map(|f| f.as_str()).unwrap_or("ndjson").to_string();
    if !["ndjson", "f32", "f16"].contains(&format.as_str()) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("format must be ndjson, f32 or f16, not {format}"),
        ));
    }
    // The instant the client sends back as its next ?since=. Taken BEFORE
    // the copy: a vector that lands between this instant and the copy is in
    // the copy and comes again next time (harmless); taken after, a vector
    // landing between the copy and the instant would never come at all.
    let snapshot_at = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
    let s = store.clone();
    let ix = index.clone();
    // The snapshot is taken under the index lock and released before the
    // first byte goes out, so a reader on a slow link never stalls search.
    let (rows, keep) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let keep = match since.as_deref() {
            Some(c) => Some(s.pan.embedded_since(&ix, c)?),
            None => None,
        };
        let rows = s.pan.all_vectors(&ix)?;
        Ok((rows, keep))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| {
        let m = e.to_string();
        if m.contains("RFC3339") {
            ApiError(StatusCode::BAD_REQUEST, m)
        } else {
            map_err(e)
        }
    })?;
    let Some(rows) = rows else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("store {store_id} has no index {index} (no vectors yet)"),
        ));
    };
    let count = match &keep {
        Some(k) => rows.ids.iter().filter(|id| k.contains(*id)).count(),
        None => rows.ids.len(),
    };
    let dim = rows.dim;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(16);
    let s = store.clone();
    let fmt = format.clone();
    tokio::task::spawn_blocking(move || {
        #[derive(Serialize)]
        struct Row<'a> {
            id: String,
            vector: &'a [f32],
        }
        // Records are sent in chunks, not one by one: 64 rows is ~0.5 MB
        // of f32, a sensible write.
        const CHUNK_ROWS: usize = 64;
        let mut chunk: Vec<u8> = Vec::with_capacity(CHUNK_ROWS * (dim * 12 + 64));
        let mut in_chunk = 0usize;
        for (i, id) in rows.ids.iter().enumerate() {
            if let Some(k) = &keep {
                if !k.contains(id) {
                    continue;
                }
            }
            let vector = &rows.data[i * dim..(i + 1) * dim];
            match fmt.as_str() {
                "ndjson" => {
                    let iri = s
                        .pan
                        .subject_for(id)
                        .ok()
                        .flatten()
                        .map(|n| n.into_string())
                        .unwrap_or_else(|| id.clone());
                    // A typed row, not a json! value: the floats are written
                    // as f32 (nine digits), not widened to f64 (seventeen).
                    let row = Row {
                        id: media_iri_bracket(&iri),
                        vector,
                    };
                    if serde_json::to_writer(&mut chunk, &row).is_err() {
                        break;
                    }
                    chunk.push(b'\n');
                }
                "f32" => {
                    chunk.push(id.len() as u8);
                    chunk.extend_from_slice(id.as_bytes());
                    for v in vector {
                        chunk.extend_from_slice(&v.to_le_bytes());
                    }
                }
                _ => {
                    chunk.push(id.len() as u8);
                    chunk.extend_from_slice(id.as_bytes());
                    for v in vector {
                        chunk.extend_from_slice(&half::f16::from_f32(*v).to_le_bytes());
                    }
                }
            }
            in_chunk += 1;
            if in_chunk == CHUNK_ROWS {
                if tx
                    .blocking_send(Ok(axum::body::Bytes::copy_from_slice(&chunk)))
                    .is_err()
                {
                    return; // the reader went away
                }
                chunk.clear();
                in_chunk = 0;
            }
        }
        if !chunk.is_empty() {
            let _ = tx.blocking_send(Ok(axum::body::Bytes::copy_from_slice(&chunk)));
        }
    });
    let body = axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
    let (content_type, dtype) = match format.as_str() {
        "ndjson" => ("application/x-ndjson", "float32-as-text"),
        "f32" => ("application/octet-stream", "float32"),
        _ => ("application/octet-stream", "float16"),
    };
    Ok((
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::HeaderName::from_static("x-pan-dim"), dim.to_string()),
            (header::HeaderName::from_static("x-pan-count"), count.to_string()),
            (header::HeaderName::from_static("x-pan-dtype"), dtype.to_string()),
            (header::HeaderName::from_static("x-pan-snapshot"), snapshot_at),
        ],
        body,
    )
        .into_response())
}

async fn imagesets_in(
    d: &Daemon,
    store_id: Option<&str>,
) -> Result<Json<Vec<ImageSetResponse>>, ApiError> {
    mediasets_in(d, &crate::imageset::IMAGESET, store_id).await
}

async fn mediasets_in(
    d: &Daemon,
    kind: &crate::imageset::SetKind,
    store_id: Option<&str>,
) -> Result<Json<Vec<ImageSetResponse>>, ApiError> {
    let store = d.store_for(store_id).map_err(map_err)?;
    let sets = store.pan.mediaset_list(kind).map_err(map_err)?;
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
    create_mediaset_in(d, &crate::imageset::IMAGESET, store_id, body).await
}

async fn create_mediaset_in(
    d: &Daemon,
    kind: &'static crate::imageset::SetKind,
    store_id: Option<&str>,
    body: ImageSetCreateBody,
) -> Result<(StatusCode, Json<ImageSetResponse>), ApiError> {
    let store = d.store_for(store_id).map_err(map_err)?;
    let s2 = store.clone();
    let p =
        tokio::task::spawn_blocking(move || s2.pan.mediaset_create(kind, body.description.as_deref()))
            .await
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(map_err)?;
    Ok((
        StatusCode::CREATED,
        Json(imageset_out(&store, p, Some(Vec::new()))),
    ))
}

#[utoipa::path(post, path = "/stores/{id}/rename-namespace", tag = "media", params(("id" = String, Path, description = "Store id")),
    request_body(content = RenameNamespaceBody, description = "Move every fact under one namespace IRI to another, in the graph and in each image's XMP"),
    responses((status = 200, body = RenameNamespaceResponse), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn rename_namespace(
    State(d): State<Shared>,
    AxPath(store_id): AxPath<String>,
    Json(body): Json<RenameNamespaceBody>,
) -> Result<Json<RenameNamespaceResponse>, ApiError> {
    let store = d.store_for(Some(&store_id)).map_err(map_err)?;
    let s2 = store.clone();
    let (from, to) = (body.from.clone(), body.to.clone());
    let r = tokio::task::spawn_blocking(move || s2.pan.rename_namespace(&from, &to))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(map_err)?;
    Ok(Json(RenameNamespaceResponse {
        store: store.entry.id.clone(),
        from: body.from,
        to: body.to,
        images: r.images,
        quads: r.quads,
    }))
}

#[utoipa::path(post, path = "/stores/{id}/indexes/{name}/rebuild", tag = "query",
    params(("id" = String, Path, description = "Store id"), ("name" = String, Path, description = "Index name = the model label, e.g. buffalo-l or qwen3-vl-embedding-2b")),
    responses((status = 200, description = "The index was built again from the graph and the .npy files on disk and replaced the one in use", body = IndexRebuildResponse),
        (status = 400, description = "No such index name in the graph, or a vector file that cannot be read", body = ErrorBody),
        (status = 404, body = ErrorBody)))]
/// For an index file that is damaged (every write to it fails with "cannot be
/// read"): one call rebuilds it from the vectors that are already on disk.
/// Nothing is written to any index while it runs.
async fn rebuild_index(
    State(d): State<Shared>,
    AxPath((store_id, name)): AxPath<(String, String)>,
) -> Result<Json<IndexRebuildResponse>, ApiError> {
    let store = d.store_for(Some(&store_id)).map_err(map_err)?;
    let s2 = store.clone();
    let n2 = name.clone();
    let r = tokio::task::spawn_blocking(move || s2.pan.rebuild_index(&n2))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(map_err)?;
    Ok(Json(IndexRebuildResponse {
        store: store.entry.id.clone(),
        index: r.index,
        dim: r.dim,
        vectors: r.vectors,
        missing: r.missing,
        missing_sample: r.missing_sample,
        seconds: r.seconds,
    }))
}

fn locate_imageset(d: &Daemon, given: &str) -> Result<(Arc<super::StoreHandle>, String), ApiError> {
    locate_mediaset(d, &crate::imageset::IMAGESET, given)
}

fn locate_mediaset(d: &Daemon, kind: &crate::imageset::SetKind, given: &str) -> Result<(Arc<super::StoreHandle>, String), ApiError> {
    let id = bare_id(given);
    let store = d.locate_mediaset(kind, &id).map_err(map_err)?.ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            format!("{} not found: {given}", kind.noun),
        )
    })?;
    Ok((store, id))
}

fn imageset_response(
    store: &Arc<super::StoreHandle>,
    id: &str,
) -> Result<ImageSetResponse, ApiError> {
    mediaset_response(store, &crate::imageset::IMAGESET, id)
}

fn mediaset_response(
    store: &Arc<super::StoreHandle>,
    kind: &crate::imageset::SetKind,
    id: &str,
) -> Result<ImageSetResponse, ApiError> {
    let p = store
        .pan
        .mediaset_get(kind, id)
        .map_err(map_err)?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("{} not found: {id}", kind.noun)))?;
    let media = store
        .pan
        .mediaset_members(kind, id)
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

#[utoipa::path(delete, path = "/imagesets/{id}", tag = "imageset", params(("id" = String, Path, description = "<pan/ImageSet/x>, full IRI, or bare id")),
    responses((status = 200, body = ImageSetDeleted, description = "The set is gone: every member's pan:relatedToId to it removed from the graph and from the member's file, the set's node and file deleted. There is no undo."), (status = 404, body = ErrorBody)))]
async fn delete_imageset(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
) -> Result<Json<ImageSetDeleted>, ApiError> {
    delete_mediaset(&d, &crate::imageset::IMAGESET, &given).await
}

async fn delete_mediaset(
    d: &Daemon,
    kind: &'static crate::imageset::SetKind,
    given: &str,
) -> Result<Json<ImageSetDeleted>, ApiError> {
    let (store, id) = locate_mediaset(d, kind, given)?;
    let s2 = store.clone();
    let sid = id.clone();
    let unlinked = tokio::task::spawn_blocking(move || s2.pan.mediaset_delete(kind, &sid))
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(map_err)?;
    Ok(Json(ImageSetDeleted {
        id: format!("<pan/{}/{id}>", kind.class),
        store: store.entry.id.clone(),
        unlinked: unlinked.iter().map(|iri| bracket_iri(iri)).collect(),
    }))
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
    mediaset_member(d, &crate::imageset::IMAGESET, given, media, add).await
}

async fn mediaset_member(
    d: &Daemon,
    kind: &'static crate::imageset::SetKind,
    given: &str,
    media: &str,
    add: bool,
) -> Result<Json<ImageSetResponse>, ApiError> {
    let (store, set_id) = locate_mediaset(d, kind, given)?;
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
            s2.pan.mediaset_add(kind, &sid, &media_id)
        } else {
            s2.pan.mediaset_remove(kind, &sid, &media_id)
        }
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(map_err)?;
    mediaset_response(&store, kind, &set_id).map(Json)
}

// ── VideoSets (pan.ttl 0.4.26, goodlux 2026-11-12): the same doors, for clips ──

#[utoipa::path(get, path = "/videosets", tag = "imageset",
    responses((status = 200, body = Vec<ImageSetResponse>)))]
async fn videosets(State(d): State<Shared>) -> Result<Json<Vec<ImageSetResponse>>, ApiError> {
    mediasets_in(&d, &crate::imageset::VIDEOSET, None).await
}

#[utoipa::path(get, path = "/stores/{id}/videosets", tag = "imageset", params(("id" = String, Path, description = "Store id")),
    responses((status = 200, body = Vec<ImageSetResponse>), (status = 404, body = ErrorBody)))]
async fn store_videosets(
    State(d): State<Shared>,
    AxPath(store_id): AxPath<String>,
) -> Result<Json<Vec<ImageSetResponse>>, ApiError> {
    mediasets_in(&d, &crate::imageset::VIDEOSET, Some(&store_id)).await
}

#[utoipa::path(post, path = "/videosets", tag = "imageset",
    request_body(content = ImageSetCreateBody, description = "The set's description; nothing else is on a set"),
    responses((status = 201, body = ImageSetResponse), (status = 400, body = ErrorBody)))]
async fn create_videoset(
    State(d): State<Shared>,
    Json(body): Json<ImageSetCreateBody>,
) -> Result<(StatusCode, Json<ImageSetResponse>), ApiError> {
    create_mediaset_in(&d, &crate::imageset::VIDEOSET, None, body).await
}

#[utoipa::path(post, path = "/stores/{id}/videosets", tag = "imageset", params(("id" = String, Path, description = "Store id")),
    request_body(content = ImageSetCreateBody),
    responses((status = 201, body = ImageSetResponse), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn create_store_videoset(
    State(d): State<Shared>,
    AxPath(store_id): AxPath<String>,
    Json(body): Json<ImageSetCreateBody>,
) -> Result<(StatusCode, Json<ImageSetResponse>), ApiError> {
    create_mediaset_in(&d, &crate::imageset::VIDEOSET, Some(&store_id), body).await
}

#[utoipa::path(get, path = "/videosets/{id}", tag = "imageset", params(("id" = String, Path, description = "<pan/VideoSet/x>, full IRI, or bare id")),
    responses((status = 200, body = ImageSetResponse), (status = 404, body = ErrorBody)))]
async fn get_videoset(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
) -> Result<Json<ImageSetResponse>, ApiError> {
    let (store, id) = locate_mediaset(&d, &crate::imageset::VIDEOSET, &given)?;
    mediaset_response(&store, &crate::imageset::VIDEOSET, &id).map(Json)
}

#[utoipa::path(delete, path = "/videosets/{id}", tag = "imageset", params(("id" = String, Path, description = "<pan/VideoSet/x>, full IRI, or bare id")),
    responses((status = 200, body = ImageSetDeleted, description = "The set is gone: every member's pan:relatedToId to it removed from the graph and from the member's file, the set's node and file deleted. There is no undo."), (status = 404, body = ErrorBody)))]
async fn delete_videoset(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
) -> Result<Json<ImageSetDeleted>, ApiError> {
    delete_mediaset(&d, &crate::imageset::VIDEOSET, &given).await
}

#[utoipa::path(post, path = "/videosets/{id}/add", tag = "imageset", params(("id" = String, Path)),
    request_body(content = ImageSetMemberBody, description = "The video to put in the set; an image is refused"),
    responses((status = 200, body = ImageSetResponse), (status = 404, body = ErrorBody)))]
async fn videoset_add(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
    Json(body): Json<ImageSetMemberBody>,
) -> Result<Json<ImageSetResponse>, ApiError> {
    mediaset_member(&d, &crate::imageset::VIDEOSET, &given, &body.media, true).await
}

#[utoipa::path(post, path = "/videosets/{id}/remove", tag = "imageset", params(("id" = String, Path)),
    request_body(content = ImageSetMemberBody, description = "The video to take out of the set"),
    responses((status = 200, body = ImageSetResponse), (status = 404, body = ErrorBody)))]
async fn videoset_remove(
    State(d): State<Shared>,
    AxPath(given): AxPath<String>,
    Json(body): Json<ImageSetMemberBody>,
) -> Result<Json<ImageSetResponse>, ApiError> {
    mediaset_member(&d, &crate::imageset::VIDEOSET, &given, &body.media, false).await
}

// ── Router ──────────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(
    info(title = "pand", description = "The Pan daemon: every media store on this machine, one door. This document IS the interface spec."),
    paths(health, stores, deliver, deliver_to, get_media, get_thumbnail, get_embedding, delete_media, get_facts, get_state, set_fields, unset_fields, query, search, similar_faces, rebuild_index, store_embeddings, store_images_changed, store_sparql_get, store_sparql_post,
          imagesets, store_imagesets, create_imageset, create_store_imageset, get_imageset, delete_imageset, imageset_add, imageset_remove,
          videosets, store_videosets, create_videoset, create_store_videoset, get_videoset, delete_videoset, videoset_add, videoset_remove, rename_namespace, redo, add_xmp),
    components(schemas(HealthResponse, StoreInfo, IndexInfo, Delivered, FactsResponse, StageStatus, StateResponse, QueryBody, SearchBody, SearchResponse, Hit, FaceHit, SimilarFacesResponse, IndexRebuildResponse, ErrorBody,
                       ImageSetResponse, ImageSetDeleted, ImageSetCreateBody, ImageSetMemberBody, RenameNamespaceBody, RenameNamespaceResponse, RedoBody, RedoResponse)),
    tags(
        (name = "meta", description = "Daemon + store status"),
        (name = "media", description = "Deliver, read, describe, delete"),
        (name = "imageset", description = "Sets a person curates, ImageSets of images and VideoSets of videos; membership is pan:relatedToId on the member"),
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
        .route("/media/{id}/embedding", get(get_embedding))
        .route("/media/{id}/facts", get(get_facts))
        .route("/media/{id}/state", get(get_state))
        .route("/media/{id}/set", post(set_fields))
        .route("/media/{id}/unset", post(unset_fields))
        .route("/media/{id}/redo", post(redo))
        .route("/media/{id}/xmp", post(add_xmp))
        .route("/query", post(query))
        .route("/search", post(search))
        .route("/faces/{id}/similar", get(similar_faces))
        .route("/imagesets", get(imagesets).post(create_imageset))
        .route("/imagesets/{id}", get(get_imageset).delete(delete_imageset))
        .route("/imagesets/{id}/add", post(imageset_add))
        .route("/imagesets/{id}/remove", post(imageset_remove))
        .route("/videosets", get(videosets).post(create_videoset))
        .route("/videosets/{id}", get(get_videoset).delete(delete_videoset))
        .route("/videosets/{id}/add", post(videoset_add))
        .route("/videosets/{id}/remove", post(videoset_remove))
        .route(
            "/stores/{id}/imagesets",
            get(store_imagesets).post(create_store_imageset),
        )
        .route(
            "/stores/{id}/videosets",
            get(store_videosets).post(create_store_videoset),
        )
        .route(
            "/stores/{id}/embeddings",
            // gzip/zstd when the client asks; the text form shrinks ~3.5x,
            // the binary forms barely (floats do not compress).
            get(store_embeddings).layer(tower_http::compression::CompressionLayer::new()),
        )
        .route(
            "/stores/{id}/images",
            get(store_images_changed).layer(tower_http::compression::CompressionLayer::new()),
        )
        .route("/stores/{id}/media", post(deliver_to))
        .route("/stores/{id}/rename-namespace", post(rename_namespace))
        .route("/stores/{id}/indexes/{name}/rebuild", post(rebuild_index))
        .route(
            "/stores/{id}/sparql",
            get(store_sparql_get).post(store_sparql_post),
        )
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(d)
}

/// Serve + run the stage ladder until ctrl-c or SIGTERM. Flushes every store
/// on the way out. SIGTERM is what `pand stop`, `pand restart` and a
/// terminal's task runner send; until 2026-11-10 it was unhandled, so it
/// killed pand at once, mid-write of whatever it was writing.
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
            let ctrl_c = tokio::signal::ctrl_c();
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("install the SIGTERM handler");
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = term.recv() => {}
                }
            }
            #[cfg(not(unix))]
            {
                let _ = ctrl_c.await;
            }
            tracing::info!("shutting down");
        })
        .await?;
    ladder.abort();
    for s in &d.stores {
        s.pan.flush()?;
    }
    Ok(())
}
