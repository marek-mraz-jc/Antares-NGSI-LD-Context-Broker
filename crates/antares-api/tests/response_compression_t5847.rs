// SPDX-License-Identifier: EUPL-1.2
//! T-5847: responses honour Accept-Encoding gzip and br. The decoded body is
//! byte-identical to the plain one, a tiny body stays plain, and a request
//! body is never decompressed.

use std::io::Read;

use antares_api::AppState;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn send(st: &AppState, req: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
    let resp = antares_api::router(st.clone())
        .oneshot(req)
        .await
        .expect("response");
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.expect("body").to_bytes();
    (parts.status, parts.headers, bytes)
}

fn get(uri: &str, encoding: Option<&str>) -> Request<Body> {
    let mut req = Request::builder().uri(uri);
    if let Some(e) = encoding {
        req = req.header("Accept-Encoding", e);
    }
    req.body(Body::empty()).expect("request")
}

/// Ten entities of one type: the attribute names repeat, as in a real page.
async fn seeded() -> AppState {
    let st = AppState::new("test".into());
    for n in 0..10 {
        let create = format!(
            r#"{{"id":"urn:ngsi-ld:Kpi:{n}","type":"Kpi",
            "kpiValue":{{"type":"Property","value":{n}}},
            "description":{{"type":"Property","value":"share of buses on time"}}}}"#
        );
        let (status, _, body) = send(
            &st,
            Request::builder()
                .method("POST")
                .uri("/ngsi-ld/v1/entities")
                .header("Content-Type", "application/json")
                .header("Content-Length", create.len())
                .body(Body::from(create))
                .expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body:?}");
    }
    st
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Vec<&'a str> {
    h.get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect()
}

fn gunzip(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(b)
        .read_to_end(&mut out)
        .expect("gzip");
    out
}

fn unbrotli(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    brotli::Decompressor::new(b, 4096)
        .read_to_end(&mut out)
        .expect("br");
    out
}

/// Asks `uri` plain and with `encoding`; returns the status of both reads, the
/// plain body and the decoded one.
async fn both(st: &AppState, uri: &str, encoding: &str) -> (StatusCode, Bytes, Vec<u8>) {
    let (status, plain_headers, plain) = send(st, get(uri, None)).await;
    assert!(
        plain_headers.get("content-encoding").is_none(),
        "no Accept-Encoding, yet {plain_headers:?}"
    );
    let (status_enc, headers, packed) = send(st, get(uri, Some(encoding))).await;
    assert_eq!(status, status_enc);
    assert_eq!(header(&headers, "content-encoding"), vec![encoding]);
    let vary = header(&headers, "vary").join(",").to_ascii_lowercase();
    assert!(vary.contains("accept-encoding"), "vary: {vary}");
    assert!(
        packed.len() < plain.len(),
        "{} >= {}",
        packed.len(),
        plain.len()
    );
    let decoded = match encoding {
        "gzip" => gunzip(&packed),
        _ => unbrotli(&packed),
    };
    (status, plain, decoded)
}

#[tokio::test(flavor = "multi_thread")]
async fn gzip_answer_decodes_to_the_plain_body() {
    let st = seeded().await;
    let (status, plain, decoded) = both(&st, "/ngsi-ld/v1/entities?type=Kpi", "gzip").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(decoded, plain.to_vec());
}

#[tokio::test(flavor = "multi_thread")]
async fn br_answer_decodes_to_the_plain_body() {
    let st = seeded().await;
    let (status, plain, decoded) = both(&st, "/ngsi-ld/v1/entities?type=Kpi", "br").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(decoded, plain.to_vec());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_problem_answer_decodes_to_the_same_problem() {
    let st = AppState::new("test".into());
    let (status, plain, decoded) =
        both(&st, "/ngsi-ld/v1/entities/urn:ngsi-ld:Kpi:missing", "gzip").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(decoded, plain.to_vec());
    let problem: serde_json::Value = serde_json::from_slice(&decoded).expect("json");
    assert_eq!(
        problem["type"],
        "https://uri.etsi.org/ngsi-ld/errors/ResourceNotFound"
    );
}

/// A body of known size under the minimum (here 6.3.4's bare 411) stays
/// plain. A streamed list has no known size, so even an empty one is packed.
#[tokio::test(flavor = "multi_thread")]
async fn a_body_under_the_minimum_stays_plain() {
    let st = AppState::new("test".into());
    let (status, headers, body) = send(
        &st,
        Request::builder()
            .method("POST")
            .uri("/ngsi-ld/v1/entities")
            .header("Content-Type", "application/json")
            .header("Accept-Encoding", "gzip, br")
            .body(Body::from("{}"))
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::LENGTH_REQUIRED);
    assert!(body.len() <= 32, "{body:?}");
    assert!(headers.get("content-encoding").is_none(), "{headers:?}");
}

/// No request decompression: a gzip body is not unpacked past the body
/// bound, so it is refused and nothing is stored.
#[tokio::test(flavor = "multi_thread")]
async fn a_gzip_request_body_is_refused() {
    use std::io::Write;
    let st = AppState::new("test".into());
    let entity = br#"{"id":"urn:ngsi-ld:Kpi:z","type":"Kpi"}"#;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(entity).expect("gzip");
    let packed = enc.finish().expect("gzip");
    let (status, _, body) = send(
        &st,
        Request::builder()
            .method("POST")
            .uri("/ngsi-ld/v1/entities")
            .header("Content-Type", "application/json")
            .header("Content-Encoding", "gzip")
            .header("Content-Length", packed.len())
            .body(Body::from(packed))
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    let (status, _, _) = send(&st, get("/ngsi-ld/v1/entities/urn:ngsi-ld:Kpi:z", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
