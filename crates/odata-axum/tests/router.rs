// SPDX-License-Identifier: EUPL-1.2
//! The service document and `$metadata` over HTTP (T-3506, ADR-0024 decisions 4 and 5): content
//! negotiation, version negotiation, the caller's limits, and the refusals of T-3441 row Q27.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing;
use axum::Router;
use http_body_util::BodyExt;
use odata_axum::{metadata_router, ODataRequest, ODataState};
use odata_csdl::{
    CsdlVersion, EntityContainer, EntitySet, EntityType, Model, Property, Schema, TypeRef,
};
use tower::ServiceExt;

const ROOT: &str = "https://example.org/odata/";

fn model() -> Model {
    let mut schema = Schema::new("City");
    let mut building = EntityType::new("Building");
    building.key.push("id".into());
    building
        .properties
        .push(Property::new("id", TypeRef::single("Edm.String")).required());
    building
        .properties
        .push(Property::new("height", TypeRef::single("Edm.Int32")));
    schema.entity_types.push(building);
    let mut container = EntityContainer::new("Service");
    container
        .entity_sets
        .push(EntitySet::new("Buildings", "City.Building"));
    schema.entity_container = Some(container);
    Model {
        version: CsdlVersion::V4_01,
        references: vec![],
        schemas: vec![schema],
    }
}

fn state() -> ODataState {
    ODataState {
        model: Arc::new(model()),
        service_root: ROOT.into(),
        url_limits: odata_url::Limits::strict(),
        csdl_limits: odata_csdl::Limits::strict(),
        max_header_len: 64,
    }
}

fn app(state: ODataState) -> Router {
    Router::new().nest_service("/odata", metadata_router(Arc::new(state)))
}

struct Answer {
    status: StatusCode,
    content_type: String,
    version: Option<String>,
    body: String,
}

async fn get_with(app: Router, uri: &str, headers: &[(&str, &str)]) -> Result<Answer, String> {
    let mut request = Request::get(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request.body(Body::empty()).map_err(|e| e.to_string())?;
    let response = app.oneshot(request).await.map_err(|e| e.to_string())?;
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let (status, content_type, version) = (
        response.status(),
        header("content-type").unwrap_or_default(),
        header("odata-version"),
    );
    let bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    Ok(Answer {
        status,
        content_type,
        version,
        body: String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())?,
    })
}

async fn get(uri: &str, headers: &[(&str, &str)]) -> Result<Answer, String> {
    get_with(app(state()), uri, headers).await
}

const V401: (&str, &str) = ("OData-MaxVersion", "4.01");

#[tokio::test]
async fn the_service_document_lists_the_sets_in_json_and_names_its_version() -> Result<(), String> {
    let answer = get("/odata/", &[]).await?;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.version.as_deref(), Some("4.0"));
    assert_eq!(
        answer.content_type,
        "application/json;odata.metadata=minimal"
    );
    assert_eq!(
        answer.body,
        r#"{"@odata.context":"https://example.org/odata/$metadata","value":[{"name":"Buildings","kind":"EntitySet","url":"Buildings"}]}"#
    );

    // The ABNF gives the service root no query (`odataRelativeUri`): `?$format` there is 400.
    let answer = get("/odata/?$format=json", &[]).await?;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);

    let answer = get("/odata", &[V401]).await?;
    assert_eq!(answer.status, StatusCode::OK);
    let answer = get("/odata/", &[V401]).await?;
    assert_eq!(answer.version.as_deref(), Some("4.01"));
    assert_eq!(answer.content_type, "application/json;metadata=minimal");
    assert!(
        answer.body.starts_with(r#"{"@context":"#),
        "{}",
        answer.body
    );
    Ok(())
}

#[tokio::test]
async fn the_service_document_takes_its_metadata_level_and_refuses_other_formats(
) -> Result<(), String> {
    let full = get(
        "/odata/",
        &[V401, ("Accept", "application/json;metadata=full")],
    )
    .await?;
    assert_eq!(full.status, StatusCode::OK);
    assert_eq!(full.content_type, "application/json;metadata=full");

    for (headers, content_type) in [
        (
            vec![("Accept", "application/json;metadata=none")],
            "application/json;odata.metadata=none",
        ),
        (
            vec![("Accept", "application/json;odata.metadata=full")],
            "application/json;odata.metadata=full",
        ),
        (
            vec![V401, ("Accept", "application/json;odata.metadata=none")],
            "application/json;metadata=none",
        ),
        // A parameter without a value and an empty range are skipped, not refused.
        (
            vec![("Accept", "application/json;charset, ,")],
            "application/json;odata.metadata=minimal",
        ),
        (
            vec![("Accept", "text/html;q=0.9, */*;q=0.1")],
            "application/json;odata.metadata=minimal",
        ),
        (
            vec![("Accept", "application/*")],
            "application/json;odata.metadata=minimal",
        ),
    ] {
        let answer = get("/odata/", &headers).await?;
        assert_eq!(answer.status, StatusCode::OK, "{headers:?}");
        assert_eq!(answer.content_type, content_type, "{headers:?}");
    }

    for (uri, headers) in [
        ("/odata/", vec![("Accept", "text/html")]),
        ("/odata/", vec![("Accept", "application/xml")]),
        (
            "/odata/",
            vec![("Accept", "application/json;metadata=most")],
        ),
    ] {
        let answer = get(uri, &headers).await?;
        assert_eq!(
            answer.status,
            StatusCode::NOT_ACCEPTABLE,
            "{uri} {headers:?}"
        );
        assert!(
            answer.body.contains(r#""code":"NotAcceptable""#),
            "{}",
            answer.body
        );
    }
    Ok(())
}

#[tokio::test]
async fn metadata_is_csdl_xml_unless_json_is_asked() -> Result<(), String> {
    for headers in [
        vec![],
        vec![("Accept", "*/*")],
        vec![("Accept", "application/*")],
    ] {
        let answer = get("/odata/$metadata", &headers).await?;
        assert_eq!(answer.status, StatusCode::OK, "{headers:?}");
        assert_eq!(answer.content_type, "application/xml");
        assert!(answer.body.starts_with("<?xml"), "{}", answer.body);
        assert!(
            answer.body.contains(r#"<EntitySet Name="Buildings""#),
            "{}",
            answer.body
        );
    }
    Ok(())
}

#[tokio::test]
async fn metadata_is_csdl_json_on_4_01_with_format_or_accept() -> Result<(), String> {
    for (uri, headers) in [
        ("/odata/$metadata?$format=json", vec![V401]),
        ("/odata/$metadata?$format=application/json", vec![V401]),
        (
            "/odata/$metadata",
            vec![V401, ("Accept", "application/json")],
        ),
        // $format overrides Accept (Protocol 4.01 8.2.1).
        (
            "/odata/$metadata?$format=json",
            vec![V401, ("Accept", "application/xml")],
        ),
        // By quality, not by position.
        (
            "/odata/$metadata",
            vec![
                V401,
                ("Accept", "application/xml;q=0.4, application/json;q=0.9"),
            ],
        ),
        (
            "/odata/$metadata",
            vec![("OData-Version", "4.01"), ("Accept", "application/json")],
        ),
    ] {
        let answer = get(uri, &headers).await?;
        assert_eq!(answer.status, StatusCode::OK, "{uri} {headers:?}");
        assert_eq!(answer.content_type, "application/json");
        assert_eq!(answer.version.as_deref(), Some("4.01"));
        assert!(
            answer.body.starts_with("{\n  \"$Version\": \"4.01\""),
            "{}",
            answer.body
        );
    }

    let xml = get(
        "/odata/$metadata",
        &[V401, ("Accept", "application/json;q=0.2, application/xml")],
    )
    .await?;
    assert_eq!(xml.content_type, "application/xml");
    let xml = get(
        "/odata/$metadata?$format=xml",
        &[V401, ("Accept", "application/json")],
    )
    .await?;
    assert_eq!(xml.content_type, "application/xml");
    Ok(())
}

#[tokio::test]
async fn json_metadata_asked_on_4_0_is_406_and_never_a_document_it_cannot_read(
) -> Result<(), String> {
    for headers in [
        vec![],
        vec![("OData-MaxVersion", "4.0")],
        vec![("OData-Version", "4.01"), ("OData-MaxVersion", "4.0")],
    ] {
        let answer = get("/odata/$metadata?$format=json", &headers).await?;
        assert_eq!(answer.status, StatusCode::NOT_ACCEPTABLE, "{headers:?}");
        assert_eq!(answer.version.as_deref(), Some("4.0"));
        assert!(!answer.body.contains("$Version"), "{}", answer.body);
    }
    Ok(())
}

#[tokio::test]
async fn an_unacceptable_accept_or_format_is_406() -> Result<(), String> {
    for (uri, headers) in [
        ("/odata/$metadata", vec![("Accept", "text/html")]),
        ("/odata/$metadata", vec![("Accept", "application/xml;q=0")]),
        ("/odata/$metadata?$format=atom", vec![]),
        ("/odata/$metadata?$format=text/plain", vec![]),
    ] {
        let answer = get(uri, &headers).await?;
        assert_eq!(
            answer.status,
            StatusCode::NOT_ACCEPTABLE,
            "{uri} {headers:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_version_or_header_this_service_cannot_read_is_400() -> Result<(), String> {
    let long = "4".repeat(65);
    for headers in [
        vec![("OData-MaxVersion", "3.0")],
        vec![("OData-Version", "5.0")],
        vec![("OData-Version", "four")],
        vec![("OData-MaxVersion", long.as_str())],
        vec![("Accept", "application/json;q=2")],
        vec![("Accept", long.as_str())],
    ] {
        let answer = get("/odata/$metadata", &headers).await?;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{headers:?}");
        assert!(
            answer.body.contains(r#""code":"BadRequest""#),
            "{}",
            answer.body
        );
        assert!(!answer.body.contains(&long), "no request value is echoed");
    }
    Ok(())
}

#[tokio::test]
async fn unknown_system_and_custom_query_options_are_400_and_not_echoed() -> Result<(), String> {
    for uri in [
        "/odata/$metadata?$foo=1",
        "/odata/$metadata?foo=1",
        "/odata/?foo=1",
        "/odata/$metadata?$top=1",
    ] {
        let answer = get(uri, &[V401]).await?;
        assert_eq!(
            answer.status,
            StatusCode::BAD_REQUEST,
            "{uri}: {}",
            answer.body
        );
        assert_eq!(answer.version.as_deref(), Some("4.01"));
        assert!(!answer.body.contains("foo"), "{}", answer.body);
        assert!(!answer.body.contains("innererror"), "{}", answer.body);
    }
    Ok(())
}

#[tokio::test]
async fn the_callers_limits_hold_on_every_request() -> Result<(), String> {
    let short = ODataState {
        url_limits: odata_url::Limits {
            max_query_len: 4,
            ..odata_url::Limits::strict()
        },
        ..state()
    };
    let answer = get_with(app(short), "/odata/$metadata?$format=json", &[V401]).await?;
    assert_eq!(answer.status, StatusCode::URI_TOO_LONG);
    assert!(
        answer.body.contains(r#""code":"URITooLong""#),
        "{}",
        answer.body
    );

    let small = ODataState {
        csdl_limits: odata_csdl::Limits {
            max_elements: 1,
            ..odata_csdl::Limits::strict()
        },
        ..state()
    };
    for format in ["xml", "json"] {
        let answer = get_with(
            app(small.clone()),
            &format!("/odata/$metadata?$format={format}"),
            &[V401],
        )
        .await?;
        assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR, "{format}");
        assert_eq!(
            answer.body,
            r#"{"error":{"code":"InternalError","message":"the service model could not be written"}}"#
        );
    }
    Ok(())
}

async fn echo(request: ODataRequest) -> String {
    format!("{} {}", request.version, request.uri)
}

/// The extractor a surface adds its own routes with: the URL relative to the nest, parsed
/// against the model, every refusal an OData error with its status.
#[tokio::test]
async fn the_extractor_parses_below_the_service_root_and_answers_refusals_with_their_status(
) -> Result<(), String> {
    let routes = Router::new()
        .route("/{*rest}", routing::get(echo))
        .with_state(Arc::new(state()));
    let app = || Router::new().nest("/odata", routes.clone());

    let answer = get_with(app(), "/odata/Buildings('b1')?$top=2", &[V401]).await?;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, "4.01 Buildings('b1')?$top=2");

    for (uri, status) in [
        ("/odata/Towers", StatusCode::NOT_FOUND),
        (
            "/odata/Buildings?$filter=width%20eq%201",
            StatusCode::BAD_REQUEST,
        ),
        ("/odata/Buildings?$search=blue", StatusCode::NOT_IMPLEMENTED),
        ("/odata/Buildings?$top=1&$top=2", StatusCode::BAD_REQUEST),
        ("/odata/Buildings?$top=x", StatusCode::BAD_REQUEST),
        ("/odata/Buildings?mine=yes", StatusCode::BAD_REQUEST),
    ] {
        let answer = get_with(app(), uri, &[]).await?;
        assert_eq!(answer.status, status, "{uri}: {}", answer.body);
        assert_eq!(answer.content_type, "application/json");
        assert!(!answer.body.contains("Towers") && !answer.body.contains("width"));
    }

    let deep = ODataState {
        url_limits: odata_url::Limits {
            max_orderby_items: 1,
            ..odata_url::Limits::strict()
        },
        ..state()
    };
    let routes = Router::new()
        .route("/{*rest}", routing::get(echo))
        .with_state(Arc::new(deep));
    let answer = get_with(
        Router::new().nest("/odata", routes),
        "/odata/Buildings?$orderby=height,id",
        &[],
    )
    .await?;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert!(
        answer.body.contains(r#""code":"LimitExceeded""#),
        "{}",
        answer.body
    );
    Ok(())
}
