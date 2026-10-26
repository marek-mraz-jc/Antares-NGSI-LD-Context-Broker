// SPDX-License-Identifier: EUPL-1.2
//! OGC SensorThings API v1.1, Sensing profile, read only, over one view:
//! `/x/views/{view}/sta/v1.1` (T-4462, `docs/analysis/sta-mapping.md`).
//!
//! Every set is read through the view's own operations: Query Entities naming the types the
//! view shows, Retrieve and Query Temporal Evolution for the series. A type or an Attribute the
//! view hides is absent from every set, count and id, and a hidden id answers the 404 an
//! unknown one does.
//!
//! A view's `representations.sta` lists named outputs (docs API/11 EP-159, T-5225), each its own
//! service under `/x/views/{view}/sta/{output}/v1.1`: its Things are the Entities of its source
//! types its filter selects, the filter a conjunct of every read and of every id looked up, and
//! its Datastreams the attributes its `datastreams` role names (every numeric Property when not
//! mapped). A Sensor or a point of interest of a type the output does not list is none of its
//! resources. A name the list does not hold, or an output that does not hold, answers 404.
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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use antares_ops::outputs::{self, Output};
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
use mapping::{local, self_link, unpart, urn_of};
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
            async move { Box::pin(serve(&ops, &caller, req)).await }
        });
        Router::new()
            .route("/{view}/sta/v1.1", handler.clone())
            .route("/{view}/sta/v1.1/", handler.clone())
            .route("/{view}/sta/v1.1/{*rest}", handler.clone())
            .route("/{view}/sta/{output}/v1.1", handler.clone())
            .route("/{view}/sta/{output}/v1.1/", handler.clone())
            .route("/{view}/sta/{output}/v1.1/{*rest}", handler)
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

/// The view's refusal, by its status; a count or sort over too many Entities is a 400 the
/// client fixes by narrowing, not an invalid filter.
fn refused(refusal: &Refusal) -> Problem {
    match refusal {
        Refusal::TooManyResults { .. } => Problem(
            StatusCode::BAD_REQUEST,
            "the query selects more Entities than one read may count or sort; narrow $filter"
                .into(),
        ),
        _ => upstream(refusal.status()),
    }
}

/// A view answer that is no success, named by what it was (T-5349: a timeout or an inner
/// failure reached the client as "refused").
fn upstream(status: StatusCode) -> Problem {
    let message = match status {
        StatusCode::BAD_REQUEST => "the filter is not valid NGSI-LD",
        StatusCode::UNAUTHORIZED
        | StatusCode::FORBIDDEN
        | StatusCode::METHOD_NOT_ALLOWED
        | StatusCode::UNPROCESSABLE_ENTITY => "the view refused the read",
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => {
            "the read took too long; narrow $filter or send it again"
        }
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
            "the broker is busy; send the read again shortly"
        }
        s if s.is_server_error() => "the read failed inside the broker",
        _ => "the view could not answer the read",
    };
    Problem(status, message.into())
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

/// One request below `/{view}/sta/v1.1` or `/{view}/sta/{output}/v1.1`: a view id and an output
/// name are `[a-z0-9-]`, so their raw segments are their names.
async fn respond(ops: &Ops, caller: &Caller, req: &Parts) -> Done {
    let uri = &req.uri;
    let mut segments = uri.path().split('/').skip(1);
    let id = segments.next().unwrap_or_default();
    // `sta`, then `v1.1` or the output's name and `v1.1`: the route matched them.
    let named = segments.nth(1).filter(|s| *s != "v1.1");
    let rest = segments
        .skip(usize::from(named.is_some()))
        .collect::<Vec<_>>()
        .join("/");
    let view = ops.view(id).ok_or_else(not_found)?;
    let output = named.map(|name| output(&view, name)).transpose()?;
    let root = named.map_or_else(|| "sta/v1.1".to_owned(), |n| format!("sta/{n}/v1.1"));
    let rest = antares_ops::percent::decoded(&rest).ok_or_else(not_found)?;
    // A list of pairs takes every query string: a name without `=` is a pair with an empty value.
    let pairs = Query::<Vec<(String, String)>>::try_from_uri(uri)
        .map(|Query(p)| p)
        .unwrap_or_default();
    let options = query::options(&pairs)?;
    let steps = query::steps(&rest);
    let ids = match (&options.filter, steps.as_slice()) {
        (Some(f), [Step { set, key: None }]) => query::by_id(f, set)?,
        _ => ById::default(),
    };
    expandable(&steps, &options.expand)?;
    let cx = Cx {
        opts: options,
        base: format!(
            "{}{PREFIX}/{id}/{root}",
            antares_ops::public_base(&req.headers)
        ),
        view: &view,
        output,
        caller,
        pairs,
        ids,
    };
    match steps.as_slice() {
        [] => Ok(json!({
            "value": SETS.iter().map(|s| json!({ "name": s, "url": format!("{}/{s}", cx.base) })).collect::<Vec<_>>(),
            "serverSettings": { "conformance": CONFORMANCE },
        })),
        [Step { set, key: None }] if set != "Things" && cx.opts.top == 0 => {
            // `$top=0` answers the count alone: here a resource is not an Entity, so the count is
            // the one a full first page knows, absent where that page is not the whole set.
            let mut cx = cx;
            cx.opts.top = query::MAX_TOP;
            let count = Box::pin(cx.set(set))
                .await?
                .get("@iot.count")
                .and_then(Value::as_u64);
            let count = count.and_then(|n| usize::try_from(n).ok());
            Ok(query::collection(Vec::new(), &[], count, None))
        }
        [Step { set, key: None }] => cx.set(set).await,
        [Step {
            set,
            key: Some(key),
        }] => cx
            .one(set, key)
            .await?
            .map(|v| query::selected(v, &cx.opts.select))
            .ok_or_else(not_found),
        [Step {
            set: from,
            key: Some(key),
        }, Step {
            set: member,
            key: None,
        }] => cx.related(from, key, member).await,
        _ => Err(not_found()),
    }
}

/// The output `name` of the view's `representations.sta` (T-5225) and the types it reads, as
/// the view answers them: a name the list does not hold, an output that does not hold, or one
/// whose sources the view shows none of serves nobody.
fn output(view: &ViewOps, name: &str) -> Result<Named, Problem> {
    let (_, found) = outputs::listed(view.representation("sta"), "sta")
        .into_iter()
        .find(|(n, _)| n == name)
        .ok_or_else(not_found)?;
    let output = found.map_err(|_| not_found())?;
    let sources = output.sources.iter().map(|s| s.entity_type.as_str());
    let types: Vec<String> = if view.shows_every_name() {
        sources.map(str::to_owned).collect()
    } else {
        let (shown, _) = view.shown_names();
        shown
            .into_iter()
            .filter(|t| output.sources.iter().any(|s| s.entity_type == local(t)))
            .collect()
    };
    if types.is_empty() {
        return Err(not_found());
    }
    Ok(Named { output, types })
}

/// A named output and the types it reads.
struct Named {
    output: Output,
    types: Vec<String>,
}

impl Named {
    /// Whether Attribute `name` (its local name) is one of the output's Datastreams: one its
    /// sources' `datastreams` role names, every one when no source maps the role.
    fn streams(&self, name: &str) -> bool {
        let named: Vec<&str> = self
            .output
            .sources
            .iter()
            .flat_map(|s| s.role("datastreams"))
            .collect();
        named.is_empty() || named.contains(&local(name))
    }

    /// The Entity `e` holding as measurements only the output's Datastreams.
    fn shaped(&self, mut e: Value) -> Value {
        e.as_object_mut().into_iter().for_each(|members| {
            members.retain(|name, member| {
                self.streams(name)
                    || !mapping::instances(member)
                        .iter()
                        .any(|i| i.get("value").is_some_and(Value::is_number))
            });
        });
        e
    }

    /// The query string `query` under the output's filter (EP-159): its `q` and `scopeQ` AND-ed
    /// with the request's, its geo-query fixed.
    fn narrowed(&self, query: &str) -> Result<String, Problem> {
        let mut pairs = BTreeMap::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let v = antares_ops::percent::decoded(v).ok_or_else(not_found)?;
            pairs.insert(k.to_owned(), v);
        }
        self.output
            .filter
            .narrow(&mut pairs)
            .map_err(|why| Problem(StatusCode::BAD_REQUEST, why.into()))?;
        Ok(pairs
            .iter()
            .map(|(k, v)| format!("{k}={}", encode(v)))
            .collect::<Vec<_>>()
            .join("&"))
    }
}

/// The `$expand` paths each set renders (T-5205 finding 18); any other is refused by name.
fn expandable(steps: &[Step], expand: &[String]) -> Result<(), Problem> {
    let target = match steps {
        [.., Step { set, .. }] => set.as_str(),
        [] => "",
    };
    let served: &[&str] = match target {
        "Things" | "Thing" => &["Locations", "Datastreams", "Datastreams/Observations"],
        "Datastreams" | "Datastream" => &["Observations"],
        _ => &[],
    };
    match expand.iter().find(|p| !served.contains(&p.as_str())) {
        Some(p) => Err(Problem(
            StatusCode::BAD_REQUEST,
            format!("$expand: {p} is not served on {target}"),
        )),
        None => Ok(()),
    }
}

/// One request's view, caller, base, options and the id and type comparisons of its `$filter`.
struct Cx<'a> {
    view: &'a ViewOps,
    /// The named output the request reads, `None` for the view's own service.
    output: Option<Named>,
    caller: &'a Caller,
    base: String,
    opts: Options,
    pairs: Vec<(String, String)>,
    ids: ById,
}

impl Cx<'_> {
    /// Whether shown type `t` is one `properties/type eq` names, every one without.
    fn wanted(&self, t: &str) -> bool {
        let wanted = &self.ids.types;
        wanted.is_empty() || wanted.iter().any(|w| w == t || w == local(t))
    }

    /// What every Query Entities names: the types the view shows (NGSI-LD 5.7.2 takes no query
    /// without one), or for a view of every type the local store.
    /// `properties/type eq` narrows it to those types the view shows (T-5205 finding 9): `None`
    /// when it shows none of them.
    fn selector(&self) -> Option<String> {
        let wanted = &self.ids.types;
        let types: Vec<String> = if let Some(named) = &self.output {
            // a named output reads its own types (T-5225), narrowed the same way
            named
                .types
                .iter()
                .filter(|t| self.wanted(t))
                .cloned()
                .collect()
        } else if self.view.shows_every_name() {
            if wanted.is_empty() {
                return Some("local=true".into());
            }
            wanted.clone()
        } else {
            let (shown, _) = self.view.shown_names();
            shown.into_iter().filter(|t| self.wanted(t)).collect()
        };
        if types.is_empty() {
            return None;
        }
        let types: Vec<String> = types.iter().map(|t| encode(t)).collect();
        Some(format!("type={}", types.join(",")))
    }

    /// The `&q=` of an entity set's `$filter`, empty without.
    fn filter(&self) -> String {
        self.ids
            .q
            .as_ref()
            .map_or_else(String::new, |q| format!("&q={}", encode(q)))
    }

    /// One Query Entities through the view: the Entities, whether more remain past them, and
    /// the count when `page.whole` asked for it.
    async fn entities(
        &self,
        extra: &str,
        page: Page,
    ) -> Result<(Vec<Value>, bool, Option<usize>), Problem> {
        let Some(selector) = self.selector() else {
            return Ok((Vec::new(), false, Some(0)));
        };
        // `modifiedAt` is the time of a value no sensor observed (STA 1.1 phenomenonTime).
        let pushdown = Pushdown {
            query: self.narrowed(&format!("{selector}{extra}"))?,
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
                let entities = match &self.output {
                    Some(named) => entities.into_iter().map(|e| named.shaped(e)).collect(),
                    None => entities,
                };
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

    /// One Entity the view shows, by URN. A key that is no URI names no Entity (NGSI-LD 4.5.1):
    /// it is the 404 of an unknown one, not the view's 400.
    async fn entity(&self, urn: Option<&str>) -> Result<Option<Value>, Problem> {
        let Some(urn) = urn.filter(|u| mapping::is_uri(u)) else {
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

    /// The query string `query` under the named output's filter, as written for the view's own.
    fn narrowed(&self, query: &str) -> Result<String, Problem> {
        self.output
            .as_ref()
            .map_or_else(|| Ok(query.to_owned()), |named| named.narrowed(query))
    }

    /// The temporal evolution of one Entity's `attribute`, its last `last` instances. Through a
    /// named output, none unless its filter selects the Entity now and `attribute` is its
    /// `location` or one of its Datastreams.
    async fn history(
        &self,
        urn: &str,
        attribute: &str,
        last: usize,
        extra: &str,
    ) -> Result<Option<Value>, Problem> {
        if let Some(named) = &self.output {
            let shown = attribute == "location" || named.streams(attribute);
            if !shown || Box::pin(self.entity(Some(urn))).await?.is_none() {
                return Ok(None);
            }
        }
        let path = format!(
            "/ngsi-ld/v1/temporal/entities/{}?attrs={}&lastN={last}&options=sysAttrs{extra}",
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
        let (ids, extra) = (&self.ids, self.filter());
        let path = format!("{}/{set}", self.base);
        if self.opts.ordered {
            return Err(Problem(
                StatusCode::BAD_REQUEST,
                "$orderby: served on a Datastream's Observations only".into(),
            ));
        }
        if self.opts.top == 0 {
            // A Thing's count alone (T-5205 finding 19): one Entity read for the operation's count.
            let page = Page {
                start: 0,
                size: 1,
                whole: self.opts.count,
            };
            let (_, _, count) = self.entities(&extra, page).await?;
            let count = count.filter(|_| ids.not.is_empty());
            return Ok(query::collection(Vec::new(), &[], count, None));
        }
        if let Some(key) = ids.is.first() {
            if ids.q.is_some() {
                return Err(Problem(
                    StatusCode::BAD_REQUEST,
                    "$filter: id eq takes no comparison beside id ne".into(),
                ));
            }
            let items = self.one(set, key).await?.into_iter().collect();
            return Ok(self.listed(&path, items, true, ids));
        }
        if set == "HistoricalLocations" {
            return self.moves(&extra, &path, ids).await;
        }
        if matches!(set, "Datastreams" | "Observations" | "ObservedProperties") {
            return self.resources(set, &extra, &path, ids).await;
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
                let mut groups: Vec<Vec<Value>> = entities
                    .iter()
                    .map(|e| {
                        target(&devices, mapping::object(e, "refDevice"))
                            .and_then(|d| mapping::sensor(&self.base, d))
                            .into_iter()
                            .collect()
                    })
                    .collect();
                // The Sensor of the Datastreams whose Entity names no Device, once, first, in
                // the first Entity's group: a group of its own counted as an Entity served and
                // the next link skipped one Device (F-ANT5-004).
                if self.opts.skip == 0 {
                    let unknown = mapping::unknown_sensor(&self.base);
                    match groups.first_mut() {
                        Some(first) => first.insert(0, unknown),
                        None => groups.push(vec![unknown]),
                    }
                }
                groups
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
        Ok(self.page(&path, groups, more, count, ids))
    }

    /// A page of a set an Entity holds several resources of: `$top` resources from the start
    /// [`Cx::resume`] finds, `@iot.nextLink` resuming after the last one served.
    async fn resources(&self, set: &str, extra: &str, path: &str, ids: &ById) -> Done {
        let start = self.resume(set, extra).await?;
        let (mut at, mut skipped) = start;
        // One group per resource, and where the next page resumes after each. Entities that hold
        // none are read past, a page of Entities at a time, until the page is full or [`WALK`]
        // Entities were read (T-5205 finding 15: 830 empty pages before the first Datastream).
        let (mut groups, mut after) = (Vec::new(), Vec::new());
        let mut more = true;
        for _ in 0..WALK / query::MAX_TOP {
            if groups.len() >= self.opts.top || !more {
                break;
            }
            let page = Page {
                start: at,
                size: self.opts.top,
                whole: false,
            };
            let (entities, left, _) = self.entities(extra, page).await?;
            more = left;
            for e in &entities {
                let items = self.items(set, e);
                let held = items.len();
                for (i, item) in items.into_iter().enumerate().skip(skipped) {
                    let done = i.saturating_add(1);
                    after.push(if done == held {
                        (at.saturating_add(1), 0)
                    } else {
                        (at, done)
                    });
                    groups.push(vec![item]);
                }
                (at, skipped) = (at.saturating_add(1), 0);
            }
        }
        let read = groups.len();
        groups.truncate(self.opts.top);
        let (items, served) = query::fit(groups);
        let resume = if served < read {
            after.get(served.saturating_sub(1)).copied()
        } else {
            more.then_some((at, 0))
        };
        let next = resume.map(|(e, i)| self.link(path, "$skiptoken", &format!("{e}.{i}")));
        Ok(self.finish(items, next, None, start == (0, 0), ids))
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
            return target(pois, Some(poi))
                .and_then(|t| mapping::feature(&self.base, t, &mapping::part(poi)));
        }
        let urn = e.get("id").and_then(Value::as_str).unwrap_or_default();
        mapping::feature(&self.base, e, &format!("{}/location", mapping::part(urn)))
    }

    /// A page of HistoricalLocations: one Query Temporal Evolution of the `location` history of
    /// `$top` Entities from `$skip`.
    async fn moves(&self, extra: &str, path: &str, ids: &ById) -> Done {
        let (top, skip) = (self.opts.top, self.opts.skip);
        let Some(selector) = self.selector() else {
            return Ok(self.page(path, Vec::new(), false, None, ids));
        };
        // ponytail: the last MAX_TOP moves of each Entity; a window per Entity when a client
        // pages a long history through this set rather than through its Thing.
        let query = format!(
            "/ngsi-ld/v1/temporal/entities?{}",
            self.narrowed(&format!(
                "{selector}{extra}&attrs=location&timerel=after&timeAt={EPOCH}&lastN={}&limit={top}&offset={skip}",
                query::MAX_TOP,
            ))?
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
            "Things" => self
                .entity(Some(key))
                .await?
                .and_then(|e| find(self.items(set, &e), key)),
            // `{E}/location/{T}`: where the Entity was at T (T-5205 finding 13).
            "Locations" if key.matches('/').count() == 2 => match urn_of(key) {
                Some(urn) => {
                    let time = key.rsplit('/').next().unwrap_or_default();
                    self.history(&urn, "location", query::MAX_SKIP, "")
                        .await?
                        .and_then(|e| mapping::location_at(base, &e, time))
                        .filter(|l| l.get("@iot.id").and_then(Value::as_str) == Some(key))
                }
                None => None,
            },
            "Locations" | "Datastreams" => self
                .entity(urn_of(key).as_deref())
                .await?
                .and_then(|e| find(self.items(set, &e), key)),
            // The current Observation, else an earlier one of its Datastream's history.
            "Observations" => match self.entity(urn_of(key).as_deref()).await? {
                None => None,
                Some(e) => match find(self.items(set, &e), key) {
                    Some(o) => Some(o),
                    None => self.earlier(key).await?,
                },
            },
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
            "Sensors" if key == mapping::UNKNOWN_SENSOR => Some(mapping::unknown_sensor(base)),
            "Sensors" => self
                .entity(Some(key))
                .await?
                .and_then(|d| mapping::sensor(base, &d)),
            // `{E}/location`, an Entity's own; else a point of interest's id, as a part.
            "FeaturesOfInterest" => match key.strip_suffix("/location") {
                Some(_) => self
                    .entity(urn_of(key).as_deref())
                    .await?
                    .filter(|e| mapping::object(e, "refPointOfInterest").is_none())
                    .and_then(|e| self.feature_of(&e, &[])),
                None => self
                    .entity(unpart(key).as_deref())
                    .await?
                    .and_then(|t| mapping::feature(base, &t, key)),
            },
            "HistoricalLocations" => match urn_of(key) {
                Some(urn) => self
                    .history(&urn, "location", query::MAX_SKIP, "")
                    .await?
                    .and_then(|e| find(mapping::historical_locations(base, &e), key)),
                None => None,
            },
            _ => None,
        };
        Ok(found)
    }

    /// A navigation property of one resource, or one of its members as `{name: value}`
    /// (SensorThings 1.1 req/resource-path): the resource is read first, so an unknown or
    /// hidden one is the 404 whatever follows it.
    async fn related(&self, from: &str, key: &str, member: &str) -> Done {
        let path = format!("{}/{member}", self_link(&self.base, from, key));
        let source = self.one(from, key).await?.ok_or_else(not_found)?;
        let owner = match from {
            "Things" | "Sensors" => Some(key.to_owned()),
            "FeaturesOfInterest" if !key.ends_with("/location") => unpart(key),
            _ => urn_of(key),
        };
        let urn = owner.as_deref().unwrap_or(key);
        let single = match (from, member) {
            ("Things", "Locations" | "Datastreams" | "HistoricalLocations") => {
                return self.of_thing(&path, urn, member).await
            }
            ("Locations", "HistoricalLocations") => return self.of_thing(&path, urn, member).await,
            ("Datastreams", "Observations") => return self.series(key).await,
            ("Locations", "Things") => {
                let e = self.entity(Some(urn)).await?.ok_or_else(not_found)?;
                return Ok(self.listed(&path, self.items(member, &e), true, &ById::default()));
            }
            // Where the Thing was at the HistoricalLocation's time (T-5205 finding 13).
            ("HistoricalLocations", "Locations") => {
                let time = key.rsplit('/').next().unwrap_or_default();
                let lid = format!("{}/location/{time}", mapping::part(urn));
                let items = self.one("Locations", &lid).await?.into_iter().collect();
                return Ok(self.listed(&path, items, true, &ById::default()));
            }
            ("Sensors", "Datastreams") => {
                let q = if key == mapping::UNKNOWN_SENSOR {
                    "!refDevice".to_owned()
                } else {
                    format!("refDevice=={}", Value::String(key.to_owned()))
                };
                return self
                    .where_(&path, &q, |b, e| mapping::datastreams(b, e, &[]))
                    .await;
            }
            ("FeaturesOfInterest", "Observations") if key.ends_with("/location") => {
                let e = self.entity(Some(urn)).await?.ok_or_else(not_found)?;
                return Ok(self.listed(
                    &path,
                    mapping::observations(&self.base, &e),
                    true,
                    &ById::default(),
                ));
            }
            ("FeaturesOfInterest", "Observations") => {
                let q = format!("refPointOfInterest=={}", Value::String(urn.to_owned()));
                return self.where_(&path, &q, mapping::observations).await;
            }
            ("ObservedProperties", "Datastreams") => return self.measuring(&path, key).await,
            ("Datastreams" | "HistoricalLocations", "Thing") => self.one("Things", urn).await?,
            ("Datastreams", "Sensor") => {
                let e = self.entity(Some(urn)).await?.ok_or_else(not_found)?;
                let device = mapping::object(&e, "refDevice").unwrap_or(mapping::UNKNOWN_SENSOR);
                self.one("Sensors", device).await?
            }
            ("Datastreams", "ObservedProperty") => {
                let (_, name, _) = mapping::split_stream(key).ok_or_else(not_found)?;
                self.one("ObservedProperties", &antares_ops::core_iri(&name))
                    .await?
            }
            // `one` found the Observation, so its id names a Datastream.
            ("Observations", "Datastream") => {
                let stream = mapping::stream_of(key).unwrap_or(key);
                self.one("Datastreams", stream).await?
            }
            ("Observations", "FeatureOfInterest") => {
                let e = self.entity(Some(urn)).await?.ok_or_else(not_found)?;
                let pois = self.referenced(std::slice::from_ref(&e), "refPointOfInterest");
                self.feature_of(&e, &pois.await?)
            }
            (_, name) if !name.contains('@') && !name.starts_with('$') => {
                source.get(name).map(|v| json!({ name: v }))
            }
            _ => None,
        };
        single
            .map(|v| query::selected(v, &self.opts.select))
            .ok_or_else(not_found)
    }

    /// A page of the resources `make` builds from each Entity `q` keeps: a Sensor's Datastreams,
    /// a point of interest's Observations.
    async fn where_(&self, path: &str, q: &str, make: fn(&str, &Value) -> Vec<Value>) -> Done {
        let extra = format!("&q={}", encode(q));
        let (entities, more, _) = self.entities(&extra, self.paged()).await?;
        let groups = entities.iter().map(|e| make(&self.base, e)).collect();
        Ok(self.page(path, groups, more, None, &ById::default()))
    }

    /// A page of the Datastreams measuring one ObservedProperty: those of the Entities that
    /// hold its Attribute.
    async fn measuring(&self, path: &str, iri: &str) -> Done {
        let extra = format!("&attrs={}", encode(&antares_ops::core_name(iri)));
        let (entities, more, _) = self.entities(&extra, self.paged()).await?;
        let groups = entities
            .iter()
            .map(|e| {
                mapping::datastreams(&self.base, e, &[])
                    .into_iter()
                    .filter(|d| {
                        mapping::split_stream(
                            d.get("@iot.id").and_then(Value::as_str).unwrap_or_default(),
                        )
                        .is_some_and(|(_, name, _)| antares_ops::core_iri(&name) == iri)
                    })
                    .collect()
            })
            .collect();
        Ok(self.page(path, groups, more, None, &ById::default()))
    }

    /// The page of Entities `$skip` and `$top` ask for.
    fn paged(&self) -> Page {
        Page {
            start: self.opts.skip,
            size: self.opts.top,
            whole: false,
        }
    }

    /// An earlier Observation by id, found in its Datastream's history.
    async fn earlier(&self, key: &str) -> Result<Option<Value>, Problem> {
        let parts = mapping::stream_of(key).and_then(|s| Some((s, mapping::split_stream(s)?)));
        let Some((stream, (urn, name, _))) = parts else {
            return Ok(None);
        };
        Ok(self
            .history(&urn, &name, query::MAX_SKIP, "")
            .await?
            .and_then(|e| find(mapping::series(&self.base, &e, &name, stream), key)))
    }

    /// The Locations, Datastreams or HistoricalLocations of one Thing, listed under `path`.
    async fn of_thing(&self, path: &str, urn: &str, set: &str) -> Done {
        let ceiling = self.opts.skip.saturating_add(self.opts.top);
        let (items, whole) = match set {
            "Locations" | "Datastreams" => {
                let e = self.entity(Some(urn)).await?.ok_or_else(not_found)?;
                (self.items(set, &e), true)
            }
            // `related` calls this for the three sets alone: the rest is HistoricalLocations.
            _ => {
                // `related` read the resource first: no `location` history is an empty list.
                let e = self
                    .history(urn, "location", ceiling.saturating_add(1), "")
                    .await?
                    .unwrap_or_default();
                let items = mapping::historical_locations(&self.base, &e);
                let whole = items.len() <= ceiling;
                (items, whole)
            }
        };
        Ok(self.listed(path, items, whole, &ById::default()))
    }

    /// The Observations of one Datastream, newest first: the current instance and the history of
    /// its Attribute read with its system times (T-5205 findings 1-3), `$skip` and `$top` bounding
    /// what the view is asked for (the newest `$skip + $top + 1`, to know whether more remain).
    async fn series(&self, key: &str) -> Done {
        let (urn, name, _) = mapping::split_stream(key).ok_or_else(not_found)?;
        let ceiling = self.opts.skip.saturating_add(self.opts.top);
        let mut extra = String::new();
        if let Some(f) = &self.opts.filter {
            for (k, v) in query::series_filter(f)? {
                let _ = write!(extra, "&{k}={}", encode(&v));
            }
        }
        // `related` found the Datastream: an Entity without a temporal record has its current
        // Observation alone.
        let history = self
            .history(&urn, &name, ceiling.saturating_add(1), &extra)
            .await?;
        let mut items = history
            .map(|e| mapping::series(&self.base, &e, &name, key))
            .unwrap_or_default();
        let read = items.len();
        if self.opts.filter.is_none() {
            let e = self.entity(Some(&urn)).await?;
            items.extend(
                e.iter()
                    .flat_map(|e| mapping::observations(&self.base, e))
                    .filter(|o| {
                        o.get("@iot.id")
                            .and_then(Value::as_str)
                            .and_then(mapping::stream_of)
                            == Some(key)
                    }),
            );
        }
        items.sort_by(|a, b| time(b).cmp(&time(a)));
        items.dedup_by(|a, b| a.get("@iot.id") == b.get("@iot.id"));
        let whole = read <= ceiling;
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
        let next = next.filter(|_| self.opts.top > 0);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn named(filter: &Value) -> Option<Named> {
        let list = json!([{"name": "a", "filter": filter, "sources": [{"type": "Car"}]}]);
        let (_, output) = outputs::listed(Some(&list), "sta").pop()?;
        Some(Named {
            output: output.ok()?,
            types: vec!["Car".into()],
        })
    }

    #[test]
    fn a_query_under_an_output_keeps_its_filter_and_refuses_what_would_undo_it() {
        let near =
            json!({"georel": "near;maxDistance==9", "geometry": "Point", "coordinates": "[1,2]"});
        let fixed = named(&near).map(|n| n.narrowed("type=Car&georel=within").map_err(|p| p.0));
        assert_eq!(fixed, Some(Err(StatusCode::BAD_REQUEST)));
        let q = named(&json!({"q": "speed<50"}));
        let broken = q.as_ref().map(|n| n.narrowed("q=%zz").map_err(|p| p.0));
        assert_eq!(broken, Some(Err(StatusCode::NOT_FOUND)));
        let joined = q.map(|n| n.narrowed("type=Car&q=a%3D%3D1").ok());
        assert_eq!(
            joined,
            Some(Some(
                "q=%28speed%3C50%29%3B%28a%3D%3D1%29&type=Car".to_owned()
            ))
        );
    }

    /// T-5349: only a refusal reads as one; a timeout, a busy broker or an inner failure keeps
    /// its status and says what it was.
    #[test]
    fn a_view_status_is_named_by_what_it_was() {
        let message = |s: StatusCode| upstream(s).1;
        for (status, says) in [
            (StatusCode::BAD_REQUEST, "the filter is not valid NGSI-LD"),
            (StatusCode::UNAUTHORIZED, "the view refused the read"),
            (StatusCode::FORBIDDEN, "the view refused the read"),
            (StatusCode::METHOD_NOT_ALLOWED, "the view refused the read"),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "the view refused the read",
            ),
            (
                StatusCode::REQUEST_TIMEOUT,
                "the read took too long; narrow $filter or send it again",
            ),
            (
                StatusCode::GATEWAY_TIMEOUT,
                "the read took too long; narrow $filter or send it again",
            ),
            (
                StatusCode::TOO_MANY_REQUESTS,
                "the broker is busy; send the read again shortly",
            ),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "the broker is busy; send the read again shortly",
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "the read failed inside the broker",
            ),
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                "the view could not answer the read",
            ),
        ] {
            assert_eq!(upstream(status).0, status);
            assert_eq!(message(status), says);
        }
    }

    #[test]
    fn a_count_over_too_many_entities_asks_for_a_narrower_filter() {
        let too_many = refused(&Refusal::TooManyResults {
            budget: 10,
            size: 11,
        });
        assert_eq!(too_many.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            too_many.1,
            "the query selects more Entities than one read may count or sort; narrow $filter"
        );
        let inner = refused(&Refusal::Inner {
            request_id: "ops-1".into(),
        });
        assert_eq!(inner.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(inner.1, "the read failed inside the broker");
    }
}
