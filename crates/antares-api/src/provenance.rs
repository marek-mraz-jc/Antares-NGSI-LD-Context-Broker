// SPDX-License-Identifier: EUPL-1.2
//! Who and what wrote a request's history (T-4665, platform ADR-N-054 2.3).
//!
//! The platform's gateway verifies the caller and names it in
//! `JC-Provenance-Channel`, `JC-Provenance-Actor`, `JC-Reason` and
//! `X-Request-Id`. Those headers are believed only from the gateway itself:
//! the request must carry the mesh identity `ANTARES_PROVENANCE_FROM` names in
//! `l5d-client-id`, the header the Linkerd proxy in front of the broker sets
//! from the caller's mTLS certificate. Unset, or from anyone else, a write is
//! `channel: api` with no actor, whatever its headers say.
//!
//! All of that is the platform's (feature `jc`, T-4854): vanilla Antares
//! reads none of these headers and records every write as `channel: api`.

use axum::http::HeaderMap;
use serde_json::{json, Value};

#[cfg(feature = "jc")]
/// The channels a write may name (ADR-N-054 2.3); anything else is `api`.
const CHANNELS: &[&str] = &["pipeline", "api", "ui", "import", "backfill", "mqtt"];
#[cfg(feature = "jc")]
const MAX_ACTOR: usize = 200;
#[cfg(feature = "jc")]
const MAX_REASON: usize = 500;
#[cfg(feature = "jc")]
const MAX_REQUEST_ID: usize = 128;

/// The provenance of a request with `headers`, the gateway's mesh identity
/// being `gateway` (None: no caller is believed).
#[cfg(feature = "jc")]
pub fn of_request(headers: &HeaderMap, gateway: Option<&str>) -> Value {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let believed = from_gateway(headers, gateway);
    let mut out = serde_json::Map::new();
    let channel = header("jc-provenance-channel")
        .filter(|c| believed && CHANNELS.contains(c))
        .unwrap_or("api");
    out.insert("channel".into(), json!(channel));
    if !believed {
        return Value::Object(out);
    }
    if let Some(actor) = header("jc-provenance-actor").filter(|a| plain(a, MAX_ACTOR)) {
        out.insert("actor".into(), json!(actor));
    }
    if let Some(reason) = header("jc-reason")
        .and_then(percent_decoded)
        .filter(|r| plain(r, MAX_REASON))
    {
        out.insert("reason".into(), json!(reason));
    }
    if let Some(id) = header("x-request-id").filter(|id| {
        (1..=MAX_REQUEST_ID).contains(&id.len())
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    }) {
        out.insert("requestId".into(), json!(id));
    }
    Value::Object(out)
}

/// Vanilla: no header is read, every write is `channel: api`.
#[cfg(not(feature = "jc"))]
pub fn of_request(_headers: &HeaderMap, _gateway: Option<&str>) -> Value {
    json!({"channel": "api"})
}

/// Whether the request may read `versions=all` (ADR-N-054 2.4): the
/// gateway grants it with `JC-Read-Versions: true` once the space's Policy
/// granted the caller `readVersions`; nobody else can.
#[cfg(feature = "jc")]
pub fn may_read_versions(headers: &HeaderMap, gateway: Option<&str>) -> bool {
    from_gateway(headers, gateway)
        && headers
            .get("jc-read-versions")
            .is_some_and(|v| v.as_bytes() == b"true")
}

/// Does the request carry the gateway's mesh identity `gateway`?
#[cfg(feature = "jc")]
fn from_gateway(headers: &HeaderMap, gateway: Option<&str>) -> bool {
    gateway.is_some_and(|g| {
        headers
            .get("l5d-client-id")
            .is_some_and(|v| v.as_bytes() == g.as_bytes())
    })
}

/// `provenance` with its `operation`.
pub fn with_operation(provenance: &Value, operation: &str) -> Value {
    let mut out = provenance.clone();
    if let Some(o) = out.as_object_mut() {
        o.insert("operation".into(), json!(operation));
    }
    out
}

/// Non-empty, at most `max` characters, no control character.
#[cfg(feature = "jc")]
fn plain(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.chars().count() <= max && !text.chars().any(char::is_control)
}

/// `%XX`-decoded UTF-8; `None` for a broken escape or bytes that are not UTF-8.
#[cfg(feature = "jc")]
fn percent_decoded(raw: &str) -> Option<String> {
    let mut out = Vec::with_capacity(raw.len());
    let mut bytes = raw.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hex = [bytes.next()?, bytes.next()?];
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GATEWAY: &str = "context-gateway.dev.serviceaccount.identity.linkerd.cluster.local";

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().expect("header"));
        }
        h
    }

    #[cfg(not(feature = "jc"))]
    #[test]
    fn vanilla_reads_no_platform_header() {
        let h = headers(&[
            ("l5d-client-id", GATEWAY),
            ("jc-provenance-channel", "pipeline"),
            ("jc-provenance-actor", "pipeline-parking"),
            ("jc-reason", "stuck"),
            ("x-request-id", "4f1c-9a07"),
        ]);
        assert_eq!(of_request(&h, Some(GATEWAY)), json!({"channel": "api"}));
    }

    #[test]
    fn the_operation_is_added() {
        assert_eq!(
            with_operation(&json!({"channel": "ui"}), "delete"),
            json!({"channel": "ui", "operation": "delete"})
        );
    }

    #[cfg(feature = "jc")]
    #[test]
    fn the_gateways_headers_are_believed_from_the_gateway_alone() {
        let named = [
            ("jc-provenance-channel", "pipeline"),
            ("jc-provenance-actor", "pipeline-parking"),
            ("jc-reason", "Sensor%20stuck%2C%20checked%20on%20site"),
            ("x-request-id", "4f1c-9a07"),
        ];
        let mut from_gateway = named.to_vec();
        from_gateway.push(("l5d-client-id", GATEWAY));
        assert_eq!(
            of_request(&headers(&from_gateway), Some(GATEWAY)),
            json!({"channel": "pipeline", "actor": "pipeline-parking",
                   "reason": "Sensor stuck, checked on site", "requestId": "4f1c-9a07"})
        );
        // A forged identity, no identity, or a broker that names no gateway: api, nobody.
        let mut forged = named.to_vec();
        forged.push((
            "l5d-client-id",
            "someone.else.serviceaccount.identity.linkerd.cluster.local",
        ));
        for (h, gateway) in [
            (headers(&forged), Some(GATEWAY)),
            (headers(&named), Some(GATEWAY)),
            (headers(&from_gateway), None),
        ] {
            assert_eq!(of_request(&h, gateway), json!({"channel": "api"}));
        }
    }

    #[cfg(feature = "jc")]
    #[test]
    fn versions_are_read_only_where_the_gateway_grants_them() {
        let grant = [("l5d-client-id", GATEWAY), ("jc-read-versions", "true")];
        assert!(may_read_versions(&headers(&grant), Some(GATEWAY)));
        assert!(!may_read_versions(&headers(&grant), None));
        assert!(!may_read_versions(
            &headers(&[("l5d-client-id", "x.y"), ("jc-read-versions", "true")]),
            Some(GATEWAY)
        ));
        assert!(!may_read_versions(
            &headers(&[("l5d-client-id", GATEWAY), ("jc-read-versions", "yes")]),
            Some(GATEWAY)
        ));
    }

    #[cfg(feature = "jc")]
    #[test]
    fn a_value_out_of_shape_is_left_out() {
        let long = "x".repeat(MAX_REASON + 1);
        let h = headers(&[
            ("l5d-client-id", GATEWAY),
            ("jc-provenance-channel", "carrier-pigeon"),
            ("jc-reason", &long),
            ("x-request-id", "a b"),
            ("jc-provenance-actor", " "),
        ]);
        assert_eq!(of_request(&h, Some(GATEWAY)), json!({"channel": "api"}));
        assert_eq!(percent_decoded("%zz"), None);
        assert_eq!(percent_decoded("%+1"), None, "a sign is no hex digit");
        assert_eq!(percent_decoded("50%"), None, "an escape cut short");
        assert_eq!(percent_decoded("%C3"), None, "half a character");
        assert_eq!(percent_decoded("N%C3%A1m"), Some("Nám".into()));
    }
}
