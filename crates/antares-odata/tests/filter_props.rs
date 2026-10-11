// SPDX-License-Identifier: EUPL-1.2
//! `$filter` against a reference (odata-linkml-mapping 5.2): random
//! filters of the served subset select, through the surface and the
//! broker's `q`, exactly the Entities an OData evaluator written here
//! selects. An absent member is OData `null`: a comparison with it is
//! false except `ne`, which holds (F3), and `not` of a string function on
//! it stays `null`, so the Entity is not selected.

use antares_ops::test_kit::TestBroker;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use tower::ServiceExt;

const CITY: &str = "https://example.org/city/";

const SCHEMA: &str = r#"{
  "id": "https://example.org/city", "name": "city",
  "prefixes": {"city": {"prefix_prefix": "city", "prefix_reference": "https://example.org/city/"}},
  "default_prefix": "city",
  "classes": {"Car": {"slots": ["speed", "label", "on"]}},
  "slots": {"speed": {"range": "float"}, "label": {"range": "string"}, "on": {"range": "boolean"}}
}"#;

/// One Car of the fixture; `None` is an absent Attribute.
struct Car {
    speed: Option<f64>,
    label: Option<&'static str>,
    on: Option<bool>,
}

const CARS: &[Car] = &[
    Car {
        speed: Some(5.0),
        label: Some("a.b"),
        on: Some(true),
    },
    Car {
        speed: Some(10.5),
        label: Some("ab\""),
        on: Some(false),
    },
    Car {
        speed: Some(30.0),
        label: None,
        on: Some(true),
    },
    Car {
        speed: None,
        label: Some("b"),
        on: None,
    },
    Car {
        speed: Some(-1.0),
        label: Some("."),
        on: Some(false),
    },
    Car {
        speed: Some(0.0),
        label: Some("\"a"),
        on: Some(true),
    },
    Car {
        speed: None,
        label: None,
        on: None,
    },
];

const NUMBERS: &[f64] = &[-1.0, 0.0, 5.0, 10.5, 30.0];
const OPS: &[&str] = &["eq", "ne", "gt", "ge", "lt", "le"];
const FUNCTIONS: &[&str] = &["contains", "startswith", "endswith"];

/// A filter of the served subset.
#[derive(Debug, Clone)]
enum F {
    Number(&'static str, f64),
    Null(bool),
    In(Vec<f64>),
    Label(bool, String),
    Function(&'static str, String, bool),
    On,
    OnIs(bool),
    And(Box<F>, Box<F>),
    Or(Box<F>, Box<F>),
}

impl F {
    fn text(&self) -> String {
        match self {
            Self::Number(op, n) => format!("speed {op} {n}"),
            Self::Null(eq) => format!("label {} null", if *eq { "eq" } else { "ne" }),
            Self::In(list) => {
                let list: Vec<String> = list.iter().map(f64::to_string).collect();
                format!("speed in ({})", list.join(","))
            }
            Self::Label(eq, s) => format!("label {} '{s}'", if *eq { "eq" } else { "ne" }),
            Self::Function(name, s, negated) => {
                let call = format!("{name}(label,'{s}')");
                if *negated {
                    format!("not {call}")
                } else {
                    call
                }
            }
            Self::On => "on".to_owned(),
            Self::OnIs(b) => format!("on eq {b}"),
            Self::And(l, r) => format!("({}) and ({})", l.text(), r.text()),
            Self::Or(l, r) => format!("({}) or ({})", l.text(), r.text()),
        }
    }

    /// The OData answer for `car`; `null` selects nothing.
    fn holds(&self, car: &Car) -> bool {
        match self {
            Self::Number(op, n) => car.speed.map_or(*op == "ne", |s| match *op {
                "eq" => (s - n).abs() < f64::EPSILON,
                "ne" => (s - n).abs() >= f64::EPSILON,
                "gt" => s > *n,
                "ge" => s >= *n,
                "lt" => s < *n,
                _ => s <= *n,
            }),
            Self::Null(eq) => car.label.is_none() == *eq,
            Self::In(list) => car
                .speed
                .is_some_and(|s| list.iter().any(|n| (s - n).abs() < f64::EPSILON)),
            Self::Label(eq, s) => car.label.map_or(!*eq, |l| (l == s) == *eq),
            Self::Function(name, s, negated) => car.label.is_some_and(|l| {
                let found = match *name {
                    "contains" => l.contains(s.as_str()),
                    "startswith" => l.starts_with(s.as_str()),
                    _ => l.ends_with(s.as_str()),
                };
                found != *negated
            }),
            Self::On => car.on == Some(true),
            Self::OnIs(b) => car.on == Some(*b),
            Self::And(l, r) => l.holds(car) && r.holds(car),
            Self::Or(l, r) => l.holds(car) || r.holds(car),
        }
    }
}

fn atom() -> impl Strategy<Value = F> {
    let number = || prop::sample::select(NUMBERS);
    let text = || {
        prop::collection::vec(prop::sample::select(&['a', 'b', '.', '"'][..]), 1..3)
            .prop_map(|c| c.into_iter().collect::<String>())
    };
    prop_oneof![
        (prop::sample::select(OPS), number()).prop_map(|(op, n)| F::Number(op, n)),
        any::<bool>().prop_map(F::Null),
        prop::collection::vec(number(), 1..3).prop_map(F::In),
        (any::<bool>(), text()).prop_map(|(eq, s)| F::Label(eq, s)),
        (prop::sample::select(FUNCTIONS), text(), any::<bool>())
            .prop_map(|(name, s, negated)| F::Function(name, s, negated)),
        Just(F::On),
        any::<bool>().prop_map(F::OnIs),
    ]
}

fn filter() -> impl Strategy<Value = F> {
    atom().prop_recursive(3, 8, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(l, r)| F::And(Box::new(l), Box::new(r))),
            (inner.clone(), inner).prop_map(|(l, r)| F::Or(Box::new(l), Box::new(r))),
        ]
    })
}

/// Percent-encodes all but RFC 3986 unreserved characters and `'(),`.
fn encode(raw: &str) -> String {
    use std::fmt::Write as _;
    raw.bytes().fold(String::new(), |mut out, b| {
        if b.is_ascii_alphanumeric() || b"-._~'(),".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
        out
    })
}

async fn put(app: &Router, method: &str, uri: &str, body: String) -> Result<(), String> {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("NGSILD-Tenant", "city")
        .header("Content-Type", "application/json")
        .header("Content-Length", body.len())
        .body(Body::from(body))
        .map_err(|e| e.to_string())?;
    let Ok(resp) = app.clone().oneshot(req).await;
    if resp.status() == StatusCode::CREATED {
        Ok(())
    } else {
        Err(format!("{uri}: {}", resp.status()))
    }
}

async fn broker() -> Result<(TestBroker, Router), String> {
    std::env::set_var("ANTARES_SCHEMA_MAX_BYTES", "65536");
    let b = TestBroker::new().await?;
    let app = b.serve(antares_odata::OData)?;
    let view = json!({
        "id": "cars", "tenant": "city", "@context": {"city": CITY},
        "selector": ["city:Car"],
        "rules": [{"types": ["city:Car"], "attributes": ["city:speed", "city:label", "city:on"]}],
        "operations": ["query_entities", "retrieve_entity"]
    });
    put(&app, "PUT", "/q/views/cars", view.to_string()).await?;
    put(&app, "PUT", "/q/schemas/city", SCHEMA.to_owned()).await?;
    for (n, car) in CARS.iter().enumerate() {
        let mut doc = Map::new();
        doc.insert("id".into(), json!(format!("urn:ngsi-ld:Car:{n}")));
        doc.insert("type".into(), json!(format!("{CITY}Car")));
        let values = [
            ("speed", car.speed.map(Value::from)),
            ("label", car.label.map(Value::from)),
            ("on", car.on.map(Value::from)),
        ];
        for (name, value) in values {
            if let Some(value) = value {
                doc.insert(
                    format!("{CITY}{name}"),
                    json!({"type": "Property", "value": value}),
                );
            }
        }
        put(
            &app,
            "POST",
            "/ngsi-ld/v1/entities",
            Value::Object(doc).to_string(),
        )
        .await?;
    }
    Ok((b, app))
}

/// The ids the surface selects for `filter`, or the status it refused with.
async fn selected(app: &Router, filter: &str) -> Result<BTreeSet<String>, String> {
    let uri = format!(
        "/x/views/cars/odata/v4/Car?$top=1000&$filter={}",
        encode(filter)
    );
    let req = Request::get(uri)
        .body(Body::empty())
        .map_err(|e| e.to_string())?;
    let Ok(resp) = app.clone().oneshot(req).await;
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .unwrap_or_default();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if status != StatusCode::OK {
        return Err(format!("{status} {body}"));
    }
    Ok(body
        .get("value")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect())
}

#[test]
fn odata_filter_translation_agrees_with_reference_eval() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let (_b, app) = rt.block_on(broker()).expect("broker");
    let mut runner = TestRunner::new(Config {
        cases: 192,
        failure_persistence: None,
        ..Config::default()
    });
    let result = runner.run(&filter(), |f| {
        let text = f.text();
        let want: BTreeSet<String> = CARS
            .iter()
            .enumerate()
            .filter(|(_, car)| f.holds(car))
            .map(|(n, _)| format!("urn:ngsi-ld:Car:{n}"))
            .collect();
        let got = rt.block_on(selected(&app, &text));
        prop_assert_eq!(got, Ok(want), "{}", text);
        Ok(())
    });
    assert!(result.is_ok(), "{result:?}");
}
