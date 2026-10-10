// SPDX-License-Identifier: EUPL-1.2
//! The operation layer every surface serves through (ADR-0027 section 1,
//! surfaces-threat-model 9.1).
//!
//! A surface (`OData`, MCP, the NGSI-LD view root and the others) never holds the
//! broker's state, its store or its SQL: it gets an [`Ops`], and every
//! operation it serves is one [`OperationId`] run through [`Ops::run`], which
//! is the broker's own NGSI-LD handler with its policy seam, bounds, tenancy,
//! history and notifications, exactly as an NGSI-LD client would reach it
//! (`AppState::call`).
//!
//! ```
//! use antares_ops::{Caller, Ops, OperationId, OpsRequest, Surface};
//! use axum::{routing::get, Router};
//!
//! struct Hello;
//!
//! impl Surface for Hello {
//!     fn name(&self) -> &str { "hello" }
//!     fn prefix(&self) -> &str { "/x/hello" }
//!     fn version_info(&self) -> serde_json::Value { serde_json::json!({}) }
//!     fn router(&self, ops: Ops) -> Router {
//!         Router::new().route("/types", get(move |caller: Caller| async move {
//!             let req = OpsRequest::new(axum::http::Method::GET, "/ngsi-ld/v1/types");
//!             match ops.run(OperationId::RetrieveEntityTypes, &caller, req).await {
//!                 Ok(answer) => answer,
//!                 Err(refusal) => refusal.ngsi_ld(),
//!             }
//!         }))
//!     }
//! }
//!
//! let mounted = antares_ops::mount(Hello);
//! assert_eq!(mounted.prefix(), "/x/hello");
//! ```
#![forbid(unsafe_code)]

mod error;
mod operation;

pub use error::{Refusal, INNER_DETAIL};
pub use operation::OperationId;

use antares_api::AppState;
use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::Router;
use std::sync::atomic::{AtomicU64, Ordering};

/// The policy seam's public types (ADR-0020), so a surface names no
/// `antares-api` item.
pub mod policy {
    pub use antares_api::policy::{
        Decision, DecisionFuture, Filter, NotifyDecision, Operation, PolicyEngine, Rule, Subject,
    };
}

/// The notification sink contract (ADR-0027 section 1), for a surface that
/// delivers notifications (WebSocket).
pub use antares_api::{DeliveryError, DeliveryFuture, NotificationSink, Outbound};

/// The headers a surface may never set on an operation: the tenant, the
/// snapshot and the policy subject come from the outer request alone (G2).
const OUTER_ONLY: [&str; 2] = ["ngsild-tenant", "ngsild-snapshot"];

fn outer_only(name: &HeaderName) -> bool {
    OUTER_ONLY.contains(&name.as_str())
        || antares_api::policy::SUBJECT_HEADERS
            .iter()
            .any(|s| s == name.as_str())
}

/// How many bytes of an inner failure's body reach the log.
const LOGGED_DETAIL_BYTES: usize = 4096;

/// The handle a surface serves with. Its fields are private: the broker's
/// state inside is never reachable from a surface (threat F1).
#[derive(Clone)]
pub struct Ops {
    state: AppState,
}

/// The headers of the request a surface is answering, as the HTTP layer
/// received them. Only an extractor makes one, so a surface cannot invent
/// the tenant or the subject an operation runs as.
#[derive(Debug, Clone)]
pub struct Caller(HeaderMap);

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = std::convert::Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        std::future::ready(Ok(Self(parts.headers.clone())))
    }
}

/// The NGSI-LD request a surface builds for one operation. It has no way to
/// carry `NGSILD-Tenant`, `NGSILD-Snapshot` or a policy subject header:
/// [`OpsRequest::header`] drops them, and the fields are private, so it is
/// the only way a header gets in.
#[derive(Debug)]
pub struct OpsRequest {
    method: Method,
    path_and_query: String,
    headers: HeaderMap,
    body: Body,
}

impl OpsRequest {
    /// A request for `path_and_query` under `/ngsi-ld/v1`.
    ///
    /// ```
    /// let req = antares_ops::OpsRequest::new(axum::http::Method::GET, "/ngsi-ld/v1/types");
    /// ```
    #[must_use]
    pub fn new(method: Method, path_and_query: impl Into<String>) -> Self {
        Self {
            method,
            path_and_query: path_and_query.into(),
            headers: HeaderMap::new(),
            body: Body::empty(),
        }
    }

    /// One more header. The tenant, the snapshot and the policy subject are
    /// the outer request's, so a header naming one of them is dropped.
    ///
    /// ```
    /// use axum::http::{HeaderName, HeaderValue, Method};
    /// let req = antares_ops::OpsRequest::new(Method::GET, "/ngsi-ld/v1/types")
    ///     .header(HeaderName::from_static("accept"), HeaderValue::from_static("application/json"));
    /// ```
    #[must_use]
    pub fn header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        if !outer_only(&name) {
            self.headers.append(name, value);
        }
        self
    }

    /// The request body.
    ///
    /// ```
    /// let req = antares_ops::OpsRequest::new(axum::http::Method::POST, "/ngsi-ld/v1/entities")
    ///     .body(r#"{"id":"urn:ngsi-ld:Car:1","type":"Car"}"#);
    /// ```
    #[must_use]
    pub fn body(mut self, body: impl Into<Body>) -> Self {
        self.body = body.into();
        self
    }
}

impl Ops {
    /// Run `op` as the caller of the outer request.
    ///
    /// The request must be the operation it names: its method, path and
    /// query match `op`'s NGSI-LD binding (`OperationId::of`), so a surface
    /// reaches the NGSI-LD API only through the operations of the matrix and
    /// never the admin or another surface's routes. A failure inside the
    /// broker (5xx) never reaches the surface with its detail: it becomes
    /// [`Refusal::Inner`] with a request id, and the detail goes to the log
    /// under that id.
    ///
    /// # Errors
    /// [`Refusal::NotAllowed`] when the request is not `op`,
    /// [`Refusal::Malformed`] when its target is not a URI,
    /// [`Refusal::Inner`] or [`Refusal::Overloaded`] when the broker failed.
    pub async fn run(
        &self,
        op: OperationId,
        caller: &Caller,
        req: OpsRequest,
    ) -> Result<Response, Refusal> {
        let (path, query) = match req.path_and_query.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (req.path_and_query.as_str(), None),
        };
        if OperationId::of(req.method.as_str(), path, query) != Some(op) {
            return Err(Refusal::NotAllowed { allow: Vec::new() });
        }
        let mut inner = axum::http::Request::builder()
            .method(req.method)
            .uri(req.path_and_query.as_str())
            .body(req.body)
            .map_err(|_| Refusal::Malformed("the operation's target is not a URI"))?;
        *inner.headers_mut() = req.headers;
        let answer = self.state.call(&caller.0, inner).await;
        if !answer.status().is_server_error() {
            return Ok(answer);
        }
        if answer.status() == StatusCode::SERVICE_UNAVAILABLE {
            return Err(Refusal::Overloaded {
                retry_after: answer
                    .headers()
                    .get(axum::http::header::RETRY_AFTER)
                    .cloned(),
            });
        }
        Err(inner_failure(op, answer).await)
    }
}

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

/// Log an inner failure under a fresh id and answer with the id alone.
async fn inner_failure(op: OperationId, answer: Response) -> Refusal {
    let request_id = format!(
        "ops-{:x}-{:x}",
        std::process::id(),
        NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)
    );
    let status = answer.status().as_u16();
    let detail = axum::body::to_bytes(answer.into_body(), LOGGED_DETAIL_BYTES)
        .await
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    tracing::warn!(
        request_id,
        operation = op.id(),
        status,
        detail,
        "an operation failed inside the broker"
    );
    Refusal::Inner { request_id }
}

/// What a surface implements: its routes, given the [`Ops`] it serves with.
pub trait Surface: Send + Sync + 'static {
    /// The name `ANTARES_API_SURFACES` selects it by.
    fn name(&self) -> &str;
    /// Where it mounts: `/x` or below it.
    fn prefix(&self) -> &str;
    /// The routes, relative to the prefix.
    fn router(&self, ops: Ops) -> Router;
    /// What `/q/health` reports about it.
    fn version_info(&self) -> serde_json::Value;
}

/// `surface` as the broker mounts it.
///
/// ```
/// # use antares_ops::{Ops, Surface};
/// # struct Empty;
/// # impl Surface for Empty {
/// #     fn name(&self) -> &str { "empty" }
/// #     fn prefix(&self) -> &str { "/x/empty" }
/// #     fn router(&self, _ops: Ops) -> axum::Router { axum::Router::new() }
/// #     fn version_info(&self) -> serde_json::Value { serde_json::Value::Null }
/// # }
/// let mounted = antares_ops::mount(Empty);
/// assert_eq!(mounted.name(), "empty");
/// ```
pub fn mount<S: Surface>(surface: S) -> Box<dyn antares_api::ApiSurface> {
    Box::new(Mounted(surface))
}

struct Mounted<S>(S);

impl<S: Surface> antares_api::ApiSurface for Mounted<S> {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn prefix(&self) -> &str {
        self.0.prefix()
    }

    fn router(&self, st: AppState) -> Router<AppState> {
        self.0.router(Ops { state: st }).with_state(())
    }

    fn version_info(&self) -> serde_json::Value {
        self.0.version_info()
    }
}

/// A broker over the built-in store, for a surface's integration tests.
#[cfg(feature = "test-kit")]
pub mod test_kit {
    use super::{mount, AppState, Surface};
    use std::sync::Arc;

    /// One broker: the NGSI-LD API, the admin surface and the surfaces a
    /// test mounts, over one in-memory store.
    pub struct TestBroker {
        state: AppState,
    }

    impl TestBroker {
        /// A wired broker with no policy engine.
        pub async fn new() -> Self {
            let mut state = AppState::new("antares-test".into());
            antares_api::wire(&mut state).await;
            Self { state }
        }

        /// The same broker asking `engine` (ADR-0020).
        #[must_use]
        pub fn with_policy(self, engine: Arc<dyn super::policy::PolicyEngine>) -> Self {
            Self {
                state: self.state.with_policy(engine),
            }
        }

        /// The broker's router with `surface` mounted beside the NGSI-LD API.
        ///
        /// # Errors
        /// The broker's refusal of the surface's prefix.
        pub fn serve<S: Surface>(&self, surface: S) -> Result<axum::Router, String> {
            let state = self
                .state
                .clone()
                .with_surfaces(vec![Box::new(antares_api::Admin), mount(surface)])?;
            Ok(antares_api::router(state))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_keeps_its_own_headers_and_body_and_drops_the_outer_ones() {
        let req = OpsRequest::new(Method::POST, "/ngsi-ld/v1/entities")
            .header(
                HeaderName::from_static("ngsild-tenant"),
                HeaderValue::from_static("evil"),
            )
            .header(
                HeaderName::from_static("ngsild-snapshot"),
                HeaderValue::from_static("urn:ngsi-ld:Snapshot:x"),
            )
            .header(
                HeaderName::from_static("content-type"),
                HeaderValue::from_static("application/json"),
            )
            .body("{}");
        assert_eq!(req.headers.len(), 1);
        assert_eq!(req.headers["content-type"], "application/json");
        assert_eq!(axum::body::HttpBody::size_hint(&req.body).exact(), Some(2));
    }
}
