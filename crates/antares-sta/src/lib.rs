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
use axum::http::header::{ALLOW, CONTENT_TYPE};
use axum::http::request::Parts;
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::any;
use axum::Router;
use mapping::{local, self_link, urn_of};
use query::{Options, Step};
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

/// The methods a read-only surface answers.
const ALLOWED: &str = "GET, HEAD, OPTIONS";

/// The start of every location history a HistoricalLocations page reads.
const EPOCH: &str = "1970-01-01T00:00:00Z";

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
    let method = &parts.method;
    if method == Method::OPTIONS {
        return (
            StatusCode::NO_CONTENT,
            [(ALLOW, HeaderValue::from_static(ALLOWED))],
        )
            .into_response();
    }
    if method != Method::GET && method != Method::HEAD {
        let mut reply = Problem(
            StatusCode::METHOD_NOT_ALLOWED,
            "this surface is read only".into(),
        )
        .reply();
        reply
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static(ALLOWED));
        return reply;
    }
    let reply = match respond(ops, caller, &parts).await {
        Ok(doc) => (
            StatusCode::OK,
            [(CONTENT_TYPE, "application/json")],
            doc.to_string(),
        )
            .into_response(),
        Err(problem) => problem.reply(),
    };
    if method == Method::HEAD {
        let (parts, _) = reply.into_parts();
        return Response::from_parts(parts, axum::body::Body::empty());
    }
    reply
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

    /// The `&q=` of `$filter`, empty without one.
    fn filter(&self) -> Result<String, Problem> {
        match &self.opts.filter {
            Some(f) => Ok(format!("&q={}", encode(&query::filter_to_q(f)?))),
            None => Ok(String::new()),
        }
    }

    /// One Query Entities through the view: the Entities, whether more remain past them, and
    /// the count when `page.whole` asked for it.
    async fn entities(
        &self,
        extra: &str,
        page: Page,
    ) -> Result<(Vec<Value>, bool, Option<usize>), Problem> {
        let pushdown = Pushdown {
            query: format!("{}{extra}", self.selector()),
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
        let extra = self.filter()?;
        let path = format!("{}/{set}", self.base);
        if set == "HistoricalLocations" {
            return self.moves(&extra, &path).await;
        }
        if !SETS.contains(&set) {
            return Err(not_found());
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
        // Only a Thing is one Entity, so only its count is the operation's.
        Ok(self.page(&path, groups, more, count.filter(|_| set == "Things")))
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
            "Datastreams" => mapping::datastreams(base, e),
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
    async fn moves(&self, extra: &str, path: &str) -> Done {
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
        Ok(self.page(path, groups, more, None))
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
        Ok(self.listed(&path, items, whole))
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
        Ok(self.listed(&path, items, whole))
    }

    /// A page of a list read whole, or cut at the ceiling when `whole` is false: `$skip` and
    /// `$top` applied here, each item its own group.
    fn listed(&self, path: &str, items: Vec<Value>, whole: bool) -> Value {
        let total = items.len();
        let groups: Vec<Vec<Value>> = items
            .into_iter()
            .skip(self.opts.skip)
            .take(self.opts.top)
            .map(|i| vec![i])
            .collect();
        let more = total > self.opts.skip.saturating_add(groups.len());
        self.page(path, groups, more, whole.then_some(total))
    }

    /// The collection of a page of groups (one per Entity): cut at the byte budget between
    /// groups, one resource per id, `@iot.nextLink` when the cut or the view left more, and
    /// `@iot.count` when asked for and known: `known`, or the page itself when it is the whole
    /// answer.
    fn page(&self, path: &str, groups: Vec<Vec<Value>>, more: bool, known: Option<usize>) -> Value {
        let read = groups.len();
        let (mut items, served) = query::fit(groups);
        let mut seen = BTreeSet::new();
        items.retain(|i| seen.insert(i.get("@iot.id").map(Value::to_string)));
        let more = more || served < read;
        let skip = self.opts.skip;
        let next = more.then(|| {
            let mut q: Vec<String> = self
                .pairs
                .iter()
                .filter(|(k, _)| k != "$skip")
                .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
                .collect();
            q.push(format!("$skip={}", skip.saturating_add(served)));
            format!("{path}?{}", q.join("&"))
        });
        let derived = (!more && skip == 0).then_some(items.len());
        let count = self.opts.count.then(|| known.or(derived)).flatten();
        query::collection(items, &self.opts.select, count, next)
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
