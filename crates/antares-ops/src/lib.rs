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
pub mod views;

pub use error::{Refusal, INNER_DETAIL};
pub use operation::OperationId;

use antares_api::AppState;
use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::Router;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

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
    /// The narrowed handle of each view served so far, so a request does
    /// not build the narrowed state and its in-process router anew.
    views: Arc<Mutex<BTreeMap<String, ViewOps>>>,
}

/// The operations of one loaded view (ADR-0023): every one runs composed
/// with the view's narrowing, in the view's one Tenant, and only when the
/// view allows it. Made by [`Ops::view`].
#[derive(Clone)]
pub struct ViewOps {
    inner: Ops,
    view: Arc<antares_api::views::CompiledView>,
    tenant: String,
    operations: Vec<OperationId>,
    linkml: Option<Arc<[u8]>>,
}

/// The Entity types a caller may see through a view and the Attributes
/// each may show, as IRIs (ADR-0023 rule 5): made by [`ViewOps::visible`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Visible {
    /// Each rule's types and the Attributes it shows, `pick` and `omit`
    /// applied: an Attribute is shown when a rule of its type shows it.
    rules: Vec<(BTreeSet<String>, BTreeSet<String>)>,
    /// Attributes removed per type, on top of the rules.
    omit_by_type: BTreeMap<String, BTreeSet<String>>,
}

impl Visible {
    /// Whether an Entity of type `name` (an IRI or a core-context term) may
    /// be seen.
    ///
    /// ```
    /// assert!(!antares_ops::Visible::default().shows_type("Car"));
    /// ```
    #[must_use]
    pub fn shows_type(&self, name: &str) -> bool {
        let ty = expand(name);
        self.rules.iter().any(|(types, _)| types.contains(&ty))
    }

    /// Whether an Entity of type `ty` may show Attribute `attr`.
    ///
    /// ```
    /// assert!(!antares_ops::Visible::default().shows_attribute("Car", "speed"));
    /// ```
    #[must_use]
    pub fn shows_attribute(&self, ty: &str, attr: &str) -> bool {
        let (ty, attr) = (expand(ty), expand(attr));
        !self
            .omit_by_type
            .get(&ty)
            .is_some_and(|gone| gone.contains(&attr))
            && self
                .rules
                .iter()
                .any(|(types, attrs)| types.contains(&ty) && attrs.contains(&attr))
    }

    /// What `filter`'s rules show: each rule's types with its Attributes,
    /// less what `pick` leaves out and `omit` and `omit_by_type` remove. A
    /// rule's condition is not read: it decides which Entities show the
    /// Attributes, not which Attributes there are.
    ///
    /// ```
    /// use antares_ops::policy::{Filter, Rule};
    /// let visible = antares_ops::Visible::from_filter(&Filter {
    ///     rules: vec![Rule {
    ///         types: vec!["https://e.org/Car".into()],
    ///         attributes: vec!["https://e.org/speed".into(), "https://e.org/vin".into()],
    ///         ..Rule::default()
    ///     }],
    ///     omit: vec!["https://e.org/vin".into()],
    ///     ..Filter::default()
    /// });
    /// assert!(visible.shows_attribute("https://e.org/Car", "https://e.org/speed"));
    /// assert!(!visible.shows_attribute("https://e.org/Car", "https://e.org/vin"));
    /// ```
    #[must_use]
    pub fn from_filter(filter: &policy::Filter) -> Self {
        let set =
            |names: &[String]| -> BTreeSet<String> { names.iter().map(|n| expand(n)).collect() };
        let (pick, omit) = (set(&filter.pick), set(&filter.omit));
        let rules = filter
            .rules
            .iter()
            .map(|rule| {
                let mut attrs = set(&rule.attributes);
                attrs.retain(|a| (pick.is_empty() || pick.contains(a)) && !omit.contains(a));
                (set(&rule.types), attrs)
            })
            .collect();
        let omit_by_type = filter
            .omit_by_type
            .iter()
            .map(|(t, attrs)| (expand(t), set(attrs)))
            .collect();
        Self {
            rules,
            omit_by_type,
        }
    }
}

/// `name` as an IRI under the core @context, as a view's names are kept.
fn expand(name: &str) -> String {
    static CORE: std::sync::LazyLock<antares_jsonld::Context> =
        std::sync::LazyLock::new(antares_jsonld::core_context);
    CORE.expand_key(name)
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
    fn new(state: AppState) -> Self {
        Self {
            state,
            views: Arc::default(),
        }
    }

    /// The operations of the loaded view `id`, or `None` when no view of
    /// that id is loaded: an unknown id, a view that failed validation and
    /// a removed one are the same answer.
    #[must_use]
    pub fn view(&self, id: &str) -> Option<ViewOps> {
        let table = self.state.views.snapshot();
        let Some(antares_api::views::ViewEntry::Loaded {
            view,
            tenant,
            operations,
            linkml,
            ..
        }) = table.get(id)
        else {
            return None;
        };
        let mut cache = self.views.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(hit) = cache.get(id).filter(|c| Arc::ptr_eq(&c.view, view)) {
            return Some(hit.clone());
        }
        let made = ViewOps {
            inner: Self::new(self.state.with_view(view.clone())),
            view: view.clone(),
            tenant: tenant.clone(),
            operations: operations
                .iter()
                .filter_map(|o| OperationId::from_id(o))
                .collect(),
            linkml: linkml.clone(),
        };
        // Only views loaded now stay cached: a removed one goes with the
        // first view served after its removal.
        cache.retain(|k, _| table.contains_key(k));
        cache.insert(id.to_owned(), made.clone());
        Some(made)
    }

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

impl ViewOps {
    /// Whether the view allows `op`.
    #[must_use]
    pub fn allows(&self, op: OperationId) -> bool {
        self.operations.contains(&op)
    }

    /// The Entity types and the Attributes the view shows, compacted under
    /// the core @context, sorted: the names a surface may offer the caller.
    #[must_use]
    pub fn shown_names(&self) -> (Vec<String>, Vec<String>) {
        self.view.shown_names()
    }

    /// The view's `LinkML` schema as its document names it (resolved `LinkML`
    /// JSON), `None` when it names none or one past its bound.
    #[must_use]
    pub fn linkml(&self) -> Option<&Arc<[u8]>> {
        self.linkml.as_ref()
    }

    /// What `caller` may see through the view for `op`, a read the view
    /// allows: the view's rules composed with the engine's answer for the
    /// operation's clause and the expanded `types` and `attrs` it names
    /// (`effective = view ∧ engine`, ADR-0023 rule 5), asked once, in the
    /// view's Tenant, failing closed as every gate does.
    ///
    /// # Errors
    /// [`Refusal::NotAllowed`] when the view does not allow `op`,
    /// [`Refusal::Denied`] when the engine refuses it.
    pub async fn visible(
        &self,
        caller: &Caller,
        op: OperationId,
        types: &[String],
        attrs: &[String],
    ) -> Result<Visible, Refusal> {
        let Some(clause) = op.gate_clause().filter(|_| self.allows(op)) else {
            return Err(Refusal::NotAllowed { allow: Vec::new() });
        };
        let tenant = antares_model::TenantId::new(&self.tenant)
            .map_err(|_| Refusal::Denied("the view's tenant is not valid".into()))?;
        let engine = match self.inner.state.policy.as_deref() {
            None => Ok(policy::Filter::default()),
            Some(engine) => {
                let subject = antares_api::policy::subject_of(&tenant, &caller.0);
                let asked = policy::Operation {
                    types,
                    attrs,
                    ..policy::Operation::new(clause)
                };
                match antares_api::policy::decide(engine, &subject, &asked).await {
                    policy::Decision::Allow => Ok(policy::Filter::default()),
                    policy::Decision::Filter(f) => Ok(f),
                    policy::Decision::Deny(why) => Err(antares_api::policy::Denied(why)),
                }
            }
        };
        antares_api::views::with_view(Some(&self.view), clause, engine)
            .map(|f| Visible::from_filter(&f))
            .map_err(|d| Refusal::Denied(d.0))
    }

    /// For each of `ops`, whether the view allows it and the policy engine
    /// could allow a request of it for `caller`, asked in one probe in the
    /// view's Tenant (MCP D-3). The probe fails closed: an engine that fails
    /// it offers nothing.
    pub async fn offered(&self, caller: &Caller, ops: &[OperationId]) -> Vec<bool> {
        let Ok(tenant) = antares_model::TenantId::new(&self.tenant) else {
            return vec![false; ops.len()];
        };
        let asked: Vec<(usize, &'static str)> = ops
            .iter()
            .enumerate()
            .filter(|(_, op)| self.allows(**op))
            .filter_map(|(i, op)| op.gate_clause().map(|c| (i, c)))
            .collect();
        let clauses: Vec<&'static str> = asked.iter().map(|(_, c)| *c).collect();
        let answers = antares_api::policy::offers(
            self.inner.state.policy.as_deref(),
            &tenant,
            &caller.0,
            &clauses,
        )
        .await;
        let mut out = vec![false; ops.len()];
        for ((i, _), yes) in asked.iter().zip(answers) {
            if let Some(slot) = out.get_mut(*i) {
                *slot = yes;
            }
        }
        out
    }

    /// Run `op` through the view, as [`Ops::run`] does, in the view's
    /// Tenant (ADR-0023 rule 10): a caller's `NGSILD-Tenant` naming another
    /// Tenant is refused, never a switch, and the answer names the Tenant
    /// only to a caller who named it (threat F4/V18).
    ///
    /// # Errors
    /// [`Refusal::NotAllowed`] when the view does not allow `op`,
    /// [`Refusal::Malformed`] when the caller names another Tenant, and
    /// everything [`Ops::run`] refuses.
    pub async fn run(
        &self,
        op: OperationId,
        caller: &Caller,
        req: OpsRequest,
    ) -> Result<Response, Refusal> {
        const TENANT: HeaderName = HeaderName::from_static("ngsild-tenant");
        const OTHER: &str = "a view serves one tenant";
        if !self.allows(op) {
            return Err(Refusal::NotAllowed { allow: Vec::new() });
        }
        let mut sent = caller.0.get_all(&TENANT).iter();
        let named = match (sent.next(), sent.next()) {
            (None, _) => false,
            (Some(one), None) => {
                let same = one
                    .to_str()
                    .ok()
                    .and_then(|t| antares_model::TenantId::new(t).ok())
                    .is_some_and(|t| t.as_str() == self.tenant);
                if !same {
                    return Err(Refusal::Malformed(OTHER));
                }
                true
            }
            (Some(_), Some(_)) => return Err(Refusal::Malformed(OTHER)),
        };
        let mut headers = caller.0.clone();
        headers.insert(
            TENANT,
            HeaderValue::from_str(&self.tenant).map_err(|_| Refusal::Malformed(OTHER))?,
        );
        let mut answer = self.inner.run(op, &Caller(headers), req).await?;
        if !named {
            answer.headers_mut().remove(TENANT);
        }
        Ok(answer)
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
    Box::new(Mounted {
        surface,
        views: std::sync::OnceLock::new(),
    })
}

/// A surface as the broker mounts it: closed, every route 404, while no
/// view is loaded (rule M4), and reported so in `/q/health`.
struct Mounted<S> {
    surface: S,
    /// The table of the state it was mounted on, for `version_info`.
    views: std::sync::OnceLock<std::sync::Arc<antares_api::views::ViewTable>>,
}

impl<S: Surface> antares_api::ApiSurface for Mounted<S> {
    fn name(&self) -> &str {
        self.surface.name()
    }

    fn prefix(&self) -> &str {
        self.surface.prefix()
    }

    fn router(&self, st: AppState) -> Router<AppState> {
        let table = st.views.clone();
        // The first state it is mounted on is the one /q/health reports.
        let _ = self.views.set(table.clone());
        self.surface
            .router(Ops::new(st))
            .layer(axum::middleware::from_fn(
                move |req: axum::extract::Request, next: axum::middleware::Next| {
                    let open = table.any_loaded();
                    async move {
                        if open {
                            next.run(req).await
                        } else {
                            Refusal::Closed.ngsi_ld()
                        }
                    }
                },
            ))
            .with_state(())
    }

    fn version_info(&self) -> serde_json::Value {
        let info = self.surface.version_info();
        if self.views.get().is_some_and(|t| t.any_loaded()) {
            serde_json::json!({"state": "open", "info": info})
        } else {
            serde_json::json!({"state": "closed", "reason": "no view is loaded", "info": info})
        }
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
        reload: crate::views::Reload,
    }

    impl TestBroker {
        /// A wired broker with no policy engine, its view table loaded and
        /// `/q/views` served.
        ///
        /// # Errors
        /// The view table's.
        pub async fn new() -> Result<Self, String> {
            let mut state = AppState::new("antares-test".into());
            antares_api::wire(&mut state).await;
            let reload = crate::views::install(&state).await?;
            Ok(Self { state, reload })
        }

        /// Another pod over the same store: its own view table, loaded and
        /// reloaded from the store like this one's.
        ///
        /// # Errors
        /// The view table's.
        pub async fn sibling(&self) -> Result<Self, String> {
            let mut state = AppState::new("antares-test".into());
            state.store = self.state.store.clone();
            antares_api::wire(&mut state).await;
            let reload = crate::views::install(&state).await?;
            Ok(Self { state, reload })
        }

        /// The same broker asking `engine` (ADR-0020).
        #[must_use]
        pub fn with_policy(self, engine: Arc<dyn super::policy::PolicyEngine>) -> Self {
            Self {
                state: self.state.with_policy(engine),
                reload: self.reload,
            }
        }

        /// The same broker with `/q` behind the admin credential (T-3569).
        #[must_use]
        pub fn with_admin_auth(self, auth: antares_api::admin_auth::AdminAuth) -> Self {
            Self {
                state: self.state.with_admin_auth(auth),
                reload: self.reload,
            }
        }

        /// Store a view document as it is, past `/q/views`' validation (what
        /// another version or a hand edit may have left in the store), and
        /// reload this broker's table.
        ///
        /// # Errors
        /// The store's.
        pub async fn store_view_doc(&self, id: &str, doc: serde_json::Value) -> Result<(), String> {
            self.state.put_view_doc(id, doc).await?;
            self.reload.now().await
        }

        /// The NGSI-LD API narrowed by the loaded view `id`: every operation
        /// composed with it as the view root will serve it (ADR-0023).
        ///
        /// # Errors
        /// When `id` is not a loaded view.
        pub fn serve_view(&self, id: &str) -> Result<axum::Router, String> {
            match self.state.views.snapshot().get(id) {
                Some(antares_api::views::ViewEntry::Loaded { view, .. }) => {
                    Ok(antares_api::router(self.state.with_view(view.clone())))
                }
                _ => Err(format!("{id:?} is not a loaded view")),
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
    fn visible_names_are_the_rules_less_pick_omit_and_omit_by_type() {
        let rule = |types: &[&str], attrs: &[&str]| policy::Rule {
            types: types.iter().map(|t| (*t).to_owned()).collect(),
            attributes: attrs.iter().map(|a| (*a).to_owned()).collect(),
            ..policy::Rule::default()
        };
        let iri = |n: &str| format!("https://e.org/{n}");
        let filter = policy::Filter {
            rules: vec![
                rule(&[&iri("Car")], &[&iri("speed"), &iri("owner"), &iri("vin")]),
                rule(&[&iri("Bus")], &[&iri("speed"), &iri("vin")]),
                rule(&[&iri("Car")], &[&iri("colour")]),
            ],
            pick: vec![iri("speed"), iri("owner"), iri("colour"), iri("vin")],
            omit: vec![iri("owner")],
            omit_by_type: BTreeMap::from([(iri("Bus"), vec![iri("vin")])]),
            ..policy::Filter::default()
        };
        let v = Visible::from_filter(&filter);
        assert!(v.shows_type(&iri("Car")) && v.shows_type(&iri("Bus")));
        assert!(!v.shows_type(&iri("Tram")));
        assert!(v.shows_attribute(&iri("Car"), &iri("speed")));
        assert!(
            v.shows_attribute(&iri("Car"), &iri("colour")),
            "rules add up"
        );
        assert!(
            !v.shows_attribute(&iri("Car"), &iri("owner")),
            "omit removes"
        );
        assert!(v.shows_attribute(&iri("Car"), &iri("vin")));
        assert!(
            !v.shows_attribute(&iri("Bus"), &iri("vin")),
            "omit_by_type removes"
        );
        assert!(!v.shows_attribute(&iri("Tram"), &iri("speed")));

        let picked = Visible::from_filter(&policy::Filter {
            pick: vec![iri("speed")],
            ..filter
        });
        assert!(
            !picked.shows_attribute(&iri("Car"), &iri("colour")),
            "pick keeps"
        );
        // A core-context term is the IRI it expands to.
        let core = Visible::from_filter(&policy::Filter {
            rules: vec![rule(&["Car"], &["location"])],
            ..policy::Filter::default()
        });
        assert!(core.shows_attribute(
            "https://uri.etsi.org/ngsi-ld/default-context/Car",
            "https://uri.etsi.org/ngsi-ld/location"
        ));
    }

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
