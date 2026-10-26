// SPDX-License-Identifier: EUPL-1.2
//! One error mapping for every surface (surfaces-threat-model 10.3, ADR-0027
//! section 5): a [`Refusal`] says which condition held, and each surface
//! renders it in its own protocol. Nothing in a refusal carries a stored
//! value, a name the caller did not send, the view's predicate or the text of
//! an inner failure, so "hidden" and "absent" are the same bytes wherever the
//! table says "as absent".

use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};

/// Why an operation was not served, as every surface reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The Entity is out of the view or does not exist: one answer for both.
    NotFound,
    /// The view does not allow the operation; `allow` lists the methods it
    /// does allow on the same resource.
    NotAllowed {
        /// The methods the view allows on the resource.
        allow: Vec<Method>,
    },
    /// The view serves the method on the resource but not this operation
    /// of it (ADR-0023 rule 4: the method stays, 422).
    NotSupported,
    /// The policy engine refused, or a write would leave the view. The
    /// reason is the engine's fixed string (ADR-0020) or the view's own.
    Denied(String),
    /// The request is malformed; the text is fixed, never an echo.
    Malformed(&'static str),
    /// A bound was exceeded; the text names the bound, never a value.
    Limit(&'static str),
    /// The surface is closed for this view (rule M4).
    Closed,
    /// The operation failed inside the broker. Its own detail stays in the
    /// log under `request_id`; the caller gets the id alone.
    Inner {
        /// The id the log line carries.
        request_id: String,
    },
    /// A remote JSON-LD `@context` the request names could not be retrieved
    /// (TS 104 176 6.3.2, 504 `LdContextNotAvailable`). The URL is the
    /// caller's own; the fetch error stays in the broker, the detail is fixed.
    ContextNotAvailable,
    /// A count or a sort the store cannot apply needs more Entities read than one request may
    /// (ADR-0028 rule 5): 400, so the caller narrows the query.
    TooManyResults {
        /// The most Entities one request reads (`RESIDUAL_SCAN`).
        budget: usize,
        /// How many the query selects.
        size: usize,
    },
    /// The database cancelled the operation at its statement timeout. The
    /// detail is fixed and names no query, so it reaches the caller: the
    /// same request asks the same again, and the gateway answers it as a
    /// `504` that says to ask for less (T-5616).
    StatementTimeout,
    /// The broker is overloaded: try again after `retry_after` seconds.
    Overloaded {
        /// The `Retry-After` the broker answered with.
        retry_after: Option<HeaderValue>,
    },
}

/// The fixed detail of [`Refusal::ContextNotAvailable`].
pub const CONTEXT_DETAIL: &str = "a JSON-LD @context the request names could not be retrieved";

/// The fixed detail of [`Refusal::StatementTimeout`], the NGSI-LD API's own.
pub const TIMEOUT_DETAIL: &str = "database statement timeout";

/// The fixed detail of an inner failure.
pub const INNER_DETAIL: &str = "the operation failed inside the broker";

impl Refusal {
    /// The HTTP status this condition answers with on an HTTP surface.
    ///
    /// ```
    /// use antares_ops::Refusal;
    /// assert_eq!(Refusal::NotFound.status(), 404);
    /// assert_eq!(Refusal::Inner { request_id: "1".into() }.status(), 500);
    /// ```
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotFound | Self::Closed => StatusCode::NOT_FOUND,
            Self::NotAllowed { .. } => StatusCode::METHOD_NOT_ALLOWED,
            Self::NotSupported => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Denied(_) => StatusCode::FORBIDDEN,
            Self::Malformed(_) | Self::TooManyResults { .. } => StatusCode::BAD_REQUEST,
            Self::Limit(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Inner { .. } | Self::StatementTimeout => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ContextNotAvailable => StatusCode::GATEWAY_TIMEOUT,
            Self::Overloaded { .. } => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// The NGSI-LD view's rendering: a `ProblemDetails` body (TS 104 176
    /// 6.3.2, 6.3.3), with `Allow` on a 405 and `Retry-After` on a 503.
    ///
    /// ```
    /// use antares_ops::Refusal;
    /// let r = Refusal::Inner { request_id: "ops-1".into() }.ngsi_ld();
    /// assert_eq!(r.status(), 500);
    /// ```
    #[must_use]
    pub fn ngsi_ld(&self) -> Response {
        let (kind, title, detail) = match self {
            Self::NotFound | Self::Closed => (
                "https://uri.etsi.org/ngsi-ld/errors/ResourceNotFound",
                "ResourceNotFound",
                "the resource does not exist".to_owned(),
            ),
            Self::NotAllowed { .. } | Self::NotSupported => (
                "https://uri.etsi.org/ngsi-ld/errors/OperationNotSupported",
                "OperationNotSupported",
                "the operation is not allowed here".to_owned(),
            ),
            Self::Denied(why) => (
                "urn:antares:error:AccessDenied",
                "AccessDenied",
                why.clone(),
            ),
            Self::Malformed(why) | Self::Limit(why) => (
                "https://uri.etsi.org/ngsi-ld/errors/BadRequestData",
                "BadRequestData",
                (*why).to_owned(),
            ),
            Self::TooManyResults { budget, size } => (
                "https://uri.etsi.org/ngsi-ld/errors/TooManyResults",
                "TooManyResults",
                format!(
                    "the query selects {size} Entities and one request reads at most {budget}: narrow it"
                ),
            ),
            Self::Inner { request_id } => (
                "https://uri.etsi.org/ngsi-ld/errors/InternalError",
                "InternalError",
                format!("{INNER_DETAIL}; request id {request_id}"),
            ),
            Self::StatementTimeout => (
                "https://uri.etsi.org/ngsi-ld/errors/InternalError",
                "InternalError",
                TIMEOUT_DETAIL.to_owned(),
            ),
            Self::ContextNotAvailable => (
                "https://uri.etsi.org/ngsi-ld/errors/LdContextNotAvailable",
                "LdContextNotAvailable",
                CONTEXT_DETAIL.to_owned(),
            ),
            Self::Overloaded { retry_after } => {
                let mut r = StatusCode::SERVICE_UNAVAILABLE.into_response();
                if let Some(after) = retry_after {
                    r.headers_mut().insert(header::RETRY_AFTER, after.clone());
                }
                return r;
            }
        };
        let body = serde_json::json!({
            "type": kind,
            "title": title,
            "status": self.status().as_u16(),
            "detail": detail,
        });
        let mut r = (
            self.status(),
            [(header::CONTENT_TYPE, "application/json")],
            body.to_string(),
        )
            .into_response();
        if let Self::NotAllowed { allow } = self {
            let methods: Vec<&str> = allow.iter().map(Method::as_str).collect();
            // Method names are tokens, so the join is always a header value.
            let allow = HeaderValue::from_str(&methods.join(", ")).ok();
            r.headers_mut().extend(allow.map(|v| (header::ALLOW, v)));
        }
        r
    }
}
