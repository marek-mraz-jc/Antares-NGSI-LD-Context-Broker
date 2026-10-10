// SPDX-License-Identifier: EUPL-1.2
//! The read tools (MCP 3.5, rows 15 to 18, 21, 22 and 24 to 32): each is
//! one row of the operation matrix, and a call is one NGSI-LD request of
//! that operation, built from checked arguments only. Write tools come with
//! T-3514.

use crate::schema::{opt, req, Arg, Args, Checked, Kind};
use antares_ops::OperationId;
use axum::http::Method;
use serde_json::{json, Value};

/// What a tool answers, and whether it pages.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolDef {
    pub name: &'static str,
    pub op: OperationId,
    pub description: &'static str,
    pub args: &'static [Arg],
    /// The member of the structured result the answer goes under.
    pub key: &'static str,
    /// Whether the answer is a list paged with `limit` and `cursor`.
    pub paged: bool,
}

const GEOMETRIES: &[&str] = &[
    "Point",
    "MultiPoint",
    "LineString",
    "MultiLineString",
    "Polygon",
    "MultiPolygon",
];
const JOINS: &[&str] = &["flat", "inline"];
const TIMERELS: &[&str] = &["before", "after", "between"];
/// The instant a history is ordered by (TS 104 175 4.11); `observedAt`
/// when absent, so data without it needs another.
const TIMEPROPERTIES: &[&str] = &["observedAt", "createdAt", "modifiedAt", "deletedAt"];

const GET_ENTITY: &[Arg] = &[
    req("id", Kind::Id),
    opt("attrs", Kind::Attrs),
    opt("pick", Kind::Attrs),
    opt("omit", Kind::Attrs),
    opt("lang", Kind::Text(64)),
    opt("join", Kind::Word(JOINS)),
    opt("joinLevel", Kind::Int(1, 3)),
];

const QUERY: &[Arg] = &[
    opt("type", Kind::Types),
    opt("id", Kind::Ids),
    opt("idPattern", Kind::Text(256)),
    opt("attrs", Kind::Attrs),
    opt("pick", Kind::Attrs),
    opt("omit", Kind::Attrs),
    opt("q", Kind::Text(4096)),
    opt("scopeQ", Kind::Text(1024)),
    opt("georel", Kind::Text(256)),
    opt("geometry", Kind::Word(GEOMETRIES)),
    opt("coordinates", Kind::Coordinates),
    opt("geoproperty", Kind::Attr),
    opt("local", Kind::Flag),
    opt("lang", Kind::Text(64)),
    opt("join", Kind::Word(JOINS)),
    opt("joinLevel", Kind::Int(1, 3)),
    opt("count", Kind::Flag),
    opt("limit", Kind::Int(1, 1000)),
    opt("cursor", Kind::Text(crate::cursor::MAX_LEN)),
];

const BATCH_QUERY: &[Arg] = &[
    req("type", Kind::Types),
    opt("id", Kind::Ids),
    opt("attrs", Kind::Attrs),
    opt("pick", Kind::Attrs),
    opt("omit", Kind::Attrs),
    opt("q", Kind::Text(4096)),
    opt("scopeQ", Kind::Text(1024)),
    opt("georel", Kind::Text(256)),
    opt("geometry", Kind::Word(GEOMETRIES)),
    opt("coordinates", Kind::Coordinates),
    opt("geoproperty", Kind::Attr),
    opt("local", Kind::Flag),
    opt("lang", Kind::Text(64)),
    opt("count", Kind::Flag),
    opt("limit", Kind::Int(1, 1000)),
    opt("cursor", Kind::Text(crate::cursor::MAX_LEN)),
];

const TEMPORAL_ONE: &[Arg] = &[
    req("id", Kind::Id),
    opt("attrs", Kind::Attrs),
    req("timerel", Kind::Word(TIMERELS)),
    req("timeAt", Kind::Text(64)),
    opt("endTimeAt", Kind::Text(64)),
    opt("timeproperty", Kind::Word(TIMEPROPERTIES)),
    opt("lastN", Kind::Int(1, 100)),
    opt("aggrMethods", Kind::Text(256)),
    opt("aggrPeriodDuration", Kind::Text(64)),
];

const TEMPORAL_QUERY: &[Arg] = &[
    opt("type", Kind::Types),
    opt("id", Kind::Ids),
    opt("attrs", Kind::Attrs),
    opt("q", Kind::Text(4096)),
    opt("scopeQ", Kind::Text(1024)),
    opt("georel", Kind::Text(256)),
    opt("geometry", Kind::Word(GEOMETRIES)),
    opt("coordinates", Kind::Coordinates),
    opt("geoproperty", Kind::Attr),
    req("timerel", Kind::Word(TIMERELS)),
    req("timeAt", Kind::Text(64)),
    opt("endTimeAt", Kind::Text(64)),
    opt("timeproperty", Kind::Word(TIMEPROPERTIES)),
    opt("lastN", Kind::Int(1, 100)),
    opt("aggrMethods", Kind::Text(256)),
    opt("aggrPeriodDuration", Kind::Text(64)),
    opt("count", Kind::Flag),
    opt("limit", Kind::Int(1, 1000)),
    opt("cursor", Kind::Text(crate::cursor::MAX_LEN)),
];

const ID_ONLY: &[Arg] = &[req("id", Kind::Text(512))];
const PAGE_ONLY: &[Arg] = &[
    opt("count", Kind::Flag),
    opt("limit", Kind::Int(1, 1000)),
    opt("cursor", Kind::Text(crate::cursor::MAX_LEN)),
];
const NONE: &[Arg] = &[];
const ATTR_VALUE: &[Arg] = &[req("id", Kind::Id), req("attrId", Kind::Attr)];
const TYPE_ONLY: &[Arg] = &[req("type", Kind::Type)];
const ATTR_ONLY: &[Arg] = &[req("attrId", Kind::Attr)];

/// Every read tool, sorted by name (2026-07-28: a deterministic order).
pub(crate) const TOOLS: [ToolDef; 15] = [
    ToolDef {
        name: "batch_query",
        op: OperationId::QueryEntitiesPost,
        description: "Query Entities with a Query document (TS 104 175 10.4.3, POST form): the Entities of the given types that match the conditions, a page at a time.",
        args: BATCH_QUERY,
        key: "entities",
        paged: true,
    },
    ToolDef {
        name: "batch_query_temporal",
        op: OperationId::TemporalQueryPost,
        description: "Query the history of Entities with a Query document (TS 104 175 11.3.3, POST form), a page of Entities at a time.",
        args: TEMPORAL_QUERY,
        key: "entities",
        paged: true,
    },
    ToolDef {
        name: "get_attribute",
        op: OperationId::RetrieveAttributeInfo,
        description: "Describe one Attribute: the types that carry it and how many instances exist (TS 104 175 10.6.8).",
        args: ATTR_ONLY,
        key: "attribute",
        paged: false,
    },
    ToolDef {
        name: "get_attribute_value",
        op: OperationId::RetrieveAttributeValue,
        description: "Read one Attribute of one Entity (TS 104 175 10.4.4).",
        args: ATTR_VALUE,
        key: "attribute",
        paged: false,
    },
    ToolDef {
        name: "get_entity",
        op: OperationId::RetrieveEntity,
        description: "Read one Entity by its id (TS 104 175 10.4.2); attrs, pick and omit narrow what it returns.",
        args: GET_ENTITY,
        key: "entity",
        paged: false,
    },
    ToolDef {
        name: "get_subscription",
        op: OperationId::RetrieveSubscription,
        description: "Read one Subscription made through this endpoint (TS 104 175 10.5.4).",
        args: ID_ONLY,
        key: "subscription",
        paged: false,
    },
    ToolDef {
        name: "get_type",
        op: OperationId::RetrieveEntityTypeInfo,
        description: "Describe one Entity type: its Attributes and how many Entities it has (TS 104 175 10.6.5).",
        args: TYPE_ONLY,
        key: "entityType",
        paged: false,
    },
    ToolDef {
        name: "list_attribute_details",
        op: OperationId::RetrieveAttributeDetails,
        description: "List the Attributes with the types that carry each (TS 104 175 10.6.7).",
        args: NONE,
        key: "attributes",
        paged: false,
    },
    ToolDef {
        name: "list_attributes",
        op: OperationId::RetrieveAttributes,
        description: "List the names of the Attributes this endpoint serves (TS 104 175 10.6.6).",
        args: NONE,
        key: "attributes",
        paged: false,
    },
    ToolDef {
        name: "list_subscriptions",
        op: OperationId::QuerySubscriptions,
        description: "List the Subscriptions made through this endpoint (TS 104 175 10.5.5), a page at a time.",
        args: PAGE_ONLY,
        key: "subscriptions",
        paged: true,
    },
    ToolDef {
        name: "list_type_details",
        op: OperationId::RetrieveEntityTypeDetails,
        description: "List the Entity types with the Attributes of each (TS 104 175 10.6.4).",
        args: NONE,
        key: "entityTypes",
        paged: false,
    },
    ToolDef {
        name: "list_types",
        op: OperationId::RetrieveEntityTypes,
        description: "List the names of the Entity types this endpoint serves (TS 104 175 10.6.3).",
        args: NONE,
        key: "entityTypes",
        paged: false,
    },
    ToolDef {
        name: "query_entities",
        op: OperationId::QueryEntities,
        description: "Query Entities (TS 104 175 10.4.3): by type, id, q, scopeQ or area, a page at a time. Give at least one of type, attrs, q, georel or local.",
        args: QUERY,
        key: "entities",
        paged: true,
    },
    ToolDef {
        name: "query_temporal",
        op: OperationId::TemporalQuery,
        description: "Query the history of Entities (TS 104 175 11.3.3) between timeAt and endTimeAt, a page of Entities at a time; lastN bounds each Attribute's instances.",
        args: TEMPORAL_QUERY,
        key: "entities",
        paged: true,
    },
    ToolDef {
        name: "retrieve_temporal",
        op: OperationId::TemporalRetrieve,
        description: "Read the history of one Entity (TS 104 175 11.3.2) between timeAt and endTimeAt; lastN bounds each Attribute's instances.",
        args: TEMPORAL_ONE,
        key: "entity",
        paged: false,
    },
];

/// The tool named `name`.
pub(crate) fn named(name: &str) -> Option<&'static ToolDef> {
    TOOLS.iter().find(|t| t.name == name)
}

/// One NGSI-LD request: method, path and query below the API root, body.
#[derive(Debug, PartialEq)]
pub(crate) struct Built {
    pub method: Method,
    pub target: String,
    pub body: Option<Value>,
}

/// `s` as one URI component: unreserved characters and `:` stay, the rest
/// is percent-encoded.
pub(crate) fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~:".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn text(c: &Checked) -> String {
    match c {
        Checked::One(s) => s.clone(),
        Checked::Many(v) => v.join(","),
        Checked::Int(n) => n.to_string(),
        Checked::Flag(b) => b.to_string(),
        Checked::Json(v) => v.to_string(),
    }
}

fn one<'a>(args: &'a Args, name: &str) -> &'a str {
    match args.get(name) {
        Some(Checked::One(s)) => s,
        _ => "",
    }
}

/// The query string of `args` but those the tool handles itself.
fn query(args: &Args, skip: &[&str], extra: &[(&str, String)]) -> String {
    let mut pairs: Vec<String> = args
        .iter()
        .filter(|(k, _)| !skip.contains(k))
        .map(|(k, v)| format!("{k}={}", encode(&text(v))))
        .collect();
    pairs.extend(extra.iter().map(|(k, v)| format!("{k}={}", encode(v))));
    if pairs.is_empty() {
        String::new()
    } else {
        format!("?{}", pairs.join("&"))
    }
}

/// Members a page tool handles itself rather than passing on.
const PAGING: [&str; 3] = ["count", "limit", "cursor"];

/// The paging members of an inner request: `count=true` always, so the
/// next cursor knows whether rows are left.
fn page(limit: u64, offset: u64) -> [(&'static str, String); 3] {
    [
        ("limit", limit.to_string()),
        ("offset", offset.to_string()),
        ("count", "true".into()),
    ]
}

/// A Query document (5.2.23) of `args`.
fn query_document(args: &Args, temporal: bool) -> Value {
    let types = text(args.get("type").unwrap_or(&Checked::Many(Vec::new())));
    let entities: Vec<Value> = match args.get("id") {
        Some(Checked::Many(ids)) => ids
            .iter()
            .map(|id| json!({"id": id, "type": types}))
            .collect(),
        _ => vec![json!({"type": types})],
    };
    let mut doc = serde_json::Map::new();
    doc.insert("type".into(), json!("Query"));
    doc.insert("entities".into(), json!(entities));
    let (mut geo, mut time) = (serde_json::Map::new(), serde_json::Map::new());
    for (k, v) in args {
        let value = match v {
            Checked::Many(list) => json!(list),
            Checked::Json(j) => j.clone(),
            Checked::Int(n) => json!(n),
            Checked::Flag(b) => json!(b),
            Checked::One(s) => json!(s),
        };
        let into = match *k {
            "type" | "id" | "count" | "limit" | "cursor" => continue,
            "georel" | "geometry" | "coordinates" | "geoproperty" => &mut geo,
            "timerel" | "timeAt" | "endTimeAt" | "timeproperty" if temporal => &mut time,
            _ => &mut doc,
        };
        into.insert((*k).to_owned(), value);
    }
    for (key, part) in [("geoQ", geo), ("temporalQ", time)] {
        if !part.is_empty() {
            doc.insert(key.into(), Value::Object(part));
        }
    }
    Value::Object(doc)
}

/// The request `tool` makes of `args`, at `offset` with `limit` when it
/// pages.
pub(crate) fn build(tool: &ToolDef, args: &Args, limit: u64, offset: u64) -> Built {
    let get = |target: String| Built {
        method: Method::GET,
        target,
        body: None,
    };
    let root = "/ngsi-ld/v1";
    match tool.op {
        OperationId::RetrieveEntity => get(format!(
            "{root}/entities/{}{}",
            encode(one(args, "id")),
            query(args, &["id"], &[])
        )),
        OperationId::RetrieveAttributeValue => get(format!(
            "{root}/entities/{}/attrs/{}",
            encode(one(args, "id")),
            encode(one(args, "attrId"))
        )),
        OperationId::QueryEntities => get(format!(
            "{root}/entities{}",
            query(args, &PAGING, &page(limit, offset))
        )),
        OperationId::QueryEntitiesPost => Built {
            method: Method::POST,
            target: format!(
                "{root}/entityOperations/query{}",
                query(&Args::new(), &[], &page(limit, offset))
            ),
            body: Some(query_document(args, false)),
        },
        OperationId::RetrieveSubscription => {
            get(format!("{root}/subscriptions/{}", encode(one(args, "id"))))
        }
        OperationId::QuerySubscriptions => get(format!(
            "{root}/subscriptions{}",
            query(args, &PAGING, &page(limit, offset))
        )),
        OperationId::RetrieveEntityTypes => get(format!("{root}/types")),
        OperationId::RetrieveEntityTypeDetails => get(format!("{root}/types?details=true")),
        OperationId::RetrieveEntityTypeInfo => {
            get(format!("{root}/types/{}", encode(one(args, "type"))))
        }
        OperationId::RetrieveAttributes => get(format!("{root}/attributes")),
        OperationId::RetrieveAttributeDetails => get(format!("{root}/attributes?details=true")),
        OperationId::RetrieveAttributeInfo => {
            get(format!("{root}/attributes/{}", encode(one(args, "attrId"))))
        }
        OperationId::TemporalRetrieve => get(format!(
            "{root}/temporal/entities/{}{}",
            encode(one(args, "id")),
            query(args, &["id"], &[])
        )),
        OperationId::TemporalQueryPost => Built {
            method: Method::POST,
            target: format!(
                "{root}/temporal/entityOperations/query{}",
                query(&Args::new(), &[], &page(limit, offset))
            ),
            body: Some(query_document(args, true)),
        },
        // TemporalQuery, the only other operation of the table
        _ => get(format!(
            "{root}/temporal/entities{}",
            query(args, &PAGING, &page(limit, offset))
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&'static str, Checked)]) -> Args {
        pairs.iter().cloned().collect()
    }

    #[test]
    fn the_table_is_sorted_and_each_tool_is_its_own_row() {
        let names: Vec<&str> = TOOLS.iter().map(|t| t.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        let mut ops: Vec<&str> = TOOLS.iter().map(|t| t.op.id()).collect();
        ops.sort_unstable();
        ops.dedup();
        assert_eq!(ops.len(), TOOLS.len());
    }

    #[test]
    fn every_tool_builds_a_request_of_its_own_operation() {
        let all = args(&[
            ("id", Checked::One("urn:ngsi-ld:Car:1".into())),
            ("attrId", Checked::One("speed".into())),
            ("type", Checked::One("Car".into())),
            ("timerel", Checked::One("after".into())),
            ("timeAt", Checked::One("2020-01-01T00:00:00Z".into())),
        ]);
        for tool in &TOOLS {
            let b = build(tool, &all, 20, 0);
            let (path, q) = match b.target.split_once('?') {
                Some((p, q)) => (p, Some(q)),
                None => (b.target.as_str(), None),
            };
            assert_eq!(
                OperationId::of(b.method.as_str(), path, q),
                Some(tool.op),
                "{} built {} {}",
                tool.name,
                b.method,
                b.target
            );
        }
    }

    #[test]
    fn values_are_encoded_and_paging_is_the_tools() {
        let a = args(&[
            ("q", Checked::One("speed>1;name==\"a b\"".into())),
            ("type", Checked::Many(vec!["Car".into(), "Bike".into()])),
            ("limit", Checked::Int(5)),
            ("cursor", Checked::One("ab".into())),
        ]);
        let b = build(&TOOLS[12], &a, 5, 40);
        assert_eq!(
            b.target,
            "/ngsi-ld/v1/entities?q=speed%3E1%3Bname%3D%3D%22a%20b%22&type=Car%2CBike&limit=5&offset=40&count=true"
        );
        let one = args(&[("id", Checked::One("urn:ngsi-ld:Car:a/b?c".into()))]);
        assert_eq!(
            build(&TOOLS[4], &one, 0, 0).target,
            "/ngsi-ld/v1/entities/urn:ngsi-ld:Car:a%2Fb%3Fc"
        );
    }

    #[test]
    fn a_query_document_carries_the_conditions() {
        let a = args(&[
            ("type", Checked::Many(vec!["Car".into()])),
            ("id", Checked::Many(vec!["urn:ngsi-ld:Car:1".into()])),
            ("q", Checked::One("speed>1".into())),
            ("georel", Checked::One("near;maxDistance==10".into())),
            ("coordinates", Checked::Json(json!([1, 2]))),
            ("timerel", Checked::One("after".into())),
            ("limit", Checked::Int(3)),
            ("local", Checked::Flag(true)),
        ]);
        assert_eq!(
            query_document(&a, true),
            json!({
                "type": "Query",
                "entities": [{"id": "urn:ngsi-ld:Car:1", "type": "Car"}],
                "q": "speed>1",
                "local": true,
                "geoQ": {"georel": "near;maxDistance==10", "coordinates": [1, 2]},
                "temporalQ": {"timerel": "after"}
            })
        );
        assert_eq!(
            query_document(&args(&[("type", Checked::Many(vec!["Car".into()]))]), false),
            json!({"type": "Query", "entities": [{"type": "Car"}]})
        );
        assert_eq!(
            query_document(&args(&[("timerel", Checked::One("after".into()))]), false),
            json!({"type": "Query", "entities": [{"type": ""}], "timerel": "after"}),
            "outside a temporal query a time member is an ordinary one"
        );
    }

    #[test]
    fn every_kind_of_value_reaches_the_query_string() {
        let a = args(&[
            ("type", Checked::Many(vec!["Car".into()])),
            ("joinLevel", Checked::Int(2)),
            ("local", Checked::Flag(true)),
            ("coordinates", Checked::Json(json!([1, 2]))),
        ]);
        let q = named("query_entities").map(|t| build(t, &a, 5, 0).target);
        assert_eq!(
            q.as_deref(),
            Some("/ngsi-ld/v1/entities?coordinates=%5B1%2C2%5D&joinLevel=2&local=true&type=Car&limit=5&offset=0&count=true")
        );
        let e = named("get_entity").map(|t| build(t, &Args::new(), 0, 0).target);
        assert_eq!(
            e.as_deref(),
            Some("/ngsi-ld/v1/entities/"),
            "a missing path member is empty"
        );
    }
}
