// SPDX-License-Identifier: EUPL-1.2
//! Per-request cost (T-6615, docs/src/request-cost.md). With
//! `ANTARES_REQUEST_COST=on` every answer carries what the request cost:
//!
//! ```text
//! Server-Timing: app;dur=1.2, db;dur=0.8, pool;dur=0.1, total;dur=2.4
//! JC-Request-Cost: cpu-ms=1.234, db-ms=0.812, statements=3, rows=20
//! ```
//!
//! and `/q/metrics` keeps the totals per tenant and surface. `app` is the
//! time spent inside the request future's polls, `db` the wall time of the
//! store statements it awaited, `pool` the wait for a pool connection,
//! `total` the wall time to the response headers. One layer over
//! [`crate::router()`] covers every surface: they all reach data through
//! [`AppState::call`], and an inner call finds the outer request's
//! accumulator already in place, so it adds to that one and answers no
//! header of its own. Off (the default) builds no layer at all.

use antares_store::cost::{self, Cost, Totals};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// The switch: `off` (the default, also when unset) or `on`.
pub const ENV: &str = "ANTARES_REQUEST_COST";

/// The header that carries the counts Server-Timing cannot (RFC 8941 dictionary).
pub const HEADER: HeaderName = HeaderName::from_static("jc-request-cost");

const SERVER_TIMING: HeaderName = HeaderName::from_static("server-timing");

/// At most this many tenants get a label of their own on `/q/metrics`; the
/// rest share `_other`. A tenant is a header any caller can send, and an
/// unbounded label set would grow the recorder for every name tried.
const MAX_TENANT_LABELS: usize = 1000;

/// Reads [`ENV`]; any value but `on` or `off` is a startup error.
pub fn parse(raw: Option<&str>) -> Result<bool, String> {
    match raw.map(str::trim) {
        None | Some("" | "off") => Ok(false),
        Some("on") => Ok(true),
        Some(other) => Err(format!("{ENV}: unknown value {other:?} (off|on)")),
    }
}

/// The layer over [`crate::router()`]: measures a request unless an outer
/// one is measured already (an in-process [`AppState::call`]).
pub(crate) async fn measure(req: Request, next: Next) -> Response {
    if cost::current().is_some() {
        return next.run(req).await;
    }
    let tenant = req
        .headers()
        .get("NGSILD-Tenant")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let surface = surface_of(req.uri().path());
    let started = Instant::now();
    let acc = Arc::new(Cost::default());
    let res = cost::measured(Arc::clone(&acc), next.run(req)).await;
    let total = started.elapsed();
    let totals = acc.totals();
    let tenant = tenant_label(tenant.as_deref());
    let (mut parts, body) = res.into_parts();
    stamp(&mut parts.headers, &totals, total);
    record(&tenant, surface, &totals);
    if parts.status == StatusCode::SWITCHING_PROTOCOLS {
        return Response::from_parts(parts, body);
    }
    let bytes = metrics::counter!(
        "antares_request_cost_response_bytes_total",
        "tenant" => tenant,
        "surface" => surface,
    );
    let body = http_body_util::BodyExt::map_frame(body, move |frame| {
        if let Some(data) = frame.data_ref() {
            bytes.increment(u64::try_from(data.len()).unwrap_or(u64::MAX));
        }
        frame
    });
    Response::from_parts(parts, axum::body::Body::new(body))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// `Server-Timing` (W3C Server Timing, durations in ms with one decimal).
#[must_use]
pub fn server_timing(t: &Totals, total: Duration) -> String {
    format!(
        "app;dur={:.1}, db;dur={:.1}, pool;dur={:.1}, total;dur={:.1}",
        ms(t.app),
        ms(t.db),
        ms(t.pool),
        ms(total)
    )
}

/// [`HEADER`]: an RFC 8941 dictionary of decimals (three fraction digits at
/// most) and integers.
#[must_use]
pub fn cost_header(t: &Totals) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "cpu-ms={:.3}, db-ms={:.3}, statements={}, rows={}",
        ms(t.app),
        ms(t.db),
        t.statements,
        t.rows
    );
    out
}

fn stamp(headers: &mut HeaderMap, t: &Totals, total: Duration) {
    if let Ok(v) = HeaderValue::from_str(&server_timing(t, total)) {
        headers.append(SERVER_TIMING, v);
    }
    if let Ok(v) = HeaderValue::from_str(&cost_header(t)) {
        headers.insert(HEADER, v);
    }
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn record(tenant: &str, surface: &'static str, t: &Totals) {
    let labels = [
        ("tenant", tenant.to_owned()),
        ("surface", surface.to_owned()),
    ];
    metrics::counter!("antares_request_cost_requests_total", &labels).increment(1);
    metrics::counter!("antares_request_cost_app_microseconds_total", &labels)
        .increment(micros(t.app));
    metrics::counter!("antares_request_cost_db_microseconds_total", &labels)
        .increment(micros(t.db));
    metrics::counter!("antares_request_cost_pool_microseconds_total", &labels)
        .increment(micros(t.pool));
    metrics::counter!("antares_request_cost_statements_total", &labels).increment(t.statements);
    metrics::counter!("antares_request_cost_rows_total", &labels).increment(t.rows);
}

/// The tenant's label: its name while fewer than [`MAX_TENANT_LABELS`] names
/// were seen, `_other` after; the default Tenant is the empty name.
fn tenant_label(raw: Option<&str>) -> String {
    static SEEN: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();
    let name = raw.unwrap_or_default();
    let seen = SEEN.get_or_init(RwLock::default);
    if seen
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(name)
    {
        return name.to_owned();
    }
    let mut seen = seen
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if seen.contains(name) || seen.len() < MAX_TENANT_LABELS {
        seen.insert(name.to_owned());
        return name.to_owned();
    }
    "_other".to_owned()
}

/// The surfaces a path label can name; any other path is `other`.
const SURFACES: &[&str] = &[
    "channels", "export", "feed", "gbfs", "geojson", "gtfsrt", "ical", "jsonstat", "linkml", "mcp",
    "ngsi-ld", "odata", "ogc", "sta", "ws",
];

/// The surface a request path belongs to: `ngsi-ld`, `admin` for `/q`, the
/// surface below a view (`/x/views/{view}/odata/...` is `odata`), or a
/// surface's own `/x/{name}` prefix. Never a client's free text.
fn surface_of(path: &str) -> &'static str {
    let mut seg = path.trim_start_matches('/').split('/');
    let known = |s: Option<&str>| {
        s.and_then(|s| SURFACES.iter().find(|k| **k == s).copied())
            .unwrap_or("other")
    };
    match seg.next() {
        Some("ngsi-ld") => "ngsi-ld",
        Some("q") => "admin",
        Some("x") => match seg.next() {
            Some("views") => known(seg.nth(1)),
            other => known(other),
        },
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8941 section 4.2.2 dictionary members whose values are bare
    /// integers (3.3.1) or decimals (3.3.2), the only shapes [`HEADER`] uses.
    fn parse_dictionary(text: &str) -> Vec<(String, String)> {
        text.split(',')
            .map(|member| {
                let member = member.trim_matches(' ');
                let (key, value) = member.split_once('=').expect("key=value");
                let mut k = key.chars();
                assert!(k.next().is_some_and(|c| c.is_ascii_lowercase() || c == '*'));
                assert!(
                    k.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-.*".contains(c))
                );
                let (int, frac) = value.split_once('.').unwrap_or((value, ""));
                assert!(!int.is_empty() && int.bytes().all(|b| b.is_ascii_digit()));
                if value.contains('.') {
                    assert!(int.len() <= 12, "decimal integer part {value}");
                    assert!((1..=3).contains(&frac.len()), "decimal fraction {value}");
                    assert!(frac.bytes().all(|b| b.is_ascii_digit()));
                } else {
                    assert!(int.len() <= 15, "integer {value}");
                }
                (key.to_owned(), value.to_owned())
            })
            .collect()
    }

    fn sample() -> Totals {
        Totals {
            app: Duration::from_micros(1_234_567),
            db: Duration::from_nanos(812_345),
            pool: Duration::from_micros(50),
            statements: 3,
            rows: 20,
        }
    }

    #[test]
    fn the_cost_header_is_an_rfc_8941_dictionary() {
        let parsed = parse_dictionary(&cost_header(&sample()));
        let want = [
            ("cpu-ms", "1234.567"),
            ("db-ms", "0.812"),
            ("statements", "3"),
            ("rows", "20"),
        ];
        let want: Vec<_> = want
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert_eq!(parsed, want);
        // an empty request is still a valid dictionary
        parse_dictionary(&cost_header(&Totals::default()));
    }

    #[test]
    fn server_timing_names_four_metrics_in_ms() {
        let text = server_timing(&sample(), Duration::from_millis(1500));
        assert_eq!(
            text,
            "app;dur=1234.6, db;dur=0.8, pool;dur=0.1, total;dur=1500.0"
        );
        // W3C Server Timing: metric-name (a token) ;dur= a number, comma separated
        for metric in text.split(", ") {
            let (name, dur) = metric.split_once(";dur=").expect("dur");
            assert!(name.bytes().all(|b| b.is_ascii_alphanumeric()));
            assert!(dur.parse::<f64>().is_ok_and(f64::is_finite));
        }
    }

    #[test]
    fn the_switch_is_on_or_off() {
        assert_eq!(parse(None), Ok(false));
        assert_eq!(parse(Some("off")), Ok(false));
        assert_eq!(parse(Some("on")), Ok(true));
        for bad in ["1", "true", "ON", "yes"] {
            assert!(parse(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn surfaces_are_named_from_a_fixed_list() {
        assert_eq!(surface_of("/ngsi-ld/v1/entities"), "ngsi-ld");
        assert_eq!(surface_of("/q/metrics"), "admin");
        assert_eq!(surface_of("/x/views/v1/odata/Things"), "odata");
        assert_eq!(surface_of("/x/views/v1/whatever"), "other");
        assert_eq!(surface_of("/x/ws"), "ws");
        assert_eq!(surface_of("/x/anything-a-client-types"), "other");
        assert_eq!(surface_of("/"), "other");
    }

    #[test]
    fn tenant_labels_are_bounded() {
        for i in 0..MAX_TENANT_LABELS {
            tenant_label(Some(&format!("t{i}")));
        }
        assert_eq!(tenant_label(Some("one-too-many")), "_other");
        assert_eq!(tenant_label(Some("t0")), "t0");
    }
}
