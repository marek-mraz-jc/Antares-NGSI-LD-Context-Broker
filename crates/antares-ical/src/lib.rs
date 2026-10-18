// SPDX-License-Identifier: EUPL-1.2
//! The Events of one view as an iCalendar feed (RFC 5545):
//! `/x/views/{view}/ical/v1/events.ics?from=…&to=…` (T-4487, `docs/analysis/ical-mapping.md`).
//!
//! The feed reads the view's Query Entities of its `Event` types, so a type or an Attribute the
//! view hides is absent; a view that shows no `Event` type has no feed (the 404 an unknown view
//! answers). An Event without a `startDate` or a `name` is left out and counted in
//! `antares_surface_records_skipped_total{surface="ical",attribute}` (EP-126), never served as
//! an invalid `VEVENT`.
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
use antares_ops::{Caller, Ops, Page, Pushdown, Scanned, Surface, ViewOps, RESIDUAL_SCAN};
use axum::extract::{Query, Request};
use axum::http::header::{ALLOW, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::any;
use axum::Router;
use chrono::Utc;
use mapping::{fold, local, vevent, Outcome, When, Window};
use serde_json::{json, Value};

/// Where the broker mounts the surface; the view and `ical/v1/events.ics` follow.
pub const PREFIX: &str = "/x/views";

/// The SDM type a feed serves, by its local name (any context's IRI ending in it).
pub const EVENT: &str = "Event";

/// The metric an Event left out of a feed increments (EP-126).
pub const SKIPPED: &str = "antares_surface_records_skipped_total";

/// The header a feed reports how many Events it left out in.
pub const SKIPPED_HEADER: &str = "antares-skipped-records";

/// The methods a read-only surface answers.
const ALLOWED: &str = "GET, HEAD, OPTIONS";

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
        Router::new().route(
            "/{view}/ical/v1/events.ics",
            any(move |caller: Caller, req: Request| {
                let ops = ops.clone();
                async move { serve(&ops, &caller, req).await }
            }),
        )
    }
}

/// A refusal: its status and a fixed message that never names what the view hides.
struct Problem(StatusCode, &'static str);

impl Problem {
    fn reply(self) -> Response {
        let body = json!({ "title": self.1, "status": self.0.as_u16() });
        (
            self.0,
            [(CONTENT_TYPE, "application/problem+json")],
            body.to_string(),
        )
            .into_response()
    }
}

const NOT_FOUND: Problem = Problem(StatusCode::NOT_FOUND, "no such calendar");

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
        let mut reply =
            Problem(StatusCode::METHOD_NOT_ALLOWED, "this surface is read only").reply();
        reply
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static(ALLOWED));
        return reply;
    }
    // `/{view}/ical/v1/events.ics`: a view id is `[a-z0-9-]`, so its raw segment is its id.
    let view = parts.uri.path().split('/').nth(1).unwrap_or_default();
    let reply = match feed(ops, caller, view, &parts.uri).await {
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
    };
    if method == Method::HEAD {
        let (parts, _) = reply.into_parts();
        return Response::from_parts(parts, axum::body::Body::empty());
    }
    reply
}

/// The `from` and `to` of the query string; any other parameter is a calendar client's own.
fn window(uri: &axum::http::Uri) -> Result<Window, Problem> {
    let pairs = Query::<Vec<(String, String)>>::try_from_uri(uri)
        .map(|Query(p)| p)
        .unwrap_or_default();
    let bound = |name: &str| -> Result<Option<When>, Problem> {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| {
                When::parse(v).ok_or(Problem(
                    StatusCode::BAD_REQUEST,
                    "from and to take an RFC 3339 date or date-time",
                ))
            })
            .transpose()
    };
    let window = Window {
        from: bound("from")?.map(When::key),
        to: bound("to")?.map(When::past),
    };
    if let (Some(from), Some(to)) = (window.from, window.to) {
        if from >= to {
            return Err(Problem(StatusCode::BAD_REQUEST, "from is after to"));
        }
    }
    Ok(window)
}

/// What every Query Entities names: the view's types whose local name is `Event`, or for a
/// view of every type the local store, narrowed by type after the read.
fn selector(view: &ViewOps) -> Option<String> {
    if view.shows_every_name() {
        return Some("local=true".into());
    }
    let (types, _) = view.shown_names();
    let events: Vec<String> = types
        .iter()
        .filter(|t| local(t) == EVENT)
        .map(|t| encode(t))
        .collect();
    (!events.is_empty()).then(|| format!("type={}", events.join(",")))
}

/// The feed of view `id` and how many Events it left out.
async fn feed(
    ops: &Ops,
    caller: &Caller,
    id: &str,
    uri: &axum::http::Uri,
) -> Result<(String, usize), Problem> {
    let id = decoded(id).ok_or(NOT_FOUND)?;
    let view = ops.view(&id).ok_or(NOT_FOUND)?;
    let pushdown = Pushdown {
        query: selector(&view).ok_or(NOT_FOUND)?,
        sys_attrs: true,
        ..Pushdown::default()
    };
    let window = window(uri)?;
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
    fold(&format!("X-WR-CALNAME:{}", mapping::escape(&id)), &mut out);
    let (mut start, mut skipped) = (0usize, 0usize);
    // ponytail: the window is applied after the read, so a feed reads every Event of the view
    // up to RESIDUAL_SCAN; push `startDate` into `q` when a view holds more than that.
    loop {
        let page = Page {
            start,
            size: PAGE,
            whole: false,
        };
        let (entities, next) = match view.scan(caller, &pushdown, None, page).await {
            Ok(Scanned::Page { entities, next, .. }) => (entities, next),
            // A Tenant that holds nothing yet answers 404 (5.5.10): an empty calendar.
            Ok(Scanned::Answer(a)) if a.status() == StatusCode::NOT_FOUND => (Vec::new(), None),
            Ok(Scanned::Answer(_)) | Err(_) => {
                return Err(Problem(
                    StatusCode::BAD_GATEWAY,
                    "the view refused the read",
                ))
            }
        };
        for entity in &entities {
            let ty = entity
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if local(ty) != EVENT {
                continue;
            }
            match vevent(entity, &window, now) {
                Outcome::Event(text) => out.push_str(&text),
                Outcome::Outside => {}
                Outcome::Skipped(attribute) => {
                    skipped = skipped.saturating_add(1);
                    metrics::counter!(SKIPPED, "surface" => "ical", "attribute" => attribute)
                        .increment(1);
                }
            }
        }
        match next {
            Some(n) if n >= RESIDUAL_SCAN => {
                return Err(Problem(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "the view holds as many events as one feed reads or more",
                ))
            }
            Some(n) => start = n,
            None => break,
        }
    }
    fold("END:VCALENDAR", &mut out);
    Ok((out, skipped))
}
