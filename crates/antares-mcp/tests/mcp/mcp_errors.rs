// SPDX-License-Identifier: EUPL-1.2
//! What a refusal says (MC18).

use crate::common::{call, fixture};
use serde_json::json;

/// MC18: a refused argument names the rule, a failed operation its status
/// and title; neither carries a hidden name, a value or an inner detail.
#[tokio::test(flavor = "multi_thread")]
async fn errors_carry_no_hidden_name_and_no_inner_detail() -> Result<(), String> {
    let f = fixture().await?;
    let hidden_attr = call(
        &f.app,
        "cars",
        "get_entity",
        json!({"id": "urn:ngsi-ld:Car:1", "attrs": ["secret"]}),
    )
    .await;
    assert!(hidden_attr.is_error(), "{}", hidden_attr.text);
    assert!(!hidden_attr.text.contains("secret"), "{}", hidden_attr.text);

    let bad_q = call(
        &f.app,
        "cars",
        "query_entities",
        json!({"type": ["Car"], "q": ")xyzzy("}),
    )
    .await;
    assert!(bad_q.is_error(), "{}", bad_q.text);
    assert!(bad_q.text.contains("400"), "{}", bad_q.text);
    assert!(!bad_q.text.contains("xyzzy"), "{}", bad_q.text);
    assert_eq!(
        bad_q.structured().pointer("/error/status"),
        Some(&json!(400)),
        "{}",
        bad_q.text
    );
    assert!(bad_q.structured().pointer("/error/detail").is_none());

    let out_of_view = call(
        &f.app,
        "cars",
        "get_entity",
        json!({"id": "urn:ngsi-ld:Car:3"}),
    )
    .await;
    let missing = call(
        &f.app,
        "cars",
        "get_entity",
        json!({"id": "urn:ngsi-ld:Car:404"}),
    )
    .await;
    assert!(
        out_of_view.is_error() && out_of_view.text.contains("404"),
        "{}",
        out_of_view.text
    );
    assert_eq!(out_of_view.text, missing.text);
    assert!(!out_of_view.text.contains("Car:3"), "{}", out_of_view.text);
    Ok(())
}
