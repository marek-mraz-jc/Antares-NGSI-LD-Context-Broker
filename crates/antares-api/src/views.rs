// SPDX-License-Identifier: EUPL-1.2
//! The composition hook of endpoint views (ADR-0023, ADR-0027 section 5,
//! T-3494): a request served through a view runs the ordinary handler with
//! one more conjunct, the view's narrowing, composed with whatever the
//! installed engine answers (`effective = view ∧ engine`, both narrowing,
//! neither widening).
//!
//! The view travels on the [`AppState`](crate::AppState)
//! ([`AppState::with_view`](crate::AppState::with_view)), not on the task:
//! everything a request spawns, every in-process call and every join walk
//! clones that state, so no path can lose the view and serve wider than it.
//! `gate!` reads it after the engine has answered and before [`resolve`], so
//! a clause that cannot be narrowed (a write, a purge) is refused under a
//! view exactly as it is under a narrowing engine.
//!
//! Loading, validating and addressing views is `antares-ops`' (T-3496); this
//! module composes one, and holds the table of the views a pod serves
//! ([`ViewTable`]) with the store seam the loader writes through, so the
//! operation layer never names the store (threat F1).

use crate::policy::{resolve, Decision, Denied, Filter, Rule, Warning};
use antares_ql::geo::GeoQuery;
use antares_ql::QNode;
use std::collections::BTreeMap;
use std::sync::Arc;

/// How many rules a composed narrowing may hold: the pairs of a view's
/// rules and an engine's are the product of two bounded lists, and the
/// product is bounded again so neither side can make the other's allocation
/// grow without limit.
pub const MAX_COMPOSED_RULES: usize = 4096;

/// A view as `gate!` applies it: the predicate `selector ∧ q ∧ scope_q ∧
/// geo_q` and the per-rule allowlists (ADR-0023 rule 11), compiled once.
/// Built only through [`CompiledView::new`], which refuses a view that
/// would serve everything.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledView {
    narrowing: Arc<Filter>,
    /// The view's id, recorded on what is made through it (a
    /// Subscription's `__view`, rule 9).
    id: String,
}

impl CompiledView {
    /// The view's narrowing. `selector` is its Entity types; every rule is
    /// kept to the types it shares with the selector, and a rule that shares
    /// none is dropped. A view with no selector, or with no rule left, is
    /// refused: a view that names nothing would otherwise read as a
    /// narrowing that narrows nothing.
    ///
    /// An Entity with several types is matched by a rule when one of its
    /// types is both in the selector and in the rule, which can show less
    /// than the two conditions read apart, never more.
    pub fn new(
        selector: &[String],
        q: Option<QNode>,
        scope_q: Option<String>,
        geo_q: Option<antares_ql::geo::GeoQuery>,
        rules: Vec<Rule>,
    ) -> Result<Self, String> {
        if selector.is_empty() {
            return Err("a view selects at least one Entity type".into());
        }
        let in_selector = |t: &String| {
            let iri = crate::repr::policy_context().expand_key(t);
            selector
                .iter()
                .any(|s| crate::repr::policy_context().expand_key(s) == iri)
        };
        let rules: Vec<Rule> = rules
            .into_iter()
            .map(|r| Rule {
                types: r.types.iter().filter(|t| in_selector(t)).cloned().collect(),
                ..r
            })
            .filter(|r| !r.types.is_empty())
            .collect();
        if rules.is_empty() {
            return Err(
                "a view shows its Entities through at least one rule of its selector's types"
                    .into(),
            );
        }
        Ok(Self {
            narrowing: Arc::new(Filter {
                q: q.map(Box::new),
                scope_q,
                geo_q: geo_q.map(Box::new),
                rules,
                ..Filter::default()
            }),
            id: String::new(),
        })
    }

    /// The same view under the id the table serves it as.
    #[must_use]
    pub fn named(mut self, id: &str) -> Self {
        id.clone_into(&mut self.id);
        self
    }

    /// The id the table serves the view as; empty for a view never named.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The narrowing this view adds to every operation served through it.
    pub fn narrowing(&self) -> &Filter {
        &self.narrowing
    }

    /// The Entity types and the Attributes the view shows, each compacted
    /// under the core @context, sorted, once each: a name the core context
    /// does not define stays the IRI, which expands to itself.
    pub fn shown_names(&self) -> (Vec<String>, Vec<String>) {
        let ctx = crate::repr::policy_context();
        let compact = |names: &mut dyn Iterator<Item = &String>| {
            names
                .map(|n| ctx.compact_iri(n))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        };
        let rules = &self.narrowing.rules;
        (
            compact(&mut rules.iter().flat_map(|r| &r.types)),
            compact(&mut rules.iter().flat_map(|r| &r.attributes)),
        )
    }
}

/// List @contexts (13.3, CIM 009 5.13.3).
const LIST_CONTEXTS: &str = "5.13.3";

/// What `gate!` hands the handler: the engine's answer for `clause`
/// composed with the view the state carries, if it carries one. Without a
/// view the answer is the engine's, unchanged.
pub fn with_view(
    view: Option<&CompiledView>,
    clause: &str,
    engine: Result<Arc<Filter>, Denied>,
) -> Result<Arc<Filter>, Denied> {
    let Some(view) = view else {
        return engine;
    };
    if clause == LIST_CONTEXTS {
        // Rule 5: the handler lists the core @context alone under a view,
        // so the view has nothing of its own to narrow there.
        return engine;
    }
    // A temporal write by id has no write guard: it falls through and is
    // refused under a view as a narrowed write is.
    if crate::policy::WRITE_BY_ID.contains(&clause) && CHECKED_WRITES.contains(&clause) {
        // Rule 7 as below, and the engine's answer is the condition its
        // write holds the stored Entity to (`gate`), which the handler
        // checks beside the view's own.
        return engine;
    }
    if CHECKED_WRITES.contains(&clause) || SUBSCRIPTIONS.contains(&clause) {
        // Rule 7: the handler checks the view on the stored Entity before
        // and after the write (`WriteGuard`); the engine's own answer
        // still refuses a narrowed write, as it does without a view.
        return match resolve(clause, Decision::Filter(Arc::unwrap_or_clone(engine?))) {
            Decision::Deny(why) => Err(Denied(why)),
            // 5.8.4 lists the view's own and keeps the engine's selection
            // among them (T-3673)
            Decision::Filter(f) if crate::policy::RECORD_LISTS.contains(&clause) => Ok(Arc::new(f)),
            _ => Ok(crate::policy::unnarrowed()),
        };
    }
    if crate::policy::RECORD_LISTS.contains(&clause) && !view.narrowing().is_empty() {
        // The view narrows Entities; read over a Registration or a
        // Context Source Subscription it would select records by what an
        // Entity carries. Refused, as before those lists took a narrowing.
        return Err(Denied(format!(
            "{clause} lists records an Entity view cannot narrow"
        )));
    }
    // An engine that narrows nothing (allow-all) leaves the view's own
    // narrowing: `compose(v, Filter::default())` is `v`, so the pairing is
    // skipped on the path every request of an allow-all broker takes
    // (T-3697, ADR-0027 section 6).
    // A query keeps a filter as it is (`resolve`), so there the view's
    // compiled narrowing itself is the answer, shared and not copied.
    let engine = engine?;
    let narrows_nothing = *engine == Filter::default();
    if narrows_nothing && crate::policy::FILTERABLE.contains(&clause) {
        return Ok(view.narrowing.clone());
    }
    let composed = if narrows_nothing {
        Filter::clone(view.narrowing())
    } else {
        compose(view.narrowing(), Arc::unwrap_or_clone(engine))?
    };
    match resolve(clause, Decision::Filter(composed)) {
        Decision::Allow => Ok(crate::policy::unnarrowed()),
        // `resolve` makes no condition of a filter
        Decision::Filter(f) | Decision::AllowIf(f) => Ok(Arc::new(f)),
        Decision::Deny(why) => Err(Denied(why)),
    }
}

/// `a ∧ b` as one [`Filter`]: every member narrows and none widens. The
/// queries are conjoined on the AST, the `scopeQ`s intersected, the
/// projections intersected (`pick`) and joined (`omit`), and the rules
/// paired: an Entity a pair holds for matches a rule of each side, and shows
/// what both show.
pub fn compose(a: &Filter, b: Filter) -> Result<Filter, Denied> {
    let (geo_q, geo_also) = and_geo(a.areas(), b.areas());
    let rules = pair_rules(&a.rules, b.rules)?;
    let mut omit = b.omit;
    for name in &a.omit {
        if !omit.contains(name) {
            omit.push(name.clone());
        }
    }
    let mut omit_by_type = b.omit_by_type;
    for (t, attrs) in &a.omit_by_type {
        let into = omit_by_type.entry(t.clone()).or_default();
        for name in attrs {
            if !into.contains(name) {
                into.push(name.clone());
            }
        }
    }
    Ok(Filter {
        q: and_q(a.q.as_deref(), b.q.map(|q| *q)).map(Box::new),
        scope_q: and_scope(a.scope_q.as_deref(), b.scope_q)?,
        geo_q,
        geo_also,
        pick: and_pick(&a.pick, b.pick),
        omit,
        rules,
        omit_by_type,
        restricted: a.restricted || b.restricted,
        warning: Warning::join(a.warning.as_ref(), b.warning),
        temporal: [&a.temporal[..], &b.temporal[..]].concat().into(),
    })
}

fn and_q(a: Option<&QNode>, b: Option<QNode>) -> Option<QNode> {
    match (a, b) {
        (Some(a), Some(b)) => Some(QNode::And(vec![a.clone(), b])),
        (Some(a), None) => Some(a.clone()),
        (None, b) => b,
    }
}

fn and_scope(a: Option<&str>, b: Option<String>) -> Result<Option<String>, Denied> {
    match (a, b) {
        (Some(a), Some(b)) => antares_ql::scope::intersect_scope_q(a, &b)
            .map(Some)
            .ok_or_else(|| Denied(crate::policy::SCOPE_NOT_NARROWABLE.to_owned())),
        (Some(a), None) => Ok(Some(a.to_owned())),
        (None, b) => Ok(b),
    }
}

/// Every area of both sides, the first in `geo_q` and the rest in
/// `geo_also`: each must hold, so none is dropped (dropping one widens).
fn and_geo<'g>(
    a: impl Iterator<Item = &'g GeoQuery>,
    b: impl Iterator<Item = &'g GeoQuery>,
) -> (Option<Box<GeoQuery>>, Vec<GeoQuery>) {
    let mut all = a.chain(b).cloned();
    (all.next().map(Box::new), all.collect())
}

/// Names compared as expanded IRIs, kept in `b`'s written form.
fn shared(a: &[String], b: Vec<String>) -> Vec<String> {
    let ctx = crate::repr::policy_context();
    let a: Vec<String> = a.iter().map(|n| ctx.expand_key(n)).collect();
    b.into_iter()
        .filter(|n| a.contains(&ctx.expand_key(n)))
        .collect()
}

/// An empty `pick` keeps everything, so it is the other side's; two lists
/// keep what both name. Two lists that share nothing keep the frame alone,
/// which `pick` cannot say with an empty list: the one member it keeps is
/// one no Entity carries.
fn and_pick(a: &[String], b: Vec<String>) -> Vec<String> {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b,
        (false, true) => a.to_vec(),
        (false, false) => {
            let both = shared(a, b);
            if both.is_empty() {
                vec![NOTHING.to_owned()]
            } else {
                both
            }
        }
    }
}

/// A rule's `ids` and `id_patterns`.
type IdNarrowing = (Vec<String>, Vec<String>);

/// The id narrowing of two rules that must both hold: one side's own when
/// the other narrows no id; else the explicit ids of each side the other
/// side selects, `None` when none is left (the pair holds for nothing).
/// Two pattern lists have no conjunction a single list can say, so they
/// refuse the composition rather than widen it.
fn and_ids(x: &Rule, y: &Rule) -> Result<Option<IdNarrowing>, Denied> {
    let narrows = |r: &Rule| !r.ids.is_empty() || !r.id_patterns.is_empty();
    if !narrows(x) {
        return Ok(Some((y.ids.clone(), y.id_patterns.clone())));
    }
    if !narrows(y) {
        return Ok(Some((x.ids.clone(), x.id_patterns.clone())));
    }
    if !x.id_patterns.is_empty() && !y.id_patterns.is_empty() {
        return Err(Denied(
            "the view and the policy both narrow ids by pattern".into(),
        ));
    }
    let mut ids: Vec<String> = x
        .ids
        .iter()
        .filter(|i| y.selects_id(i))
        .chain(y.ids.iter().filter(|i| x.selects_id(i)))
        .cloned()
        .collect();
    ids.sort();
    ids.dedup();
    Ok((!ids.is_empty()).then_some((ids, Vec::new())))
}

/// A member name no Entity carries: an expanded IRI under a scheme of its
/// own, so no `@context` can map a term onto it.
const NOTHING: &str = "urn:antares:views:nothing";

fn pair_rules(a: &[Rule], b: Vec<Rule>) -> Result<Vec<Rule>, Denied> {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => return Ok(b),
        (false, true) => return Ok(a.to_vec()),
        (false, false) => {}
    }
    if a.len().saturating_mul(b.len()) > MAX_COMPOSED_RULES {
        return Err(Denied(format!(
            "the view and the policy compose to more than {MAX_COMPOSED_RULES} rules"
        )));
    }
    let mut out = Vec::new();
    for x in a {
        for y in &b {
            let types = shared(&x.types, y.types.clone());
            if types.is_empty() {
                continue;
            }
            let Some((ids, id_patterns)) = and_ids(x, y)? else {
                continue;
            };
            let (geo_q, geo_also) = and_geo(x.areas(), y.areas());
            out.push(Rule {
                types,
                q: and_q(x.q.as_deref(), y.q.as_deref().cloned()).map(Box::new),
                scope_q: and_scope(x.scope_q.as_deref(), y.scope_q.clone())?,
                geo_q,
                geo_also,
                ids,
                id_patterns,
                // a side that shows every Attribute leaves the other's list
                attributes: match (x.every_attribute, y.every_attribute) {
                    (true, _) => y.attributes.clone(),
                    (false, true) => x.attributes.clone(),
                    (false, false) => shared(&x.attributes, y.attributes.clone()),
                },
                every_attribute: x.every_attribute && y.every_attribute,
                temporal: [x.temporal.as_slice(), y.temporal.as_slice()].concat(),
            });
        }
    }
    // No pair shares a type: nothing is shown. An empty list would read as
    // "no rules", which shows everything; a rule naming no type holds for
    // nothing.
    if out.is_empty() {
        out.push(Rule::default());
    }
    Ok(out)
}

/// The reserved tenant the view documents live under (ADR-0012, ADR-0023
/// rule 10). No client may name it, and no NGSI-LD route reads the
/// document kind they are stored as.
pub const VIEW_TENANT: &str = "antares-views";

/// The reserved tenant the policy engine's rule documents live under
/// (ADR-0020 amendment), one document per Tenant, keyed by its name.
pub const POLICY_TENANT: &str = "antares-policy";

/// One view of the table.
#[derive(Debug, Clone)]
pub enum ViewEntry {
    /// A view that loaded: its narrowing, and what `/q/health` says of it.
    Loaded {
        /// The narrowing every operation through the view is composed with.
        view: Arc<CompiledView>,
        /// The one Tenant it serves.
        tenant: String,
        /// The operations it allows, as `operation-matrix.yaml` ids.
        operations: Vec<String>,
        /// What the loader found worth an operator's eye: a rule whose
        /// condition reads an Attribute it does not show (ADR-0023 rule
        /// 11), a temporal view keyed on a mutable member (rule 8).
        notes: Vec<String>,
    },
    /// A stored view that failed validation: never served, named in
    /// `/q/health` with the reason.
    Invalid {
        /// Why it did not load.
        reason: String,
    },
}

/// The views this pod serves, swapped whole on every reload so a request
/// sees one table or the next, never half of each (ADR-0023 rule 10). Empty
/// until a view loads: no view means nothing served (rule M4).
#[derive(Debug, Default)]
pub struct ViewTable {
    entries: std::sync::RwLock<Arc<BTreeMap<String, ViewEntry>>>,
    /// The digest of the view documents the entries were loaded from
    /// (threat V26): pods over one store report the same value.
    revision: std::sync::atomic::AtomicU64,
    admin: std::sync::OnceLock<axum::Router>,
    policy: std::sync::RwLock<serde_json::Value>,
    schemas: std::sync::RwLock<Arc<BTreeMap<String, Schema>>>,
}

/// The id prefix a Tenant's schema document is stored under, beside the
/// view documents in [`VIEW_TENANT`]. A view id is `[a-z0-9-]`, so no view
/// shares a key with a schema.
pub const SCHEMA_ID_PREFIX: &str = "schema:";

/// A Tenant's `LinkML` schema as written to `/q/schemas/{tenant}` (T-3628):
/// JSON the broker checked for size and well-formedness only. A surface
/// parses it and caches the parse per `revision`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    /// Changes whenever the bytes do: a digest of them, opaque.
    pub revision: u64,
    /// The document as written.
    pub bytes: Arc<[u8]>,
}

impl Schema {
    /// The schema of `bytes`, its revision their digest.
    #[must_use]
    pub fn new(bytes: &[u8]) -> Self {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        Self {
            revision: h.finish(),
            bytes: bytes.into(),
        }
    }
}

impl ViewTable {
    /// The table as it is now.
    pub fn snapshot(&self) -> Arc<BTreeMap<String, ViewEntry>> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replace the whole table, loaded from the documents whose digest is
    /// `revision`.
    pub fn replace(&self, entries: BTreeMap<String, ViewEntry>, revision: u64) {
        *self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(entries);
        self.revision
            .store(revision, std::sync::atomic::Ordering::Release);
    }

    /// The revision `/q/health` reports as `viewsRevision`: sixteen hex
    /// digits, the same on every pod that loaded the same documents.
    pub fn revision(&self) -> String {
        format!(
            "{:016x}",
            self.revision.load(std::sync::atomic::Ordering::Acquire)
        )
    }

    /// Replace every Tenant's schema.
    pub fn replace_schemas(&self, schemas: BTreeMap<String, Schema>) {
        *self
            .schemas
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(schemas);
    }

    /// The schema of `tenant`, if one is stored.
    pub fn schema(&self, tenant: &str) -> Option<Schema> {
        self.schemas
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(tenant)
            .cloned()
    }

    /// What `/q/health` lists of the schemas: each Tenant's size and
    /// revision, never the document.
    pub fn schema_report(&self) -> serde_json::Value {
        let schemas = self
            .schemas
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        serde_json::Value::Object(
            schemas
                .iter()
                .map(|(tenant, s)| {
                    let v = serde_json::json!({
                        "bytes": s.bytes.len(),
                        "revision": format!("{:016x}", s.revision),
                    });
                    (tenant.clone(), v)
                })
                .collect(),
        )
    }

    /// Whether at least one view loaded: a surface that serves through views
    /// is closed until one has (rule M4).
    pub fn any_loaded(&self) -> bool {
        self.snapshot()
            .values()
            .any(|e| matches!(e, ViewEntry::Loaded { .. }))
    }

    /// What `/q/health` lists: each view, `loaded` with its tenant,
    /// operations and notes, or `invalid` with the reason.
    pub fn report(&self) -> serde_json::Value {
        let views = self.snapshot();
        serde_json::Value::Object(
            views
                .iter()
                .map(|(id, e)| {
                    let v = match e {
                        ViewEntry::Loaded {
                            tenant,
                            operations,
                            notes,
                            ..
                        } => serde_json::json!({
                            "state": "loaded",
                            "tenant": tenant,
                            "operations": operations.len(),
                            "notes": notes,
                        }),
                        ViewEntry::Invalid { reason } => {
                            serde_json::json!({"state": "invalid", "reason": reason})
                        }
                    };
                    (id.clone(), v)
                })
                .collect(),
        )
    }

    /// What `/q/health` lists of the policy rule documents (ADR-0020
    /// amendment): set by the loader on every reload, `null` until it runs
    /// with an engine that reads documents. Tenant and revision only.
    pub fn policy_report(&self) -> serde_json::Value {
        self.policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replace what [`policy_report`](Self::policy_report) answers.
    pub fn set_policy_report(&self, report: serde_json::Value) {
        *self
            .policy
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = report;
    }

    /// The `/q/views` routes the loader serves, merged into the admin
    /// surface and so relative to `/q` (`/views`, `/views/{id}`). Set once,
    /// before the router is built.
    ///
    /// # Errors
    /// When routes were set already.
    pub fn set_admin(&self, routes: axum::Router) -> Result<(), String> {
        self.admin
            .set(routes)
            .map_err(|_| "the /q/views routes are set already".to_owned())
    }

    /// The `/q/views` routes, if a loader set them.
    pub fn admin(&self) -> Option<axum::Router> {
        self.admin.get().cloned()
    }
}

/// The writes whose handlers check ADR-0023 rule 7 on the stored Entity
/// (the crate's write guard): Create Entity, Update and Append Attributes, Partial
/// Attribute Update, Delete Attribute, Delete Entity, Merge, Replace Entity,
/// Replace Attribute and Set Attribute Value, and the batch create, upsert,
/// update, merge and delete, member by member. Every other write is refused
/// under a view, as a narrowed write is ([`resolve`]).
pub const CHECKED_WRITES: [&str; 14] = [
    "5.6.1", "5.6.2", "5.6.3", "5.6.4", "5.6.5", "5.6.6", "5.6.17", "5.6.18", "5.6.19", "5.6.7",
    "5.6.8", "5.6.9", "5.6.20", "5.6.10",
];

/// The Subscription operations a view serves (rule 9, 5.8.1 to 5.8.5): run
/// as the root runs them, on the view's own Subscriptions alone, each
/// stored with the view's id in [`VIEW_MEMBER`].
pub const SUBSCRIPTIONS: [&str; 5] = ["5.8.1", "5.8.2", "5.8.3", "5.8.4", "5.8.5"];

/// The member a Subscription made through a view records the view's id in,
/// beside the subject (rule 9). Under the `__` prefix the broker reserves
/// (ADR-0020): never set by a client, never served.
pub const VIEW_MEMBER: &str = "__view";

/// Whether the stored Subscription `sub` is the view's own: made through
/// `view` when there is one, at the root when there is none.
pub(crate) fn owns(view: Option<&CompiledView>, sub: &serde_json::Value) -> bool {
    sub.get(VIEW_MEMBER).and_then(serde_json::Value::as_str) == view.map(CompiledView::id)
}

/// The view a stored Subscription was made through, as the table serves it
/// to `tenant` now: `None` for one made at the root, `Some(None)` when its
/// view is no longer loaded for that Tenant (a removed view stops delivery).
pub(crate) fn of_subscription(
    st: &crate::state::AppState,
    tenant: &antares_model::TenantId,
    sub: &serde_json::Value,
) -> Option<Option<Arc<CompiledView>>> {
    let id = sub.get(VIEW_MEMBER)?.as_str().unwrap_or_default();
    Some(match st.views.snapshot().get(id) {
        Some(ViewEntry::Loaded {
            view, tenant: t, ..
        }) if t == tenant.as_str() => Some(view.clone()),
        _ => None,
    })
}

/// `watchedAttributes` (expanded) as a Subscription through `view` keeps
/// them: the ones some rule of the view shows (rule 9); `Err` when none is.
pub(crate) fn watched_in(
    view: &CompiledView,
    watched: &[serde_json::Value],
) -> Result<Vec<serde_json::Value>, String> {
    let shown: Vec<String> = crate::repr::type_rules(view.narrowing())
        .map(|f| f.rules.iter().flat_map(|r| r.attributes.clone()).collect())
        .unwrap_or_default();
    let kept: Vec<serde_json::Value> = watched
        .iter()
        .filter(|w| w.as_str().is_some_and(|w| shown.iter().any(|s| s == w)))
        .cloned()
        .collect();
    if kept.is_empty() {
        Err("watchedAttributes names no Attribute this view shows".into())
    } else {
        Ok(kept)
    }
}

/// Why a write through a view whose post-image the view would not select is
/// refused (rule 7). The caller saw the pre-image, so it learns nothing new.
pub const MOVES_OUT: &str = "the write would move the Entity out of this view";
/// Why a write naming an Attribute the view does not show is refused whole
/// (rule 7, never a silent drop).
pub const NOT_WRITABLE: &str = "the write names an Attribute this view does not let it write";
/// Why a Relationship write through a view is refused when its target is
/// not shown: hidden and absent answer alike (threat-model D7).
pub const TARGET_NOT_SHOWN: &str =
    "a Relationship written through this view must point at an Entity it shows";

/// ADR-0023 rule 7 for one write through a view: the stored Entity must be
/// in the view before the write (else it is not found, rule 3) and after it,
/// and every Attribute the write names must be one a rule holding for the
/// Entity shows, before and after. The checks run inside the store's own
/// read-modify-write, on the document it is about to commit, so no
/// concurrent write slips between check and commit. The names are judged on
/// the pre-image too, so a hidden Attribute answers the same 403 whether the
/// Entity holds it or not.
///
/// A refusal inside the store closure has only the closure's error type to
/// travel in, so the guard records it and aborts the commit, and the handler
/// turns the record into the 403 with [`WriteGuard::verdict`] right after
/// the store call.
#[derive(Debug)]
pub(crate) struct WriteGuard {
    /// The view's selection, as an engine's write condition is held.
    sel: crate::policy::Condition,
    /// The expanded names of the Attributes the write sets or removes.
    written: Vec<String>,
    refused: std::sync::OnceLock<&'static str>,
}

impl WriteGuard {
    /// The guard of the view `st` serves through for a write naming
    /// `written` (expanded Attribute names), or `None` without a view.
    /// `scope` is the Entity's own member and no Attribute (4.18): the
    /// allowlist does not govern it, the post-image check does.
    pub(crate) fn of<'a>(
        st: &crate::state::AppState,
        written: impl IntoIterator<Item = &'a str>,
    ) -> Option<Self> {
        let narrowing = st.view.as_deref()?.narrowing();
        Some(Self {
            sel: crate::policy::Condition::new(narrowing),
            written: written.into_iter().map(str::to_owned).collect(),
            refused: std::sync::OnceLock::new(),
        })
    }

    /// Whether the view selects the stored document `doc`. A linked path in
    /// the view's `q` never holds here, as in a rule (ADR-0023 rule 11): the
    /// check runs inside the store's write and reads no other Entity.
    pub(crate) fn sees(&self, doc: &serde_json::Value) -> bool {
        self.sel.holds(doc)
    }

    /// Why the view refuses `doc` with the written names on it, if it does.
    fn judge(&self, doc: &serde_json::Value) -> Option<&'static str> {
        self.judge_names(doc, self.written.iter().map(String::as_str).collect())
    }

    fn judge_names(&self, doc: &serde_json::Value, written: Vec<&str>) -> Option<&'static str> {
        if !self.sees(doc) {
            Some(MOVES_OUT)
        } else if !self
            .sel
            .rules
            .as_deref()
            .is_none_or(|r| r.admits_write(doc, &written, crate::repr::policy_context()))
        {
            Some(NOT_WRITABLE)
        } else {
            None
        }
    }

    fn refuse(&self, why: &'static str) -> antares_model::NgsiError {
        let _ = self.refused.set(why);
        antares_model::NgsiError::InternalError(why.into())
    }

    /// The pre-image check on the stored document: an Entity out of the
    /// view is not found, the answer an absent one gets; a name the view
    /// does not let the write set is refused (recorded).
    pub(crate) fn pre(
        &self,
        id: &str,
        doc: &serde_json::Value,
    ) -> Result<(), antares_model::NgsiError> {
        match self.judge(doc) {
            None => Ok(()),
            Some(MOVES_OUT) => Err(antares_model::NgsiError::ResourceNotFound(format!(
                "entity {id} not found"
            ))),
            Some(why) => Err(self.refuse(why)),
        }
    }

    /// [`Self::pre`] for a write that replaces the whole Entity: every
    /// Attribute the stored Entity holds counts as written, since a replace
    /// deletes the ones it omits (threat-model D6).
    pub(crate) fn pre_whole(
        &self,
        id: &str,
        doc: &serde_json::Value,
    ) -> Result<(), antares_model::NgsiError> {
        let held = doc
            .as_object()
            .into_iter()
            .flat_map(|o| o.keys())
            .map(String::as_str)
            .filter(|k| !antares_model::is_meta(k) && !k.starts_with("__"));
        let names = self
            .written
            .iter()
            .map(String::as_str)
            .chain(held)
            .collect();
        match self.judge_names(doc, names) {
            None => Ok(()),
            Some(MOVES_OUT) => Err(antares_model::NgsiError::ResourceNotFound(format!(
                "entity {id} not found"
            ))),
            Some(why) => Err(self.refuse(why)),
        }
    }

    /// The post-image check on `doc` as the write leaves it. A refusal is
    /// recorded for [`Self::verdict`] and aborts the commit.
    pub(crate) fn post(&self, doc: &serde_json::Value) -> Result<(), antares_model::NgsiError> {
        match self.judge(doc) {
            None => Ok(()),
            Some(why) => Err(self.refuse(why)),
        }
    }

    /// A document no store closure holds yet, as a create writes it.
    pub(crate) fn admits(&self, doc: &serde_json::Value) -> Result<(), Denied> {
        match self.judge(doc) {
            None => Ok(()),
            Some(why) => Err(Denied(why.to_owned())),
        }
    }

    /// The refusal [`Self::pre`] or [`Self::post`] recorded, as the 403 the
    /// handler answers.
    pub(crate) fn verdict(&self) -> Result<(), Denied> {
        match self.refused.get() {
            Some(why) => Err(Denied((*why).to_owned())),
            None => Ok(()),
        }
    }

    /// Every Relationship target an expanded fragment writes must be an
    /// Entity this view shows; an absent target answers as a hidden one.
    pub(crate) async fn targets(
        &self,
        st: &crate::state::AppState,
        tenant: &antares_model::TenantId,
        fragment: &serde_json::Value,
    ) -> Result<(), Denied> {
        for target in relationship_targets(fragment) {
            let doc = st
                .store
                .get(tenant, antares_store::Kind::Entity, target)
                .await
                .ok()
                .flatten();
            if !doc.is_some_and(|d| self.sees(&d)) {
                return Err(Denied(TARGET_NOT_SHOWN.to_owned()));
            }
        }
        Ok(())
    }
}

/// Rule 7 for an Entity a create writes, outside any store closure: in the
/// view, with Attributes it shows and Relationships to Entities it shows.
pub(crate) async fn admits_new(
    st: &crate::state::AppState,
    tenant: &antares_model::TenantId,
    doc: &serde_json::Value,
) -> Result<(), Denied> {
    let Some(g) = WriteGuard::of(
        st,
        crate::attrs::attr_iris_of(doc).iter().map(String::as_str),
    ) else {
        return Ok(());
    };
    g.admits(doc)?;
    g.targets(st, tenant, doc).await
}

/// The ids every Relationship and ListRelationship instance of an expanded
/// Entity or fragment points at.
fn relationship_targets(doc: &serde_json::Value) -> Vec<&str> {
    let instances = doc
        .as_object()
        .into_iter()
        .flat_map(|o| o.values())
        .filter_map(serde_json::Value::as_array)
        .flatten();
    let mut out = Vec::new();
    for inst in instances {
        for member in ["object", "objectList"] {
            match inst.get(member) {
                Some(serde_json::Value::String(id)) => out.push(id.as_str()),
                Some(serde_json::Value::Array(a)) => out.extend(a.iter().filter_map(|e| {
                    e.as_str()
                        .or_else(|| e.get("object").and_then(serde_json::Value::as_str))
                })),
                _ => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{
        DecisionFuture, NotifyDecision, Operation, PolicyEngine, Subject, FILTERABLE,
    };
    use crate::AppState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tower::ServiceExt;

    struct Narrowing(Filter);

    impl PolicyEngine for Narrowing {
        fn name(&self) -> &str {
            "narrowing"
        }
        fn decide<'a>(&'a self, _s: &'a Subject, op: &'a Operation<'a>) -> DecisionFuture<'a> {
            let answer = if FILTERABLE.contains(&op.clause) {
                Decision::Filter(self.0.clone())
            } else {
                Decision::Allow
            };
            Box::pin(std::future::ready(answer))
        }
        fn pre_notify(&self, _s: &Subject, _sub: &Value, _n: &mut Value) -> NotifyDecision {
            NotifyDecision::Deliver
        }
    }

    fn rule(types: &[&str], q: Option<&str>, attributes: &[&str]) -> Rule {
        Rule {
            types: types.iter().map(|t| (*t).to_owned()).collect(),
            q: q.map(|q| Box::new(antares_ql::parse_q(q).expect("q"))),
            attributes: attributes.iter().map(|a| (*a).to_owned()).collect(),
            ..Rule::default()
        }
    }

    #[test]
    fn the_context_list_is_the_one_read_a_view_does_not_narrow() {
        let view = cars();
        assert_eq!(
            with_view(Some(&view), LIST_CONTEXTS, Ok(Arc::new(Filter::default()))),
            Ok(Arc::new(Filter::default()))
        );
        assert!(
            with_view(Some(&view), "5.13.4", Ok(Arc::new(Filter::default()))).is_err(),
            "serving a stored @context is refused under a view"
        );
    }

    /// T-3668: a temporal write by id has no write guard, so under a view
    /// it is refused, condition or not.
    #[test]
    fn a_temporal_write_by_id_is_refused_under_a_view() {
        let view = cars();
        let red = Filter {
            q: Some(Box::new(antares_ql::parse_q("colour==\"red\"").expect("q"))),
            ..Filter::default()
        };
        for clause in ["5.6.12", "5.6.13", "5.6.14", "5.6.15"] {
            assert!(crate::policy::WRITE_BY_ID.contains(&clause), "{clause}");
            assert!(with_view(Some(&view), clause, Ok(Arc::new(Filter::default()))).is_err());
            assert!(with_view(Some(&view), clause, Ok(Arc::new(red.clone()))).is_err());
        }
    }

    /// T-3673: Query Subscriptions keeps the engine's selection among the
    /// view's own; the Registration and CSR Subscription lists are not
    /// narrowed by an Entity view at all.
    #[test]
    fn a_record_list_under_a_view_keeps_the_engines_selection_or_is_refused() {
        let view = cars();
        let mine = Filter {
            q: Some(Box::new(
                antares_ql::parse_q("subject==\"alice\"").expect("q"),
            )),
            ..Filter::default()
        };
        assert_eq!(
            with_view(Some(&view), "5.8.4", Ok(Arc::new(mine.clone()))),
            Ok(Arc::new(mine.clone()))
        );
        assert_eq!(
            with_view(Some(&view), "5.8.4", Ok(Arc::new(Filter::default()))),
            Ok(Arc::new(Filter::default()))
        );
        // a view that only selects, so nothing projects and only the rule
        // above refuses
        let fast = CompiledView::new(
            &["Car".into()],
            Some(antares_ql::parse_q("speed>50").expect("q")),
            None,
            None,
            vec![rule(&["Car"], None, &[])],
        )
        .expect("view");
        assert!(!fast.narrowing().projects());
        for v in [&view, &fast] {
            for clause in ["5.10.2", "5.11.5"] {
                assert!(with_view(Some(v), clause, Ok(Arc::new(Filter::default()))).is_err());
                assert!(with_view(Some(v), clause, Ok(Arc::new(mine.clone()))).is_err());
            }
        }
    }

    /// Cars only, showing `speed` and `colour`, never `secret`.
    fn cars() -> CompiledView {
        CompiledView::new(
            &["Car".into()],
            None,
            None,
            None,
            vec![rule(&["Car"], None, &["speed", "colour"])],
        )
        .expect("view")
    }

    async fn send(
        st: &AppState,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value, axum::http::HeaderMap) {
        let req = Request::builder().method(method).uri(uri);
        let req = match body {
            Some(v) => {
                let payload = v.to_string();
                req.header("Content-Type", "application/json")
                    .header("Content-Length", payload.len())
                    .body(Body::from(payload))
            }
            None => req.body(Body::empty()),
        }
        .expect("request");
        let resp = crate::router(st.clone())
            .oneshot(req)
            .await
            .expect("response");
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            headers,
        )
    }

    /// Five Cars (two with `secret`), two Bikes; seeded through a state
    /// without a view, which shares its store with every view state.
    async fn seeded(engine: Option<Filter>) -> AppState {
        let mut st = AppState::new("me".into());
        if let Some(f) = engine {
            st = st.with_policy(Arc::new(Narrowing(f)));
        }
        crate::wire(&mut st).await;
        for i in 0..5 {
            let mut car = json!({"id": format!("urn:ngsi-ld:Car:{i}"), "type": "Car",
                "speed": {"type": "Property", "value": i * 10},
                "colour": {"type": "Property", "value": if i % 2 == 0 { "red" } else { "blue" }}});
            if i < 2 {
                car["secret"] = json!({"type": "Property", "value": 7});
            }
            let (s, b, _) = send(&st, "POST", "/ngsi-ld/v1/entities", Some(car)).await;
            assert_eq!(s, StatusCode::CREATED, "{s} {b}");
        }
        for i in 0..2 {
            let bike = json!({"id": format!("urn:ngsi-ld:Bike:{i}"), "type": "Bike",
                "speed": {"type": "Property", "value": 5}});
            let (s, b, _) = send(&st, "POST", "/ngsi-ld/v1/entities", Some(bike)).await;
            assert_eq!(s, StatusCode::CREATED, "{s} {b}");
        }
        st
    }

    fn ids(body: &Value) -> Vec<String> {
        let mut v: Vec<String> = body
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| e["id"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// G8: a view alone sends no `NGSILD-Warning`; an engine's warning
    /// reaches every read it narrows, directly and through a view, once.
    #[tokio::test]
    async fn only_the_engine_warns_and_its_warning_survives_the_view() {
        let warnings = |h: &axum::http::HeaderMap| -> Vec<String> {
            h.get_all("NGSILD-Warning")
                .iter()
                .filter_map(|v| v.to_str().ok().map(str::to_owned))
                .collect()
        };
        let reads = [
            "/ngsi-ld/v1/entities?type=Car",
            "/ngsi-ld/v1/entities/urn:ngsi-ld:Car:3",
            "/ngsi-ld/v1/types",
            "/ngsi-ld/v1/attributes",
        ];
        let st = seeded(None).await;
        let view = st.with_view(Arc::new(cars()));
        for uri in reads {
            let (s, body, headers) = send(&view, "GET", uri, None).await;
            assert_eq!(s, StatusCode::OK, "{uri}: {body}");
            assert!(warnings(&headers).is_empty(), "{uri}: a view alone");
        }

        let warned = Filter {
            warning: Some(Warning::new("types left out by the filter: Bike").expect("text")),
            ..Filter::default()
        };
        let st = seeded(Some(warned)).await;
        let view = st.with_view(Arc::new(cars()));
        for (who, st) in [("engine", &st), ("view and engine", &view)] {
            for uri in reads {
                let (s, body, headers) = send(st, "GET", uri, None).await;
                assert_eq!(s, StatusCode::OK, "{who} {uri}: {body}");
                assert_eq!(
                    warnings(&headers),
                    ["199 me \"types left out by the filter: Bike\""],
                    "{who} {uri}"
                );
            }
        }
    }

    /// Rule 1: `NGSILD-Results-Count` and the `Link` pages describe the
    /// narrowed set, the Cars, and nothing the view does not select.
    #[tokio::test]
    async fn count_and_pages_describe_the_view() {
        let st = seeded(None).await;
        let view = st.with_view(Arc::new(cars()));
        let uri = "/ngsi-ld/v1/entities?type=Car,Bike&count=true&limit=2";
        let (s, body, headers) = send(&view, "GET", uri, None).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(headers["NGSILD-Results-Count"], "5");
        assert_eq!(ids(&body).len(), 2);
        let mut seen = ids(&body);
        let next_of = |h: &axum::http::HeaderMap| {
            h.get_all("Link")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(", <"))
                .find(|l| l.contains("rel=\"next\""))
                .and_then(|l| l.trim_start_matches('<').split_once('>'))
                .map(|(target, _)| target)
                .map(str::to_owned)
        };
        let mut next = next_of(&headers);
        assert!(next.is_some(), "a first page of 2 out of 5 links the next");
        while let Some(path) = next {
            let (_, page, h) = send(&view, "GET", &path, None).await;
            seen.extend(ids(&page));
            next = next_of(&h);
        }
        seen.sort();
        assert_eq!(
            seen,
            (0..5)
                .map(|i| format!("urn:ngsi-ld:Car:{i}"))
                .collect::<Vec<_>>()
        );
    }

    /// Rule 2: `q` and `orderBy` over an Attribute the view hides answer as
    /// they do over Entities that never had it.
    #[tokio::test]
    async fn a_hidden_attribute_reads_as_absent() {
        let st = seeded(None).await;
        let view = st.with_view(Arc::new(cars()));
        for (q, expect) in [
            ("secret%3D%3D7", 0),
            ("secret", 0),
            ("!secret", 5),
            ("secret%3E1", 0),
        ] {
            let (s, body, _) = send(
                &view,
                "GET",
                &format!("/ngsi-ld/v1/entities?type=Car&q={q}"),
                None,
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{q}: {body}");
            assert_eq!(ids(&body).len(), expect, "{q}: {body}");
        }
        let (_, body, _) = send(
            &view,
            "GET",
            "/ngsi-ld/v1/entities?type=Car&orderBy=secret,speed",
            None,
        )
        .await;
        let (_, plain, _) = send(
            &view,
            "GET",
            "/ngsi-ld/v1/entities?type=Car&orderBy=speed",
            None,
        )
        .await;
        let order = |b: &Value| {
            b.as_array()
                .expect("list")
                .iter()
                .map(|e| e["id"].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&body), order(&plain), "secret does not order");
        assert!(
            body.as_array()
                .expect("list")
                .iter()
                .all(|e| e.get("secret").is_none()),
            "{body}"
        );
    }

    /// The view applies with no engine and with `AllowAll`: the default
    /// engine behind a view serves only the view.
    #[tokio::test]
    async fn allow_all_behind_a_view_serves_only_the_view() {
        let st = seeded(None)
            .await
            .with_policy(Arc::new(crate::policy::AllowAll));
        let view = st.with_view(Arc::new(cars()));
        let (_, body, _) = send(&view, "GET", "/ngsi-ld/v1/entities?type=Bike", None).await;
        assert_eq!(ids(&body), Vec::<String>::new());
        let (s, _, _) = send(
            &view,
            "GET",
            "/ngsi-ld/v1/entities/urn:ngsi-ld:Bike:0",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "out of view reads as not found");
        let (s, body, _) = send(&view, "GET", "/ngsi-ld/v1/entities/urn:ngsi-ld:Car:0", None).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert!(body.get("secret").is_none(), "{body}");
        // without the view, the same state serves everything
        let (_, body, _) = send(&st, "GET", "/ngsi-ld/v1/entities?type=Bike", None).await;
        assert_eq!(ids(&body).len(), 2);
    }

    /// An engine `Filter` and a view both narrow, neither widens: the engine
    /// keeps the slow Cars, the view keeps the Cars, the answer is both.
    #[tokio::test]
    async fn the_engine_and_the_view_both_narrow() {
        let slow = Filter {
            q: Some(Box::new(antares_ql::parse_q("speed<30").expect("q"))),
            pick: vec!["speed".into(), "secret".into()],
            ..Filter::default()
        };
        let st = seeded(Some(slow)).await;
        let view = st.with_view(Arc::new(cars()));
        let (_, body, _) = send(&view, "GET", "/ngsi-ld/v1/entities?type=Car,Bike", None).await;
        assert_eq!(
            ids(&body),
            vec![
                "urn:ngsi-ld:Car:0",
                "urn:ngsi-ld:Car:1",
                "urn:ngsi-ld:Car:2"
            ]
        );
        for e in body.as_array().expect("list") {
            assert!(e.get("speed").is_some(), "both show speed: {e}");
            assert!(e.get("colour").is_none(), "the engine hides colour: {e}");
            assert!(e.get("secret").is_none(), "the view hides secret: {e}");
        }
    }

    /// Under a view, a write [`CHECKED_WRITES`] names runs and is judged on
    /// the stored Entity (rule 7), a batch member by member; any other write
    /// is a narrowed write and refused, and an engine's narrowing still
    /// refuses a checked one.
    #[tokio::test]
    async fn a_write_under_a_view_is_checked_or_refused() {
        let st = seeded(None).await;
        let view = st.with_view(Arc::new(cars()));
        let car = json!({"id": "urn:ngsi-ld:Car:9", "type": "Car"});
        let (s, body, _) = send(&view, "POST", "/ngsi-ld/v1/entities", Some(car.clone())).await;
        assert_eq!(s, StatusCode::CREATED, "{body}");
        let bike = json!({"id": "urn:ngsi-ld:Bike:9", "type": "Bike"});
        let (s, body, _) = send(&view, "POST", "/ngsi-ld/v1/entities", Some(bike)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
        assert!(body.to_string().contains(MOVES_OUT), "{body}");
        let batch = json!([{"id": "urn:ngsi-ld:Car:7", "type": "Car"}]);
        let (s, body, _) = send(
            &view,
            "POST",
            "/ngsi-ld/v1/entityOperations/upsert",
            Some(batch),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{body}");
        let batch = json!([
            {"id": "urn:ngsi-ld:Car:8", "type": "Car"},
            {"id": "urn:ngsi-ld:Bike:8", "type": "Bike"}
        ]);
        let (s, body, _) = send(
            &view,
            "POST",
            "/ngsi-ld/v1/entityOperations/create",
            Some(batch),
        )
        .await;
        assert_eq!(s, StatusCode::MULTI_STATUS, "{body}");
        assert_eq!(body["success"], json!(["urn:ngsi-ld:Car:8"]), "{body}");
        assert_eq!(body["errors"][0]["error"]["detail"], MOVES_OUT, "{body}");
        let ids = json!(["urn:ngsi-ld:Car:9", "urn:ngsi-ld:Bike:8"]);
        let (s, body, _) = send(
            &view,
            "POST",
            "/ngsi-ld/v1/entityOperations/delete",
            Some(ids),
        )
        .await;
        assert_eq!(s, StatusCode::MULTI_STATUS, "{body}");
        assert_eq!(body["success"], json!(["urn:ngsi-ld:Car:9"]), "{body}");
        let narrowed = Filter {
            pick: vec!["speed".into()],
            ..Filter::default()
        };
        assert!(with_view(Some(&cars()), "5.6.1", Ok(Arc::new(narrowed))).is_err());
        assert_eq!(
            with_view(Some(&cars()), "5.6.1", Ok(Arc::new(Filter::default()))),
            Ok(Arc::new(Filter::default()))
        );
    }

    #[test]
    fn a_view_that_names_nothing_is_refused() {
        assert!(
            CompiledView::new(&[], None, None, None, vec![rule(&["Car"], None, &["a"])]).is_err()
        );
        assert!(CompiledView::new(&["Car".into()], None, None, None, vec![]).is_err());
        assert!(
            CompiledView::new(
                &["Car".into()],
                None,
                None,
                None,
                vec![rule(&["Bike"], None, &["a"])]
            )
            .is_err(),
            "a rule outside the selector shows nothing"
        );
    }

    /// Gap G2: both rules' id narrowings hold in the pair, and a pair no id
    /// is left for holds for nothing.
    #[test]
    fn a_pair_keeps_both_id_narrowings() {
        let by = |ids: &[&str], patterns: &[&str]| Rule {
            ids: ids.iter().map(|i| (*i).to_owned()).collect(),
            id_patterns: patterns.iter().map(|p| (*p).to_owned()).collect(),
            ..rule(&["Car"], None, &["speed"])
        };
        let ids_of = |a: Rule, b: Rule| {
            let pair = pair_rules(&[a], vec![b]).expect("pairs");
            (pair[0].ids.clone(), pair[0].id_patterns.clone())
        };
        let (c1, c2, c10) = ("urn:Car:1", "urn:Car:2", "urn:Car:10");
        let s = |v: &[&str]| v.iter().map(|i| (*i).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            ids_of(by(&[], &[]), by(&[c1], &["urn:Car:.*"])),
            (s(&[c1]), s(&["urn:Car:.*"]))
        );
        assert_eq!(ids_of(by(&[c2], &[]), by(&[], &[])), (s(&[c2]), s(&[])));
        assert_eq!(
            ids_of(by(&[c1, c2], &[]), by(&[c2, c10], &[])),
            (s(&[c2]), s(&[]))
        );
        assert_eq!(
            ids_of(by(&[], &["urn:Car:1.*"]), by(&[c1, c2, c10], &[])),
            (s(&[c1, c10]), s(&[])),
            "explicit ids through the other side's pattern"
        );
        assert_eq!(
            ids_of(by(&[c2], &[]), by(&[c1], &[])),
            (s(&[]), s(&[])),
            "nothing left: the pair holds for nothing"
        );
        assert_eq!(
            pair_rules(&[by(&[c2], &[])], vec![by(&[c1], &[])]),
            Ok(vec![Rule::default()])
        );
        assert!(
            pair_rules(&[by(&[], &["a"])], vec![by(&[], &["b"])]).is_err(),
            "two pattern lists refuse rather than widen"
        );
    }

    /// T-3697: the allow-all shortcut widens nothing. For every shape a
    /// view's narrowing takes, composing it with an engine that narrows
    /// nothing is the narrowing itself.
    #[test]
    fn a_view_composed_with_an_engine_that_narrows_nothing_is_the_view() {
        let area = antares_ql::geo::GeoQuery::from_params(
            &[
                ("georel".to_owned(), "near;maxDistance==10".to_owned()),
                ("geometry".to_owned(), "Point".to_owned()),
                ("coordinates".to_owned(), "[1,2]".to_owned()),
            ]
            .into_iter()
            .collect(),
        )
        .expect("geo")
        .expect("an area");
        let views = [
            cars(),
            CompiledView::new(
                &["Car".into(), "Bike".into()],
                Some(antares_ql::parse_q("speed>1").expect("q")),
                Some("/city/#".into()),
                Some(area),
                vec![
                    rule(&["Car"], Some("colour==\"red\""), &["speed"]),
                    Rule {
                        ids: vec!["urn:ngsi-ld:Bike:1".into()],
                        ..rule(&["Bike"], None, &[])
                    },
                ],
            )
            .expect("view"),
        ];
        for v in &views {
            assert_eq!(
                compose(v.narrowing(), Filter::default()),
                Ok(v.narrowing().clone())
            );
            assert_eq!(
                with_view(Some(v), "5.7.2", Ok(Arc::new(Filter::default()))),
                Ok(Arc::new(v.narrowing().clone()))
            );
        }
    }

    #[test]
    fn a_side_over_every_attribute_leaves_the_other_sides_list() {
        let every = Rule {
            every_attribute: true,
            ..rule(&["Car"], None, &[])
        };
        let both = pair_rules(
            std::slice::from_ref(&every),
            vec![rule(&["Car"], None, &["speed"])],
        )
        .expect("pairs");
        assert_eq!(both[0].attributes, vec!["speed"]);
        assert!(!both[0].every_attribute);
        let both =
            pair_rules(&[rule(&["Car"], None, &["colour"])], vec![every.clone()]).expect("pairs");
        assert_eq!(both[0].attributes, vec!["colour"]);
        assert!(!both[0].every_attribute);
        let both = pair_rules(std::slice::from_ref(&every), vec![every.clone()]).expect("pairs");
        assert!(both[0].every_attribute && both[0].attributes.is_empty());
    }

    #[test]
    fn rules_pair_and_disjoint_types_show_nothing() {
        let view = vec![rule(&["Car"], Some("speed>1"), &["speed", "colour"])];
        let engine = vec![rule(
            &["Car", "Bike"],
            Some("colour==\"red\""),
            &["speed", "secret"],
        )];
        let both = pair_rules(&view, engine).expect("pairs");
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].types, vec!["Car"]);
        assert_eq!(both[0].attributes, vec!["speed"]);
        assert!(matches!(both[0].q.as_deref(), Some(QNode::And(v)) if v.len() == 2));

        let none = pair_rules(&view, vec![rule(&["Bike"], None, &["speed"])]).expect("pairs");
        assert_eq!(
            none,
            vec![Rule::default()],
            "a rule with no type holds for nothing"
        );
        let doc = json!({"id": "urn:ngsi-ld:Car:1", "type": "Car", "speed": {"type": "Property", "value": 9}});
        let f = Filter {
            rules: none,
            ..Filter::default()
        };
        assert!(f.shown(&doc, crate::repr::policy_context()).is_none());
    }

    #[test]
    fn neither_side_widens() {
        let a = Filter {
            pick: vec!["speed".into()],
            omit: vec!["x".into()],
            scope_q: Some("/a".into()),
            ..Filter::default()
        };
        let b = Filter {
            pick: vec!["colour".into()],
            omit: vec!["y".into()],
            scope_q: Some("/a/b".into()),
            restricted: true,
            ..Filter::default()
        };
        let f = compose(&a, b).expect("composes");
        assert_eq!(f.pick, vec![NOTHING], "disjoint picks keep the frame alone");
        assert_eq!(f.omit, vec!["y", "x"]);
        assert!(f.scope_q.is_some() && f.restricted);
        assert_eq!(
            compose(&Filter::default(), Filter::default()).expect("empty"),
            Filter::default()
        );
    }

    /// A `within` box from longitude `west` to `east`, 1 degree either side
    /// of the equator.
    fn band(west: f64, east: f64) -> antares_ql::geo::GeoQuery {
        let params = std::collections::HashMap::from([
            ("georel".to_owned(), "within".to_owned()),
            ("geometry".to_owned(), "Polygon".to_owned()),
            (
                "coordinates".to_owned(),
                json!([[
                    [west, -1.0],
                    [east, -1.0],
                    [east, 1.0],
                    [west, 1.0],
                    [west, -1.0]
                ]])
                .to_string(),
            ),
        ]);
        antares_ql::geo::GeoQuery::from_params(&params)
            .expect("parses")
            .expect("present")
    }

    /// A Car at longitude `lon` in the internal form `Rule::holds` reads.
    fn car_at(lon: f64) -> Value {
        let ctx = crate::repr::policy_context();
        json!({"id": "urn:ngsi-ld:Car:x", "type": [ctx.expand_key("Car")],
               (antares_ql::geo::LOCATION_IRI): [{"type": "GeoProperty",
                   "value": {"type": "Point", "coordinates": [lon, 0.0]}}]})
    }

    /// ADR-0023 rule 1: a view's area and an engine's area at the same level
    /// compose, and every one holds. Dropping either would widen.
    #[test]
    fn two_areas_compose_and_both_hold() {
        let ctx = crate::repr::policy_context();
        let car = ctx.expand_key("Car");
        let a = Filter {
            geo_q: Some(Box::new(band(0.0, 2.0))),
            rules: vec![Rule {
                geo_q: Some(Box::new(band(0.0, 2.0))),
                ..rule(&[&car], None, &["speed"])
            }],
            ..Filter::default()
        };
        let b = Filter {
            geo_q: Some(Box::new(band(1.0, 3.0))),
            rules: vec![Rule {
                geo_q: Some(Box::new(band(1.0, 3.0))),
                ..rule(&[&car], None, &["speed"])
            }],
            ..Filter::default()
        };
        let both = compose(&a, b).expect("two areas compose");
        assert_eq!(both.areas().count(), 2, "{both:?}");
        assert_eq!(both.rules.len(), 1);
        let pair = &both.rules[0];
        assert_eq!(pair.areas().count(), 2);
        assert!(pair.holds(&car_at(1.5), ctx), "inside both");
        assert!(!pair.holds(&car_at(0.5), ctx), "inside the view's only");
        assert!(!pair.holds(&car_at(2.5), ctx), "inside the engine's only");
        // three levels deep: a composed narrowing composes again
        let again = compose(
            &both,
            Filter {
                geo_q: Some(Box::new(band(1.4, 1.6))),
                ..Filter::default()
            },
        )
        .expect("three areas compose");
        assert_eq!(again.areas().count(), 3);
    }

    #[test]
    fn too_many_rules_are_refused() {
        let many: Vec<Rule> = (0..65).map(|_| rule(&["Car"], None, &["a"])).collect();
        assert!(pair_rules(&many, many.clone()).is_err());
    }

    /// A view's area and an engine's area both narrow a query, its count
    /// and its pages; the request's own area still applies on top.
    #[tokio::test]
    async fn a_view_area_and_an_engine_area_both_narrow_the_query() {
        let mut st = AppState::new("me".into()).with_policy(Arc::new(Narrowing(Filter {
            geo_q: Some(Box::new(band(-0.5, 1.5))),
            ..Filter::default()
        })));
        crate::wire(&mut st).await;
        for lon in 0..4 {
            let car = json!({"id": format!("urn:ngsi-ld:Car:at{lon}"), "type": "Car",
                "speed": {"type": "Property", "value": lon},
                "location": {"type": "GeoProperty",
                             "value": {"type": "Point", "coordinates": [lon, 0]}}});
            let (s, b, _) = send(&st, "POST", "/ngsi-ld/v1/entities", Some(car)).await;
            assert_eq!(s, StatusCode::CREATED, "{s} {b}");
        }
        let view = CompiledView::new(
            &["Car".into()],
            None,
            None,
            Some(band(0.5, 3.5)),
            vec![rule(&["Car"], None, &["speed", "location"])],
        )
        .expect("view");
        let view = st.with_view(Arc::new(view));
        let (s, body, headers) = send(
            &view,
            "GET",
            "/ngsi-ld/v1/entities?type=Car&count=true&limit=1",
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(ids(&body), vec!["urn:ngsi-ld:Car:at1"], "only inside both");
        assert_eq!(
            headers
                .get("NGSILD-Results-Count")
                .and_then(|v| v.to_str().ok()),
            Some("1")
        );
        let (_, body, _) = send(
            &view,
            "GET",
            "/ngsi-ld/v1/entities?type=Car&limit=1&offset=1",
            None,
        )
        .await;
        assert_eq!(ids(&body), Vec::<String>::new(), "no second page");
        let (_, body, _) = send(
            &view,
            "GET",
            "/ngsi-ld/v1/entities?type=Car&georel=within&geometry=Polygon&coordinates=%5B%5B%5B1.5%2C-1%5D%2C%5B3.5%2C-1%5D%2C%5B3.5%2C1%5D%2C%5B1.5%2C1%5D%2C%5B1.5%2C-1%5D%5D%5D",
            None,
        )
        .await;
        assert_eq!(
            ids(&body),
            Vec::<String>::new(),
            "the request's area on top"
        );
    }

    #[test]
    fn without_a_view_the_engines_answer_is_unchanged() {
        let f = Filter {
            pick: vec!["a".into()],
            ..Filter::default()
        };
        assert_eq!(
            with_view(None, "5.6.1", Ok(Arc::new(f.clone()))),
            Ok(Arc::new(f))
        );
        assert_eq!(
            with_view(Some(&cars()), "5.7.2", Err(Denied("no".into()))),
            Err(Denied("no".into()))
        );
    }
}
