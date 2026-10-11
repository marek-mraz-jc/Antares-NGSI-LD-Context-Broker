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

pub mod edm;
mod filter;
mod names;
mod read;

use antares_ops::{Caller, Ops, Surface, ViewOps};
use axum::extract::{MatchedPath, Request};
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

/// One request to a view's endpoint. The path is read as sent, below the
/// prefix (`/{view}/odata/v4...`): a view id is `[a-z0-9-]`, so its raw
/// segment is its id, and `ca%72s` never reaches the view `cars`.
async fn serve(ops: &Ops, mut req: Request) -> Response {
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
    let Some(edm) = model(&id, &view, csdl_version(&req)) else {
        return not_found();
    };
    let state = Arc::new(ODataState {
        model: Arc::new(edm.csdl.clone()),
        service_root: format!("{PREFIX}/{id}/odata/v4/"),
        url_limits: odata_url::Limits::strict(),
        csdl_limits: odata_csdl::Limits::strict(),
        max_header_len: MAX_HEADER_LEN,
    });
    // The inner router matches its own path; the outer match goes.
    req.extensions_mut().remove::<MatchedPath>();
    let reads = Arc::new(read::Reads {
        view,
        edm: Arc::new(edm),
        state: state.clone(),
    });
    let api =
        odata_axum::metadata_router(state.clone()).merge(
            Router::new()
                .fallback(
                    move |method: Method,
                          caller: Caller,
                          request: ODataRequest,
                          headers: HeaderMap| async move {
                        read::serve(&reads, &method, &caller, &request, &headers).await
                    },
                )
                .with_state(state),
        );
    let Ok(resp) = Router::new()
        .nest_service(&format!("/{id}/odata/v4"), api)
        .oneshot(req)
        .await;
    resp
}

/// The model of `view` at `version`, `None` when its Tenant has no schema
/// this surface can read.
fn model(id: &str, view: &ViewOps, version: CsdlVersion) -> Option<edm::Edm> {
    let (revision, bytes) = view.schema()?;
    let schema = parsed(revision, &bytes)?;
    edm::build(id, &schema, &view.shown_by_type(), version).ok()
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
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    // ponytail: dropped whole when full; an LRU when one pod serves more
    // than SCHEMA_CACHE schemas at once.
    if cache.len() >= SCHEMA_CACHE {
        cache.clear();
    }
    cache.insert(revision, read.clone());
    read
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
