// SPDX-License-Identifier: EUPL-1.2
//! The policy seam: one trait, one built-in engine, every engine an addon
//! (ADR-0020).
//!
//! The broker takes no authorization decision of its own. It asks the
//! engine a deployment gave it and obeys, and what an engine may answer is
//! deliberately narrow: allow, deny, or narrow the operation. Nothing here
//! parses a credential or validates a token — authentication, rate limiting
//! and quotas stay in the gateway in front of the broker.
//!
//! Everything in this module is core code, compiled and tested in every
//! build. The only engine the broker ships is [`AllowAll`], and conformance
//! is asserted against it; any other engine is an addon crate outside
//! `crates/`, behind an off-by-default `antares-broker` feature.
//!
//! The seam fails closed: an engine that errors, panics or runs past
//! [`TIMEOUT`] denies. A deployment that wires in a broken engine loses
//! service, never its access rules.

use antares_ql::geo::GeoQuery;
use antares_ql::QNode;
use axum::http::HeaderMap;
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;

/// The `@context` [`Filter::project`] reads rule conditions with.
static CORE_CONTEXT: LazyLock<antares_jsonld::Context> =
    LazyLock::new(antares_jsonld::core_context);
use std::time::Duration;

/// What [`PolicyEngine::decide`] hands back. Boxed because an engine that
/// asks a policy server has to await, and the trait must stay object-safe:
/// the broker holds one `Arc<dyn PolicyEngine>` chosen at startup.
pub type DecisionFuture<'a> = Pin<Box<dyn Future<Output = Decision> + Send + 'a>>;

/// What [`PolicyEngine::decide_each`] hands back: one [`Decision`] per
/// Operation it was given, in the same order.
pub type DecisionsFuture<'a> = Pin<Box<dyn Future<Output = Vec<Decision>> + Send + 'a>>;

/// How long an engine has to answer before the broker stops waiting and
/// denies. Deployment knob (`ANTARES_POLICY_TIMEOUT_MS`), read once at
/// first use. The default is short on purpose: the seam sits in front of
/// every request, and an engine that cannot answer inside it is an outage
/// either way — failing closed at 250 ms is the difference between a 403
/// and a broker that stops accepting.
pub static TIMEOUT: LazyLock<Duration> = LazyLock::new(|| {
    let env = std::env::var("ANTARES_POLICY_TIMEOUT_MS").ok();
    Duration::from_millis(timeout_ms(env.as_deref()))
});

/// `ANTARES_POLICY_TIMEOUT_MS` in milliseconds: a positive integer, else 250.
fn timeout_ms(env: Option<&str>) -> u64 {
    env.and_then(|v| v.parse().ok())
        .filter(|n: &u64| *n > 0)
        .unwrap_or(250)
}

/// The reasons the seam denies on its own, rather than on an engine's word.
/// Fixed strings: they reach the client in a ProblemDetails `detail`, and
/// an engine's own text about why it failed is not the client's business.
pub const ENGINE_TIMED_OUT: &str = "the policy engine did not answer in time";
pub const ENGINE_FAILED: &str = "the policy engine failed";

/// The clauses whose operations act on everything the tenant holds. There
/// is no narrowed form of "delete every entity" or "snapshot the tenant",
/// so a [`Decision::Filter`] on one of them is answered as a deny
/// ([`resolve`]).
pub const WHOLE_TENANT: [&str; 4] = ["5.6.21", "5.16.1", "5.16.2", "5.16.7"];

/// The clauses whose handlers read the [`Filter`] an engine returns: the
/// reads that can serve less than they were asked for. Every other clause
/// takes its operation whole — a create either writes the Entity or does
/// not — so a [`Decision::Filter`] there would be dropped, and the broker
/// would perform in full an operation the engine believed it had narrowed.
/// That is the one direction the seam may not fail in, so those clauses
/// answer a narrowing as a deny ([`resolve`]).
/// Discovery (5.7.5 to 5.7.10) is among them: ADR-0023 rule 5 computes it
/// from the Entities the narrowing selects.
pub const FILTERABLE: [&str; 12] = [
    "5.7.1", "5.7.2", "5.7.3", "5.7.4", "5.7.5", "5.7.6", "5.7.7", "5.7.8", "5.7.9", "5.7.10",
    "5.14.4", "5.14.5",
];

/// Who is asking. The headers are the ones a deployment names for the
/// seam to carry, copied verbatim and never interpreted: the broker does
/// not know what a token is. They never leave this process — stripped from every
/// forwarded request, absent from notifications, dead letters and logs,
/// which is why [`std::fmt::Debug`] here prints names and not values.
#[derive(Clone)]
pub struct Subject {
    pub tenant: antares_model::TenantId,
    pub headers: Vec<(String, String)>,
}

impl std::fmt::Debug for Subject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subject")
            .field("tenant", &self.tenant)
            .field(
                "headers",
                &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// What is being asked for, after negotiation and expansion: the request
/// the broker is about to run, in the terms the operation itself uses.
/// Every name is expanded, so an engine writes its rules against IRIs and
/// not against whatever short name the caller's `@context` happened to use.
pub struct Operation<'a> {
    /// The CIM 009 clause of the operation, e.g. `"5.6.1"`.
    pub clause: &'static str,
    /// The Entity identifiers the operation names, as the handler holds
    /// them; empty for a query that names none.
    pub ids: &'a [&'a str],
    pub types: &'a [String],
    pub attrs: &'a [String],
    pub q: Option<&'a QNode>,
    pub scope_q: Option<&'a str>,
    pub geo: Option<&'a GeoQuery>,
    /// The request body of a write, expanded. `None` for a read.
    pub body: Option<&'a Value>,
    /// What a write by id lands on, as stored (ADR-0023 rule 7): `None`
    /// for a read, a create, or an id the tenant does not hold.
    pub stored: Option<&'a StoredImage>,
}

/// The stored Entity a write by id changes, as an engine judges it
/// (ADR-0023 rule 7, operation matrix item 2, T-3483): what the Entity is,
/// not what the request says it is. Names only, expanded; never a value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredImage {
    pub types: Vec<String>,
    pub scope: Vec<String>,
    pub attribute_names: Vec<String>,
}

impl StoredImage {
    /// The image of one stored, expanded Entity document.
    pub fn of(doc: &Value) -> Self {
        let strings = |v: Option<&Value>| match v {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        };
        Self {
            types: strings(doc.get("type")),
            scope: strings(doc.get("scope")),
            attribute_names: doc
                .as_object()
                .map(|o| {
                    o.keys()
                        .filter(|k| {
                            !k.starts_with("__")
                                && !k.starts_with('@')
                                && !matches!(
                                    k.as_str(),
                                    "id" | "type"
                                        | "scope"
                                        | "createdAt"
                                        | "modifiedAt"
                                        | "deletedAt"
                                )
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// Shape and counts, never the payload: an `Operation` carries the
/// caller's data, and a log line is not where it belongs.
impl std::fmt::Debug for Operation<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Operation")
            .field("clause", &self.clause)
            .field("ids", &self.ids.len())
            .field("types", &self.types.len())
            .field("attrs", &self.attrs.len())
            .field("q", &self.q.is_some())
            .field("scope_q", &self.scope_q.is_some())
            .field("geo", &self.geo.is_some())
            .field("body", &self.body.is_some())
            .field("stored", &self.stored.is_some())
            .finish()
    }
}

impl Operation<'_> {
    /// The operation with nothing but its clause: what a handler that
    /// carries no ids, types or attributes passes, and the base every other
    /// call site fills in with struct-update syntax.
    pub const fn new(clause: &'static str) -> Operation<'static> {
        Operation {
            clause,
            ids: &[],
            types: &[],
            attrs: &[],
            q: None,
            scope_q: None,
            geo: None,
            body: None,
            stored: None,
        }
    }
}

/// What an engine may answer. There is no third state: an operation is
/// allowed, refused, or allowed over less than it asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    /// Refused, with the engine's own reason. Answered 403 with a
    /// ProblemDetails whose type is an Antares URI — Table 6.3.2-1 names no
    /// access-denied error, so this is an Antares decision.
    Deny(String),
    /// Allowed over less. The caller cannot tell a hidden entity from an
    /// absent one.
    Filter(Filter),
}

/// How much less. Every member narrows and none widens: the `q`, `scopeQ`
/// and `geo_q` are conjoined with the caller's own, `pick` keeps a subset of
/// the members, `omit` removes some.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    /// Conjoined into the query the store runs, on the AST — never on the
    /// query string, where the 4.9 precedence of `;` against `|` would have
    /// to be re-derived by whoever writes the rule. Boxed, like `geo_q`: a
    /// parsed query is half the size of the struct, and every [`Decision`]
    /// and [`NotifyDecision`] would carry it inline.
    pub q: Option<Box<QNode>>,
    /// Conjoined the same way with the request's own `scopeQ` (4.18).
    pub scope_q: Option<String>,
    /// An area the Entity must lie in (4.10), held beside the request's own
    /// geoquery and never in its place: 7.2.5 lets a request carry one, so
    /// the store query takes both and every one must hold (ADR-0023 rule 1).
    /// A [`GeoQuery`] is only built by `GeoQuery::from_params`, which bounds
    /// it like a request's. Boxed: a parsed geometry is large, and every
    /// [`Decision`] would carry its size.
    pub geo_q: Option<Box<GeoQuery>>,
    /// Further areas, every one of which must hold as well: a view and an
    /// engine that both narrow by area compose into `geo_q` and these
    /// (ADR-0023 rule 1). An engine with one area leaves this empty; read
    /// them all through [`Filter::areas`].
    pub geo_also: Vec<GeoQuery>,
    /// Members removed from every document served.
    pub omit: Vec<String>,
    /// If non-empty, the only members served beside the document frame.
    pub pick: Vec<String>,
    /// Attributes allowed per rule (ADR-0023 rule 11, gap G4): when
    /// non-empty, an Entity is served only if at least one [`Rule`] holds
    /// for it, and it keeps the union of the `attributes` of the rules that
    /// hold, nothing else beside its frame. An Entity no rule holds for is
    /// left out of the answer. Applied to every document the answer
    /// carries, joined targets and notified Entities included, each by its
    /// own rules. It intersects with `pick`: both narrow.
    pub rules: Vec<Rule>,
    /// Attributes removed per Entity type: an Entity loses every Attribute
    /// any of its types omits.
    pub omit_by_type: BTreeMap<String, Vec<String>>,
    /// Answer [`RESTRICTED_HEADER`]`: true`, so a client can know the
    /// answer was narrowed. Narrowing is otherwise silent.
    pub restricted: bool,
}

/// One allowlist of a [`Filter`] (ADR-0023 rule 11): the Entities of
/// `types` for which the condition holds show `attributes`. The condition
/// (`q`, `scope_q`, `geo_q`, each optional) is the policy's: it is read on
/// the stored Entity, which may carry Attributes the caller cannot see.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rule {
    /// Type names, at least one; a rule naming none holds for nothing.
    pub types: Vec<String>,
    /// On the Entity's own members. A linked path (4.9
    /// `LinkedEntityRelation`) never holds: the visible set of one Entity
    /// must not depend on another Entity the caller may not read.
    pub q: Option<Box<QNode>>,
    pub scope_q: Option<String>,
    pub geo_q: Option<Box<GeoQuery>>,
    /// Further areas the Entity must lie in, as [`Filter::geo_also`].
    pub geo_also: Vec<GeoQuery>,
    /// Entity ids the rule holds for (gap G2). With `id_patterns`, an
    /// Entity is selected when its id is one of these or matches one of
    /// them; both empty select every id.
    pub ids: Vec<String>,
    /// Regular expressions an Entity id may match instead, each anchored
    /// at both ends. One that does not compile, or compiles past
    /// `antares_ql::regex`'s bound, matches nothing (fail closed).
    pub id_patterns: Vec<String>,
    /// The Attributes an Entity this rule holds for shows.
    pub attributes: Vec<String>,
}

impl Rule {
    /// Every area the Entity must lie in: `geo_q` and `geo_also`.
    pub fn areas(&self) -> impl Iterator<Item = &GeoQuery> {
        self.geo_q.as_deref().into_iter().chain(&self.geo_also)
    }

    /// Whether the rule names `doc` by one of its types and, when it
    /// narrows ids, by its id. Neither changes over an Entity's history,
    /// so a temporal instance is selected the same way (unlike the
    /// condition, [`Rule::is_conditional`]).
    pub fn selects(&self, doc: &Value) -> bool {
        entity_types(doc)
            .iter()
            .any(|t| self.types.iter().any(|r| r == t))
            && match doc.get("id").and_then(Value::as_str) {
                Some(id) => self.selects_id(id),
                None => self.ids.is_empty() && self.id_patterns.is_empty(),
            }
    }

    /// Whether the rule's `ids` and `id_patterns` select `id`.
    pub fn selects_id(&self, id: &str) -> bool {
        (self.ids.is_empty() && self.id_patterns.is_empty())
            || self.ids.iter().any(|i| i == id)
            || self.id_patterns.iter().any(|p| {
                antares_ql::regex::compile(&format!("^(?:{p})$")).is_ok_and(|re| re.is_match(id))
            })
    }

    /// Whether the rule has a condition beside its types.
    pub fn is_conditional(&self) -> bool {
        self.q.is_some() || self.scope_q.is_some() || self.areas().next().is_some()
    }

    /// Whether the rule holds for exactly the Entities of its types: no
    /// condition, no ids, no id patterns (what [`Self::holds`] reads beside
    /// the types).
    pub fn admits_by_type_alone(&self) -> bool {
        !self.is_conditional() && self.ids.is_empty() && self.id_patterns.is_empty()
    }

    /// Whether the rule holds for `doc`, an Entity in the form `ctx`
    /// expands names to (the broker's internal form: expanded keys, each
    /// Attribute an array of instances).
    pub fn holds(&self, doc: &Value, ctx: &antares_jsonld::Context) -> bool {
        self.selects(doc)
            && self
                .scope_q
                .as_deref()
                .is_none_or(|s| antares_ql::scope::scope_matches(s, doc))
            && self.areas().all(|g| g.matches(doc, ctx))
            && self.q.as_deref().is_none_or(|q| {
                !q.has_linked_paths() && antares_ql::eval::eval_q(q, doc, ctx, &|_| None)
            })
    }

    /// The Attributes the condition reads, expanded through `ctx`: the head
    /// of every `q` term and the geoquery's GeoProperty.
    pub fn reads(&self, ctx: &antares_jsonld::Context) -> Vec<String> {
        let mut out: Vec<String> = self
            .q
            .as_deref()
            .map(|q| {
                q.attribute_paths()
                    .into_iter()
                    .filter_map(|p| p.split(['.', '[', '{']).next())
                    .map(|h| ctx.expand_key(h))
                    .collect()
            })
            .unwrap_or_default();
        for g in self.areas() {
            out.push(if g.geoproperty.is_empty() {
                antares_ql::geo::LOCATION_IRI.to_owned()
            } else {
                ctx.expand_key(&g.geoproperty)
            });
        }
        out.sort();
        out.dedup();
        out
    }

    /// ADR-0023 rule 11 disclosure: the Attributes the condition reads that
    /// the rule does not show. Seeing the rule's Attributes tells the
    /// caller the condition held, so these are what the visible set
    /// discloses; the view loader reports them.
    pub fn discloses(&self, ctx: &antares_jsonld::Context) -> Vec<String> {
        self.reads(ctx)
            .into_iter()
            .filter(|r| !self.attributes.iter().any(|a| ctx.expand_key(a) == *r))
            .collect()
    }
}

/// What a [`Filter`]'s rules let one Entity show: `only` (when set) is the
/// union of the matched rules' Attributes, `omitted` what its types omit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Shown {
    pub only: Option<std::collections::BTreeSet<String>>,
    pub omitted: std::collections::BTreeSet<String>,
}

impl Shown {
    /// Nothing beside the frame: what a projection falls back to for an
    /// Entity no rule holds for, should one reach it.
    pub fn nothing() -> Self {
        Shown {
            only: Some(Default::default()),
            omitted: Default::default(),
        }
    }

    pub fn allows(&self, k: &str) -> bool {
        self.only.as_ref().is_none_or(|o| o.contains(k)) && !self.omitted.contains(k)
    }

    /// The same set with every name mapped, for a document in another form.
    pub fn map_names(&self, f: impl Fn(&str) -> String) -> Self {
        Shown {
            only: self.only.as_ref().map(|o| o.iter().map(|n| f(n)).collect()),
            omitted: self.omitted.iter().map(|n| f(n)).collect(),
        }
    }
}

/// The types of an Entity document: its `type` member, a string or an
/// array of strings (5.2.4), in whatever form the document is in.
pub fn entity_types(doc: &Value) -> Vec<&str> {
    match doc.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// What identifies the document rather than describing it: 5.2.4 makes
/// `id` and `type` mandatory members of an Entity, and an answer without
/// them is not an Entity at all. A projection never removes these.
const FRAME: [&str; 3] = ["id", "type", "@context"];

impl Filter {
    /// Every area the Entity must lie in: `geo_q` and `geo_also`.
    pub fn areas(&self) -> impl Iterator<Item = &GeoQuery> {
        self.geo_q.as_deref().into_iter().chain(&self.geo_also)
    }

    /// Apply `pick`/`omit` and the rules to one document by member name.
    /// This is the reference semantics `run_policy_contract` holds an
    /// engine's answer against; a served document is projected through the
    /// request's own 5.5.2 representation instead
    /// (`repr::narrow_projection`), where the names are expanded against
    /// the request `@context` first. Rule conditions are read with the core
    /// `@context`; a document no rule holds for keeps its frame here, while
    /// the served paths leave it out.
    pub fn project(&self, doc: &mut Value) {
        let shown = self.has_rules().then(|| {
            self.shown(doc, &CORE_CONTEXT)
                .unwrap_or_else(Shown::nothing)
        });
        let Some(obj) = doc.as_object_mut() else {
            return;
        };
        if !self.pick.is_empty() {
            obj.retain(|k, _| FRAME.contains(&k.as_str()) || self.pick.iter().any(|p| p == k));
        }
        for name in &self.omit {
            if FRAME.contains(&name.as_str()) {
                continue;
            }
            obj.remove(name);
        }
        if let Some(shown) = shown {
            obj.retain(|k, _| FRAME.contains(&k.as_str()) || shown.allows(k));
        }
    }

    /// True when the filter would change nothing, which is how an engine
    /// that means "allow" can say so with an empty `Filter`.
    pub fn is_empty(&self) -> bool {
        self.q.is_none()
            && self.scope_q.is_none()
            && self.areas().next().is_none()
            && self.omit.is_empty()
            && self.pick.is_empty()
            && !self.has_rules()
    }

    /// Whether the filter carries rules or per-type omits (ADR-0023 rule 11).
    pub fn has_rules(&self) -> bool {
        !self.rules.is_empty() || !self.omit_by_type.is_empty()
    }

    /// Whether a rule holds for `doc` (every document without rules): what
    /// [`Self::shown`] answers `Some` for, without building the set.
    pub fn admits(&self, doc: &Value, ctx: &antares_jsonld::Context) -> bool {
        self.rules.is_empty() || self.rules.iter().any(|r| r.holds(doc, ctx))
    }

    /// ADR-0023 rule 11: what `doc` may show, or `None` when `rules` is
    /// non-empty and none holds for it (the Entity is out). An Entity sees
    /// the union of the Attributes of the rules that hold for it, less what
    /// any of its types omits. Names compare as written: the caller brings
    /// rules and document into one form first (`repr::type_rules` expands
    /// both), and `ctx` is the one the conditions' names expand through.
    pub fn shown(&self, doc: &Value, ctx: &antares_jsonld::Context) -> Option<Shown> {
        let only = if self.rules.is_empty() {
            None
        } else {
            let mut held = self.rules.iter().filter(|r| r.holds(doc, ctx)).peekable();
            held.peek()?;
            Some(held.flat_map(|r| r.attributes.iter().cloned()).collect())
        };
        if self.omit_by_type.is_empty() {
            return Some(Shown {
                only,
                omitted: Default::default(),
            });
        }
        let omitted = entity_types(doc)
            .into_iter()
            .filter_map(|t| self.omit_by_type.get(t))
            .flatten()
            .cloned()
            .collect();
        Some(Shown { only, omitted })
    }

    /// ADR-0023 rule 7 per rule (K22): a write is allowed only when its
    /// post-image falls whole inside one rule that holds for it and shows
    /// every Attribute the write names, none of which its types omit. The
    /// check a view's engine runs before it allows a write.
    pub fn admits_write(
        &self,
        post_image: &Value,
        written: &[&str],
        ctx: &antares_jsonld::Context,
    ) -> bool {
        let types = entity_types(post_image);
        let omitted = |w: &str| {
            types
                .iter()
                .filter_map(|t| self.omit_by_type.get(*t))
                .any(|o| o.iter().any(|a| a == w))
        };
        if written.iter().any(|w| omitted(w)) {
            return false;
        }
        self.rules.is_empty()
            || self.rules.iter().any(|r| {
                r.holds(post_image, ctx)
                    && written.iter().all(|w| r.attributes.iter().any(|a| a == w))
            })
    }

    /// Say the answer was narrowed, when the engine asked for it to be
    /// said. Narrowing is otherwise silent: a caller cannot tell an Entity
    /// it may not see from one that is not there.
    pub fn mark_restricted(&self, headers: &mut HeaderMap) {
        if self.restricted {
            headers.insert(
                RESTRICTED_HEADER,
                axum::http::HeaderValue::from_static("true"),
            );
        }
    }

    /// The request's own query parameters, narrowed by this decision.
    ///
    /// The `q` conjunction is made on the AST and rendered back once
    /// (`antares_ql`'s renderer parenthesises an `Or` inside an `And`, so
    /// 4.9's `;`-over-`|` precedence is the renderer's problem and not the
    /// rule writer's). Every consumer below reads the narrowed parameters:
    /// the store push-down, the local re-check that 5.7.2.4 runs over
    /// merged results, and the query the request is forwarded with.
    ///
    /// A `scopeQ` narrowing is set when the request carries none, and
    /// intersected with the request's own when it carries one. 4.19's `and`
    /// is over independent per-pattern predicates, so it distributes over
    /// the `,`/`|` disjunction and the intersection is itself a Scope
    /// Query — `antares_ql::scope::intersect_scope_q` writes it. Where the
    /// product cannot be written down the seam answers the only way that is
    /// not wider than the engine decided: it refuses.
    ///
    /// `geo_q` stays out of the parameters: they hold one geoquery (7.2.5)
    /// and it is the request's. The narrowing's area reaches the store
    /// query beside it through `repr::narrowing`.
    ///
    /// ADR-0023 rule 2: a term of the request's own `q` on an Attribute
    /// this `pick`/`omit` hides reads as a term on an Attribute the Entity
    /// does not have (see [`Filter::hide_terms`]). `ctx` is the request's,
    /// the one its `q` terms expand through.
    pub fn narrow_params(
        &self,
        params: &std::collections::HashMap<String, String>,
        ctx: &antares_jsonld::Context,
    ) -> Result<std::collections::HashMap<String, String>, Denied> {
        let mut out = params.clone();
        // the request's own `q` is parsed and refused long before a filter
        // reaches it; an unparsable one here is a caller that narrowed
        // something it never validated
        let own = match out.get("q").map(|q| antares_ql::parse_q(q)).transpose() {
            Ok(own) => own.map(|own| self.hide_terms(own, ctx)),
            Err(_) => return Err(Denied(ENGINE_FAILED.to_owned())),
        };
        // threat V23: the policy part is held to its share of the narrowed
        // caps on its own, so whether it fits never depends on the caller's
        // `q` (see `antares_ql::parse_narrowed_q`)
        if self
            .q
            .as_deref()
            .is_some_and(|q| !antares_ql::fits_policy_q(q))
        {
            return Err(Denied(ENGINE_FAILED.to_owned()));
        }
        let narrowed = match (own, &self.q) {
            (Some(own), Some(extra)) => Some(QNode::And(vec![own, (**extra).clone()])),
            (own, extra) => own.or_else(|| extra.as_deref().cloned()),
        };
        if let Some(narrowed) = narrowed {
            out.insert("q".to_owned(), narrowed.to_string());
        }
        if let Some(scope) = &self.scope_q {
            let narrowed = match out.get("scopeQ") {
                None => scope.clone(),
                Some(own) => antares_ql::scope::intersect_scope_q(own, scope)
                    .ok_or_else(|| Denied(SCOPE_NOT_NARROWABLE.to_owned()))?,
            };
            out.insert("scopeQ".to_owned(), narrowed);
        }
        Ok(out)
    }
}

impl Filter {
    /// Whether this `pick`/`omit` hides the Attribute a q term names as
    /// `written`, `iri` once expanded. A pick narrows Attributes only: the
    /// system members it never names stay readable, as in the projection.
    fn hides(&self, written: &str, iri: &str) -> bool {
        let named = |names: &[String]| {
            names
                .iter()
                .any(|n| crate::repr::policy_context().expand_key(n) == iri)
        };
        named(&self.omit)
            || (!self.pick.is_empty()
                && antares_ql::is_non_system_attr(written)
                && !named(&self.pick))
    }

    /// ADR-0023 rule 2 (T-3482): a term on an Attribute this narrowing
    /// hides reads as a term on one the Entity does not have — a
    /// comparison or `attr` is false, `!attr` is true — so the hidden value
    /// is never compared, on the store path or in memory. The q grammar
    /// has no constants, so false is written `a;!a` and true `a|!a`, on
    /// the hidden name itself: both hold whatever the Entity carries. The
    /// path's top Attribute decides, so a sub-attribute or a hop through a
    /// hidden Relationship is as hidden as the Attribute. Per-type rules
    /// are not read here: their q is read on what the rules show
    /// (`repr::client_q`).
    pub fn hide_terms(&self, node: QNode, ctx: &antares_jsonld::Context) -> QNode {
        if self.omit.is_empty() && self.pick.is_empty() {
            return node;
        }
        let constant = |top: &str, holds: bool| {
            let is = |negated| QNode::Exists {
                path: antares_ql::QPath::dotted(vec![top.to_owned()]),
                negated,
            };
            let pair = vec![is(false), is(true)];
            if holds {
                QNode::Or(pair)
            } else {
                QNode::And(pair)
            }
        };
        let hidden = |path: &antares_ql::QPath| {
            path.top()
                .filter(|top| self.hides(top, &ctx.expand_key(top)))
                .map(str::to_owned)
        };
        match node {
            QNode::And(nodes) => {
                QNode::And(nodes.into_iter().map(|n| self.hide_terms(n, ctx)).collect())
            }
            QNode::Or(nodes) => {
                QNode::Or(nodes.into_iter().map(|n| self.hide_terms(n, ctx)).collect())
            }
            QNode::Cmp { path, op, value } => match hidden(&path) {
                Some(top) => constant(&top, false),
                None => QNode::Cmp { path, op, value },
            },
            QNode::Exists { path, negated } => match hidden(&path) {
                Some(top) => constant(&top, negated),
                None => QNode::Exists { path, negated },
            },
        }
    }
}

/// The response header a `Filter { restricted: true }` adds. It is not in
/// the `NGSILD-` namespace: that prefix is ETSI's, carries the headers
/// clause 6.3 defines (`NGSILD-Tenant`, `NGSILD-EntityMap`,
/// `NGSILD-Results-Count`, `NGSILD-Warning`), and a broker-invented header
/// under it would collide with whatever a later version puts there — the
/// same reason a refusal answers `urn:antares:error:AccessDenied` rather
/// than an invented `uri.etsi.org` type.
pub const RESTRICTED_HEADER: &str = "Antares-Results-Restricted";

/// The refusal a `scopeQ` narrowing gets when its intersection with the
/// request's own cannot be written as a Scope Query — one side selects
/// nothing, or the distributed product is too large to express (see
/// [`Filter::narrow_params`]).
pub const SCOPE_NOT_NARROWABLE: &str =
    "the request's own scope query cannot be narrowed by the policy engine";

/// What an engine may answer about one notification, for one subscription.
#[derive(Debug, Clone, PartialEq)]
pub enum NotifyDecision {
    Deliver,
    /// Send it, narrowed: the same `pick`/`omit` projection the query path
    /// applies, over the entities of `data`. A notification `Filter` that
    /// carries `q` or `scopeQ` is refused as a [`NotifyDecision::Drop`]:
    /// the entities were selected by the subscription's own conditions long
    /// before this point, there is nothing left to re-run the query against,
    /// and delivering the notification unfiltered would tell the engine a
    /// narrowing was applied that never was.
    Filter(Filter),
    /// Do not send. 5.8.6 counts this as no attempt at all — the
    /// notification was never sent, so it is neither a success nor a
    /// failure, and `timesSent` does not move.
    Drop,
}

/// The seam. One implementation ships with the broker; every other one is
/// an addon crate a deployment builds itself.
pub trait PolicyEngine: Send + Sync {
    /// The name a deployment selects the engine by, and the name
    /// `/q/health` reports.
    fn name(&self) -> &str;

    /// Fires once per request, after negotiation and expansion, before the
    /// operation and before any fan-out (ADR-0014 `on_request`).
    fn decide<'a>(&'a self, subject: &'a Subject, op: &'a Operation<'a>) -> DecisionFuture<'a>;

    /// Fires once per batch (5.6.7 to 5.6.10, 5.6.20) with one Operation
    /// per member, so a batch is refused member by member and the allowed
    /// members are applied (207). The answer holds one Decision per
    /// Operation, in order; an answer of any other length denies the whole
    /// batch. The default asks [`decide`](Self::decide) once per member; an
    /// engine that asks a server overrides it to ask once.
    fn decide_each<'a>(
        &'a self,
        subject: &'a Subject,
        ops: &'a [Operation<'a>],
    ) -> DecisionsFuture<'a> {
        Box::pin(async move {
            let mut out = Vec::with_capacity(ops.len());
            for op in ops {
                out.push(self.decide(subject, op).await);
            }
            out
        })
    }

    /// Fires once per notification document per subscription, before the
    /// egress check and the send (ADR-0014 `pre_notify`). Synchronous: it
    /// sits inside the delivery the broker is about to make, and an engine
    /// that has to ask a server for every notification is a design the seam
    /// declines to make easy.
    fn pre_notify(
        &self,
        subject: &Subject,
        sub: &Value,
        notification: &mut Value,
    ) -> NotifyDecision;

    /// Whether the engine reads rule documents through
    /// [`load`](Self::load) (ADR-0020 amendment). No by default: `/q/policy`
    /// then answers 404, as it does for a broker without an engine.
    fn loads_documents(&self) -> bool {
        false
    }

    /// Take `document` as the rule document of `tenant`, or drop that
    /// Tenant's document when it is `Value::Null`. An `Err` leaves the
    /// engine as it was: the previous document keeps deciding. The reason
    /// goes back to the admin caller who wrote the document and nowhere
    /// else, so it may name parts of it. The default refuses every
    /// document.
    ///
    /// # Errors
    /// Why the engine refuses the document.
    fn load(&self, tenant: &str, document: &Value) -> Result<(), String> {
        let _ = (tenant, document);
        Err(format!(
            "the policy engine {:?} reads no rule documents",
            self.name()
        ))
    }
}

/// The name of the engine the broker ships, which is also the name
/// `/q/health` reports when no engine is attached at all: the two are the
/// same decision — allow — and an operator reading it learns what the
/// broker does, not which object made it happen.
pub const BUILT_IN_NAME: &str = "allow-all";

/// The engine the broker ships, and the one conformance is asserted
/// against: it decides nothing, so the broker behaves exactly as it did
/// before the seam existed.
///
/// A deployment that attaches no engine does not get an instance of this:
/// it gets no engine at all ([`AppState::policy`](crate::state::AppState)
/// is `None`), and every gate answers the empty [`Filter`] without building
/// a [`Subject`], boxing a future or arming the [`TIMEOUT`] timer. The two
/// paths decide the same thing, which is what
/// `the_built_in_answer_is_the_bypassed_answer` holds them to.
pub struct AllowAll;

impl PolicyEngine for AllowAll {
    fn name(&self) -> &str {
        BUILT_IN_NAME
    }

    fn decide<'a>(&'a self, _subject: &'a Subject, _op: &'a Operation<'a>) -> DecisionFuture<'a> {
        Box::pin(std::future::ready(Decision::Allow))
    }

    fn pre_notify(&self, _s: &Subject, _sub: &Value, _n: &mut Value) -> NotifyDecision {
        NotifyDecision::Deliver
    }
}

/// A `Filter` only means something where the broker can serve less than it
/// was asked for, and [`FILTERABLE`] is that list. Everywhere else the
/// narrowing would be silently dropped: "purge every entity, but only the
/// ones you may see" is a different operation from the one the client
/// asked for and would delete less than the 204 claims, and a create writes
/// its Entity or does not. Those operations take the strict reading: narrow
/// means refuse. An empty `Filter` asks for nothing and is an allow.
pub fn resolve(clause: &str, decision: Decision) -> Decision {
    match decision {
        Decision::Filter(f) if !FILTERABLE.contains(&clause) => {
            if f.is_empty() {
                Decision::Allow
            } else if WHOLE_TENANT.contains(&clause) {
                Decision::Deny(format!(
                    "{clause} acts on everything the tenant holds and cannot be narrowed"
                ))
            } else {
                Decision::Deny(format!(
                    "{clause} is performed whole or not at all and cannot be narrowed"
                ))
            }
        }
        other => other,
    }
}

/// Ask the engine, and fail closed. An engine that panics, or that runs
/// past [`TIMEOUT`], denies: a seam that waved the request through on its
/// own failure would turn a broken addon into an open door.
///
/// The panic is caught in place rather than on a spawned task, because the
/// task boundary would demand `'static` of the operation, which borrows the
/// request. It is caught TWICE, because an engine has two places to fail:
/// `decide` returns a boxed future, and a synchronous engine does its whole
/// decision in the call that builds that future — the reference engine is
/// `Box::pin(ready(self.judge(..)))` — so guarding only the future guards
/// the half that does nothing.
///
/// [`TIMEOUT`] can only race the future. Work done before the future exists
/// holds the executor thread, and no timer inside the same task can
/// interrupt it; an engine that blocks is a deployment's own bug, and the
/// bound that catches it is the request timeout in front of the broker.
pub async fn decide(engine: &dyn PolicyEngine, subject: &Subject, op: &Operation<'_>) -> Decision {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use futures_util::FutureExt as _;
        let built =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.decide(subject, op)));
        let Ok(fut) = built else {
            tracing::error!("policy engine {} panicked; denying", engine.name());
            metrics::counter!("antares_policy_failures_total", "reason" => "panic").increment(1);
            return Decision::Deny(ENGINE_FAILED.to_owned());
        };
        let guarded = std::panic::AssertUnwindSafe(fut).catch_unwind();
        match tokio::time::timeout(*TIMEOUT, guarded).await {
            Ok(Ok(d)) => resolve(op.clause, d),
            Ok(Err(_)) => {
                tracing::error!("policy engine {} panicked; denying", engine.name());
                metrics::counter!("antares_policy_failures_total", "reason" => "panic")
                    .increment(1);
                Decision::Deny(ENGINE_FAILED.to_owned())
            }
            Err(_) => {
                tracing::error!(
                    "policy engine {} did not answer within {:?}; denying",
                    engine.name(),
                    *TIMEOUT
                );
                metrics::counter!("antares_policy_failures_total", "reason" => "timeout")
                    .increment(1);
                Decision::Deny(ENGINE_TIMED_OUT.to_owned())
            }
        }
    }
    // The browser build has no timer to race against and aborts on panic;
    // it also loads no addon, so the engine is always the built-in one.
    #[cfg(target_arch = "wasm32")]
    {
        resolve(op.clause, engine.decide(subject, op).await)
    }
}

/// [`decide`] for a batch: one answer per Operation, failing closed the
/// same way. A panic, a timeout (one [`TIMEOUT`] for the whole batch) or an
/// answer whose length does not match the batch denies every member, and
/// each answer passes [`resolve`] for its own clause.
pub async fn decide_each(
    engine: &dyn PolicyEngine,
    subject: &Subject,
    ops: &[Operation<'_>],
) -> Vec<Decision> {
    let all = |why: &str| vec![Decision::Deny(why.to_owned()); ops.len()];
    #[cfg(not(target_arch = "wasm32"))]
    let answer = {
        use futures_util::FutureExt as _;
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engine.decide_each(subject, ops)
        }));
        let Ok(fut) = built else {
            tracing::error!("policy engine {} panicked; denying", engine.name());
            metrics::counter!("antares_policy_failures_total", "reason" => "panic").increment(1);
            return all(ENGINE_FAILED);
        };
        let guarded = std::panic::AssertUnwindSafe(fut).catch_unwind();
        match tokio::time::timeout(*TIMEOUT, guarded).await {
            Ok(Ok(d)) => d,
            Ok(Err(_)) => {
                tracing::error!("policy engine {} panicked; denying", engine.name());
                metrics::counter!("antares_policy_failures_total", "reason" => "panic")
                    .increment(1);
                return all(ENGINE_FAILED);
            }
            Err(_) => {
                tracing::error!(
                    "policy engine {} did not answer within {:?}; denying",
                    engine.name(),
                    *TIMEOUT
                );
                metrics::counter!("antares_policy_failures_total", "reason" => "timeout")
                    .increment(1);
                return all(ENGINE_TIMED_OUT);
            }
        }
    };
    #[cfg(target_arch = "wasm32")]
    let answer = engine.decide_each(subject, ops).await;
    if answer.len() != ops.len() {
        tracing::error!(
            "policy engine {} answered {} decisions for {} operations; denying",
            engine.name(),
            answer.len(),
            ops.len()
        );
        metrics::counter!("antares_policy_failures_total", "reason" => "length").increment(1);
        return all(ENGINE_FAILED);
    }
    answer
        .into_iter()
        .zip(ops)
        .map(|(d, op)| resolve(op.clause, d))
        .collect()
}

/// Whether any request of each of `clauses` could be allowed for the caller
/// of `headers`: one batch-shaped probe a surface lists its operations by
/// (MCP D-3), never a decision on a request. Each clause is asked as an
/// Operation that names nothing, through [`decide_each`], so it fails
/// closed the same way (a panic, a timeout or a short answer hides every
/// clause), and a narrowing on a clause that cannot be narrowed hides it.
/// With no engine attached every clause is offered.
// ponytail: no `PolicyEngine::offers` hook; an engine that has to answer a
// probe differently from an empty Operation gets one when it exists.
pub async fn offers(
    engine: Option<&dyn PolicyEngine>,
    tenant: &antares_model::TenantId,
    headers: &HeaderMap,
    clauses: &[&'static str],
) -> Vec<bool> {
    let Some(engine) = engine else {
        return vec![true; clauses.len()];
    };
    let ops: Vec<Operation<'_>> = clauses
        .iter()
        .map(|clause| Operation {
            clause,
            ids: &[],
            types: &[],
            attrs: &[],
            q: None,
            scope_q: None,
            geo: None,
            body: None,
            stored: None,
        })
        .collect();
    decide_each(engine, &subject_of(tenant, headers), &ops)
        .await
        .into_iter()
        .map(|d| !matches!(d, Decision::Deny(_)))
        .collect()
}

/// Ask the engine about one notification, and fail closed: an engine that
/// panics drops the notification. The document it was handed may already be
/// half-edited, and the broker cannot know which half — so it is not sent.
pub fn pre_notify(
    engine: &dyn PolicyEngine,
    subject: &Subject,
    sub: &Value,
    notification: &mut Value,
) -> NotifyDecision {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine.pre_notify(subject, sub, notification)
    })) {
        Ok(d) => d,
        Err(_) => {
            tracing::error!(
                "policy engine {} panicked on a notification; dropping it",
                engine.name()
            );
            metrics::counter!("antares_policy_failures_total", "reason" => "panic").increment(1);
            NotifyDecision::Drop
        }
    }
}

/// The ProblemDetails `type` and `title` of a refusal. Table 6.3.2-1 names
/// no access-denied error, so none is invented under the ETSI namespace: a
/// refusal is answered with this broker's own URN, documented in the book
/// and noted in the 6.3.2 ledger entry. A client that reads error types can
/// tell a policy refusal from a spec error by the namespace alone.
pub const ACCESS_DENIED_TYPE: &str = "urn:antares:error:AccessDenied";
pub const ACCESS_DENIED_TITLE: &str = "AccessDenied";

/// The request headers copied into a [`Subject`], comma-separated
/// (`ANTARES_POLICY_SUBJECT_HEADERS`), matched case-insensitively and read
/// once at first use. Empty by default: an engine that wants an identity
/// names the header that carries it, and a broker that copied every header
/// into the subject would be handing an addon the whole request.
pub static SUBJECT_HEADERS: LazyLock<Vec<String>> = LazyLock::new(|| {
    std::env::var("ANTARES_POLICY_SUBJECT_HEADERS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
});

/// Who this request is from, as far as the seam is concerned: the tenant it
/// addresses and the headers a deployment named. A header the request does
/// not carry is simply absent — the seam invents nothing.
pub fn subject_of(tenant: &antares_model::TenantId, headers: &HeaderMap) -> Subject {
    let mut carried = Vec::new();
    for name in SUBJECT_HEADERS.iter() {
        for value in headers.get_all(name.as_str()) {
            if let Ok(v) = value.to_str() {
                carried.push((name.clone(), v.to_owned()));
            }
        }
    }
    Subject {
        tenant: tenant.clone(),
        headers: carried,
    }
}

/// The member a Subscription carries its creator's subject in. A
/// broker-internal member like `__context` and `__via`: a client can
/// neither set it nor read it back, it is stripped from every served
/// representation and from the 5.8.1.4 copy forwarded to a Context Source,
/// and 5.2.12 defines no such member — the whole `__` prefix is the
/// broker's, so a member added later inherits every one of those rules
/// instead of a new list to forget.
pub(crate) const SUBJECT_MEMBER: &str = "__subject";

/// What a write by id hands the engine about the Entity it lands on.
#[derive(Debug, Default)]
pub(crate) struct PreImage {
    /// The stored Entity's types joined with the ones the request body
    /// names: an engine judging by type judges the Entity the write changes
    /// and what it changes it to (ADR-0020).
    pub types: Vec<String>,
    /// The stored Entity itself ([`Operation::stored`]).
    pub stored: Option<StoredImage>,
}

/// The [`PreImage`] of a write by id. The store is read once per request,
/// and only with an engine attached, so vanilla reads nothing more. An
/// absent Entity or a store error gives no stored image, and the write
/// answers it as before.
///
/// Decided (worker-2, T-3483, owner may override): the write still reads
/// the Entity again inside its own store operation. Handing it this image,
/// read outside the store's transaction, would turn a concurrent write
/// into a lost update; the gate's read is the one the type lookup already
/// made, so no read is added.
pub(crate) async fn pre_image(
    st: &crate::state::AppState,
    tenant: &antares_model::TenantId,
    id: &str,
    body: Option<&Value>,
) -> PreImage {
    if st.policy.is_none() {
        return PreImage::default();
    }
    let stored = st
        .store
        .get(tenant, antares_store::Kind::Entity, id)
        .await
        .ok()
        .flatten();
    image(stored, body)
}

/// The [`PreImage`] of a temporal write by id (5.6.12 to 5.6.16, T-3607):
/// the Entity's temporal evolution, which a history-only Entity has
/// without a current state, else its current state. Read only with an
/// engine attached, as [`pre_image`].
pub(crate) async fn temporal_pre_image(
    st: &crate::state::AppState,
    tenant: &antares_model::TenantId,
    id: &str,
) -> PreImage {
    if st.policy.is_none() {
        return PreImage::default();
    }
    match st.temporal.get(tenant, id).await.ok().flatten() {
        Some(evolution) => image(Some(evolution), None),
        None => pre_image(st, tenant, id, None).await,
    }
}

/// The image of a stored document, with the types the body adds.
fn image(stored: Option<Value>, body: Option<&Value>) -> PreImage {
    let mut types: Vec<String> = Vec::new();
    for doc in stored.as_ref().into_iter().chain(body) {
        for t in entity_types(doc) {
            if !types.iter().any(|o| o == t) {
                types.push(t.to_owned());
            }
        }
    }
    PreImage {
        types,
        stored: stored.as_ref().map(StoredImage::of),
    }
}

/// The stored form of a subject: only the headers a deployment named, since
/// the tenant is where the subscription already lives. `None` when the
/// subject carries nothing, so a deployment that named no header stores no
/// member at all.
pub(crate) fn subject_member(subject: &Subject) -> Option<Value> {
    (!subject.headers.is_empty())
        .then(|| serde_json::to_value(&subject.headers).ok())
        .flatten()
}

/// The subject a notification is delivered under: the one stored when the
/// subscription was created. A subscription created before the deployment
/// named its headers — or by the broker itself, for the 5.8.1.4 internal
/// copies — simply has none, and the engine is asked about a subject with
/// no headers rather than about the wrong one.
pub(crate) fn stored_subject(tenant: &antares_model::TenantId, sub: &Value) -> Subject {
    Subject {
        tenant: tenant.clone(),
        headers: sub
            .get(SUBJECT_MEMBER)
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default(),
    }
}

/// Remove every broker-internal member from a document about to be served.
///
/// The `__` prefix is the broker's: the notification `@context` (5.8.6), the
/// 6.3.18 Via chain, a snapshot's synthetic tenant and [`SUBJECT_MEMBER`]
/// all live under it, and no NGSI-LD data type defines a member there. One
/// predicate rather than a list per document type, so a member added later
/// is hidden by construction instead of by remembering every serve point.
pub(crate) fn strip_internal(doc: &mut Value) {
    if let Some(o) = doc.as_object_mut() {
        o.retain(|k, _| !k.starts_with("__"));
    }
}

/// Whether a stored document was made by this subject. Compared on the
/// headers alone: the tenant is where the document already lives, and a
/// deployment that named no header has no subjects to tell apart — every
/// request is the same one, and everything it stored is its own.
pub(crate) fn belongs_to(doc: &Value, subject: &Subject) -> bool {
    stored_subject(&subject.tenant, doc).headers == subject.headers
}

/// A refusal on its way out of a handler. Its own type rather than an
/// `ApiError`, so this module names nothing above it and stays a leaf; the
/// `?` in a handler converts it through `From`.
#[derive(Debug, Clone, PartialEq)]
pub struct Denied(pub String);

/// The one call a handler makes. Every operation passes through it exactly
/// once, which is what `every_route_asks_the_policy_engine_exactly_once` asserts by
/// walking the router with a counting engine.
///
/// The returned [`Filter`] is what the answer has to be narrowed by; an
/// allow returns the empty filter, which narrows nothing.
pub async fn gate(
    engine: &dyn PolicyEngine,
    tenant: &antares_model::TenantId,
    headers: &HeaderMap,
    op: &Operation<'_>,
) -> Result<Filter, Denied> {
    match decide(engine, &subject_of(tenant, headers), op).await {
        Decision::Allow => Ok(Filter::default()),
        Decision::Filter(f) => Ok(f),
        Decision::Deny(why) => Err(Denied(why)),
    }
}

/// [`gate`](fn@gate) for a batch: the refusal reason of each member, `None` where
/// the member is allowed. A batch writes its members whole, so a `Filter`
/// on a member is a refusal ([`resolve`]: no batch clause is FILTERABLE).
pub async fn gate_each(
    engine: &dyn PolicyEngine,
    tenant: &antares_model::TenantId,
    headers: &HeaderMap,
    ops: &[Operation<'_>],
) -> Vec<Option<String>> {
    decide_each(engine, &subject_of(tenant, headers), ops)
        .await
        .into_iter()
        .map(|d| match d {
            Decision::Deny(why) => Some(why),
            Decision::Allow | Decision::Filter(_) => None,
        })
        .collect()
}

/// The members of a JSON object, or nothing when it is not one.
#[cfg(any(test, feature = "test-kit"))]
fn members(v: &Value) -> Vec<String> {
    v.as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

/// The contract every engine has to hold, so an addon's own tests can call
/// it. It asserts the three things an engine can actually get wrong — it
/// stops answering, it hands back an answer the seam has to override, or it
/// puts something into a notification that was not there — and it asserts
/// them through the seam, so an engine that passes here passes as the
/// broker will call it.
///
/// The core runs it against [`AllowAll`]; `examples/plugin-example` runs it
/// against the reference engine.
#[cfg(any(test, feature = "test-kit"))]
pub async fn run_policy_contract(engine: &dyn PolicyEngine) {
    let name = engine.name();
    assert!(!name.is_empty(), "an engine answers to a name");

    let subject = Subject {
        tenant: antares_model::TenantId::default(),
        headers: vec![("X-Subject".into(), "someone".into())],
    };
    let doc = serde_json::json!({
        "id": "urn:ngsi-ld:Vehicle:1",
        "type": "Vehicle",
        "speed": {"type": "Property", "value": 10},
        "brand": {"type": "Property", "value": "Skoda"}
    });

    // The engine answers, and the answer is its own: a deny carrying one of
    // the seam's own reasons means it timed out or panicked instead.
    for clause in ["5.6.1", "5.7.2", "5.8.1"] {
        match decide(engine, &subject, &Operation::new(clause)).await {
            Decision::Allow => {}
            Decision::Deny(why) => assert!(
                why != ENGINE_TIMED_OUT && why != ENGINE_FAILED,
                "{name}: {clause} was denied by the seam, not by the engine: {why}"
            ),
            Decision::Filter(f) => {
                let mut narrowed = doc.clone();
                f.project(&mut narrowed);
                let before = members(&doc);
                for key in members(&narrowed) {
                    assert!(
                        before.contains(&key),
                        "{name}: the filter for {clause} added {key:?} to the answer"
                    );
                }
            }
        }
    }

    // An operation the broker performs whole is allowed or refused, never
    // done to less than it says: WHOLE_TENANT because there is no narrowed
    // form of "delete everything", and the writes because no handler there
    // reads a Filter at all (FILTERABLE).
    for clause in WHOLE_TENANT
        .iter()
        .chain(["5.6.1", "5.6.6", "5.8.1", "5.9.2"].iter())
    {
        assert!(
            !matches!(
                decide(engine, &subject, &Operation::new(clause)).await,
                Decision::Filter(_)
            ),
            "{name}: {clause} was answered with a filter"
        );
    }

    // `pre_notify` holds the notification by `&mut`, which is the one place
    // an engine can widen rather than narrow: a member it puts there is a
    // member no subscriber asked for and no store answered with.
    let sub = serde_json::json!({"id": "urn:ngsi-ld:Subscription:1", "type": "Subscription"});
    let mut notification = serde_json::json!({
        "id": "urn:ngsi-ld:Notification:1",
        "type": "Notification",
        "subscriptionId": "urn:ngsi-ld:Subscription:1",
        "notifiedAt": "2026-01-01T00:00:00Z",
        "data": [doc.clone()]
    });
    let before = members(&notification);
    let entity_members = members(&doc);
    if pre_notify(engine, &subject, &sub, &mut notification) != NotifyDecision::Drop {
        for key in members(&notification) {
            assert!(
                before.contains(&key),
                "{name}: pre_notify added {key:?} to the notification"
            );
        }
        let served = notification
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for entity in &served {
            for key in members(entity) {
                assert!(
                    entity_members.contains(&key),
                    "{name}: pre_notify added {key:?} to a notified Entity"
                );
            }
        }
    }

    // And the seam stops waiting for this engine, whatever it does.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let slow = Slow(engine);
        assert_eq!(
            decide(&slow, &subject, &Operation::new("5.6.1")).await,
            Decision::Deny(ENGINE_TIMED_OUT.to_owned()),
            "{name}: the seam waited past its timeout instead of denying"
        );
    }
}

/// The same engine, one timeout slower: what the contract wraps it in to
/// prove the seam stops waiting.
#[cfg(all(any(test, feature = "test-kit"), not(target_arch = "wasm32")))]
struct Slow<'e>(&'e dyn PolicyEngine);

#[cfg(all(any(test, feature = "test-kit"), not(target_arch = "wasm32")))]
impl PolicyEngine for Slow<'_> {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn decide<'a>(&'a self, subject: &'a Subject, op: &'a Operation<'a>) -> DecisionFuture<'a> {
        Box::pin(async move {
            tokio::time::sleep(*TIMEOUT * 2 + Duration::from_millis(50)).await;
            self.0.decide(subject, op).await
        })
    }

    fn pre_notify(&self, s: &Subject, sub: &Value, n: &mut Value) -> NotifyDecision {
        self.0.pre_notify(s, sub, n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `admits` is `shown(..).is_some()` without the set (T-3667).
    #[test]
    fn admits_is_whether_shown_answers() {
        let ctx = antares_jsonld::core_context();
        let car = serde_json::json!({"id": "urn:ngsi-ld:Car:1", "type": "Car"});
        let rule = |t: &str| Rule {
            types: vec![t.to_owned()],
            ..Rule::default()
        };
        let cases = [
            Filter::default(),
            Filter {
                omit_by_type: BTreeMap::from([("Car".to_owned(), vec!["speed".to_owned()])]),
                ..Filter::default()
            },
            Filter {
                rules: vec![rule("Car")],
                ..Filter::default()
            },
            Filter {
                rules: vec![rule("User")],
                ..Filter::default()
            },
            Filter {
                rules: vec![rule("User"), rule("Car")],
                ..Filter::default()
            },
        ];
        let admitted: Vec<bool> = cases.iter().map(|f| f.admits(&car, &ctx)).collect();
        assert_eq!(admitted, [true, true, true, false, true]);
        for f in &cases {
            assert_eq!(f.admits(&car, &ctx), f.shown(&car, &ctx).is_some(), "{f:?}");
        }
    }

    /// T-3482: only the caller's own q is rewritten; the engine's
    /// condition may read what it hides from the caller.
    #[test]
    fn a_hidden_term_is_rewritten_and_the_engines_own_q_is_not() {
        let filter = Filter {
            omit: vec!["salary".into()],
            q: Some(Box::new(antares_ql::parse_q("salary>0").expect("q"))),
            ..Filter::default()
        };
        let ctx = antares_jsonld::core_context();
        let params = std::collections::HashMap::from([(
            "q".to_owned(),
            "salary==1|!salary|name==\"x\"".to_owned(),
        )]);
        let out = filter.narrow_params(&params, &ctx).expect("narrowed");
        assert_eq!(
            out["q"],
            "(salary;!salary|(salary|!salary)|name==\"x\");salary>0"
        );
        let nothing_hidden = Filter::default();
        let out = nothing_hidden
            .narrow_params(&params, &ctx)
            .expect("narrowed");
        assert_eq!(out["q"], params["q"]);
    }
    use futures_util::FutureExt as _;

    const NS: &str = "https://uri.etsi.org/ngsi-ld/default-context/";

    fn iri(n: &str) -> String {
        format!("{NS}{n}")
    }

    /// Zone A: the square (0,0)-(1,1).
    fn zone_a() -> Box<GeoQuery> {
        let params = std::collections::HashMap::from([
            ("georel".to_owned(), "within".to_owned()),
            ("geometry".to_owned(), "Polygon".to_owned()),
            (
                "coordinates".to_owned(),
                "[[[0,0],[1,0],[1,1],[0,1],[0,0]]]".to_owned(),
            ),
        ]);
        Box::new(
            GeoQuery::from_params(&params)
                .expect("geoquery")
                .expect("present"),
        )
    }

    fn rule(t: &str, attrs: &[&str]) -> Rule {
        Rule {
            types: vec![iri(t)],
            attributes: attrs.iter().map(|a| iri(a)).collect(),
            ..Rule::default()
        }
    }

    /// `{Car in zone A: speed, location, status}`, `{Car: status}`,
    /// `{User: name}`, in the internal form `repr::type_rules` produces.
    fn zones() -> Filter {
        Filter {
            rules: vec![
                Rule {
                    geo_q: Some(zone_a()),
                    attributes: vec![
                        iri("speed"),
                        antares_ql::geo::LOCATION_IRI.to_owned(),
                        iri("status"),
                    ],
                    ..rule("Car", &[])
                },
                rule("Car", &["status"]),
                rule("User", &["name"]),
            ],
            ..Filter::default()
        }
    }

    fn car(lon: f64) -> Value {
        json!({"id": "urn:ngsi-ld:Car:1", "type": [iri("Car")],
               (iri("speed")): [{"type": "Property", "value": 80}],
               (iri("status")): [{"type": "Property", "value": "ok"}],
               "https://uri.etsi.org/ngsi-ld/location": [{"type": "GeoProperty",
                   "value": {"type": "Point", "coordinates": [lon, 0.5]}}]})
    }

    fn ctx() -> antares_jsonld::Context {
        antares_jsonld::core_context()
    }

    /// Gap G2: a rule that narrows ids selects by id, and a document with no
    /// id only when it narrows none.
    #[test]
    fn a_rule_selects_by_type_and_id() {
        let any = rule("Car", &["speed"]);
        let one = Rule {
            ids: vec!["urn:ngsi-ld:Car:1".into()],
            ..rule("Car", &["speed"])
        };
        let doc = car(0.5);
        let anonymous = json!({"type": [iri("Car")]});
        assert!(any.selects(&doc) && one.selects(&doc));
        assert!(any.selects(&anonymous), "no id narrowing, no id needed");
        assert!(!one.selects(&anonymous), "an id narrowing needs an id");
        assert!(!one.selects(&json!({"id": "urn:ngsi-ld:Car:2", "type": [iri("Car")]})));
        assert!(!one.selects(&json!({"id": "urn:ngsi-ld:Car:1", "type": [iri("Bus")]})));
    }

    /// ADR-0023 rule 11: the union over the rules that hold, nothing for an
    /// Entity none holds for, and an omit on any type wins.
    #[test]
    fn shown_is_the_union_of_the_rules_that_hold_and_any_omit_wins() {
        let f = zones();
        let ctx = ctx();
        let inside = f.shown(&car(0.5), &ctx).expect("in view");
        let outside = f.shown(&car(5.0), &ctx).expect("in view");
        assert!(inside.allows(&iri("speed")) && inside.allows(&iri("status")));
        assert!(!outside.allows(&iri("speed")), "speed only in zone A");
        assert!(outside.allows(&iri("status")));
        let robot = json!({"id": "urn:ngsi-ld:Robot:1", "type": [iri("Robot")]});
        assert_eq!(f.shown(&robot, &ctx), None, "no rule holds: out");
        let both = json!({"id": "urn:x", "type": [iri("Car"), iri("User")],
                          (iri("name")): [{"type": "Property", "value": "n"}]});
        let shown = f.shown(&both, &ctx).expect("in view");
        assert!(
            shown.allows(&iri("name")) && shown.allows(&iri("status")),
            "union"
        );
        let omitting = Filter {
            omit_by_type: BTreeMap::from([(iri("User"), vec![iri("status")])]),
            ..zones()
        };
        let shown = omitting.shown(&both, &ctx).expect("in view");
        assert!(!shown.allows(&iri("status")), "an omit on any type wins");
        let only_omit = Filter {
            omit_by_type: omitting.omit_by_type.clone(),
            ..Filter::default()
        };
        let shown = only_omit.shown(&robot, &ctx).expect("no rules: in");
        assert!(shown.allows(&iri("serial")), "no rules: omit alone");
    }

    /// A rule whose `q` follows a link never holds (fail closed), and a
    /// rule naming no type holds for nothing.
    #[test]
    fn a_rule_reading_a_link_or_naming_no_type_never_holds() {
        let ctx = ctx();
        let linked = Rule {
            q: Some(Box::new(
                antares_ql::parse_q("ownedBy{name}==\"x\"").expect("q"),
            )),
            ..rule("Car", &["speed"])
        };
        assert!(linked.q.as_deref().is_some_and(QNode::has_linked_paths));
        assert!(!linked.holds(&car(0.5), &ctx));
        let negated = Rule {
            q: Some(Box::new(antares_ql::parse_q("!ownedBy{name}").expect("q"))),
            ..rule("Car", &["speed"])
        };
        assert!(!negated.holds(&car(0.5), &ctx), "not even negated");
        assert!(!Rule::default().holds(&car(0.5), &ctx));
        let local = Rule {
            q: Some(Box::new(antares_ql::parse_q("speed>50").expect("q"))),
            ..rule("Car", &["status"])
        };
        assert!(local.holds(&car(0.5), &ctx));
    }

    /// ADR-0023 rule 7 per rule (K22): a write must fall whole inside one
    /// rule that shows every Attribute it names.
    #[test]
    fn write_that_adds_speed_outside_zone_a_is_refused() {
        let f = zones();
        let ctx = ctx();
        let speed = iri("speed");
        let status = iri("status");
        assert!(f.admits_write(&car(0.5), &[&speed, &status], &ctx));
        assert!(
            !f.admits_write(&car(5.0), &[&speed], &ctx),
            "speed outside A"
        );
        assert!(f.admits_write(&car(5.0), &[&status], &ctx));
        let name = iri("name");
        assert!(
            !f.admits_write(&car(0.5), &[&speed, &name], &ctx),
            "no one rule shows both"
        );
        let robot = json!({"id": "urn:ngsi-ld:Robot:1", "type": [iri("Robot")]});
        assert!(!f.admits_write(&robot, &[], &ctx), "outside every rule");
    }

    /// The loader's disclosure report: zone A's rule reads `location` and
    /// shows it, so it discloses nothing; a rule on `speed>100` showing only
    /// `status` discloses `speed`.
    #[test]
    fn discloses_names_what_the_condition_reads_and_the_rule_does_not_show() {
        let ctx = ctx();
        assert!(zones().rules[0].discloses(&ctx).is_empty());
        let fast = Rule {
            q: Some(Box::new(antares_ql::parse_q("speed>100").expect("q"))),
            geo_q: Some(zone_a()),
            ..rule("Car", &["status"])
        };
        assert_eq!(
            fast.discloses(&ctx),
            vec![
                iri("speed"),
                "https://uri.etsi.org/ngsi-ld/location".to_owned()
            ]
        );
    }

    #[test]
    fn project_keeps_the_union_and_the_frame() {
        let f = Filter {
            rules: vec![
                Rule {
                    types: vec!["Car".to_owned()],
                    attributes: vec!["speed".to_owned(), "email".to_owned()],
                    ..Rule::default()
                },
                Rule {
                    types: vec!["User".to_owned()],
                    attributes: vec!["name".to_owned()],
                    ..Rule::default()
                },
            ],
            omit_by_type: BTreeMap::from([("User".to_owned(), vec!["email".to_owned()])]),
            ..Filter::default()
        };
        let mut doc = json!({"id": "urn:x", "type": ["Car", "User"], "@context": "c",
                             "speed": 1, "name": 2, "email": 3, "serial": 4});
        f.project(&mut doc);
        assert_eq!(
            doc,
            json!({"id": "urn:x", "type": ["Car", "User"], "@context": "c",
                   "speed": 1, "name": 2})
        );
    }
    use serde_json::json;

    fn subject() -> Subject {
        Subject {
            tenant: antares_model::TenantId::default(),
            headers: vec![("X-Subject".into(), "a-token-shaped-string".into())],
        }
    }

    /// Run the contract and report whether it refused the engine.
    async fn contract_holds(engine: &dyn PolicyEngine) -> bool {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::AssertUnwindSafe(run_policy_contract(engine))
            .catch_unwind()
            .await;
        std::panic::set_hook(hook);
        outcome.is_ok()
    }

    #[tokio::test]
    async fn the_built_in_engine_holds_the_contract() {
        run_policy_contract(&AllowAll).await;
    }

    /// An engine that narrows is what the seam is for, and the contract
    /// must not stand in its way.
    struct Narrowing;

    impl PolicyEngine for Narrowing {
        fn name(&self) -> &str {
            "narrowing"
        }
        fn decide<'a>(&'a self, _s: &'a Subject, _o: &'a Operation<'a>) -> DecisionFuture<'a> {
            Box::pin(std::future::ready(Decision::Filter(Filter {
                omit: vec!["brand".into()],
                restricted: true,
                ..Filter::default()
            })))
        }
        fn pre_notify(&self, _s: &Subject, _sub: &Value, n: &mut Value) -> NotifyDecision {
            if let Some(data) = n.get_mut("data").and_then(Value::as_array_mut) {
                for entity in data {
                    if let Some(o) = entity.as_object_mut() {
                        o.remove("brand");
                    }
                }
            }
            NotifyDecision::Filter(Filter {
                omit: vec!["brand".into()],
                ..Filter::default()
            })
        }
    }

    #[tokio::test]
    async fn an_engine_that_only_narrows_holds_the_contract() {
        assert!(contract_holds(&Narrowing).await);
    }

    /// The contract has teeth or it proves nothing: this engine puts a
    /// member into a notification nobody asked for.
    struct Widening;

    impl PolicyEngine for Widening {
        fn name(&self) -> &str {
            "widening"
        }
        fn decide<'a>(&'a self, _s: &'a Subject, _o: &'a Operation<'a>) -> DecisionFuture<'a> {
            Box::pin(std::future::ready(Decision::Allow))
        }
        fn pre_notify(&self, _s: &Subject, _sub: &Value, n: &mut Value) -> NotifyDecision {
            if let Some(o) = n.as_object_mut() {
                o.insert("stowaway".into(), json!(true));
            }
            NotifyDecision::Deliver
        }
    }

    #[tokio::test]
    async fn the_contract_refuses_an_engine_that_widens_a_notification() {
        assert!(
            !contract_holds(&Widening).await,
            "the contract passed an engine that added a member to a notification"
        );
    }

    #[test]
    fn a_pick_keeps_the_frame_and_drops_the_rest() {
        let mut doc = json!({"id": "urn:ngsi-ld:Vehicle:1", "type": "Vehicle",
                             "speed": 1, "brand": "Skoda"});
        Filter {
            pick: vec!["speed".into()],
            ..Filter::default()
        }
        .project(&mut doc);
        assert_eq!(
            doc,
            json!({"id": "urn:ngsi-ld:Vehicle:1", "type": "Vehicle", "speed": 1})
        );
    }

    #[test]
    fn an_omit_cannot_remove_what_makes_it_an_entity() {
        let mut doc = json!({"id": "urn:ngsi-ld:Vehicle:1", "type": "Vehicle", "speed": 1});
        Filter {
            omit: vec!["id".into(), "type".into(), "speed".into()],
            ..Filter::default()
        }
        .project(&mut doc);
        assert_eq!(
            doc,
            json!({"id": "urn:ngsi-ld:Vehicle:1", "type": "Vehicle"})
        );
    }

    #[test]
    fn a_pick_of_something_absent_adds_nothing() {
        let mut doc = json!({"id": "urn:ngsi-ld:Vehicle:1", "type": "Vehicle", "speed": 1});
        Filter {
            pick: vec!["mileage".into()],
            ..Filter::default()
        }
        .project(&mut doc);
        assert_eq!(
            doc,
            json!({"id": "urn:ngsi-ld:Vehicle:1", "type": "Vehicle"})
        );
    }

    #[test]
    fn a_projection_leaves_a_non_object_alone() {
        let mut doc = json!(["not", "an", "object"]);
        Filter {
            pick: vec!["speed".into()],
            omit: vec!["brand".into()],
            ..Filter::default()
        }
        .project(&mut doc);
        assert_eq!(doc, json!(["not", "an", "object"]));
    }

    #[test]
    fn narrowing_a_whole_tenant_operation_is_a_refusal() {
        let narrowed = Decision::Filter(Filter {
            omit: vec!["speed".into()],
            ..Filter::default()
        });
        for clause in WHOLE_TENANT {
            assert!(matches!(
                resolve(clause, narrowed.clone()),
                Decision::Deny(_)
            ));
        }
        // and every other clause outside FILTERABLE, because the handler
        // there never reads the Filter
        for clause in ["5.6.1", "5.6.6", "5.8.1", "5.9.2", "5.13.2", "5.8.4"] {
            assert!(
                matches!(resolve(clause, narrowed.clone()), Decision::Deny(_)),
                "{clause} dropped a narrowing instead of refusing it"
            );
        }
    }

    /// The reads that do read it keep it: the seam refuses what would be
    /// dropped, and nothing more.
    #[test]
    fn a_filterable_read_keeps_its_narrowing() {
        let narrowed = Decision::Filter(Filter {
            omit: vec!["speed".into()],
            ..Filter::default()
        });
        for clause in FILTERABLE {
            assert_eq!(resolve(clause, narrowed.clone()), narrowed, "{clause}");
        }
    }

    #[test]
    fn an_empty_filter_on_a_whole_tenant_operation_is_not_a_refusal() {
        assert_eq!(
            resolve("5.6.21", Decision::Filter(Filter::default())),
            Decision::Allow
        );
    }

    struct Panicking;

    impl PolicyEngine for Panicking {
        fn name(&self) -> &str {
            "panicking"
        }
        fn decide<'a>(&'a self, _s: &'a Subject, _o: &'a Operation<'a>) -> DecisionFuture<'a> {
            Box::pin(async { panic!("the engine is broken") })
        }
        fn pre_notify(&self, _s: &Subject, _sub: &Value, _n: &mut Value) -> NotifyDecision {
            panic!("the engine is broken")
        }
    }

    /// The other half of the same failure: a synchronous engine decides in
    /// the call that BUILDS the future, so a guard around the future alone
    /// never sees it. The reference engine has exactly this shape.
    struct PanickingEagerly;

    impl PolicyEngine for PanickingEagerly {
        fn name(&self) -> &str {
            "panicking-eagerly"
        }
        fn decide<'a>(&'a self, _s: &'a Subject, _o: &'a Operation<'a>) -> DecisionFuture<'a> {
            panic!("the engine is broken")
        }
        fn pre_notify(&self, _s: &Subject, _sub: &Value, _n: &mut Value) -> NotifyDecision {
            NotifyDecision::Deliver
        }
    }

    #[tokio::test]
    async fn an_engine_that_panics_before_it_returns_a_future_denies_too() {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let decided = decide(&PanickingEagerly, &subject(), &Operation::new("5.6.1")).await;
        std::panic::set_hook(hook);
        assert_eq!(decided, Decision::Deny(ENGINE_FAILED.to_owned()));
    }

    #[tokio::test]
    async fn an_engine_that_panics_denies_rather_than_allows() {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let decided = decide(&Panicking, &subject(), &Operation::new("5.6.1")).await;
        let mut notification = json!({"type": "Notification"});
        let notified = pre_notify(&Panicking, &subject(), &json!({}), &mut notification);
        std::panic::set_hook(hook);
        assert_eq!(decided, Decision::Deny(ENGINE_FAILED.to_owned()));
        assert_eq!(notified, NotifyDecision::Drop);
    }

    /// Real time, not a paused clock: the workspace tokio is built without
    /// `test-util`, so the test pays the timeout it asserts.
    #[tokio::test]
    async fn an_engine_that_never_answers_denies() {
        struct Never;
        impl PolicyEngine for Never {
            fn name(&self) -> &str {
                "never"
            }
            fn decide<'a>(&'a self, _s: &'a Subject, _o: &'a Operation<'a>) -> DecisionFuture<'a> {
                Box::pin(std::future::pending())
            }
            fn pre_notify(&self, _s: &Subject, _sub: &Value, _n: &mut Value) -> NotifyDecision {
                NotifyDecision::Deliver
            }
        }
        assert_eq!(
            decide(&Never, &subject(), &Operation::new("5.6.1")).await,
            Decision::Deny(ENGINE_TIMED_OUT.to_owned())
        );
    }

    #[test]
    fn the_subject_never_prints_its_header_values() {
        let printed = format!("{:?}", subject());
        assert!(printed.contains("X-Subject"), "{printed}");
        assert!(
            !printed.contains("a-token-shaped-string"),
            "a header value reached a log line: {printed}"
        );
    }

    #[test]
    fn an_operation_never_prints_the_payload() {
        let body = json!({"id": "urn:ngsi-ld:Vehicle:1", "plate": "BB-123-XY"});
        let printed = format!(
            "{:?}",
            Operation {
                body: Some(&body),
                ..Operation::new("5.6.1")
            }
        );
        assert!(!printed.contains("BB-123-XY"), "{printed}");
    }

    /// T-3552: the policy survivors of mutants run 38073511508.
    #[test]
    fn the_timeout_takes_a_positive_integer_or_falls_back() {
        assert_eq!(timeout_ms(None), 250);
        assert_eq!(timeout_ms(Some("0")), 250, "zero would deny every request");
        assert_eq!(timeout_ms(Some("x")), 250);
        assert_eq!(timeout_ms(Some("1")), 1);
        assert_eq!(timeout_ms(Some("300")), 300);
    }

    #[test]
    fn a_single_string_counts_as_a_one_element_list() {
        let doc = json!({"type": "T", "scope": "/a"});
        assert_eq!(entity_types(&doc), ["T"]);
        assert_eq!(StoredImage::of(&doc).scope, ["/a"]);
        let listed = json!({"type": ["T", "U"], "scope": ["/a", "/b"]});
        assert_eq!(entity_types(&listed), ["T", "U"]);
        assert_eq!(StoredImage::of(&listed).scope, ["/a", "/b"]);
    }

    #[test]
    fn an_operation_logs_its_shape_and_never_its_payload() {
        let op = Operation::new("5.6.1");
        let shown = format!("{op:?}");
        assert!(shown.starts_with("Operation {"), "{shown}");
        assert!(shown.contains("clause: \"5.6.1\""), "{shown}");
        assert!(shown.contains("ids: 0"), "{shown}");
    }

    #[test]
    fn a_rule_is_conditional_on_any_one_of_its_conditions() {
        let plain = rule("Car", &["speed"]);
        assert!(!plain.is_conditional());
        let q = Rule {
            q: Some(Box::new(antares_ql::parse_q("speed>1").expect("q"))),
            ..plain.clone()
        };
        let scope = Rule {
            scope_q: Some("/a".to_owned()),
            ..plain.clone()
        };
        assert!(q.is_conditional() && scope.is_conditional());
    }

    #[test]
    fn shown_nothing_admits_no_attribute() {
        let nothing = Shown::nothing();
        assert_eq!(nothing.only, Some(Default::default()));
        assert!(!nothing.allows(&iri("name")));
    }

    #[test]
    fn a_write_is_refused_only_for_the_attribute_its_type_omits() {
        let ctx = ctx();
        let f = Filter {
            omit_by_type: BTreeMap::from([(iri("User"), vec![iri("status")])]),
            ..Filter::default()
        };
        let user = json!({"id": "urn:x", "type": [iri("User")]});
        assert!(f.admits_write(&user, &[iri("name").as_str()], &ctx));
        assert!(!f.admits_write(&user, &[iri("status").as_str()], &ctx));
    }

    /// T-3552 (mutants run 38081990103): the image of a stored Entity names
    /// its Attributes, never what frames, dates or annotates it.
    #[test]
    fn a_stored_image_names_attributes_only() {
        let doc = json!({"id": "urn:ngsi-ld:Car:1", "type": [iri("Car")], "scope": ["/a"],
                         "createdAt": "t", "modifiedAt": "t", "deletedAt": "t",
                         "@context": [], "__tenant": "acme",
                         (iri("speed")): [{"type": "Property", "value": 1}]});
        let image = StoredImage::of(&doc);
        assert_eq!(image.types, vec![iri("Car")]);
        assert_eq!(image.scope, vec!["/a".to_owned()]);
        assert_eq!(image.attribute_names, vec![iri("speed")]);
    }

    #[test]
    fn mapping_the_names_maps_both_sets() {
        let shown = Shown {
            only: Some(std::collections::BTreeSet::from(["a".to_owned()])),
            omitted: std::collections::BTreeSet::from(["b".to_owned()]),
        };
        let mapped = shown.map_names(str::to_uppercase);
        assert_eq!(
            mapped.only,
            Some(std::collections::BTreeSet::from(["A".to_owned()]))
        );
        assert_eq!(
            mapped.omitted,
            std::collections::BTreeSet::from(["B".to_owned()])
        );
    }

    #[test]
    fn a_filter_says_its_areas_its_rules_and_its_restriction() {
        let both = Filter {
            geo_q: Some(zone_a()),
            geo_also: vec![*zone_a()],
            ..Filter::default()
        };
        assert_eq!(both.areas().count(), 2);
        let also = Filter {
            geo_also: vec![*zone_a()],
            ..Filter::default()
        };
        assert_eq!(also.areas().count(), 1);
        assert!(!also.is_empty());

        assert!(!Filter::default().has_rules());
        assert!(zones().has_rules(), "rules alone");
        let omits = Filter {
            omit_by_type: BTreeMap::from([(iri("User"), vec![iri("status")])]),
            ..Filter::default()
        };
        assert!(omits.has_rules(), "per-type omits alone");

        let mut headers = HeaderMap::new();
        Filter::default().mark_restricted(&mut headers);
        assert!(headers.get(RESTRICTED_HEADER).is_none());
        Filter {
            restricted: true,
            ..Filter::default()
        }
        .mark_restricted(&mut headers);
        assert_eq!(
            headers.get(RESTRICTED_HEADER).and_then(|v| v.to_str().ok()),
            Some("true")
        );
    }

    #[test]
    fn only_a_pick_or_an_omit_hides_a_term() {
        let speed = iri("speed");
        assert!(!Filter::default().hides("speed", &speed));
        let pick = Filter {
            pick: vec!["status".into()],
            ..Filter::default()
        };
        assert!(pick.hides("speed", &speed));
        assert!(!pick.hides("status", &iri("status")));
        let omit = Filter {
            omit: vec!["speed".into()],
            ..Filter::default()
        };
        assert!(omit.hides("speed", &speed));
    }

    /// Refuses upserts, allows the rest; `short` answers one decision
    /// fewer than it was asked for.
    struct Batch {
        short: bool,
    }

    impl PolicyEngine for Batch {
        fn name(&self) -> &str {
            "batch"
        }
        fn decide<'a>(&'a self, _s: &'a Subject, o: &'a Operation<'a>) -> DecisionFuture<'a> {
            Box::pin(std::future::ready(if o.clause == "5.6.8" {
                Decision::Deny("no upserts".into())
            } else {
                Decision::Allow
            }))
        }
        fn decide_each<'a>(
            &'a self,
            s: &'a Subject,
            ops: &'a [Operation<'a>],
        ) -> DecisionsFuture<'a> {
            Box::pin(async move {
                let mut out = Vec::new();
                for op in ops {
                    out.push(self.decide(s, op).await);
                }
                if self.short {
                    out.pop();
                }
                out
            })
        }
        fn pre_notify(&self, _s: &Subject, _sub: &Value, _n: &mut Value) -> NotifyDecision {
            NotifyDecision::Deliver
        }
    }

    #[tokio::test]
    async fn a_batch_is_answered_member_by_member_and_a_short_answer_denies_all() {
        let ops = [Operation::new("5.6.7"), Operation::new("5.6.8")];
        let each = decide_each(&Batch { short: false }, &subject(), &ops).await;
        assert_eq!(
            each,
            vec![Decision::Allow, Decision::Deny("no upserts".into())]
        );
        let short = decide_each(&Batch { short: true }, &subject(), &ops).await;
        assert_eq!(short, vec![Decision::Deny(ENGINE_FAILED.to_owned()); 2]);
        let gated = gate_each(
            &Batch { short: false },
            &antares_model::TenantId::default(),
            &HeaderMap::new(),
            &ops,
        )
        .await;
        assert_eq!(gated, vec![None, Some("no upserts".to_owned())]);
    }

    #[tokio::test]
    async fn the_pre_image_joins_stored_and_written_types_once_each() {
        let tenant = antares_model::TenantId::default();
        let id = "urn:ngsi-ld:Car:pre";
        let stored = json!({"id": id, "type": [iri("Car")],
                            (iri("speed")): [{"type": "Property", "value": 1}]});
        let body = json!({"type": [iri("Car"), iri("Vehicle")]});
        let bare = crate::state::AppState::new("pre-image".into());
        bare.store
            .create(&tenant, antares_store::Kind::Entity, id, stored)
            .await
            .expect("create");
        let none = pre_image(&bare, &tenant, id, Some(&body)).await;
        assert!(
            none.types.is_empty() && none.stored.is_none(),
            "no engine, no read"
        );
        let st = bare.with_policy(std::sync::Arc::new(AllowAll));
        let image = pre_image(&st, &tenant, id, Some(&body)).await;
        assert_eq!(image.types, vec![iri("Car"), iri("Vehicle")]);
        assert_eq!(
            image.stored.map(|s| s.attribute_names),
            Some(vec![iri("speed")])
        );
    }
}
