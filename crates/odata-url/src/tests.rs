// SPDX-License-Identifier: EUPL-1.2
//! Unit tests: every rule of the subset, its errors, its limits and the canonical writer.

use super::*;

/// Buildings with an address, rooms, tags, sensors and an owner; sensors with a value.
struct Service;

impl Model for Service {
    fn entity_set(&self, name: &str) -> Option<&str> {
        match name {
            "Buildings" => Some("Building"),
            "Sensors" => Some("Sensor"),
            _ => None,
        }
    }

    fn member(&self, ty: &str, name: &str) -> Option<Member<'_>> {
        let primitive = Member::Primitive { collection: false };
        Some(match (ty, name) {
            ("Building", "id" | "height" | "name" | "built" | "location")
            | ("Address", "street" | "city")
            | ("Room", "area")
            | ("Sensor", "value" | "at")
            | ("Person", "name") => primitive,
            ("Building", "tags") => Member::Primitive { collection: true },
            ("Building", "address") => Member::Complex {
                ty: "Address",
                collection: false,
            },
            ("Building", "rooms") => Member::Complex {
                ty: "Room",
                collection: true,
            },
            ("Building" | "Address", "sensors") => Member::Navigation {
                ty: "Sensor",
                collection: true,
            },
            ("Building" | "Sensor", "owner") => Member::Navigation {
                ty: "Person",
                collection: false,
            },
            ("Sensor", "building") => Member::Navigation {
                ty: "Building",
                collection: false,
            },
            _ => return None,
        })
    }
}

fn ok(uri: &str) -> ODataUri {
    let parsed = parse(uri, &Service, &Limits::strict());
    assert!(parsed.is_ok(), "{uri}: {parsed:?}");
    parsed.unwrap_or(ODataUri::ServiceDocument)
}

fn err(uri: &str) -> UrlError {
    err_within(uri, &Limits::strict())
}

fn err_within(uri: &str, limits: &Limits) -> UrlError {
    let parsed = parse(uri, &Service, limits);
    assert!(parsed.is_err(), "{uri}: {parsed:?}");
    parsed.err().unwrap_or(UrlError {
        kind: UrlErrorKind::Unresolved,
        offset: usize::MAX,
    })
}

fn options(uri: &str) -> QueryOptions {
    match ok(uri) {
        ODataUri::Resource { options, .. } => options,
        other => {
            assert!(
                matches!(other, ODataUri::Resource { .. }),
                "{uri}: {other:?}"
            );
            QueryOptions::default()
        }
    }
}

fn path(uri: &str) -> Vec<Segment> {
    match ok(uri) {
        ODataUri::Resource { path, .. } => path,
        other => {
            assert!(
                matches!(other, ODataUri::Resource { .. }),
                "{uri}: {other:?}"
            );
            Vec::new()
        }
    }
}

fn filter(text: &str) -> Expr {
    options(&format!("Buildings?$filter={text}"))
        .filter
        .unwrap_or(Expr::Literal(Literal::Null))
}

fn syntax(uri: &str, offset: usize) {
    let e = err(uri);
    assert!(
        matches!(e.kind, UrlErrorKind::Syntax { .. }),
        "{uri}: {e:?}"
    );
    assert_eq!(e.offset, offset, "{uri}: {e}");
}

fn refused(uri: &str, feature: Feature, offset: usize) {
    let e = err(uri);
    assert_eq!(e.kind, UrlErrorKind::NotImplemented(feature), "{uri}: {e}");
    assert_eq!(e.offset, offset, "{uri}: {e}");
}

fn unresolved(uri: &str, offset: usize) {
    let e = err(uri);
    assert_eq!(e.kind, UrlErrorKind::Unresolved, "{uri}: {e}");
    assert_eq!(e.offset, offset, "{uri}: {e}");
}

fn member(path: &[&str]) -> Expr {
    Expr::Member(MemberPath {
        variable: None,
        path: path.iter().map(|p| (*p).to_owned()).collect(),
    })
}

fn int(value: i64) -> Expr {
    Expr::Literal(Literal::Int(value))
}

fn string(text: &str) -> Literal {
    Literal::String(text.to_owned())
}

#[test]
fn the_service_forms() {
    assert_eq!(ok(""), ODataUri::ServiceDocument);
    assert_eq!(
        ok("$metadata"),
        ODataUri::Metadata {
            options: QueryOptions::default()
        }
    );
    assert_eq!(
        ok("$metadata?"),
        ODataUri::Metadata {
            options: QueryOptions::default()
        }
    );
    let json = QueryOptions {
        format: Some(Format::Json),
        ..QueryOptions::default()
    };
    assert_eq!(
        ok("$metadata?$format=json"),
        ODataUri::Metadata {
            options: json.clone()
        }
    );
    assert_eq!(ok("$batch?format=JSON"), ODataUri::Batch { options: json });
    assert_eq!(
        ok("$batch"),
        ODataUri::Batch {
            options: QueryOptions::default()
        }
    );
    let entity = ok("$entity?$id=Buildings('b1')&$format=xml");
    let ODataUri::Entity { options } = entity else {
        unreachable_form(&entity);
        return;
    };
    assert_eq!(options.id.as_deref(), Some("Buildings('b1')"));
    assert_eq!(options.format, Some(Format::Xml));
    // `$entity` takes `$id` and `$format` only; `$select` is no custom option either.
    syntax("$entity?$id=x&$select=name", 14);
    syntax("$entity?$format=json", 20);
    syntax("$entity", 7);
    refused("$entity/Model.Building?$id=x", Feature::TypeCast, 0);
    syntax("$metadata?$top=1", 10);
    refused("$crossjoin(Buildings,Sensors)", Feature::CrossJoin, 0);
    refused("$all", Feature::CrossJoin, 0);
    syntax("$metadata#Buildings", 9);
}

/// URL Conventions section 4 and the ABNF `resourcePath`: no path segment is
/// empty, so a trailing or doubled `/` is a syntax error at the empty segment,
/// never the resource without it and never a name that does not resolve
/// (T-3731).
#[test]
fn an_empty_path_segment_is_a_syntax_error() {
    syntax("Buildings/", 10);
    syntax("Buildings//", 10);
    syntax("Buildings//name", 10);
    syntax("Buildings/?$top=1", 10);
    syntax("Buildings('b1')/", 16);
    syntax("Buildings('b1')//name", 16);
}

fn unreachable_form(uri: &ODataUri) {
    assert!(matches!(uri, ODataUri::Entity { .. }), "{uri:?}");
}

#[test]
fn resource_paths() {
    let set = || Segment::EntitySet("Buildings".into());
    let single = |l: Literal| Segment::Key(Key::Single(KeyValue::Literal(l)));
    assert_eq!(path("Buildings"), vec![set()]);
    assert_eq!(path("Buildings('b1')"), vec![set(), single(string("b1"))]);
    assert_eq!(
        path("Buildings%28%27b1%27%29"),
        vec![set(), single(string("b1"))]
    );
    assert_eq!(path("Buildings(7)"), vec![set(), single(Literal::Int(7))]);
    assert_eq!(
        path("Buildings(@k)"),
        vec![
            set(),
            Segment::Key(Key::Single(KeyValue::Alias("k".into())))
        ]
    );
    assert_eq!(
        path("Buildings(id='b1',height=@h)"),
        vec![
            set(),
            Segment::Key(Key::Compound(vec![
                ("id".into(), KeyValue::Literal(string("b1"))),
                ("height".into(), KeyValue::Alias("h".into())),
            ]))
        ]
    );
    assert_eq!(path("Buildings/$count"), vec![set(), Segment::Count]);
    assert_eq!(path("Buildings/$ref"), vec![set(), Segment::Ref]);
    assert_eq!(path("Buildings/$query"), vec![set(), Segment::Query]);
    assert_eq!(
        path("Buildings('b1')/name/$value"),
        vec![
            set(),
            single(string("b1")),
            Segment::Property("name".into()),
            Segment::Value
        ]
    );
    assert_eq!(
        path("Buildings('b1')/address/city"),
        vec![
            set(),
            single(string("b1")),
            Segment::Property("address".into()),
            Segment::Property("city".into())
        ]
    );
    assert_eq!(
        path("Buildings('b1')/sensors(3)/owner/$ref"),
        vec![
            set(),
            single(string("b1")),
            Segment::Navigation("sensors".into()),
            single(Literal::Int(3)),
            Segment::Navigation("owner".into()),
            Segment::Ref,
        ]
    );
    assert_eq!(path("Buildings('b1')/rooms/$count").len(), 4);
    assert_eq!(path("Buildings('b1')/tags/$count").len(), 4);
    assert_eq!(path("Buildings('b1')/tags").len(), 3);
    assert_eq!(path("Buildings('b1')/rooms").len(), 3);
    assert_eq!(path("Buildings('b1')/sensors").len(), 3);
}

#[test]
fn resource_path_errors() {
    unresolved("Towers", 0);
    // As the OASIS case `Model.Rejection`: an action import is no resource of the subset.
    syntax("Model.Buildings", 5);
    syntax("('x')", 0);
    unresolved("Buildings/b1", 10);
    unresolved("Buildings/1", 10);
    unresolved("Buildings('b1')/width", 16);
    refused("Buildings/Model.Tower", Feature::TypeCast, 10);
    refused("Buildings('b1')/$value", Feature::MediaValue, 16);
    refused("Buildings/$filter(height gt 1)", Feature::FilterSegment, 10);
    refused("Buildings/$each", Feature::Each, 10);
    refused("Buildings('b1')/rooms/0", Feature::Ordinal, 22);
    refused("Buildings('b1')/tags/-1", Feature::Ordinal, 21);
    // After `$count` nothing follows; `$value` only after a single primitive.
    syntax("Buildings/$count/x", 16);
    syntax("Buildings('b1')/address/$value", 24);
    syntax("Buildings('b1')/name/x", 22);
    syntax("Buildings('b1')/tags/x", 22);
    syntax("Buildings('b1')/rooms/area", 26);
    syntax("Buildings(", 10);
    syntax("Buildings(id", 12);
    syntax("Buildings(id=", 13);
    syntax("Buildings(id='b1'", 17);
    syntax("Buildings(id='b1';height=1)", 17);
    syntax("Buildings('b1'", 14);
    syntax("Buildings(@)", 11);
    syntax("Buildings('b1')x", 15);
}

#[test]
fn query_options() {
    let o = options("Buildings?$top=5&skip=2&$count=true&$skiptoken=abc%20d&$schemaversion=1.0&@h=10&mode=fast&debug");
    assert_eq!((o.top, o.skip, o.count), (Some(5), Some(2), Some(true)));
    assert_eq!(o.skiptoken.as_deref(), Some("abc%20d"));
    assert_eq!(o.schemaversion.as_deref(), Some("1.0"));
    assert_eq!(o.aliases, vec![("h".to_owned(), Literal::Int(10))]);
    assert_eq!(
        o.custom,
        vec![
            ("mode".to_owned(), Some("fast".to_owned())),
            ("debug".to_owned(), None)
        ]
    );
    assert_eq!(options("Buildings?$count=FALSE").count, Some(false));
    assert_eq!(
        options("Buildings?$schemaversion=*")
            .schemaversion
            .as_deref(),
        Some("*")
    );
    assert_eq!(options("Buildings?$format=atom").format, Some(Format::Atom));
    assert_eq!(
        options("Buildings?$format=application/json;odata.metadata=none").format,
        Some(Format::MediaType(
            "application/json;odata.metadata=none".into()
        ))
    );
    assert_eq!(
        options("Buildings?%6Dode=x").custom,
        vec![("mode".to_owned(), Some("x".to_owned()))]
    );
    assert_eq!(options("Buildings?%C3%A9=x").custom.len(), 1);
    assert_eq!(options("Buildings?").top, None);
    let dup = err("Buildings?$top=1&top=2");
    assert_eq!(
        (dup.kind, dup.offset),
        (UrlErrorKind::DuplicateOption("$top"), 17)
    );
}

#[test]
fn query_option_errors() {
    syntax("Buildings?$top=x", 15);
    syntax("Buildings?$count=yes", 17);
    syntax("Buildings?$format=", 18);
    syntax("Buildings?$format=a/", 20);
    syntax("Buildings?$skiptoken=", 21);
    syntax("Buildings?$schemaversion=", 25);
    syntax("Buildings?$id=", 14);
    syntax("Buildings?=x", 10);
    syntax("Buildings?$x=1", 10);
    syntax("Buildings?$top=1$skip=1", 16);
    syntax("Buildings?$top =1", 14);
    syntax("Buildings?@=1", 11);
    syntax("Buildings?@h", 12);
    refused("Buildings?@h=height", Feature::AliasExpression, 13);
    refused("Buildings?@h=1 add 2", Feature::AliasExpression, 13);
    let big = err("Buildings?$top=99999999999999999999");
    assert_eq!(
        big.kind,
        UrlErrorKind::LimitExceeded {
            limit: LimitKind::Number,
            max: usize::MAX
        }
    );
    for (name, feature) in [
        ("$search=blue", Feature::Search),
        ("compute=1 as x", Feature::Compute),
        ("$index=1", Feature::Index),
        ("$deltatoken=x", Feature::DeltaToken),
        ("$apply=groupby((name))", Feature::Apply),
    ] {
        refused(&format!("Buildings?{name}"), feature, 10);
    }
    // `$deltatoken` and `$apply` need their `$`: without it they are custom options.
    assert_eq!(options("Buildings?apply=x").custom.len(), 1);
}

#[test]
fn filter_expressions() {
    assert_eq!(
        filter("height gt 10"),
        Expr::Compare(
            Box::new(member(&["height"])),
            CompareOp::Gt,
            Box::new(int(10))
        )
    );
    for (word, op) in [
        ("eq", CompareOp::Eq),
        ("ne", CompareOp::Ne),
        ("ge", CompareOp::Ge),
        ("lt", CompareOp::Lt),
        ("le", CompareOp::Le),
    ] {
        assert_eq!(
            filter(&format!("height {word} 1")),
            Expr::Compare(Box::new(member(&["height"])), op, Box::new(int(1)))
        );
    }
    let a = || {
        Box::new(Expr::Compare(
            Box::new(member(&["height"])),
            CompareOp::Gt,
            Box::new(int(1)),
        ))
    };
    let b = || {
        Box::new(Expr::Compare(
            Box::new(member(&["height"])),
            CompareOp::Lt,
            Box::new(int(9)),
        ))
    };
    assert_eq!(filter("height gt 1 and height lt 9"), Expr::And(a(), b()));
    assert_eq!(filter("height gt 1 or height lt 9"), Expr::Or(a(), b()));
    assert_eq!(filter("not (height gt 1)"), Expr::Not(a()));
    assert_eq!(filter("not%20(height%20gt%201)"), Expr::Not(a()));
    assert_eq!(
        filter("height gt 1 or height lt 9 and height lt 9"),
        Expr::Or(a(), Box::new(Expr::And(b(), b())))
    );
    assert_eq!(
        filter("(height gt 1 or height lt 9) and height lt 9"),
        Expr::And(Box::new(Expr::Or(a(), b())), b())
    );
    assert_eq!(
        filter("name in ('a', 'b')"),
        Expr::In(Box::new(member(&["name"])), vec![string("a"), string("b")])
    );
    assert_eq!(
        filter("name in ()"),
        Expr::In(Box::new(member(&["name"])), vec![])
    );
    assert_eq!(filter("@p"), Expr::Alias("p".into()));
    assert_eq!(
        filter("address/city eq 'Bonn'"),
        Expr::Compare(
            Box::new(member(&["address", "city"])),
            CompareOp::Eq,
            Box::new(Expr::Literal(string("Bonn")))
        )
    );
    assert_eq!(filter("owner/name"), member(&["owner", "name"]));
    assert_eq!(filter("tags"), member(&["tags"]));
    assert_eq!(filter("rooms"), member(&["rooms"]));
    assert_eq!(
        filter("name/@Core.Description"),
        member(&["name", "@Core.Description"])
    );
    assert_eq!(filter("@Core.Messages"), member(&["@Core.Messages"]));
    assert_eq!(
        filter("tags/@Core.Size%23max"),
        member(&["tags", "@Core.Size#max"])
    );
    assert_eq!(filter("true"), Expr::Literal(Literal::Bool(true)));
}

#[test]
fn methods_and_lambdas() {
    let call = |m: Method, path: &[&str], text: &str| {
        Expr::Call(m, vec![member(path), Expr::Literal(string(text))])
    };
    assert_eq!(
        filter("contains(name,'x')"),
        call(Method::Contains, &["name"], "x")
    );
    assert_eq!(
        filter("startswith( name , 'x' )"),
        call(Method::StartsWith, &["name"], "x")
    );
    assert_eq!(
        filter("ENDSWITH(name,'x')"),
        call(Method::EndsWith, &["name"], "x")
    );
    let geo = filter("geo.intersects(location,geography'SRID=4326;Point(1 2)')");
    assert!(
        matches!(geo, Expr::Call(Method::GeoIntersects, _)),
        "{geo:?}"
    );
    let any = filter("rooms/any(r:r/area gt 10)");
    let Expr::Any {
        collection,
        lambda: Some((variable, body)),
    } = any
    else {
        assert!(matches!(any, Expr::Any { .. }), "{any:?}");
        return;
    };
    assert_eq!(
        (collection.path, variable.as_str()),
        (vec!["rooms".to_owned()], "r")
    );
    assert_eq!(
        *body,
        Expr::Compare(
            Box::new(Expr::Member(MemberPath {
                variable: Some("r".into()),
                path: vec!["area".into()]
            })),
            CompareOp::Gt,
            Box::new(int(10))
        )
    );
    assert_eq!(
        filter("tags/any()"),
        Expr::Any {
            collection: MemberPath {
                variable: None,
                path: vec!["tags".into()]
            },
            lambda: None
        }
    );
    assert_eq!(filter("tags/any( )"), filter("tags/any()"));
    let nested = filter("sensors/any(s:s/owner/name eq 'x' and tags/any(t:t eq 'y'))");
    assert!(matches!(nested, Expr::Any { .. }), "{nested:?}");
    let bare = filter("tags/any(t:t)");
    let Expr::Any {
        lambda: Some((_, body)),
        ..
    } = bare
    else {
        assert!(matches!(bare, Expr::Any { .. }), "{bare:?}");
        return;
    };
    assert_eq!(
        *body,
        Expr::Member(MemberPath {
            variable: Some("t".into()),
            path: vec![]
        })
    );
}

#[test]
fn filter_errors() {
    let at = |text: &str| "Buildings?$filter=".len().saturating_add(text.len());
    syntax("Buildings?$filter=", 18);
    syntax("Buildings?$filter=height gt", 27);
    syntax("Buildings?$filter=(height gt 1", at("(height gt 1"));
    syntax("Buildings?$filter=contains(name)", at("contains(name"));
    syntax(
        "Buildings?$filter=contains(name,'x'",
        at("contains(name,'x'"),
    );
    syntax("Buildings?$filter=name in ('a'", at("name in ('a'"));
    syntax("Buildings?$filter=name/x", at("name/x"));
    syntax("Buildings?$filter=tags/x", at("tags/x"));
    syntax("Buildings?$filter=tags/any(t)", at("tags/any(t"));
    syntax("Buildings?$filter=tags/any(t:t", at("tags/any(t:t"));
    syntax("Buildings?$filter=tags/any(:t)", at("tags/any("));
    syntax("Buildings?$filter=width(1)", at("width"));
    syntax("Buildings?$filter=Model.f.", at("Model.f."));
    syntax("Buildings?$filter=@Core.", at("@Core."));
    syntax(
        "Buildings?$filter=name/@Core.Size%23",
        at("name/@Core.Size%23"),
    );
    unresolved("Buildings?$filter=width gt 1", 18);
    unresolved("Buildings?$filter=@Size/x", 18);
    unresolved("Buildings?$filter=@Core/x", 18);
    unresolved("Buildings?$filter=address/width", 26);
    let base = "Buildings?$filter=".len();
    for (text, feature, offset) in [
        ("$root/Buildings", Feature::Root, 0),
        ("$it/height", Feature::ImplicitVariable, 0),
        ("$this", Feature::ImplicitVariable, 0),
        ("-height", Feature::Arithmetic, 0),
        ("height add 1", Feature::Arithmetic, 7),
        ("height has 1", Feature::Has, 7),
        ("name in tags", Feature::InExpression, 8),
        ("name in (name)", Feature::InExpression, 8),
        ("length(name) gt 1", Feature::Method, 0),
        ("geo.distance(location,location) lt 1", Feature::Method, 0),
        ("cast(name,Edm.String)", Feature::CastIsOf, 0),
        ("isof(Model.Tower)", Feature::CastIsOf, 0),
        ("[1,2]", Feature::JsonLiteral, 0),
        ("{\"a\":1}", Feature::JsonLiteral, 0),
        ("Model.f(1)", Feature::Operation, 0),
        ("Model.Tower", Feature::TypeCast, 0),
        ("name/Model.f()", Feature::Operation, 5),
        ("address/Model.Ext/city", Feature::TypeCast, 8),
        ("tags/Model.Ext", Feature::TypeCast, 5),
        ("sensors/count()", Feature::Operation, 8),
        ("tags/$count gt 1", Feature::NestedCount, 4),
        ("tags/$filter(true)", Feature::PathInExpression, 4),
        ("sensors(1)/value", Feature::PathInExpression, 7),
        ("tags/all(t:t eq 'x')", Feature::AllLambda, 5),
        ("@Core.Messages/any()", Feature::AnnotationPath, 0),
        ("name/@Core.Size/x", Feature::AnnotationPath, 5),
        ("tags/@Core.Size/x", Feature::AnnotationPath, 5),
        ("height gt duration'P1D'", Feature::Literal, 10),
    ] {
        refused(
            &format!("Buildings?$filter={text}"),
            feature,
            base.saturating_add(offset),
        );
    }
}

#[test]
fn select_and_orderby() {
    let o = options("Buildings?$select=*,name,address/city,@Core.Messages,address,tags,owner&$orderby=height desc,name asc,id");
    assert_eq!(
        o.select,
        Some(vec![
            SelectItem::Star,
            SelectItem::Path(vec!["name".into()]),
            SelectItem::Path(vec!["address".into(), "city".into()]),
            SelectItem::Annotation("@Core.Messages".into()),
            SelectItem::Path(vec!["address".into()]),
            SelectItem::Path(vec!["tags".into()]),
            SelectItem::Path(vec!["owner".into()]),
        ])
    );
    let by = o.orderby.unwrap_or_default();
    assert_eq!(
        by.iter().map(|o| o.descending).collect::<Vec<_>>(),
        vec![true, false, false]
    );
    unresolved("Buildings?$select=width", 18);
    unresolved("Buildings?$select=address/width", 26);
    syntax("Buildings?$select=", 18);
    syntax("Buildings?$select=name,", 23);
    refused("Buildings?$select=Model.Tower/name", Feature::TypeCast, 18);
    refused(
        "Buildings?$select=address($select=city)",
        Feature::NestedOption,
        26,
    );
    refused("Buildings?$select=tags($top=1)", Feature::NestedOption, 23);
    refused(
        "Buildings?$select=@Core.Messages($top=1)",
        Feature::NestedOption,
        33,
    );
    refused(
        "Buildings?$select=address/@Core.Messages",
        Feature::AnnotationPath,
        26,
    );
}

#[test]
fn expand() {
    let o = options("Buildings?$expand=sensors($select=value;$expand=owner;$levels=2),owner/$ref,*,*/$ref,*($levels=max),address/sensors");
    let items = o.expand.unwrap_or_default();
    assert_eq!(items.len(), 6);
    assert_eq!(
        items.first(),
        Some(&ExpandItem {
            path: ExpandPath::Path(vec!["sensors".into()]),
            kind: ExpandKind::Entities {
                select: Some(vec![SelectItem::Path(vec!["value".into()])]),
                expand: Some(vec![ExpandItem {
                    path: ExpandPath::Path(vec!["owner".into()]),
                    kind: ExpandKind::Entities {
                        select: None,
                        expand: None,
                        levels: None
                    },
                }]),
                levels: Some(Levels::Count(2)),
            },
        })
    );
    assert_eq!(items.get(1).map(|i| &i.kind), Some(&ExpandKind::Ref));
    assert_eq!(items.get(2).map(|i| &i.path), Some(&ExpandPath::Star));
    assert_eq!(items.get(3).map(|i| &i.kind), Some(&ExpandKind::Ref));
    assert_eq!(
        items.get(4).map(|i| &i.kind),
        Some(&ExpandKind::Entities {
            select: None,
            expand: None,
            levels: Some(Levels::Max)
        })
    );
    assert_eq!(
        items.get(5).map(|i| &i.path),
        Some(&ExpandPath::Path(vec!["address".into(), "sensors".into()]))
    );
}

#[test]
fn expand_errors() {
    let base = "Buildings?$expand=".len();
    for (text, feature, offset) in [
        ("$value", Feature::MediaValue, 0),
        ("@Core.Messages", Feature::AnnotationPath, 0),
        ("Model.Tower/sensors", Feature::TypeCast, 0),
        ("sensors/Model.Probe", Feature::TypeCast, 8),
        ("sensors/Probe", Feature::TypeCast, 8),
        ("sensors/$count", Feature::NestedCount, 7),
        ("sensors($filter=true)", Feature::NestedOption, 8),
        ("sensors($top=1;$select=value)", Feature::NestedOption, 8),
        ("sensors(@a=1)", Feature::NestedOption, 8),
        ("sensors/$ref($top=1)", Feature::NestedOption, 13),
        ("address/*", Feature::PathStar, 8),
    ] {
        refused(
            &format!("Buildings?$expand={text}"),
            feature,
            base.saturating_add(offset),
        );
    }
    unresolved("Buildings?$expand=width", 18);
    unresolved("Buildings?$expand=name", 18);
    syntax("Buildings?$expand=", 18);
    syntax("Buildings?$expand=address", 25);
    syntax("Buildings?$expand=sensors($levels=0)", 34);
    syntax("Buildings?$expand=sensors($select=value", 39);
    syntax("Buildings?$expand=sensors($x=1)", 26);
    syntax("Buildings?$expand=sensors/$ref($levels=1)", 31);
    syntax("Buildings?$expand=*(", 20);
    syntax("Buildings?$expand=*($levels=1", 29);
    let deep = err("Buildings?$expand=sensors($levels=4)");
    assert_eq!(
        deep.kind,
        UrlErrorKind::LimitExceeded {
            limit: LimitKind::ExpandDepth,
            max: 3
        }
    );
    assert_eq!(deep.offset, 34);
    let nested = err("Buildings?$expand=sensors($expand=building($expand=sensors($expand=owner)))");
    assert_eq!(
        nested.kind,
        UrlErrorKind::LimitExceeded {
            limit: LimitKind::ExpandDepth,
            max: 3
        }
    );
}

#[test]
fn literals_in_keys_and_filters() {
    for (text, literal) in [
        ("null", Literal::Null),
        ("false", Literal::Bool(false)),
        ("-12", Literal::Int(-12)),
        ("1.5", Literal::Decimal("1.5".into())),
        ("2024-02-29", Literal::Date("2024-02-29".into())),
        (
            "2024-02-29T10:00:00Z",
            Literal::DateTimeOffset("2024-02-29T10:00:00Z".into()),
        ),
        (
            "2024-02-29T10%3A00%2B01%3A00",
            Literal::DateTimeOffset("2024-02-29T10:00+01:00".into()),
        ),
        ("10:00:00.5", Literal::TimeOfDay("10:00:00.5".into())),
        ("'it''s'", string("it's")),
        ("'%C3%A9'", string("\u{e9}")),
    ] {
        assert_eq!(filter(text), Expr::Literal(literal.clone()), "{text}");
        assert_eq!(
            path(&format!("Buildings({text})")).get(1),
            Some(&Segment::Key(Key::Single(KeyValue::Literal(literal)))),
            "{text}"
        );
    }
}

#[test]
fn limits_are_checked() {
    let tiny = Limits {
        max_path_len: 20,
        max_query_len: 30,
        max_path_segments: 2,
        max_query_options: 2,
        max_expr_depth: 2,
        max_expr_nodes: 6,
        max_expand_depth: 1,
        max_expand_items: 2,
        max_select_items: 2,
        max_orderby_items: 2,
        max_in_list_len: 2,
        max_literal_len: 6,
    };
    let limit = |uri: &str, kind: LimitKind| {
        let e = err_within(uri, &tiny);
        assert!(
            matches!(e.kind, UrlErrorKind::LimitExceeded { limit, .. } if limit == kind),
            "{uri}: {e}"
        );
        e.offset
    };
    assert_eq!(
        limit("Buildings('b1')/address/city", LimitKind::PathLength),
        20
    );
    assert_eq!(
        limit(
            "Buildings?$filter=name eq 'abcdefghijklmnopq'",
            LimitKind::QueryLength
        ),
        40
    );
    limit("Sensors(1)/owner", LimitKind::PathSegments);
    limit("Sensors?$top=1&$skip=1&a=1", LimitKind::QueryOptions);
    limit("Sensors?$filter=(((true)))", LimitKind::ExprDepth);
    limit(
        "Sensors?$filter=value eq 1 or at eq 2",
        LimitKind::ExprNodes,
    );
    limit("Sensors?$expand=owner($expand=x)", LimitKind::ExpandDepth);
    limit("Sensors?$expand=owner,owner,owner", LimitKind::ExpandItems);
    limit("Sensors?$select=value,at,value", LimitKind::SelectItems);
    limit("Sensors?$orderby=value,at,value", LimitKind::OrderbyItems);
    limit("Sensors?$filter=value in (1,2,3)", LimitKind::InListLength);
    limit("Sensors?$filter='abcdefgh'", LimitKind::LiteralLength);
    let fits = parse("Sensors?$filter=value in (1,2)", &Service, &tiny);
    assert!(fits.is_ok(), "{fits:?}");
}

#[test]
fn normalisation() {
    assert_eq!(normalize("Buildings"), "Buildings");
    assert_eq!(normalize("%42uildings%2F"), "Buildings%2F");
    assert_eq!(normalize("%7e%2d%5F%2E%30"), "~-_.0");
    // A decoded hex digit never spells a new escape.
    assert_eq!(normalize("%6%65"), "%6%65");
    assert_eq!(normalize("%%36%35"), "%%365");
    assert_eq!(normalize("%25%36%35"), "%2565");
    assert_eq!(normalize("%%6G%41"), "%%6GA");
    assert_eq!(normalize("\u{e9}%41"), "\u{e9}A");
    assert_eq!(normalize("%4"), "%4");
    refused("B%C3%A4ume", Feature::NonAsciiIdentifier, 1);
}

#[test]
fn a_form_encoded_query_reads_as_written_plain() {
    // T-3719: `%24` names a system option, `+` is whitespace where `RWS` stands, `%2F` and
    // `%3D` are the query's `/` and `=`.
    let o = options("Buildings?%24top=1&%24Skip=2&%24filter=height+gt+5&%24orderby=height+desc");
    assert_eq!((o.top, o.skip), (Some(1), Some(2)));
    assert_eq!(o.filter, Some(filter("height gt 5")));
    assert_eq!(o.orderby, options("Buildings?$orderby=height desc").orderby);
    assert!(o.custom.is_empty(), "{:?}", o.custom);
    assert_eq!(
        filter("height+gt+5+and+name+eq+%27x%27"),
        filter("height gt 5 and name eq 'x'")
    );
    let sensors = options("Buildings?$expand=sensors(%24select%3Dvalue)").expand;
    assert_eq!(
        sensors,
        options("Buildings?$expand=sensors($select=value)").expand
    );
    assert_eq!(
        filter("address%2Fcity+eq+%27Bonn%27"),
        filter("address/city eq 'Bonn'")
    );
    let o = options("Buildings?%24select=address%2Fcity&%24expand=address%2Fsensors");
    assert_eq!((o.select, o.expand), {
        let plain = options("Buildings?$select=address/city&$expand=address/sensors");
        (plain.select, plain.expand)
    });
    assert_eq!(
        options("Buildings?@h%3D10").aliases,
        vec![("h".to_owned(), Literal::Int(10))]
    );
    // A custom option keeps its `=` as written, and a key keeps its `%2F`.
    assert_eq!(
        options("Buildings?mode%3Dfast").custom,
        vec![("mode%3Dfast".to_owned(), None)]
    );
    let key = ok("Buildings('a%2Fb')").to_string();
    assert!(key.contains("a%2Fb"), "{key}");
    // Encoded twice is a custom option; a duplicate across both spellings is one.
    assert_eq!(
        options("Buildings?%2524top=1").custom,
        vec![("%2524top".to_owned(), Some("1".to_owned()))]
    );
    let dup = err("Buildings?%24top=1&$top=2");
    assert_eq!(dup.kind, UrlErrorKind::DuplicateOption("$top"));
    // Where `RWS` stands a `+` could only be a sign, which means nothing.
    assert_eq!(filter("height gt +5"), filter("height gt 5"));
    assert_eq!(filter("height+gt+%2B5"), filter("height gt 5"));
    assert_eq!(filter("height+gt+-5"), filter("height gt -5"));
    // A literal keeps its `+`, and a decoded `&` or `=` never splits an option.
    assert_eq!(
        filter("name+eq+'a+b'"),
        Expr::Compare(
            Box::new(member(&["name"])),
            CompareOp::Eq,
            Box::new(Expr::Literal(string("a+b")))
        )
    );
    let o = options("Buildings?$filter=name+eq+'a%26top%3D1'");
    assert_eq!(o.top, None);
    assert_eq!(o.filter, Some(filter("name eq 'a&top=1'")));
    // `BWS` is not `RWS`: a `+` after a comma is no whitespace.
    syntax("Buildings?$select=name,+height", 23);
    // No whitespace after `gt` is still none: the error stands at the `5`.
    syntax("Buildings?$filter=height+gt5", 27);
}

#[test]
fn the_canonical_form_parses_back() {
    for uri in [
        "",
        "$metadata?$format=json",
        "$batch?$format=xml",
        "$entity?$id=Buildings('b1')&$format=atom",
        "Buildings('b1')/address/city",
        "Buildings(id='b%20''1',height=@h)/sensors(1)/owner/$ref?@h=2",
        "Buildings/$count",
        "Buildings('x')/name/$value",
        "Buildings/$query",
        "Buildings?$filter=(height gt 1 or height lt 2) and not (name eq 'a%23b') and name in ('x','y')",
        "Buildings?$filter=height gt -1.5e3 and built lt 2024-01-01T00:00:00%2B01:00 and built ge 2024-01-01",
        "Buildings?$filter=not (not true) or (height ne 1 and false) or (height eq 1) eq true",
        "Buildings?$filter=height le 1 eq (height ge 2)",
        "Buildings?$filter=contains(name,'%C3%A9') and startswith(name,'a') and endswith(name,'z')",
        "Buildings?$filter=geo.intersects(location,geography'SRID=4326;Polygon((0 0,1 0,1 1,0 0))')",
        "Buildings?$filter=geo.intersects(location,geography'SRID=0;Point(1 2)')",
        "Buildings?$filter=rooms/any(r:r/area gt 1) and tags/any() and @p and null eq 10:00",
        "Buildings?$filter=name/@Core.Size%23max eq 1",
        "Buildings?$select=*,name,address/city,@Core.Messages&$orderby=height desc,name",
        "Buildings?$expand=sensors($select=value;$expand=owner;$levels=1),owner/$ref,*($levels=max),*/$ref",
        "Buildings?$top=1&$skip=2&$count=true&$format=application/json&$skiptoken=x&$schemaversion=*&mode=a&flag",
    ] {
        let first = ok(uri);
        let canonical = first.to_string();
        assert_eq!(ok(&canonical), first, "{uri} wrote {canonical}");
        assert_eq!(ok(&canonical).to_string(), canonical, "{uri}");
    }
    assert_eq!(
        ok("Buildings?$filter=height gt 1 and (height lt 2 or name eq 'it''s')").to_string(),
        "Buildings?$filter=height%20gt%201%20and%20(height%20lt%202%20or%20name%20eq%20'it''s')"
    );
    assert_eq!(
        ok("Buildings?$filter=name eq 'a%20b%23'").to_string(),
        "Buildings?$filter=name%20eq%20'a%20b%23'"
    );
}

#[test]
fn errors_name_themselves() {
    let e = err("Buildings?$search=x");
    assert_eq!(e.to_string(), "not implemented at byte 10: $search");
    let e = err("Towers");
    assert_eq!(e.to_string(), "unknown name at byte 0");
    let e = err("Buildings?$top=1&$top=1");
    assert_eq!(e.to_string(), "system query option $top given twice");
    let e = err("Buildings?$top=x");
    assert_eq!(e.to_string(), "syntax error at byte 15: expected DIGIT");
    let e = err_within(
        "Buildings",
        &Limits {
            max_path_len: 1,
            ..Limits::strict()
        },
    );
    assert_eq!(
        e.to_string(),
        format!(
            "limit exceeded at byte 1: {} (max 1)",
            LimitKind::PathLength.as_str()
        )
    );
}

#[test]
fn every_feature_and_limit_has_its_own_name() {
    use Feature as F;
    use LimitKind as L;
    let features = [
        F::Search,
        F::Compute,
        F::Index,
        F::Apply,
        F::DeltaToken,
        F::Root,
        F::ImplicitVariable,
        F::AliasExpression,
        F::FilterSegment,
        F::CrossJoin,
        F::TypeCast,
        F::Ordinal,
        F::Operation,
        F::Each,
        F::MediaValue,
        F::NestedOption,
        F::NestedCount,
        F::PathInExpression,
        F::Arithmetic,
        F::Has,
        F::Method,
        F::CastIsOf,
        F::AllLambda,
        F::InExpression,
        F::JsonLiteral,
        F::Literal,
        F::NonAsciiIdentifier,
        F::AnnotationPath,
        F::PathStar,
    ];
    let limits = [
        L::PathLength,
        L::QueryLength,
        L::PathSegments,
        L::QueryOptions,
        L::ExprDepth,
        L::ExprNodes,
        L::ExpandDepth,
        L::ExpandItems,
        L::SelectItems,
        L::OrderbyItems,
        L::InListLength,
        L::LiteralLength,
        L::Number,
    ];
    let mut names: Vec<&str> = features
        .iter()
        .map(|f| f.as_str())
        .chain(limits.iter().map(|l| l.as_str()))
        .collect();
    let all = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), all);
    assert!(!names.contains(&""));
}

#[test]
fn every_option_names_itself_when_given_twice() {
    for (option, value) in [
        ("$filter", "true"),
        ("$select", "name"),
        ("$expand", "owner"),
        ("$orderby", "name"),
        ("$top", "1"),
        ("$skip", "1"),
        ("$count", "true"),
        ("$format", "json"),
        ("$skiptoken", "x"),
        ("$schemaversion", "1"),
        ("$id", "x"),
    ] {
        let e = err(&format!("Buildings?{option}={value}&{option}={value}"));
        assert_eq!(e.kind, UrlErrorKind::DuplicateOption(option), "{option}");
    }
}

#[test]
fn the_rest_of_the_grammar() {
    // Signs and whitespace in their encoded forms.
    assert_eq!(
        filter("height%09gt%09%2B1"),
        Expr::Compare(
            Box::new(member(&["height"])),
            CompareOp::Gt,
            Box::new(int(1))
        )
    );
    let base = "Buildings?$filter=".len();
    for (text, feature, offset) in [
        ("%5B1]", Feature::JsonLiteral, 0),
        ("%7B}", Feature::JsonLiteral, 0),
        ("height has Model.Kind'a'", Feature::Has, 7),
    ] {
        refused(
            &format!("Buildings?$filter={text}"),
            feature,
            base.saturating_add(offset),
        );
    }
    for name in ["contains", "length", "cast", "nothing"] {
        unresolved(&format!("Buildings?$filter={name}"), base);
    }
    // `not` needs whitespace: `not(` is a call of an unknown function.
    syntax("Buildings?$filter=not(true)", base.saturating_add(3));
    unresolved(
        "Buildings?$filter=tags/any(t:t/x eq 1)",
        base.saturating_add(13),
    );
    syntax("Buildings?$filter=name/1", base.saturating_add(5));
    syntax("Buildings?$filter=tags/any", base.saturating_add(8));
    syntax("Buildings?$filter=tags/all", base.saturating_add(8));
    syntax("Buildings?$filter=tags/1", base.saturating_add(5));
    assert_eq!(filter("address"), member(&["address"]));
    assert_eq!(
        filter("owner eq null"),
        Expr::Compare(
            Box::new(member(&["owner"])),
            CompareOp::Eq,
            Box::new(Expr::Literal(Literal::Null))
        )
    );
    syntax("Buildings?$orderby=name x", 24);
    unresolved("Buildings('b1')/name?$select=x", 29);
    assert_eq!(
        options("Buildings?$format=a%2Fb/c").format,
        Some(Format::MediaType("a%2Fb/c".into()))
    );
    assert_eq!(
        options("Buildings?$format=jsonx/y").format,
        Some(Format::MediaType("jsonx/y".into()))
    );
    syntax("Buildings?$format=/x", 18);
    syntax("Buildings?$format=abc", 21);
    assert_eq!(filter("height\tgt 1"), filter("height gt 1"));
    syntax("Buildings?%zz", 10);
    syntax("Buildings?ab#", 12);
    let rings = ok("Buildings?$filter=geo.intersects(location,geography'SRID=0;Polygon((0 0,1 0,0 0),(0 0,2 0,0 0))')");
    assert_eq!(ok(&rings.to_string()), rings);
    let punctuated = ok("Buildings?$filter=name eq 'a-b.c_d~e!f(g)h*i,j;k$l&m=n:o@p+q'");
    assert_eq!(ok(&punctuated.to_string()), punctuated);
}
