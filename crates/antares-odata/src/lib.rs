// SPDX-License-Identifier: EUPL-1.2
//! The OData surface of a view (`docs/analysis/odata-linkml-mapping.md`,
//! T-3508): `/x/views/{view}/odata/v4` serves the view's Entities as OData
//! 4.01 EntitySets, its model the Tenant's `LinkML` schema intersected with
//! what the view shows ([`edm`]). Every request runs as one operation
//! through [`antares_ops`], so the operation's own gate fires and the view
//! narrows the answer as it does on the NGSI-LD view root.
//!
//! A view whose Tenant has no schema, or one this surface cannot read, has
//! no OData endpoint: every path answers `404` as an unknown view does
//! (surfaces-threat-model D12).
//!
//! ```
//! use antares_ops::Surface;
//! let odata = antares_odata::OData;
//! assert_eq!(odata.prefix(), "/x/views");
//! assert_eq!(antares_ops::mount(odata).name(), "odata");
//! ```

#![forbid(unsafe_code)]

mod apply;
mod batch;
mod below;
pub mod edm;
mod filter;
mod names;
mod paths;
mod read;
mod route;
mod write;

use antares_ops::{Caller, OperationId, Ops, Surface, ViewOps};
use axum::body::Bytes;
use axum::extract::{MatchedPath, Request};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::any;
use axum::Router;
use odata_axum::{ODataErrorResponse, ODataRequest, ODataState};
use odata_csdl::CsdlVersion;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use tower::ServiceExt as _;

/// Where the surface is mounted; the view id and `odata/v4` follow.
pub const PREFIX: &str = "/x/views";

/// The bounds a Tenant's schema is read under (threat LK7). The bytes are
/// already bounded by `ANTARES_SCHEMA_MAX_BYTES` when they are written.
pub const SCHEMA_LIMITS: linkml_model::Limits = linkml_model::Limits {
    max_bytes: 16 << 20,
    max_depth: 64,
    max_elements: 100_000,
};

/// The longest request header the surface reads (`Accept`, `OData-*`).
pub const MAX_HEADER_LEN: usize = 4096;

/// How many parsed schemas a pod keeps, by revision.
const SCHEMA_CACHE: usize = 64;

/// How many built models a pod keeps (T-3441 7.3).
const MODEL_CACHE: usize = 256;

/// The OData surface.
#[derive(Debug, Clone, Copy, Default)]
pub struct OData;

impl Surface for OData {
    fn name(&self) -> &'static str {
        "odata"
    }

    fn prefix(&self) -> &'static str {
        PREFIX
    }

    fn version_info(&self) -> serde_json::Value {
        serde_json::json!({
            "endpoint": format!("{PREFIX}/{{view}}/odata/v4"),
            "odataVersions": ["4.0", "4.01"],
        })
    }

    fn router(&self, ops: Ops) -> Router {
        let handler = any(move |req: Request| {
            let ops = ops.clone();
            async move { serve(&ops, req).await }
        });
        Router::new()
            .route("/{view}/odata/v4", handler.clone())
            .route("/{view}/odata/v4/", handler.clone())
            .route("/{view}/odata/v4/{*rest}", handler)
    }
}

/// `HEAD` is the `GET` of the same resource, through the same gate, with
/// its status and headers and no body (RFC 9110 9.3.2, T-3723); the
/// `Content-Length` is the one the `GET` would send.
async fn serve(ops: &Ops, mut req: Request) -> Response {
    if req.method() != Method::HEAD {
        return respond(ops, req).await;
    }
    *req.method_mut() = Method::GET;
    // The GET's headers already carry its Content-Length (T-3737).
    let (parts, _) = respond(ops, req).await.into_parts();
    Response::from_parts(parts, axum::body::Body::empty())
}

/// One request to a view's endpoint. The path is read as sent, below the
/// prefix (`/{view}/odata/v4...`): a view id is `[a-z0-9-]`, so its raw
/// segment is its id, and `ca%72s` never reaches the view `cars`.
async fn respond(ops: &Ops, mut req: Request) -> Response {
    let id = req
        .uri()
        .path()
        .split('/')
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let Some(view) = ops.view(&id) else {
        return not_found();
    };
    let Some((edm, csdl, metadata_etag)) = model(&id, &view, csdl_version(&req)) else {
        return not_found();
    };
    let state = Arc::new(ODataState {
        model: csdl,
        // Absolute: a path-only context URL is read as a file path by
        // Microsoft.OData.Core on Linux (T-3688).
        service_root: format!(
            "{}{PREFIX}/{id}/odata/v4/",
            antares_ops::public_base(req.headers())
        ),
        url_limits: odata_url::Limits::strict(),
        csdl_limits: odata_csdl::Limits::strict(),
        max_header_len: MAX_HEADER_LEN,
        metadata_etag,
        custom_options: read::NGSILD_OPTIONS
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
    });
    // The inner router matches its own path; the outer match goes.
    req.extensions_mut().remove::<MatchedPath>();
    let req = match query_in_body(req, state.url_limits.max_query_len).await {
        Ok(req) => req,
        Err(refused) => return refused.into_response(),
    };
    let reads = Arc::new(read::Reads {
        view,
        edm,
        state: state.clone(),
    });
    let api = odata_axum::metadata_router(state.clone()).merge(
        Router::new()
            .fallback(
                move |method: Method,
                      caller: Caller,
                      request: ODataRequest,
                      headers: HeaderMap,
                      body: Bytes| async move {
                    route::serve(&reads, &method, &caller, &request, &headers, &body).await
                },
            )
            .with_state(state.clone()),
    );
    let inner_root = format!("/{id}/odata/v4/");
    let routed = Router::new().nest_service(&format!("/{id}/odata/v4"), api);
    if req.method() == Method::POST && req.uri().path() == format!("{inner_root}$batch") {
        let path_root = format!("{PREFIX}{inner_root}");
        let roots = (path_root.as_str(), inner_root.as_str());
        return batch::serve(&routed, roots, req, state.url_limits.max_query_len).await;
    }
    let Ok(resp) = routed.oneshot(req).await;
    resp
}

/// `POST {resource}/$query` (Protocol 11.2.5.2, T-3441 Q22): the options
/// of the body, joined to the URL's, served as the `GET` they stand for,
/// so the parser, the refusals and the engine see one request (O24). The
/// body is bounded by the query length the parser takes.
async fn query_in_body(req: Request, max: usize) -> Result<Request, ODataErrorResponse> {
    let path = req.uri().path();
    let Some(resource) = path
        .strip_suffix("/$query")
        .filter(|_| req.method() == Method::POST)
    else {
        return Ok(req);
    };
    let resource = resource.to_owned();
    let kind = req
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|v| v.trim().to_ascii_lowercase());
    let form = match kind.as_deref() {
        Some("text/plain") => false,
        Some("application/x-www-form-urlencoded") => true,
        _ => {
            return Err(ODataErrorResponse::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "UnsupportedMediaType",
                "a query body is text/plain or application/x-www-form-urlencoded",
            ))
        }
    };
    let (mut parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, max).await.map_err(|_| {
        ODataErrorResponse::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "PayloadTooLarge",
            "the query body is longer than this service reads",
        )
    })?;
    let bad = || {
        ODataErrorResponse::new(
            StatusCode::BAD_REQUEST,
            "BadRequest",
            "the query body is not a URL query",
        )
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| bad())?.trim();
    // A form encodes a space as `+`; a URL query reads `+` as itself.
    let text = if form {
        text.replace('+', "%20")
    } else {
        text.to_owned()
    };
    let query: Vec<&str> = parts
        .uri
        .query()
        .into_iter()
        .chain(Some(text.as_str()))
        .filter(|q| !q.is_empty())
        .collect();
    let uri = match query.join("&") {
        q if q.is_empty() => resource,
        q => format!("{resource}?{q}"),
    };
    parts.uri = uri.parse().map_err(|_| bad())?;
    parts.method = Method::GET;
    parts.headers.remove(CONTENT_TYPE);
    parts.headers.remove(CONTENT_LENGTH);
    Ok(Request::from_parts(parts, axum::body::Body::empty()))
}

/// What a model is a function of (T-3441 7.3): the view, the types and
/// Attributes it shows, the writes it offers, its Tenant's schema
/// revision, the OData version.
/// The key is compared whole, so two views never share a model by a
/// collision. ponytail: no engine probe bits yet; section 4's Capabilities
/// add them to the key when the model reads the engine.
type ModelKey = (String, edm::Shown, edm::Writable, u64, bool);

/// A built model, its CSDL document and that document's `ETag`.
type Built = (Arc<edm::Edm>, Arc<odata_csdl::Model>, Option<String>);

/// The model of `view` at `version`, built once per key on this pod (O21),
/// `None` when its Tenant has no schema this surface can read.
fn model(id: &str, view: &ViewOps, version: CsdlVersion) -> Option<Built> {
    static CACHE: LazyLock<Mutex<HashMap<ModelKey, Built>>> = LazyLock::new(Mutex::default);
    let (revision, bytes) = view.schema()?;
    let shown = view.shown_by_type();
    let writable = edm::Writable {
        insert: view.allows(OperationId::CreateEntity),
        update: view.allows(OperationId::MergeEntity) || view.allows(OperationId::ReplaceEntity),
        delete: view.allows(OperationId::DeleteEntity),
    };
    let key = (
        id.to_owned(),
        shown,
        writable,
        revision,
        version == CsdlVersion::V4_01,
    );
    if let Some(hit) = CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
    {
        return Some(hit.clone());
    }
    let schema = parsed(revision, &bytes)?;
    // A view of `*` names no type; it shows the whole schema (ADR-0023 rule 12).
    let every = view.shows_every_name().then(|| schema.slot_iris_by_class());
    // A view's names are the Entities' IRIs; the schema's may be another vocabulary's.
    let viewed;
    let schema = if every.is_some() {
        &*schema
    } else {
        viewed = schema.as_shown(&key.1, antares_ops::core_iri);
        &viewed
    };
    let edm = edm::build(id, schema, every.as_ref().unwrap_or(&key.1), key.2, version).ok()?;
    tracing::debug!(
        view = id,
        schema_revision = revision,
        "the view's OData model was built"
    );
    let etag = odata_axum::metadata_etag(&edm.csdl, &odata_csdl::Limits::strict());
    let built = (Arc::new(edm.clone()), Arc::new(edm.csdl), etag);
    remember(
        &mut CACHE.lock().unwrap_or_else(PoisonError::into_inner),
        MODEL_CACHE,
        key,
        built.clone(),
    );
    Some(built)
}

/// The schema of `bytes`, parsed once per revision on this pod.
fn parsed(revision: u64, bytes: &[u8]) -> Option<Arc<linkml_model::Model>> {
    type Cache = Mutex<HashMap<u64, Option<Arc<linkml_model::Model>>>>;
    static CACHE: LazyLock<Cache> = LazyLock::new(Cache::default);
    if let Some(hit) = CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&revision)
    {
        return hit.clone();
    }
    let read = linkml_model::Model::read(bytes, &SCHEMA_LIMITS)
        .map(Arc::new)
        .inspect_err(|e| unreadable(revision, e))
        .ok();
    remember(
        &mut CACHE.lock().unwrap_or_else(PoisonError::into_inner),
        SCHEMA_CACHE,
        revision,
        read.clone(),
    );
    read
}

/// Keeps `value` under `key` in a cache of at most `max` entries.
/// ponytail: a full cache is dropped whole; an LRU when one pod serves more
/// views or schemas at once than the bound.
fn remember<K: std::hash::Hash + Eq, V>(cache: &mut HashMap<K, V>, max: usize, key: K, value: V) {
    if cache.len() >= max {
        cache.clear();
    }
    cache.insert(key, value);
}

/// Logged once per revision on this pod: the result is cached.
fn unreadable(revision: u64, e: &linkml_model::Error) {
    tracing::warn!(
        revision,
        error = %e,
        "the Tenant's schema cannot be read; OData is closed for its views"
    );
}

/// The version the response is written in (Protocol 8.2.6, 8.2.7), as
/// `odata-axum` negotiates it, so the model and the document agree.
fn csdl_version(req: &Request) -> CsdlVersion {
    let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok());
    match odata_json::negotiate_version(header("OData-Version"), header("OData-MaxVersion")) {
        Ok(odata_json::ODataVersion::V4_0) => CsdlVersion::V4_0,
        // A refused header is answered 400 by the router; the model is not read.
        _ => CsdlVersion::V4_01,
    }
}

fn not_found() -> Response {
    ODataErrorResponse::new(StatusCode::NOT_FOUND, "NotFound", "no such resource").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_surface_names_its_endpoint() {
        assert_eq!((OData.name(), OData.prefix()), ("odata", "/x/views"));
        assert_eq!(OData.version_info()["endpoint"], "/x/views/{view}/odata/v4");
    }

    #[test]
    fn the_parse_cache_stays_bounded() {
        let schema = br#"{"id": "https://e.org/s", "name": "s"}"#;
        for revision in 0..=u64::try_from(SCHEMA_CACHE).unwrap_or(u64::MAX) {
            assert!(parsed(revision, schema).is_some());
        }
        assert!(
            parsed(1 << 40, schema).is_some(),
            "past the bound: the cache starts over"
        );
    }
}
