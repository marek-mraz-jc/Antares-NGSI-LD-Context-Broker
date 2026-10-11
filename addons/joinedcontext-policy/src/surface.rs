// SPDX-License-Identifier: EUPL-1.2
//! The access surface, `/x/views/{view}/access` (README section 4, plan
//! section 5.6): the caller's grants and the `AuthZEN` check, answered from
//! the table the engine decides with.

use crate::access::{authzen, check, Grants};
use crate::subject::Caller as JcCaller;
use crate::{odrl, ucast, JoinedcontextPolicy};
use antares_api::policy::Subject;
use antares_ops::{Caller, OperationId, Ops, Refusal, Surface, ViewOps};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path};
use axum::http::header::{ACCEPT, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::Value;
use std::sync::Arc;

/// The name `ANTARES_API_SURFACES` selects it by.
pub const NAME: &str = "access";

/// The largest `access/check` body read.
const MAX_CHECK_BYTES: usize = 64 * 1024;

/// The surface over one engine: the one the broker decides with.
pub struct Access {
    engine: Arc<JoinedcontextPolicy>,
}

impl Access {
    /// The surface over `engine`.
    #[must_use]
    pub fn new(engine: Arc<JoinedcontextPolicy>) -> Self {
        Self { engine }
    }
}

impl Surface for Access {
    fn name(&self) -> &'static str {
        NAME
    }

    fn prefix(&self) -> &'static str {
        "/x/views"
    }

    fn version_info(&self) -> Value {
        serde_json::json!({"routes": ["/x/views/{view}/access", "/x/views/{view}/access/check"]})
    }

    fn router(&self, ops: Ops) -> Router {
        let (ops_get, engine_get) = (ops.clone(), self.engine.clone());
        let (ops_check, engine_check) = (ops, self.engine.clone());
        Router::new()
            .route(
                "/{view}/access",
                get(
                    move |Path(view): Path<String>, caller: Caller, headers: HeaderMap| async move {
                        document(&ops_get, &engine_get, &view, &caller, &headers)
                    },
                ),
            )
            .route(
                "/{view}/access/check",
                post(
                    move |Path(view): Path<String>, caller: Caller, body: Bytes| async move {
                        decide(&ops_check, &engine_check, &view, &caller, &body)
                    },
                )
                .layer(DefaultBodyLimit::max(MAX_CHECK_BYTES)),
            )
    }
}

/// The platform caller the subject headers carry, read as the broker reads
/// them for the engine (`policy::subject_of`): visible ASCII values only.
fn caller_of(caller: &Caller) -> JcCaller {
    let headers = caller
        .subject()
        .into_iter()
        .filter_map(|(name, value)| {
            let value = HeaderValue::from_bytes(&value).ok()?;
            Some((name, value.to_str().ok()?.to_owned()))
        })
        .collect();
    JcCaller::of(&Subject {
        tenant: antares_model::TenantId::default(),
        headers,
    })
}

/// Whether the view serves the operation-matrix row `row`.
fn serves(view: &ViewOps) -> impl Fn(&str) -> bool + '_ {
    move |row| OperationId::from_id(row).is_some_and(|op| view.allows(op))
}

fn document(
    ops: &Ops,
    engine: &JoinedcontextPolicy,
    id: &str,
    caller: &Caller,
    headers: &HeaderMap,
) -> Response {
    let Some(view) = ops.view(id) else {
        return Refusal::NotFound.ngsi_ld();
    };
    let table = engine.table(view.tenant());
    let who = caller_of(caller);
    let grants = Grants::of(
        table.as_deref().map(|t| t.through(Some(id))),
        &who,
        chrono::Utc::now(),
    );
    let served = serves(&view);
    let accept = headers.get(ACCEPT).and_then(|v| v.to_str().ok());
    match representation(accept) {
        Representation::AuthZen => typed(
            &authzen(&who, id, &grants, &served).to_string(),
            "application/json",
        ),
        Representation::Odrl | Representation::Turtle => {
            let url = format!("{}/x/views/{id}/access", antares_ops::public_base(headers));
            let doc = odrl::policy(&who, &grants, &served, &url);
            if representation(accept) == Representation::Odrl {
                typed(&doc.to_string(), odrl::ODRL_JSON)
            } else {
                typed(&odrl::turtle(&doc), odrl::TURTLE)
            }
        }
        Representation::GrantAst => typed(
            &ucast::grant_ast(&grants, &served).to_string(),
            ucast::GRANT_AST_JSON,
        ),
    }
}

fn typed(body: &str, media_type: &'static str) -> Response {
    ([(CONTENT_TYPE, media_type)], body.to_owned()).into_response()
}

fn decide(
    ops: &Ops,
    engine: &JoinedcontextPolicy,
    id: &str,
    caller: &Caller,
    body: &[u8],
) -> Response {
    let Some(view) = ops.view(id) else {
        return Refusal::NotFound.ngsi_ld();
    };
    let Ok(query) = serde_json::from_slice::<Value>(body) else {
        return Refusal::Malformed("the request body is not JSON").ngsi_ld();
    };
    let Some(action) = query.pointer("/action/name").and_then(Value::as_str) else {
        return Refusal::Malformed("action.name is required").ngsi_ld();
    };
    let entity_type = query.pointer("/resource/type").and_then(Value::as_str);
    let table = engine.table(view.tenant());
    let who = caller_of(caller);
    let grants = Grants::of(
        table.as_deref().map(|t| t.through(Some(id))),
        &who,
        chrono::Utc::now(),
    );
    let answer = check(&grants, action, entity_type, &serves(&view));
    typed(&answer.to_string(), "application/json")
}

/// The representations `GET access` serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Representation {
    AuthZen,
    Odrl,
    Turtle,
    GrantAst,
}

/// The first representation `Accept` names that this surface serves, in
/// the caller's order; anything else is the `AuthZEN` document, never a 406.
fn representation(accept: Option<&str>) -> Representation {
    for offer in accept.unwrap_or_default().split(',') {
        match offer.split(';').next().unwrap_or_default().trim() {
            odrl::ODRL_JSON => return Representation::Odrl,
            odrl::TURTLE => return Representation::Turtle,
            ucast::GRANT_AST_JSON => return Representation::GrantAst,
            "application/json" | "application/ld+json" => return Representation::AuthZen,
            _ => {}
        }
    }
    Representation::AuthZen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_served_representation_wins_and_anything_else_is_authzen() {
        for (accept, want) in [
            (None, Representation::AuthZen),
            (
                Some("text/turtle, application/json"),
                Representation::Turtle,
            ),
            (
                Some("text/csv;q=1, application/odrl+json;q=0.5"),
                Representation::Odrl,
            ),
            (Some(ucast::GRANT_AST_JSON), Representation::GrantAst),
            (
                Some("application/ld+json, text/turtle"),
                Representation::AuthZen,
            ),
            (Some("*/*"), Representation::AuthZen),
            (Some("text/csv"), Representation::AuthZen),
        ] {
            assert_eq!(representation(accept), want, "{accept:?}");
        }
    }
}
