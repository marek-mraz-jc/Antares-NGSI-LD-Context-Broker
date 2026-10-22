// SPDX-License-Identifier: EUPL-1.2
//! The Events of one view as an iCalendar feed (RFC 5545):
//! `/x/views/{view}/ical/v1/events.ics?from=…&to=…&q=…` (T-4487, T-4901,
//! `docs/analysis/ical-mapping.md`).
//!
//! The feed renders the view's `representations.ical` field mapping (none: the `sdm-event`
//! preset over `Event`) over one NGSI-LD Query Entities of the mapping's type through the view:
//! `q`, `scopeQ`, `georel`, `geometry`, `coordinates`, `geoproperty`, `id`, `idPattern`, `lang`
//! and `attrs` pass through as Query Entities reads them, so a type or an Attribute the view
//! hides is absent and the Policy decides as for NGSI-LD. Any other parameter is `400`. A view
//! that shows no Entity of the type, or does not allow Query Entities, has no feed (the 404 an
//! unknown view answers). An Entity the mapping cannot render is left out and counted in
//! `antares_surface_records_skipped_total{surface="ical",reason}` (EP-126), never served as an
//! invalid `VEVENT`.
//!
//! `POST /x/views/{view}/ical/v1/preview` renders a candidate mapping over the first Entities
//! the request's subject reads through the view, for the Portal's Endpoint form: only with the
//! broker's admin bearer token (the Portal's), nothing stored.
//!
//! ```
//! use antares_ops::Surface;
//! let ical = antares_ical::Ical;
//! assert_eq!(ical.name(), "ical");
//! assert_eq!(ical.prefix(), "/x/views");
//! ```

#![forbid(unsafe_code)]

pub mod mapping;

use antares_ops::percent::{decoded, encode};
use antares_ops::{
    Caller, OperationId, Ops, Page, Pushdown, Refusal, Scanned, Surface, ViewOps, RESIDUAL_SCAN,
};
use axum::extract::{Query, Request};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderName, Method, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::any;
use axum::Router;
use chrono::Utc;
use mapping::{fold, local, IcalMapping, Outcome, Plan, When, Window};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Where the broker mounts the surface; the view and `ical/v1/events.ics` follow.
pub const PREFIX: &str = "/x/views";

/// The metric an Event left out of a feed increments (EP-126).
pub const SKIPPED: &str = "antares_surface_records_skipped_total";

/// The header a feed reports how many Events it left out in.
pub const SKIPPED_HEADER: &str = "antares-skipped-records";

/// The Query Entities parameters a feed passes through as they are (TS 104 175 6.4.3.2).
pub const PASSED: [&str; 9] = [
    "q",
    "scopeQ",
    "georel",
    "geometry",
    "coordinates",
    "geoproperty",
    "id",
    "idPattern",
    "lang",
];

/// The largest preview request body, in bytes.
pub const MAX_PREVIEW_BYTES: usize = 16 * 1024;

/// The most Entities one preview renders.
pub const MAX_PREVIEW_LIMIT: usize = 50;

/// How many `VEVENT`s a preview answers.
const PREVIEW_EVENTS: usize = 3;

/// The Entities one upstream page reads.
const PAGE: usize = 100;

/// The iCalendar surface.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ical;

impl Surface for Ical {
    fn name(&self) -> &'static str {
        "ical"
    }

    fn prefix(&self) -> &'static str {
        PREFIX
    }

    fn version_info(&self) -> Value {
        json!({ "endpoint": format!("{PREFIX}/{{view}}/ical/v1/events.ics"), "conformsTo": ["https://www.rfc-editor.org/rfc/rfc5545"] })
    }

    fn router(&self, ops: Ops) -> Router {
        let preview_ops = ops.clone();
        Router::new()
            .route(
                "/{view}/ical/v1/events.ics",
                any(move |caller: Caller, req: Request| {
                    let ops = ops.clone();
                    async move { serve(&ops, &caller, req).await }
                }),
            )
            .route(
                "/{view}/ical/v1/preview",
                any(move |caller: Caller, req: Request| {
                    let ops = preview_ops.clone();
                    async move { preview(&ops, &caller, req).await }
                }),
            )
    }
}

/// A refusal: its status and a fixed message that never names what the view hides.
enum Problem {
    /// The surface's own refusal.
    Own(StatusCode, String),
    /// The NGSI-LD refusal of the query the feed ran.
    Refused(Refusal),
    /// The store's own answer to that query (a broken `q`: 400).
    Answer(Box<Response>),
}

impl Problem {
    fn new(status: StatusCode, title: impl Into<String>) -> Self {
        Self::Own(status, title.into())
    }

    fn reply(self) -> Response {
        match self {
            Self::Own(status, title) => {
                let body = json!({ "title": title, "status": status.as_u16() });
                (
                    status,
                    [(CONTENT_TYPE, "application/problem+json")],
                    body.to_string(),
                )
                    .into_response()
            }
            Self::Refused(refusal) => refusal.ngsi_ld(),
            Self::Answer(answer) => *answer,
        }
    }
}

fn not_found() -> Problem {
    Problem::new(StatusCode::NOT_FOUND, "no such calendar")
}

async fn serve(ops: &Ops, caller: &Caller, req: Request) -> Response {
    let (parts, _) = req.into_parts();
    let refused =
        || Problem::new(StatusCode::METHOD_NOT_ALLOWED, "this surface is read only").reply();
    antares_ops::read_only(&parts.method, refused, async {
        match feed(ops, caller, view_segment(&parts.uri), &parts.uri).await {
            Ok((body, skipped)) => (
                StatusCode::OK,
                [
                    (CONTENT_TYPE, "text/calendar; charset=utf-8".to_owned()),
                    (HeaderName::from_static(SKIPPED_HEADER), skipped.to_string()),
                ],
                body,
            )
                .into_response(),
            Err(problem) => problem.reply(),
        }
    })
    .await
}

/// `/{view}/ical/v1/…`: a view id is `[a-z0-9-]`, so its raw segment is its id.
fn view_segment(uri: &axum::http::Uri) -> &str {
    uri.path().split('/').nth(1).unwrap_or_default()
}

/// A feed's query string read: the window, what passes through to Query Entities, the `type`
/// a caller named and the `attrs` it selects.
#[derive(Debug, Default)]
struct Asked {
    window: Window,
    passed: String,
    types: Vec<String>,
    attrs: Option<Vec<String>>,
}

fn asked(uri: &axum::http::Uri) -> Result<Asked, Problem> {
    let bad = |title: String| Problem::new(StatusCode::BAD_REQUEST, title);
    // a list of pairs reads any query string (lossy UTF-8): this never fails
    let pairs = Query::<Vec<(String, String)>>::try_from_uri(uri)
        .map(|Query(p)| p)
        .unwrap_or_default();
    let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
    for (key, value) in &pairs {
        if seen.insert(key, value).is_some() {
            return Err(bad(format!("{key} is given more than once")));
        }
    }
    let date = |name: &str| -> Result<Option<When>, Problem> {
        seen.get(name)
            .map(|v| {
                When::parse(v)
                    .ok_or_else(|| bad("from and to take an RFC 3339 date or date-time".to_owned()))
            })
            .transpose()
    };
    let mut out = Asked {
        window: Window {
            from: date("from")?.map(When::key),
            to: date("to")?.map(When::past),
        },
        ..Asked::default()
    };
    if let (Some(from), Some(to)) = (out.window.from, out.window.to) {
        if from >= to {
            return Err(bad("from is after to".to_owned()));
        }
    }
    let list = |v: &str| v.split(',').map(str::to_owned).collect::<Vec<_>>();
    for (key, value) in seen {
        match key {
            "from" | "to" => {}
            "type" => out.types = list(value),
            "attrs" => out.attrs = Some(list(value)),
            k if PASSED.contains(&k) => {
                let _ = write!(out.passed, "&{k}={}", encode(value));
            }
            other => {
                return Err(bad(format!(
                    "{other:?} is not a parameter of this feed: from, to, type, attrs, {}",
                    PASSED.join(", ")
                )))
            }
        }
    }
    Ok(out)
}

/// The view's mapping compiled; a mapping that does not compile serves nobody, and the
/// reason is the preview's to tell (the Portal checks it at propose).
fn plan(view: &ViewOps) -> Result<Plan, Problem> {
    let Some(written) = view.representation("ical") else {
        return Ok(Plan::default());
    };
    mapping::plan_of(written)
        .map(|(plan, degraded)| {
            if degraded {
                tracing::warn!(
                    "the view's calendar mapping is of a newer version: its preset is served"
                );
            }
            plan
        })
        .map_err(|_| {
            Problem::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the view's calendar mapping does not hold; its Endpoint must be published again",
            )
        })
}

/// What every Query Entities names: the view's types whose local name is the mapping's, or
/// for a view of every type the local store, narrowed by type after the read. A caller's
/// `type` may only name that type.
fn selector(view: &ViewOps, plan: &Plan, types: &[String]) -> Result<String, Problem> {
    let wanted = plan.entity_type();
    if types.iter().any(|t| local(t) != wanted) {
        return Err(Problem::new(
            StatusCode::BAD_REQUEST,
            format!("this calendar maps the type {wanted} alone"),
        ));
    }
    if view.shows_every_name() {
        return Ok("local=true".into());
    }
    let (shown, _) = view.shown_names();
    let matching: Vec<String> = shown
        .iter()
        .filter(|t| local(t) == wanted)
        .map(|t| encode(t))
        .collect();
    if matching.is_empty() {
        return Err(not_found());
    }
    Ok(format!("type={}", matching.join(",")))
}

/// The view, its plan and the pushdown of a feed or a preview request.
fn query(
    ops: &Ops,
    id: &str,
    asked: &Asked,
    mapping: Option<Plan>,
) -> Result<(ViewOps, Plan, Pushdown), Problem> {
    let id = decoded(id).ok_or_else(not_found)?;
    // a view that does not allow Query Entities has no feed, as an unknown one
    let view = ops
        .view(&id)
        .filter(|v| v.allows(OperationId::QueryEntities))
        .ok_or_else(not_found)?;
    let plan = match mapping {
        Some(plan) => plan,
        None => plan(&view)?,
    };
    let pushdown = Pushdown {
        query: format!("{}{}", selector(&view, &plan, &asked.types)?, asked.passed),
        attrs: asked.attrs.clone(),
        sys_attrs: true,
        ..Pushdown::default()
    };
    Ok((view, plan, pushdown))
}

/// One upstream page: its Entities of the plan's type and the next offset.
async fn page(
    view: &ViewOps,
    caller: &Caller,
    pushdown: &Pushdown,
    plan: &Plan,
    page: Page,
) -> Result<(Vec<Value>, Option<usize>), Problem> {
    let (entities, next) = match view.scan(caller, pushdown, None, page).await {
        Ok(Scanned::Page { entities, next, .. }) => (entities, next),
        // A Tenant that holds nothing yet answers 404 (5.5.10): an empty calendar.
        Ok(Scanned::Answer(a)) if a.status() == StatusCode::NOT_FOUND => (Vec::new(), None),
        Ok(Scanned::Answer(a)) => return Err(Problem::Answer(Box::new(a))),
        Err(refusal) => return Err(Problem::Refused(refusal)),
    };
    let wanted = plan.entity_type();
    let typed = entities
        .into_iter()
        .filter(|e| local(e.get("type").and_then(Value::as_str).unwrap_or_default()) == wanted)
        .collect();
    Ok((typed, next))
}

/// The feed of view `id` and how many Events it left out.
async fn feed(
    ops: &Ops,
    caller: &Caller,
    id: &str,
    uri: &axum::http::Uri,
) -> Result<(String, usize), Problem> {
    let asked = asked(uri)?;
    let (view, plan, pushdown) = query(ops, id, &asked, None)?;
    let now = Utc::now();
    let mut out = String::new();
    for line in [
        "BEGIN:VCALENDAR",
        "VERSION:2.0",
        "PRODID:-//Antares//antares-ical//EN",
        "CALSCALE:GREGORIAN",
    ] {
        fold(line, &mut out);
    }
    let name = decoded(id).unwrap_or_default();
    fold(
        &format!("X-WR-CALNAME:{}", mapping::escape(&name)),
        &mut out,
    );
    let (mut start, mut skipped) = (0usize, 0usize);
    // ponytail: the window is applied after the read, so a feed reads every Event the query
    // selects up to RESIDUAL_SCAN; a caller narrows with q (startDate>=…) past that.
    loop {
        let at = Page {
            start,
            size: PAGE,
            whole: false,
        };
        let (entities, next) = page(&view, caller, &pushdown, &plan, at).await?;
        for entity in &entities {
            match plan.vevent(entity, &asked.window, now) {
                Outcome::Event(text) => out.push_str(&text),
                Outcome::Outside => {}
                Outcome::Skipped(reason) => {
                    skipped = skipped.saturating_add(1);
                    metrics::counter!(SKIPPED, "surface" => "ical", "reason" => reason)
                        .increment(1);
                }
            }
        }
        match next {
            Some(n) if n >= RESIDUAL_SCAN => {
                return Err(Problem::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "the query selects as many events as one feed reads or more: narrow it with q",
                ))
            }
            Some(n) => start = n,
            None => break,
        }
    }
    fold("END:VCALENDAR", &mut out);
    Ok((out, skipped))
}

/// The body of `POST …/ical/v1/preview`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewBody {
    /// The candidate `representations.ical`.
    mapping: Value,
    /// How many Entities to render, 1 to [`MAX_PREVIEW_LIMIT`] (the default).
    #[serde(default)]
    limit: Option<usize>,
}

async fn preview(ops: &Ops, caller: &Caller, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    // before anything else, so a caller without the token learns nothing about the view
    if !ops.admits_admin(&parts.headers) {
        return Problem::new(StatusCode::FORBIDDEN, "the preview serves the Portal alone").reply();
    }
    if parts.method != Method::POST {
        return Problem::new(StatusCode::METHOD_NOT_ALLOWED, "a preview is a POST").reply();
    }
    let Ok(bytes) = axum::body::to_bytes(body, MAX_PREVIEW_BYTES).await else {
        return Problem::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("a preview body is at most {MAX_PREVIEW_BYTES} bytes"),
        )
        .reply();
    };
    match previewed(ops, caller, view_segment(&parts.uri), &parts.uri, &bytes).await {
        Ok(answer) => (StatusCode::OK, axum::Json(answer)).into_response(),
        Err(problem) => problem.reply(),
    }
}

/// The counts and the first `VEVENT`s of a candidate mapping over the first Entities read.
async fn previewed(
    ops: &Ops,
    caller: &Caller,
    id: &str,
    uri: &axum::http::Uri,
    bytes: &[u8],
) -> Result<Value, Problem> {
    let bad = |title: String| Problem::new(StatusCode::BAD_REQUEST, title);
    let body: PreviewBody =
        serde_json::from_slice(bytes).map_err(|e| bad(format!("not a preview body: {e}")))?;
    let limit = body.limit.unwrap_or(MAX_PREVIEW_LIMIT);
    if !(1..=MAX_PREVIEW_LIMIT).contains(&limit) {
        return Err(bad(format!("limit is 1 to {MAX_PREVIEW_LIMIT}")));
    }
    let candidate = serde_json::from_value::<IcalMapping>(body.mapping)
        .map_err(|e| e.to_string())
        .and_then(|m| Plan::compile(&m))
        .map_err(|why| bad(format!("the mapping does not hold: {why}")))?;
    let asked = asked(uri)?;
    let (view, plan, pushdown) = query(ops, id, &asked, Some(candidate))?;
    let first = Page {
        start: 0,
        size: limit,
        whole: false,
    };
    let (entities, _) = page(&view, caller, &pushdown, &plan, first).await?;
    let now = Utc::now();
    let (mut valid, mut outside) = (0usize, 0usize);
    let mut skipped: BTreeMap<&str, usize> = BTreeMap::new();
    let mut events = Vec::new();
    for entity in &entities {
        match plan.vevent(entity, &asked.window, now) {
            Outcome::Event(text) => {
                valid = valid.saturating_add(1);
                if events.len() < PREVIEW_EVENTS {
                    events.push(text);
                }
            }
            Outcome::Outside => outside = outside.saturating_add(1),
            Outcome::Skipped(reason) => {
                let n = skipped.entry(reason).or_default();
                *n = n.saturating_add(1);
            }
        }
    }
    Ok(json!({
        "read": entities.len(),
        "valid": valid,
        "outside": outside,
        "skipped": skipped,
        "events": events,
    }))
}
