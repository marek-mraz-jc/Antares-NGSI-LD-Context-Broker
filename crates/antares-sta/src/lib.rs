// SPDX-License-Identifier: EUPL-1.2
//! OGC SensorThings API v1.1, Sensing profile, read only, over one view:
//! `/x/views/{view}/sta/v1.1` (T-4462, `docs/analysis/sta-mapping.md`).
//!
//! Every set is read through the view's own operations: Query Entities naming the types the
//! view shows, Retrieve and Query Temporal Evolution for the series. A type or an Attribute the
//! view hides is absent from every set, count and id, and a hidden id answers the 404 an
//! unknown one does.
//!
//! ```
//! use antares_ops::Surface;
//! let sta = antares_sta::Sta;
//! assert_eq!(sta.name(), "sta");
//! assert_eq!(sta.prefix(), "/x/views");
//! ```

#![forbid(unsafe_code)]

pub mod mapping;
pub mod query;

use std::collections::BTreeSet;
use std::fmt::Write as _;

use antares_ops::percent::encode;
use antares_ops::{
    Caller, OperationId, Ops, OpsRequest, Page, Pushdown, Refusal, Scanned, Surface, ViewOps,
};
use axum::extract::{Query, Request};
use axum::http::header::CONTENT_TYPE;
use axum::http::request::Parts;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::any;
use axum::Router;
use mapping::{local, self_link, urn_of};
use query::{ById, Options, Step};
use serde_json::{json, Value};

/// Where the broker mounts the surface; the view and `sta/v1.1` follow.
pub const PREFIX: &str = "/x/views";

/// The entity sets of the Sensing profile, in the order the service document lists them.
pub const SETS: [&str; 8] = [
    "Things",
    "Locations",
    "HistoricalLocations",
    "Datastreams",
    "Sensors",
    "Observations",
    "ObservedProperties",
    "FeaturesOfInterest",
];

/// The conformance classes this read surface meets.
const CONFORMANCE: [&str; 3] = [
    "http://www.opengis.net/spec/iot_sensing/1.1/req/datamodel",
    "http://www.opengis.net/spec/iot_sensing/1.1/req/resource-path",
    "http://www.opengis.net/spec/iot_sensing/1.1/req/request-data",
];

/// The start of every location history a HistoricalLocations page reads.
const EPOCH: &str = "1970-01-01T00:00:00Z";

/// The most Entities a `$skip` on a set of several resources per Entity walks to find its start.
const WALK: usize = 10 * query::MAX_TOP;

/// The SensorThings surface.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sta;

impl Surface for Sta {
    fn name(&self) -> &'static str {
        "sta"
    }

    fn prefix(&self) -> &'static str {
        PREFIX
    }

    fn version_info(&self) -> Value {
        json!({ "endpoint": format!("{PREFIX}/{{view}}/sta/v1.1"), "conformsTo": CONFORMANCE })
    }

    fn router(&self, ops: Ops) -> Router {
        let handler = any(move |caller: Caller, req: Request| {
            let ops = ops.clone();
            async move { serve(&ops, &caller, req).await }
        });
        Router::new()
            .route("/{view}/sta/v1.1", handler.clone())
            .route("/{view}/sta/v1.1/", handler.clone())
            .route("/{view}/sta/v1.1/{*rest}", handler)
    }
}

/// A refusal: its status and a message that never names what the view hides.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Problem(StatusCode, String);

impl Problem {
    /// The OData JSON error body SensorThings answers.
    fn reply(self) -> Response {
        let body = json!({ "error": { "code": self.0.as_str(), "message": self.1 } });
        (
            self.0,
            [(CONTENT_TYPE, "application/json")],
            body.to_string(),
        )
            .into_response()
    }
}

impl From<query::Bad> for Problem {
    fn from(bad: query::Bad) -> Self {
        Self(StatusCode::BAD_REQUEST, format!("{}: {}", bad.0, bad.1))
    }
}

/// The one answer for an unknown and a hidden resource.
fn not_found() -> Problem {
    Problem(StatusCode::NOT_FOUND, "no such resource".into())
}

/// The view's refusal, by its status.
fn refused(refusal: &Refusal) -> Problem {
    upstream(refusal.status())
}

/// A view answer that is no success.
fn upstream(status: StatusCode) -> Problem {
    match status {
        StatusCode::BAD_REQUEST => Problem(status, "the filter is not valid NGSI-LD".into()),
        _ => Problem(status, "the view refused the read".into()),
    }
}

type Done = Result<Value, Problem>;

async fn serve(ops: &Ops, caller: &Caller, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    let refused = || {
        Problem(
            StatusCode::METHOD_NOT_ALLOWED,
            "this surface is read only".into(),
        )
        .reply()
    };
    antares_ops::read_only(&parts.method, refused, async {
        match respond(ops, caller, &parts).await {
            Ok(doc) => (
                StatusCode::OK,
                [(CONTENT_TYPE, "application/json")],
                doc.to_string(),
            )
                .into_response(),
            Err(problem) => problem.reply(),
        }
    })
    .await
}

/// One request below `/{view}/sta/v1.1`: a view id is `[a-z0-9-]`, so its raw segment is its id.
async fn respond(ops: &Ops, caller: &Caller, req: &Parts) -> Done {
    let uri = &req.uri;
    let mut segments = uri.path().split('/').skip(1);
    let id = segments.next().unwrap_or_default();
    // `sta`, `v1.1`: the route matched them.
    let rest = segments.skip(2).collect::<Vec<_>>().join("/");
    let view = ops.view(id).ok_or_else(not_found)?;
    let rest = antares_ops::percent::decoded(&rest).ok_or_else(not_found)?;
    // A list of pairs takes every query string: a name without `=` is a pair with an empty value.
    let pairs = Query::<Vec<(String, String)>>::try_from_uri(uri)
        .map(|Query(p)| p)
        .unwrap_or_default();
    let cx = Cx {
        opts: query::options(&pairs)?,
        base: format!(
            "{}{PREFIX}/{id}/sta/v1.1",
            antares_ops::public_base(&req.headers)
        ),
        view: &view,
        caller,
        pairs,
    };
    match query::steps(&rest).as_slice() {
        [] => Ok(json!({
            "value": SETS.iter().map(|s| json!({ "name": s, "url": format!("{}/{s}", cx.base) })).collect::<Vec<_>>(),
            "serverSettings": { "conformance": CONFORMANCE },
        })),
        [Step { set, key: None }] => cx.set(set).await,
        [Step {
            set,
            key: Some(key),
        }] => cx.one(set, key).await?.ok_or_else(not_found),
        [Step {
            set: thing,
            key: Some(key),
        }, Step { set, key: None }]
            if thing == "Things" =>
        {
            cx.of_thing(key, set).await
        }
        [Step {
            set: stream,
            key: Some(key),
        }, Step { set, key: None }]
            if stream == "Datastreams" && set == "Observations" =>
        {
            cx.series(key).await
        }
        _ => Err(not_found()),
    }
}

/// One request's view, caller, base and options.
struct Cx<'a> {
    view: &'a ViewOps,
    caller: &'a Caller,
    base: String,
    opts: Options,
    pairs: Vec<(String, String)>,
}

impl Cx<'_> {
    /// What every Query Entities names: the types the view shows (NGSI-LD 5.7.2 takes no query
    /// without one), or for a view of every type the local store.
    fn selector(&self) -> String {
        if self.view.shows_every_name() {
            return "local=true".into();
        }
        let (types, _) = self.view.shown_names();
        let types: Vec<String> = types.iter().map(|t| encode(t)).collect();
        format!("type={}", types.join(","))
    }

    /// The name the view answers an Attribute under, from the local name an id carries.
    fn attribute(&self, name: &str) -> String {
        let (_, attrs) = self.view.shown_names();
        attrs
            .into_iter()
            .find(|a| local(a) == name)
            .unwrap_or_else(|| name.to_owned())
    }

    /// `$filter` of an entity set: its id comparisons and the `&q=` of the rest, empty without.
    fn filter(&self) -> Result<(ById, String), Problem> {
        let ids = match &self.opts.filter {
            Some(f) => query::by_id(f)?,
            None => ById::default(),
        };
        let q = ids
            .q
            .as_ref()
            .map_or_else(String::new, |q| format!("&q={}", encode(q)));
        Ok((ids, q))
    }

    /// One Query Entities through the view: the Entities, whether more remain past them, and
    /// the count when `page.whole` asked for it.
    async fn entities(
        &self,
        extra: &str,
        page: Page,
    ) -> Result<(Vec<Value>, bool, Option<usize>), Problem> {
        // `modifiedAt` is the time of a value no sensor observed (STA 1.1 phenomenonTime).
        let pushdown = Pushdown {
            query: format!("{}{extra}", self.selector()),
            sys_attrs: true,
            ..Pushdown::default()
        };
        match self.view.scan(self.caller, &pushdown, None, page).await {
            Ok(Scanned::Page {
                entities,
                next,
                count,
                ..
            }) => {
                let more = next.is_some()
                    || count.is_some_and(|n| page.start.saturating_add(entities.len()) < n);
                Ok((entities, more, count))
            }
            // A Tenant that holds nothing yet answers 404 (5.5.10): nothing to read.
            Ok(Scanned::Answer(a)) if a.status() == StatusCode::NOT_FOUND => {
                Ok((Vec::new(), false, Some(0)))
            }
            Ok(Scanned::Answer(a)) => Err(upstream(a.status())),
            Err(refusal) => Err(refused(&refusal)),
        }
    }

    /// The Entities of `ids` the view shows, in one Query Entities.
    async fn by_ids(&self, ids: &[&str]) -> Result<Vec<Value>, Problem> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let list: Vec<String> = ids.iter().map(|i| encode(i)).collect();
        let page = Page {
            start: 0,
            size: ids.len(),
            whole: false,
        };
        let extra = format!("&id={}", list.join(","));
        Ok(self.entities(&extra, page).await?.0)
    }

    /// One Entity the view shows, by URN.
    async fn entity(&self, urn: Option<&str>) -> Result<Option<Value>, Problem> {
        let Some(urn) = urn else {
            return Ok(None);
        };
        Ok(self.by_ids(&[urn]).await?.into_iter().next())
    }

    /// One GET through the view: its JSON; a 404 is none.
    async fn get(&self, op: OperationId, path: String) -> Result<Option<Value>, Problem> {
        let answer = self
            .view
            .run(op, self.caller, OpsRequest::new(Method::GET, path))
            .await
            .map_err(|r| refused(&r))?;
        match answer.status() {
            StatusCode::NOT_FOUND => return Ok(None),
            s if !s.is_success() => return Err(upstream(s)),
            _ => {}
        }
        let bytes = self
            .view
            .read_answer(answer.into_body())
            .await
            .map_err(|r| refused(&r))?;
        Ok(Some(serde_json::from_slice(&bytes).unwrap_or_default()))
    }

    /// The temporal evolution of one Entity's `attribute`, its last `last` instances.
    async fn history(
        &self,
        urn: &str,
        attribute: &str,
        last: usize,
        extra: &str,
    ) -> Result<Option<Value>, Problem> {
        let path = format!(
            "/ngsi-ld/v1/temporal/entities/{}?attrs={}&lastN={last}{extra}",
            encode(urn),
            encode(attribute)
        );
        self.get(OperationId::TemporalRetrieve, path).await
    }

    /// A page of an entity set: one Query Entities of `$top` Entities from `$skip`.
    async fn set(&self, set: &str) -> Done {
        if !SETS.contains(&set) {
            return Err(not_found());
        }
        let (ids, extra) = self.filter()?;
        let path = format!("{}/{set}", self.base);
        if let Some(key) = ids.is.first() {
            if ids.q.is_some() {
                return Err(Problem(
                    StatusCode::BAD_REQUEST,
                    "$filter: id eq takes no comparison beside id ne".into(),
                ));
            }
            let items = self.one(set, key).await?.into_iter().collect();
            return Ok(self.listed(&path, items, true, &ids));
        }
        if set == "HistoricalLocations" {
            return self.moves(&extra, &path, &ids).await;
        }
        if matches!(set, "Datastreams" | "Observations" | "ObservedProperties") {
            return self.resources(set, &extra, &path, &ids).await;
        }
        let page = Page {
            start: self.opts.skip,
            size: self.opts.top,
            whole: self.opts.count,
        };
        let (entities, more, count) = self.entities(&extra, page).await?;
        let groups = match set {
            "Sensors" => {
                let devices = self.referenced(&entities, "refDevice").await?;
                entities
                    .iter()
                    .map(|e| {
                        target(&devices, mapping::object(e, "refDevice"))
                            .and_then(|d| mapping::sensor(&self.base, d))
                            .into_iter()
                            .collect()
                    })
                    .collect()
            }
            "FeaturesOfInterest" => {
                let pois = self.referenced(&entities, "refPointOfInterest").await?;
                entities
                    .iter()
                    .map(|e| self.feature_of(e, &pois).into_iter().collect())
                    .collect()
            }
            _ => entities.iter().map(|e| self.items(set, e)).collect(),
        };
        // Only a Thing is one Entity, so only its count is the operation's, and only while no
        // id is left out of the page after the read.
        let count = count.filter(|_| set == "Things" && ids.not.is_empty());
        Ok(self.page(&path, groups, more, count, &ids))
    }

    /// A page of a set an Entity holds several resources of: `$top` resources from the start
    /// [`Cx::resume`] finds, `@iot.nextLink` resuming after the last one served.
    async fn resources(&self, set: &str, extra: &str, path: &str, ids: &ById) -> Done {
        let (first, skipped) = self.resume(set, extra).await?;
        let page = Page {
            start: first,
            size: self.opts.top,
            whole: false,
        };
        let (entities, more, _) = self.entities(extra, page).await?;
        // One group per resource, and where the next page resumes after each.
        let (mut groups, mut after) = (Vec::new(), Vec::new());
        for (n, e) in entities.iter().enumerate() {
            let at = first.saturating_add(n);
            let items = self.items(set, e);
            let held = items.len();
            let from = if n == 0 { skipped } else { 0 };
            for (i, item) in items.into_iter().enumerate().skip(from) {
                let done = i.saturating_add(1);
                after.push(if done == held {
                    (at.saturating_add(1), 0)
                } else {
                    (at, done)
                });
                groups.push(vec![item]);
            }
        }
        let read = groups.len();
        groups.truncate(self.opts.top);
        let (items, served) = query::fit(groups);
        let resume = if served < read {
            after.get(served.saturating_sub(1)).copied()
        } else {
            more.then_some((first.saturating_add(entities.len()), 0))
        };
        let next = resume.map(|(e, i)| self.link(path, "$skiptoken", &format!("{e}.{i}")));
        Ok(self.finish(items, next, None, (first, skipped) == (0, 0), ids))
    }

    /// Where a page of [`Cx::resources`] starts, as an Entity offset and the resources of that
    /// Entity already served: the `$skiptoken`, else `$skip` resources counted from the first
    /// Entity, a page of Entities at a time over at most [`WALK`] of them.
    async fn resume(&self, set: &str, extra: &str) -> Result<(usize, usize), Problem> {
        if let Some(at) = self.opts.resume {
            return Ok(at);
        }
        let (mut left, mut at) = (self.opts.skip, 0usize);
        let reads = WALK / query::MAX_TOP;
        for read in 0..=reads {
            if left == 0 {
                return Ok((at, 0));
            }
            if read == reads {
                break;
            }
            let page = Page {
                start: at,
                size: query::MAX_TOP,
                whole: false,
            };
            let (entities, more, _) = self.entities(extra, page).await?;
            for e in &entities {
                let held = self.items(set, e).len();
                if left < held {
                    return Ok((at, left));
                }
                left = left.saturating_sub(held);
                at = at.saturating_add(1);
            }
            if !more {
                return Ok((at, 0));
            }
        }
        Err(Problem(
            StatusCode::BAD_REQUEST,
            format!("$skip: deeper than {WALK} Entities into this set; follow @iot.nextLink"),
        ))
    }

    /// The resources one Entity holds of a set read from its current state.
    fn items(&self, set: &str, e: &Value) -> Vec<Value> {
        let base = self.base.as_str();
        match set {
            "Things" => {
                let expand: Vec<&str> = self.opts.expand.iter().map(String::as_str).collect();
                mapping::thing(base, e, &expand).into_iter().collect()
            }
            "Locations" => mapping::location(base, e).into_iter().collect(),
            "Datastreams" => {
                let expand: Vec<&str> = self.opts.expand.iter().map(String::as_str).collect();
                mapping::datastreams(base, e, &expand)
            }
            "Observations" => mapping::observations(base, e),
            _ => mapping::observed_properties(base, e),
        }
    }

    /// The Entities the page's `relationship`s point at, as the view shows them.
    async fn referenced(
        &self,
        entities: &[Value],
        relationship: &str,
    ) -> Result<Vec<Value>, Problem> {
        let mut ids: Vec<&str> = entities
            .iter()
            .filter_map(|e| mapping::object(e, relationship))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        self.by_ids(&ids).await
    }

    /// An Entity's FeatureOfInterest (rule 4): its `refPointOfInterest` target if the view shows
    /// it, else its own location when it points at none.
    fn feature_of(&self, e: &Value, pois: &[Value]) -> Option<Value> {
        if let Some(poi) = mapping::object(e, "refPointOfInterest") {
            return target(pois, Some(poi)).and_then(|t| mapping::feature(&self.base, t, poi));
        }
        let urn = e.get("id").and_then(Value::as_str).unwrap_or_default();
        mapping::feature(&self.base, e, &format!("{urn}/location"))
    }

    /// A page of HistoricalLocations: one Query Temporal Evolution of the `location` history of
    /// `$top` Entities from `$skip`.
    async fn moves(&self, extra: &str, path: &str, ids: &ById) -> Done {
        let (top, skip) = (self.opts.top, self.opts.skip);
        // ponytail: the last MAX_TOP moves of each Entity; a window per Entity when a client
        // pages a long history through this set rather than through its Thing.
        let query = format!(
            "/ngsi-ld/v1/temporal/entities?{}{extra}&attrs=location&timerel=after&timeAt={EPOCH}&lastN={}&limit={top}&offset={skip}",
            self.selector(),
            query::MAX_TOP,
        );
        let answer = self.get(OperationId::TemporalQuery, query).await?;
        let entities = answer
            .as_ref()
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // A full page may have more behind it; the temporal count counts more than the
        // Entities holding a location, so it is not read.
        let more = entities.len() == top;
        let groups = entities
            .iter()
            .map(|e| mapping::historical_locations(&self.base, e))
            .collect();
        Ok(self.page(path, groups, more, None, ids))
    }

    /// One resource by id: its Entity read through the view, the resource found among those it
    /// holds by the whole id.
    async fn one(&self, set: &str, key: &str) -> Result<Option<Value>, Problem> {
        let base = self.base.as_str();
        let found = match set {
            "Things" | "Locations" | "Datastreams" | "Observations" => self
                .entity(urn_of(key))
                .await?
                .and_then(|e| find(self.items(set, &e), key)),
            "ObservedProperties" => {
                let extra = format!("&attrs={}", encode(&antares_ops::core_name(key)));
                let page = Page {
                    start: 0,
                    size: 1,
                    whole: false,
                };
                let (entities, _, _) = self.entities(&extra, page).await?;
                entities
                    .first()
                    .and_then(|e| find(mapping::observed_properties(base, e), key))
            }
            "Sensors" => self
                .entity(Some(key))
                .await?
                .and_then(|d| mapping::sensor(base, &d)),
            "FeaturesOfInterest" => match key.strip_suffix("/location") {
                Some(urn) => self
                    .entity(Some(urn))
                    .await?
                    .and_then(|e| self.feature_of(&e, &[])),
                None => self
                    .entity(Some(key))
                    .await?
                    .and_then(|t| mapping::feature(base, &t, key)),
            },
            "HistoricalLocations" => match urn_of(key) {
                Some(urn) => self
                    .history(urn, "location", query::MAX_SKIP, "")
                    .await?
                    .and_then(|e| find(mapping::historical_locations(base, &e), key)),
                None => None,
            },
            _ => None,
        };
        Ok(found)
    }

    /// The Locations, Datastreams or HistoricalLocations of one Thing.
    async fn of_thing(&self, urn: &str, set: &str) -> Done {
        let path = format!("{}/{set}", self_link(&self.base, "Things", urn));
        let ceiling = self.opts.skip.saturating_add(self.opts.top);
        let (items, whole) = match set {
            "Locations" | "Datastreams" => {
                let e = self.entity(Some(urn)).await?.ok_or_else(not_found)?;
                (self.items(set, &e), true)
            }
            "HistoricalLocations" => {
                let e = self
                    .history(urn, "location", ceiling.saturating_add(1), "")
                    .await?
                    .ok_or_else(not_found)?;
                let items = mapping::historical_locations(&self.base, &e);
                let whole = items.len() <= ceiling;
                (items, whole)
            }
            _ => return Err(not_found()),
        };
        Ok(self.listed(&path, items, whole, &ById::default()))
    }

    /// The Observations of one Datastream: the history of its Attribute, `$skip` and `$top`
    /// bounding what the view is asked for (one more, to know whether more remain).
    async fn series(&self, key: &str) -> Done {
        let (urn, name, _) = mapping::split_stream(key).ok_or_else(not_found)?;
        let attribute = self.attribute(name);
        let ceiling = self.opts.skip.saturating_add(self.opts.top);
        let mut extra = String::new();
        if let Some(f) = &self.opts.filter {
            let series = query::series_filter(f)?;
            for (k, v) in series.window {
                let _ = write!(extra, "&{k}={}", encode(&v));
            }
            if let Some(q) = series.q {
                let _ = write!(extra, "&q={}", encode(&q));
            }
        }
        let e = self
            .history(urn, &attribute, ceiling.saturating_add(1), &extra)
            .await?
            .ok_or_else(not_found)?;
        let mut items = mapping::series(&self.base, &e, &attribute, key);
        if self.opts.newest_first {
            items.sort_by(|a, b| time(b).cmp(&time(a)));
        }
        let whole = items.len() <= ceiling;
        let path = format!("{}/Observations", self_link(&self.base, "Datastreams", key));
        Ok(self.listed(&path, items, whole, &ById::default()))
    }

    /// A page of a list read whole, or cut at the ceiling when `whole` is false: `$skip` and
    /// `$top` applied here, each item its own group.
    fn listed(&self, path: &str, items: Vec<Value>, whole: bool, ids: &ById) -> Value {
        let total = items.len();
        let groups: Vec<Vec<Value>> = items
            .into_iter()
            .skip(self.opts.skip)
            .take(self.opts.top)
            .map(|i| vec![i])
            .collect();
        let more = total > self.opts.skip.saturating_add(groups.len());
        self.page(path, groups, more, whole.then_some(total), ids)
    }

    /// The collection of a page of groups (one per Entity): cut at the byte budget between
    /// groups, one resource per id, `@iot.nextLink` when the cut or the view left more, and
    /// `@iot.count` when asked for and known: `known`, or the page itself when it is the whole
    /// answer.
    fn page(
        &self,
        path: &str,
        groups: Vec<Vec<Value>>,
        more: bool,
        known: Option<usize>,
        ids: &ById,
    ) -> Value {
        let read = groups.len();
        let (items, served) = query::fit(groups);
        let skip = self.opts.skip;
        let next = (more || served < read)
            .then(|| self.link(path, "$skip", &skip.saturating_add(served).to_string()));
        self.finish(items, next, known, skip == 0, ids)
    }

    /// The collection of a page's resources: those `ids` admits, each id once, `@iot.count` when
    /// asked for and `known` or, on a `first` page that is the whole answer, the page's own.
    fn finish(
        &self,
        mut items: Vec<Value>,
        next: Option<String>,
        known: Option<usize>,
        first: bool,
        ids: &ById,
    ) -> Value {
        let mut seen = BTreeSet::new();
        items.retain(|i| {
            let id = i.get("@iot.id");
            ids.admits(id.and_then(Value::as_str)) && seen.insert(id.map(Value::to_string))
        });
        let derived = (next.is_none() && first).then_some(items.len());
        let count = self.opts.count.then(|| known.or(derived)).flatten();
        query::collection(items, &self.opts.select, count, next)
    }

    /// The `@iot.nextLink` of `path`: the request's options, `name` set to `value` in place of
    /// any `$skip` or `$skiptoken`.
    fn link(&self, path: &str, name: &str, value: &str) -> String {
        let mut q: Vec<String> = self
            .pairs
            .iter()
            .filter(|(k, _)| k != "$skip" && k != "$skiptoken")
            .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
            .collect();
        q.push(format!("{name}={value}"));
        format!("{path}?{}", q.join("&"))
    }
}

/// An Observation's `phenomenonTime`.
fn time(observation: &Value) -> Option<&str> {
    observation.get("phenomenonTime").and_then(Value::as_str)
}

/// The Entity of `pool` whose id is `id`.
fn target<'a>(pool: &'a [Value], id: Option<&str>) -> Option<&'a Value> {
    pool.iter()
        .find(|e| e.get("id").and_then(Value::as_str) == id)
}

/// The resource of `items` whose id is `key`.
fn find(items: Vec<Value>, key: &str) -> Option<Value> {
    items
        .into_iter()
        .find(|i| i.get("@iot.id").and_then(Value::as_str) == Some(key))
}
