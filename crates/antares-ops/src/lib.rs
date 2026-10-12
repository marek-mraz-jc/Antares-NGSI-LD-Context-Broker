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
pub mod mapping;
mod names;
mod operation;
mod scan;
pub mod views;

/// The residual of a standard query (ADR-0028 rule 2), re-exported so a surface compiles into
/// it through `antares-ops` alone (rule M2).
pub use antares_residual as residual;
pub use error::{Refusal, INNER_DETAIL};
pub use operation::OperationId;
pub use scan::{Page, Pushdown, Scanned, RESIDUAL_SCAN, TOO_COMPLEX};

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
pub use antares_api::{build_message, DeliveryError, DeliveryFuture, NotificationSink, Outbound};
/// The error a sink's endpoint check answers with.
pub use antares_model::{NgsiError, TenantId};

/// The request bounds a surface applies to what it parses itself (a
/// WebSocket message never passes the HTTP bounds layer).
pub mod bounds {
    pub use antares_api::bounds::{json_depth, MAX_BATCH_ITEMS, MAX_BODY_BYTES, MAX_JSON_DEPTH};
}

/// The federation transport contract (ADR-0025 rule 8, ADR-0027 section 9),
/// for a crate that serves some forwards in process.
pub use antares_api::{AdmitFuture, BuiltForward, ForwardAnswer, ForwardFuture, ForwardTransport};

/// The URL this broker is reached at (`ANTARES_PUBLIC_URL`, else the
/// request's `Host`), for a surface whose documents name its resources by
/// absolute URL.
pub use antares_api::public_base;

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
    /// The surface these operations are run for, named on each one's log
    /// line (threat G10); `None` for the broker's own in-process forwards.
    surface: Option<Arc<str>>,
}

/// The operations of one loaded view (ADR-0023): every one runs composed
/// with the view's narrowing, in the view's one Tenant, and only when the
/// view allows it. Made by [`Ops::view`].
#[derive(Clone)]
pub struct ViewOps {
    inner: Ops,
    view: Arc<antares_api::views::CompiledView>,
    tenant: antares_model::TenantId,
    operations: Vec<OperationId>,
    mapping: Option<Arc<mapping::Mapping>>,
    /// The core @context with the view's own merged in: what a name a caller
    /// writes in an NGSI-LD parameter means here (`names.rs`).
    names: Arc<antares_jsonld::Context>,
}

/// The Entity types a caller may see through a view and the Attributes
/// each may show, as IRIs (ADR-0023 rule 5): made by [`ViewOps::visible`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Visible {
    /// Each rule's types and the Attributes it shows; `None` for an
    /// engine's grant over the whole Entity (`Rule::every_attribute`), which
    /// shows every Attribute of its types.
    rules: Vec<(BTreeSet<String>, Option<BTreeSet<String>>)>,
    /// The filter's `pick` (empty keeps every name) and `omit`, applied to
    /// every rule.
    pick: BTreeSet<String>,
    omit: BTreeSet<String>,
    /// Attributes removed per type, on top of the rules.
    omit_by_type: BTreeMap<String, BTreeSet<String>>,
    /// A filter without rules narrows by none (a `*` view, ADR-0023 rule
    /// 12): every type and every Attribute, less `pick` and `omit`.
    every: bool,
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
        self.every || self.rules.iter().any(|(types, _)| types.contains(&ty))
    }

    /// Whether an Entity of type `ty` may show Attribute `attr`.
    ///
    /// ```
    /// assert!(!antares_ops::Visible::default().shows_attribute("Car", "speed"));
    /// ```
    #[must_use]
    pub fn shows_attribute(&self, ty: &str, attr: &str) -> bool {
        let (ty, attr) = (expand(ty), expand(attr));
        let kept = (self.pick.is_empty() || self.pick.contains(&attr))
            && !self.omit.contains(&attr)
            && !self
                .omit_by_type
                .get(&ty)
                .is_some_and(|gone| gone.contains(&attr));
        kept && (self.every
            || self.rules.iter().any(|(types, attrs)| {
                types.contains(&ty) && attrs.as_ref().is_none_or(|a| a.contains(&attr))
            }))
    }

    /// What `filter`'s rules show: each rule's types with its Attributes
    /// (every one for a rule over the whole Entity), less what `pick` leaves
    /// out and `omit` and `omit_by_type` remove; a filter without rules
    /// shows every type and Attribute, less the same. A rule's condition is
    /// not read: it decides which Entities show the Attributes, not which
    /// Attributes there are.
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
        let rules = filter
            .rules
            .iter()
            .map(|rule| {
                let attrs = (!rule.every_attribute).then(|| set(&rule.attributes));
                (set(&rule.types), attrs)
            })
            .collect();
        Self {
            rules,
            pick: set(&filter.pick),
            omit: set(&filter.omit),
            omit_by_type: filter
                .omit_by_type
                .iter()
                .map(|(t, attrs)| (expand(t), set(attrs)))
                .collect(),
            every: filter.rules.is_empty(),
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

impl Caller {
    /// The policy subject headers the caller sent, in the order
    /// `ANTARES_POLICY_SUBJECT_HEADERS` names them, each with every value:
    /// what a surface compares when it binds something to one subject
    /// (a resumed WebSocket connection, ADR-0026 rule 7).
    #[must_use]
    pub fn subject(&self) -> Vec<(String, Vec<u8>)> {
        antares_api::policy::SUBJECT_HEADERS
            .iter()
            .flat_map(|name| {
                self.0
                    .get_all(name.as_str())
                    .iter()
                    .map(move |v| (name.clone(), v.as_bytes().to_vec()))
            })
            .collect()
    }
}

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
    context: Option<Arc<serde_json::Value>>,
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
            context: None,
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

    /// The @context the request's names are read under, in place of the
    /// outer request's `Link` header (a read's `q`, `attrs`, `type`): a
    /// surface that names Attributes outside the core @context defines its
    /// own terms, and the outer client cannot redefine them.
    ///
    /// ```
    /// let req = antares_ops::OpsRequest::new(axum::http::Method::GET, "/ngsi-ld/v1/entities?type=Car&q=speed>1")
    ///     .context(serde_json::json!({"Car": "https://example.org/Car", "speed": "https://example.org/speed"}));
    /// ```
    #[must_use]
    pub fn context(mut self, context: serde_json::Value) -> Self {
        self.context = Some(Arc::new(context));
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
    fn new(state: AppState, surface: Option<Arc<str>>) -> Self {
        Self {
            state,
            views: Arc::default(),
            surface,
        }
    }

    /// The operations of the loaded view `id`, or `None` when no view of
    /// that id is loaded: an unknown id, a view that failed validation and
    /// a removed one are the same answer.
    #[must_use]
    ///
    /// A view of another model (one with a `mapping`) is `None` here too:
    /// only the NGSI-LD view root rebuilds its answers, so every other
    /// surface answers it as an unknown view ([`Self::mapped_view`]).
    pub fn view(&self, id: &str) -> Option<ViewOps> {
        self.mapped_view(id).filter(|v| v.mapping.is_none())
    }

    /// [`Self::view`], a view of another model included: for the NGSI-LD
    /// view root, which inverts each request and rebuilds each answer
    /// through [`ViewOps::mapping`].
    #[must_use]
    pub fn mapped_view(&self, id: &str) -> Option<ViewOps> {
        let table = self.state.views.snapshot();
        let Some(antares_api::views::ViewEntry::Loaded {
            view,
            tenant,
            operations,
            mapping,
            context,
            ..
        }) = table.get(id)
        else {
            return None;
        };
        // The loader stored the tenant it validated, so this never refuses.
        let tenant = antares_model::TenantId::new(tenant).ok()?;
        // the loader attaches nothing else, so a failed downcast is a view
        // no surface serves
        let mapping = match mapping {
            None => None,
            Some(a) => Some(a.0.clone().downcast::<mapping::Mapping>().ok()?),
        };
        let mut cache = self.views.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(hit) = cache.get(id).filter(|c| Arc::ptr_eq(&c.view, view)) {
            return Some(hit.clone());
        }
        let made = ViewOps {
            inner: Self::new(self.state.with_view(view.clone()), self.surface.clone()),
            view: view.clone(),
            tenant,
            operations: operations
                .iter()
                .filter_map(|o| OperationId::from_id(o))
                .collect(),
            mapping,
            names: Arc::new(names::of_view(context.as_deref())?),
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
        self.run_in(op, caller, req, None).await
    }

    /// [`Self::run`], logged with the view (id and Tenant) it runs through.
    async fn run_in(
        &self,
        op: OperationId,
        caller: &Caller,
        req: OpsRequest,
        view: Option<(&str, &str)>,
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
        let call = async {
            match req.context {
                Some(context) => self.state.call_in_context(&caller.0, inner, context).await,
                None => self.state.call(&caller.0, inner).await,
            }
        };
        // The engine learns the view from the in-process scope, never from
        // a header a client could send (ADR-0020 amendment, T-3702).
        let answer = match view {
            Some((id, _)) => antares_api::policy::through_view(id, call).await,
            None => call.await,
        };
        if let Some(surface) = &self.surface {
            // Threat G10: which surface and view ran it; never the subject
            // or an argument (ADR-0020).
            let status = answer.status().as_u16();
            if let Some((view, tenant)) = view {
                tracing::info!(
                    surface = &**surface,
                    view,
                    tenant,
                    operation = op.id(),
                    status,
                    "surface operation"
                );
            } else {
                tracing::info!(
                    surface = &**surface,
                    operation = op.id(),
                    status,
                    "surface operation"
                );
            }
        }
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
    /// The compiled Mapping of a view of another model, `None` for a view
    /// of the store's own model.
    #[must_use]
    pub fn mapping(&self) -> Option<&mapping::Mapping> {
        self.mapping.as_deref()
    }

    /// Whether the view allows `op`.
    #[must_use]
    pub fn allows(&self, op: OperationId) -> bool {
        self.operations.contains(&op)
    }

    /// [`Ops::forward_in_process`] through this view: the forward is
    /// served as a request to the view's Tenant through the view is, the
    /// view's rules composed with that Tenant's engine (ADR-0023 rule 5,
    /// ADR-0025 rule 5).
    pub async fn forward_in_process(&self, req: BuiltForward<'_>, self_url: &str) -> ForwardAnswer {
        self.inner
            .forward_through(Some(self.view.id()), req, self_url)
            .await
    }

    /// The one Tenant the view serves (ADR-0023 rule 10).
    #[must_use]
    pub fn tenant(&self) -> &str {
        self.tenant.as_str()
    }

    /// The `LinkML` schema of the view's Tenant as written to
    /// `/q/schemas/{tenant}`, with its revision, if one is stored (T-3628).
    /// The surface parses it under its own limits and caches the parse per
    /// revision; another Tenant's schema never reaches it.
    #[must_use]
    pub fn schema(&self) -> Option<(u64, Arc<[u8]>)> {
        self.inner
            .state
            .views
            .schema(self.tenant.as_str())
            .map(|s| (s.revision, s.bytes))
    }

    /// The Entity types and the Attributes the view shows, compacted under
    /// the core @context, sorted: the names a surface may offer the caller.
    #[must_use]
    pub fn shown_names(&self) -> (Vec<String>, Vec<String>) {
        self.view.shown_names()
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
        let engine = match self.inner.state.policy.as_deref() {
            None => Ok(antares_api::policy::unnarrowed()),
            Some(engine) => {
                let asked = policy::Operation {
                    types,
                    attrs,
                    ..policy::Operation::new(clause)
                };
                let gated = antares_api::policy::gate(engine, &self.tenant, &caller.0, &asked);
                // The engine learns the view in process (T-3702), as for a run.
                antares_api::policy::through_view(self.view.id(), gated)
                    .await
                    .map(Arc::new)
            }
        };
        antares_api::views::with_view(Some(&self.view), clause, engine)
            .map(|f| Visible::from_filter(&f))
            .map_err(|d| Refusal::Denied(d.0))
    }

    /// Whether the view shows every type and Attribute (ADR-0023 rule 12):
    /// [`Self::shown_names`] and [`Self::shown_by_type`] are then empty and a
    /// surface offers a name by its syntax alone.
    #[must_use]
    pub fn shows_every_name(&self) -> bool {
        self.view.shows_every_name()
    }

    /// Each Entity type the view shows with the Attributes it shows of it,
    /// as expanded IRIs: the union of the view's rules, type by type. A
    /// surface that builds a model from a schema intersects it with this.
    #[must_use]
    pub fn shown_by_type(
        &self,
    ) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
        let mut out: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
            std::collections::BTreeMap::new();
        for rule in &self.view.narrowing().rules {
            for ty in &rule.types {
                out.entry(ty.clone())
                    .or_default()
                    .extend(rule.attributes.iter().cloned());
            }
        }
        out
    }

    /// For each of `ops`, whether the view allows it and the policy engine
    /// could allow a request of it for `caller`, asked in one probe in the
    /// view's Tenant (MCP D-3). The probe fails closed: an engine that fails
    /// it offers nothing.
    pub async fn offered(&self, caller: &Caller, ops: &[OperationId]) -> Vec<bool> {
        let asked = |op: &OperationId| self.allows(*op) && op.gate_clause().is_some();
        let clauses: Vec<&'static str> = ops
            .iter()
            .filter(|op| asked(op))
            .filter_map(|op| op.gate_clause())
            .collect();
        let mut answers = antares_api::policy::through_view(
            self.view.id(),
            antares_api::policy::offers(
                self.inner.state.policy.as_deref(),
                &self.tenant,
                &caller.0,
                &clauses,
            ),
        )
        .await
        .into_iter();
        // The answers come in the order asked; a short answer offers nothing.
        ops.iter()
            .map(|op| asked(op) && answers.next().unwrap_or(false))
            .collect()
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
                    .is_some_and(|t| t.as_str() == self.tenant.as_str());
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
            HeaderValue::from_str(self.tenant.as_str()).map_err(|_| Refusal::Malformed(OTHER))?,
        );
        let mut answer = self
            .inner
            .run_in(
                op,
                &Caller(headers),
                req,
                Some((self.view.id(), self.tenant.as_str())),
            )
            .await?;
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
    /// Whether it serves only while a view is loaded (fail-closed, T-3496).
    /// A surface of the NGSI-LD root itself, which runs every operation
    /// through the policy seam unnarrowed (the WebSocket channel of a
    /// Tenant, ADR-0026 rule 2), answers `false`.
    fn needs_view(&self) -> bool {
        true
    }
}

/// The name an NGSI-LD answer gives the expanded IRI `iri` when the
/// request names no `@context`: its compaction under the core context, the
/// IRI itself when the core context has no term for it. A surface that
/// reads an answer by IRI maps its keys through this.
///
/// ```
/// assert_eq!(antares_ops::core_name("https://uri.etsi.org/ngsi-ld/location"), "location");
/// assert_eq!(antares_ops::core_name("https://example.org/speed"), "https://example.org/speed");
/// ```
#[must_use]
pub fn core_name(iri: &str) -> String {
    static CORE: std::sync::LazyLock<antares_jsonld::Context> =
        std::sync::LazyLock::new(antares_jsonld::core_context);
    CORE.compact_iri(iri)
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

/// Register the forward transport `make` builds over the broker's
/// operations: every federation forward asks it first, and a forward it
/// does not serve goes over HTTP (ADR-0025 rule 8). The composition root
/// calls this once, before the state serves.
///
/// ```
/// use antares_ops::{BuiltForward, ForwardFuture, ForwardTransport};
///
/// /// Serves nothing: every forward stays HTTP.
/// struct Nothing;
///
/// impl ForwardTransport for Nothing {
///     fn forward<'a>(&'a self, _req: BuiltForward<'a>) -> Option<ForwardFuture<'a>> {
///         None
///     }
/// }
///
/// let transport: std::sync::Arc<dyn ForwardTransport> = std::sync::Arc::new(Nothing);
/// # let _ = transport;
/// ```
///
/// # Errors
/// When a transport is already registered.
pub fn with_forward_transport(
    state: AppState,
    make: impl FnOnce(Ops) -> Arc<dyn ForwardTransport>,
) -> Result<AppState, String> {
    let transport = make(Ops::new(state.clone(), None));
    state.with_forward_transport(transport)
}

impl Ops {
    /// Serve the forward `req` through this broker's own router, in process
    /// (ADR-0025 rule 2). `self_url` is the self URL its endpoint starts
    /// with; the rest of its target is the path the router serves. The
    /// inner request carries the forward's headers and payload and nothing
    /// else of the triggering request: the caller map is its subject headers
    /// alone (rules 2 and 4), so a registration without a `tenant` lands in
    /// the default Tenant and a snapshot is never inherited. The answer is
    /// read as an HTTP one is: the body up to the federation response cap,
    /// the source's `NGSILD-Warning` values up to their cap.
    ///
    /// Past [`antares_api::bounds::MAX_IN_PROCESS_CALL_DEPTH`] the forward
    /// is not made, in process or over HTTP (rule 3): it answers 508 for an
    /// `exclusive` or `redirect` registration, whose failure is the
    /// operation's answer, and 504 otherwise, which the merge reports as
    /// code 199 (6.3.17), as a loop is.
    pub async fn forward_in_process(&self, req: BuiltForward<'_>, self_url: &str) -> ForwardAnswer {
        self.forward_through(None, req, self_url).await
    }

    /// [`Self::forward_in_process`] through the view `view` (T-3935): the
    /// engine of the target learns it as for a request through that view.
    async fn forward_through(
        &self,
        view: Option<&str>,
        req: BuiltForward<'_>,
        self_url: &str,
    ) -> ForwardAnswer {
        if antares_api::state::in_process_depth() >= antares_api::bounds::MAX_IN_PROCESS_CALL_DEPTH
        {
            let status = if req.proxied() { 508 } else { 504 };
            return (status, serde_json::Value::Null, Vec::new());
        }
        // A target outside `self_url` is the transport's mistake, and a
        // header 5.9.2 let through that HTTP could not send either: the
        // forward is not made in process, and the answer says so (502).
        let Some(inner) = in_process_request(&req, self_url) else {
            return (502, serde_json::Value::Null, Vec::new());
        };
        let answer = self
            .state
            .call_on_task(view, req.subject().clone(), inner)
            .await;
        let status = answer.status().as_u16();
        let warnings = answer
            .headers()
            .get_all("ngsild-warning")
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_owned))
            .take(antares_api::bounds::MAX_PEER_WARNINGS)
            .collect();
        let cap = *antares_api::bounds::MAX_FED_RESPONSE_BYTES;
        let body = axum::body::to_bytes(answer.into_body(), cap)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(serde_json::Value::Null);
        (status, body, warnings)
    }
}

/// The request [`Ops::forward_in_process`] hands the router for `req`: its
/// method, the path and query of its target after `self_url`, its headers
/// and its payload with the length the HTTP client would state, and nothing
/// else (ADR-0025 equivalence layer 1). `None` when the target does not lie
/// under `self_url` or a header is one no request can carry.
#[must_use]
pub fn in_process_request(
    req: &BuiltForward<'_>,
    self_url: &str,
) -> Option<axum::http::Request<Body>> {
    let path = req.target().and_then(|target| {
        target
            .strip_prefix(self_url)
            .filter(|rest| rest.starts_with('/'))
            .map(str::to_owned)
    })?;
    let mut inner = axum::http::Request::builder()
        .method(req.method().clone())
        .uri(path);
    for (name, value) in req.headers() {
        inner = inner.header(name.as_str(), value.as_str());
    }
    // The HTTP client states the length of what it sends.
    let body = match req.body() {
        Some(bytes) => {
            inner = inner.header(axum::http::header::CONTENT_LENGTH, bytes.len());
            Body::from(bytes.to_vec())
        }
        None => Body::empty(),
    };
    inner.body(body).ok()
}

/// The export grants of internal context sources (ADR-0025 rule 5), kept
/// under their reserved tenant, and the admin routes a module adds beside
/// `/q/views`.
impl Ops {
    /// Every stored grant document.
    ///
    /// # Errors
    /// The store's.
    pub async fn grant_docs(&self) -> Result<Vec<serde_json::Value>, String> {
        self.state.grant_docs().await
    }

    /// The stored grant document `id`, if there is one.
    ///
    /// # Errors
    /// The store's.
    pub async fn grant_doc(&self, id: &str) -> Result<Option<serde_json::Value>, String> {
        self.state.grant_doc(id).await
    }

    /// Store grant document `id`; `true` when it replaced one.
    ///
    /// # Errors
    /// The store's.
    pub async fn put_grant_doc(&self, id: &str, doc: serde_json::Value) -> Result<bool, String> {
        self.state.put_grant_doc(id, doc).await
    }

    /// Remove grant document `id`; `false` when there was none.
    ///
    /// # Errors
    /// The store's.
    pub async fn delete_grant_doc(&self, id: &str) -> Result<bool, String> {
        self.state.delete_grant_doc(id).await
    }

    /// Serve `routes` on the admin surface, relative to `/q` and behind its
    /// credential. Before the router is built.
    pub fn add_admin(&self, routes: Router) {
        self.state.views.add_admin(routes);
    }
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
        let routes = self
            .surface
            .router(Ops::new(st, Some(Arc::from(self.surface.name()))));
        if !self.surface.needs_view() {
            return routes.with_state(());
        }
        routes
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
        if !self.surface.needs_view() || self.views.get().is_some_and(|t| t.any_loaded()) {
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

    /// The `/q` credential [`TestBroker::with_admin_auth`] takes, for the
    /// surface crates, which reach the API crate through this one alone.
    pub use antares_api::admin_auth::AdminAuth;

    /// Let this process forward to loopback and private addresses, as a
    /// test's own listener is one (the egress policy refuses them otherwise).
    pub fn allow_private_egress() {
        antares_jsonld::allow_private_egress(true);
    }

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
            Self::with_sinks(Vec::new(), None).await
        }

        /// [`TestBroker::new`] delivering through `sinks` too (a surface's
        /// own notification binding, ADR-0026) and asking `engine`, both
        /// set before the notifier is wired, as the broker sets them, so
        /// `pre_notify` runs on every notification.
        ///
        /// # Errors
        /// The view table's.
        pub async fn with_sinks(
            sinks: Vec<Box<dyn super::NotificationSink>>,
            engine: Option<Arc<dyn super::policy::PolicyEngine>>,
        ) -> Result<Self, String> {
            let mut state = AppState::new("antares-test".into());
            for sink in sinks {
                state = state.with_sink(sink);
            }
            if let Some(engine) = engine {
                state = state.with_policy(engine);
            }
            antares_api::wire(&mut state).await;
            let reload = crate::views::install(&state).await?;
            Ok(Self { state, reload })
        }

        /// One retention sweep at `now` (T-3900), as the minute loop runs it.
        pub async fn sweep_retention(
            &self,
            now: chrono::DateTime<chrono::Utc>,
        ) -> Vec<crate::views::retention::Deleted> {
            self.reload.sweep_retention(now).await
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

        /// The same broker asking `engine` (ADR-0020). Its view table was
        /// loaded before: `/q/policy` keeps answering as without an engine.
        #[must_use]
        pub fn with_policy(self, engine: Arc<dyn super::policy::PolicyEngine>) -> Self {
            Self {
                state: self.state.with_policy(engine),
                reload: self.reload,
            }
        }

        /// The same broker with the forward transport `make` builds
        /// registered (ADR-0025 rule 8), as the composition root registers
        /// it.
        ///
        /// # Errors
        /// When a transport is registered already.
        pub fn with_forward_transport(
            self,
            make: impl FnOnce(super::Ops) -> Arc<dyn super::ForwardTransport>,
        ) -> Result<Self, String> {
            Ok(Self {
                state: super::with_forward_transport(self.state, make)?,
                reload: self.reload,
            })
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

        /// Rebuild this broker's table from the store now, as its interval
        /// would after another pod's write.
        ///
        /// # Errors
        /// The store's.
        pub async fn reload_now(&self) -> Result<(), String> {
            self.reload.now().await
        }

        /// The broker's router: the NGSI-LD API and `/q`.
        pub fn router(&self) -> axum::Router {
            antares_api::router(self.state.clone())
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

        /// The broker's router with every surface `mounted` (each made by
        /// [`mount`]) beside the NGSI-LD API.
        ///
        /// # Errors
        /// The broker's refusal of a surface's prefix.
        pub fn serve_mounted(
            &self,
            mounted: Vec<Box<dyn antares_api::ApiSurface>>,
        ) -> Result<axum::Router, String> {
            let mut all: Vec<Box<dyn antares_api::ApiSurface>> = vec![Box::new(antares_api::Admin)];
            all.extend(mounted);
            Ok(antares_api::router(self.state.clone().with_surfaces(all)?))
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

    /// [`ViewOps::scan`](super::ViewOps::scan) with `residual` under a `budget` of upstream
    /// Entities read in upstream pages of `upstream`, and how many it read: the bound a test
    /// crosses with a handful of Entities instead of [`RESIDUAL_SCAN`](super::RESIDUAL_SCAN).
    ///
    /// # Errors
    /// [`ViewOps::scan`](super::ViewOps::scan)'s.
    pub async fn scan_within(
        view: &super::ViewOps,
        caller: &super::Caller,
        pushdown: &super::Pushdown,
        residual: &super::residual::Program,
        page: super::Page,
        (budget, upstream): (usize, usize),
    ) -> Result<(super::Scanned, usize), super::Refusal> {
        view.fill(caller, pushdown, residual, page, (budget, upstream))
            .await
    }

    /// A policy [`Filter`](super::policy::Filter) conditioned on `q` (4.9),
    /// for a test engine's `AllowIf`: a surface reaches the query language
    /// through this crate alone (ADR-0027 rule M1).
    ///
    /// # Errors
    /// `q` does not parse.
    pub fn q_filter(q: &str) -> Result<super::policy::Filter, String> {
        let q = antares_ql::parse_q(q).map_err(|e| e.to_string())?;
        Ok(super::policy::Filter {
            q: Some(Box::new(q)),
            ..super::policy::Filter::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "test-kit")]
    #[test]
    fn a_test_engine_condition_is_parsed_or_refused() {
        let f = test_kit::q_filter("colour==\"red\"").unwrap_or_default();
        assert!(f.q.is_some(), "{f:?}");
        assert_eq!(
            f.q.map(|q| q.to_string()).as_deref(),
            Some("colour==\"red\"")
        );
        assert!(test_kit::q_filter("colour==").is_err());
    }

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
        assert!(v.shows_type(&iri("Car")));
        assert!(v.shows_type(&iri("Bus")));
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

    /// A `*` view (ADR-0023 rule 12) narrows by no rule: every type and
    /// Attribute shows, less `pick`, `omit` and `omit_by_type`; the
    /// default still shows nothing.
    #[test]
    fn a_filter_without_rules_shows_every_name_less_pick_and_omit() {
        let iri = |n: &str| format!("https://e.org/{n}");
        let open = Visible::from_filter(&policy::Filter::default());
        assert!(open.shows_type(&iri("Tram")));
        assert!(open.shows_attribute(&iri("Tram"), &iri("speed")));
        let narrowed = Visible::from_filter(&policy::Filter {
            pick: vec![iri("speed"), iri("vin")],
            omit: vec![iri("vin")],
            omit_by_type: BTreeMap::from([(iri("Bus"), vec![iri("speed")])]),
            ..policy::Filter::default()
        });
        assert_eq!(
            [
                narrowed.shows_attribute(&iri("Car"), &iri("speed")),
                narrowed.shows_attribute(&iri("Car"), &iri("vin")),
                narrowed.shows_attribute(&iri("Car"), &iri("colour")),
                narrowed.shows_attribute(&iri("Bus"), &iri("speed")),
            ],
            [true, false, false, false]
        );
        assert!(!Visible::default().shows_type(&iri("Car")));
    }

    /// T-3798: an engine's grant over the whole Entity (`every_attribute`,
    /// ADR-0023 rule 11 as amended by T-3637) shows every Attribute of its
    /// types, less `pick`, `omit` and `omit_by_type`, and none of a type no
    /// rule names.
    #[test]
    fn a_rule_over_the_whole_entity_shows_every_attribute_of_its_types() {
        let iri = |n: &str| format!("https://e.org/{n}");
        let whole = |types: &[&str]| policy::Rule {
            types: types.iter().map(|t| iri(t)).collect(),
            every_attribute: true,
            ..policy::Rule::default()
        };
        let open = Visible::from_filter(&policy::Filter {
            rules: vec![whole(&["Car"])],
            ..policy::Filter::default()
        });
        assert_eq!(
            [
                open.shows_type(&iri("Car")),
                open.shows_attribute(&iri("Car"), &iri("speed")),
                open.shows_type(&iri("Bus")),
                open.shows_attribute(&iri("Bus"), &iri("speed")),
            ],
            [true, true, false, false]
        );
        let narrowed = Visible::from_filter(&policy::Filter {
            rules: vec![
                whole(&["Car", "Bus"]),
                policy::Rule {
                    types: vec![iri("Tram")],
                    attributes: vec![iri("speed")],
                    ..policy::Rule::default()
                },
            ],
            pick: vec![iri("speed"), iri("vin"), iri("colour")],
            omit: vec![iri("vin")],
            omit_by_type: BTreeMap::from([(iri("Bus"), vec![iri("colour")])]),
            ..policy::Filter::default()
        });
        assert_eq!(
            [
                narrowed.shows_attribute(&iri("Car"), &iri("speed")),
                narrowed.shows_attribute(&iri("Car"), &iri("colour")),
                narrowed.shows_attribute(&iri("Car"), &iri("vin")),
                narrowed.shows_attribute(&iri("Car"), &iri("owner")),
                narrowed.shows_attribute(&iri("Bus"), &iri("colour")),
                narrowed.shows_attribute(&iri("Tram"), &iri("colour")),
                narrowed.shows_type(&iri("Truck")),
            ],
            [true, true, false, false, false, false, false]
        );
    }

    #[test]
    fn an_iri_is_named_as_an_answer_without_a_context_names_it() {
        assert_eq!(
            core_name("https://uri.etsi.org/ngsi-ld/location"),
            "location"
        );
        assert_eq!(
            core_name("https://example.org/speed"),
            "https://example.org/speed"
        );
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
