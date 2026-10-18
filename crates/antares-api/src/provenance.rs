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

use axum::http::HeaderMap;
use serde_json::{json, Map, Value};

/// The channels a write may name (ADR-N-054 2.3); anything else is `api`.
const CHANNELS: &[&str] = &["pipeline", "api", "ui", "import", "backfill", "mqtt"];
const MAX_ACTOR: usize = 200;
const MAX_REASON: usize = 500;
const MAX_REQUEST_ID: usize = 128;

/// The provenance of a request with `headers`, the gateway's mesh identity
/// being `gateway` (None: no caller is believed).
pub fn of_request(headers: &HeaderMap, gateway: Option<&str>) -> Value {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let believed = gateway.is_some_and(|g| header("l5d-client-id") == Some(g));
    let mut out = Map::new();
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

/// `provenance` with its `operation`.
pub fn with_operation(provenance: &Value, operation: &str) -> Value {
    let mut out = provenance.clone();
    if let Some(o) = out.as_object_mut() {
        o.insert("operation".into(), json!(operation));
    }
    out
}

/// Non-empty, at most `max` characters, no control character.
fn plain(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.chars().count() <= max && !text.chars().any(char::is_control)
}

/// `%XX`-decoded UTF-8; `None` for a broken escape or bytes that are not UTF-8.
fn percent_decoded(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
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
        assert_eq!(percent_decoded("%C3"), None, "half a character");
        assert_eq!(percent_decoded("N%C3%A1m"), Some("Nám".into()));
        assert_eq!(
            with_operation(&json!({"channel": "ui"}), "delete"),
            json!({"channel": "ui", "operation": "delete"})
        );
    }
}
