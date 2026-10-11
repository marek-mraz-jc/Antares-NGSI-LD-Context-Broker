// SPDX-License-Identifier: EUPL-1.2
//! A view of another model through the view root (gap G18,
//! `docs/analysis/view-mapping.md` sections 4 and 5): the request inverted
//! into the source model, run through the view as any other, and the answer
//! rebuilt in the target model.

use antares_ops::mapping::Mapping;
use antares_ops::{Caller, OperationId, OpsRequest, Refusal, ViewOps};
use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;

/// The largest answer the view root rebuilds, in bytes.
pub const MAX_ANSWER_BYTES: usize = 64 * 1024 * 1024;

/// Why a request that names its own `@context` is refused.
pub const OWN_CONTEXT: &str =
    "a mapped view names its attributes by its own @context; send no Link header";
/// Why a `GeoJSON` answer is refused.
pub const NO_GEOJSON: &str = "a mapped view answers JSON and JSON-LD, not GeoJSON";
/// Why an answer too large to rebuild is refused.
pub const TOO_LARGE: &str = "the answer is larger than a mapped view rebuilds";

/// Serve `op` through `view`, whose mapping is `mapping`. `path` is the
/// request path below the view's API root.
pub(crate) async fn serve(
    view: &ViewOps,
    mapping: &Mapping,
    op: OperationId,
    caller: &Caller,
    parts: Parts,
    body: Body,
    path: &str,
) -> Result<Response, Refusal> {
    if parts.headers.contains_key(header::LINK) {
        return Err(Refusal::Malformed(OWN_CONTEXT));
    }
    if parts
        .headers
        .get_all(header::ACCEPT)
        .iter()
        .any(|v| v.to_str().is_ok_and(|v| v.contains("geo+json")))
    {
        return Err(Refusal::Malformed(NO_GEOJSON));
    }
    let pairs = axum::extract::Query::<Vec<(String, String)>>::try_from_uri(&parts.uri)
        .map(|q| q.0)
        .map_err(|_| Refusal::Malformed(antares_ops::mapping::NOT_SERVED))?;
    let inverted = mapping.invert(&pairs).map_err(Refusal::Malformed)?;
    if inverted.selects_nothing {
        return if op == OperationId::QueryEntities {
            Ok(empty_page(&parts, &pairs))
        } else {
            Err(Refusal::NotFound)
        };
    }
    // `/entities/{id}/attrs/{attr}[/value]`: the attribute segment
    let mut attribute = None;
    let path = if op == OperationId::RetrieveAttributeValue {
        let mut segments: Vec<String> = path.split('/').map(str::to_owned).collect();
        let at = segments
            .iter()
            .position(|s| s == "attrs")
            .and_then(|i| i.checked_add(1))
            .ok_or(Refusal::NotFound)?;
        let segment = segments.get_mut(at).ok_or(Refusal::NotFound)?;
        let (slot, source) = mapping
            .source_attribute(segment)
            .map_err(Refusal::Malformed)?;
        *segment = source;
        attribute = Some(slot);
        segments.join("/")
    } else {
        path.to_owned()
    };
    let target = if inverted.pairs.is_empty() {
        path
    } else {
        format!("{path}?{}", encode(&inverted.pairs))
    };
    let mut inner = OpsRequest::new(parts.method, target)
        .body(body)
        .context(mapping.inner_context().clone());
    for (name, value) in &parts.headers {
        inner = inner.header(name.clone(), value.clone());
    }
    let answer = view.run(op, caller, inner).await?;
    let (mut parts, body) = answer.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_ANSWER_BYTES)
        .await
        .map_err(|_| Refusal::Limit(TOO_LARGE))?;
    parts.headers.remove(header::CONTENT_LENGTH);
    if !parts.status.is_success() {
        // the broker words a refusal in the inner request's terms
        let text = mapping.reword(&String::from_utf8_lossy(&bytes));
        return Ok(Response::from_parts(parts, Body::from(text)));
    }
    // HEAD, 204: no body to rebuild
    let Ok(mut json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(Response::from_parts(parts, Body::from(bytes)));
    };
    match attribute {
        Some(slot) => mapping.rebuild_attribute(slot, &mut json),
        None => mapping.rebuild(&mut json, &inverted.keep),
    }
    Ok(Response::from_parts(parts, Body::from(json.to_string())))
}

/// Query Entities whose `type` names nothing the view serves: an empty
/// page, answered without asking the store (section 4.1).
fn empty_page(parts: &Parts, pairs: &[(String, String)]) -> Response {
    let ld = parts
        .headers
        .get_all(header::ACCEPT)
        .iter()
        .any(|v| v.to_str().is_ok_and(|v| v.contains("application/ld+json")));
    let mut r = Response::new(Body::from("[]"));
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(if ld {
            "application/ld+json"
        } else {
            "application/json"
        }),
    );
    if pairs.iter().any(|(k, v)| k == "count" && v == "true") {
        r.headers_mut()
            .insert("ngsild-results-count", HeaderValue::from_static("0"));
    }
    *r.status_mut() = StatusCode::OK;
    r
}

/// A query string from decoded pairs: every byte outside RFC 3986's
/// unreserved set percent-encoded.
fn encode(pairs: &[(String, String)]) -> String {
    let one = |s: &str| {
        s.bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                    char::from(b).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect::<String>()
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", one(k), one(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_is_encoded_byte_for_byte() {
        let pairs = vec![
            ("q".to_owned(), "a==\"x y\";b>1|c~=\"ž\"".to_owned()),
            ("attrs".to_owned(), "a,b".to_owned()),
        ];
        assert_eq!(
            encode(&pairs),
            "q=a%3D%3D%22x%20y%22%3Bb%3E1%7Cc~%3D%22%C5%BE%22&attrs=a%2Cb"
        );
        let back = axum::extract::Query::<Vec<(String, String)>>::try_from_uri(
            &format!("/e?{}", encode(&pairs)).parse().unwrap_or_default(),
        )
        .map(|q| q.0)
        .unwrap_or_default();
        assert_eq!(back, pairs);
    }
}
