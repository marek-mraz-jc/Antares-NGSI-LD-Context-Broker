// SPDX-License-Identifier: EUPL-1.2
//! Tool arguments (MCP 3.2): one table per tool names its arguments and
//! their kinds; the same kinds build the JSON Schema 2020-12 a client is
//! shown and check what a client sends, so the two cannot disagree. A
//! value outside an enum and a hidden name answer the same refusal, which
//! names the argument and the rule, never the value (MC2, MC18).

use serde_json::{json, Map, Value};

/// An Entity type name (platform `TYPE_NAME`).
const TYPE_NAME: &str = "^[A-Za-z][A-Za-z0-9_-]*$";
/// An Attribute name, a compact term or an IRI (platform `ATTR_NAME`).
const ATTR_NAME: &str = "^[A-Za-z][A-Za-z0-9_\\-.:/#]*$";
/// An NGSI-LD Entity id (platform `ENTITY_URN`).
const ENTITY_URN: &str = "^urn:ngsi-ld:[^\\s]+$";
/// The most names an enum lists before it falls back to the pattern.
const MAX_ENUM: usize = 256;
/// The most items a list argument takes.
const MAX_ITEMS: usize = 100;
/// The most types a type list takes.
const MAX_TYPES: usize = 50;

/// What one argument holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// An Entity id.
    Id,
    /// A list of Entity ids.
    Ids,
    /// One Entity type the view shows.
    Type,
    /// A list of Entity types the view shows.
    Types,
    /// One Attribute the view shows.
    Attr,
    /// A list of Attributes the view shows.
    Attrs,
    /// A free string of at most this many bytes (`q`, `idPattern`, ...).
    Text(usize),
    /// One of these words.
    Word(&'static [&'static str]),
    /// An integer in this closed range.
    Int(u64, u64),
    /// `true` or `false`.
    Flag,
    /// A `GeoJSON` `coordinates` array, sent as its JSON text.
    Coordinates,
    /// A whole Entity: `id`, a type the view shows and Attributes it shows.
    Entity,
    /// A list of whole Entities, at most [`MAX_ITEMS`].
    Entities,
    /// Attributes of one Entity, and its type at most: no `id`.
    Fragment,
    /// One Attribute's body.
    Attribute,
    /// An object of these members.
    Object(&'static [Arg]),
    /// A list of objects of these members, at most this many.
    ListOf(&'static [Arg], usize),
    /// A notification endpoint the deployment lets a tool name (MC12).
    Endpoint,
}

/// The endpoints a subscription made through a tool may notify, comma
/// separated; unset, none (MCP section 10 rule 7).
pub const SUBSCRIPTION_ENDPOINTS_ENV: &str = "ANTARES_MCP_SUBSCRIPTION_ENDPOINTS";

/// The deployment's endpoint allowlist.
fn endpoints() -> &'static [String] {
    static ENDPOINTS: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
        crate::listed(std::env::var(SUBSCRIPTION_ENDPOINTS_ENV).ok().as_deref()).unwrap_or_default()
    });
    &ENDPOINTS
}

/// Whether `uri` is one of `allowed`, or below one that ends in `/`.
fn endpoint_allowed(uri: &str, allowed: &[String]) -> bool {
    allowed
        .iter()
        .any(|a| uri == a || (a.ends_with('/') && uri.starts_with(a.as_str())))
}

/// One argument of a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Arg {
    pub name: &'static str,
    pub kind: Kind,
    pub required: bool,
}

/// An optional argument.
pub(crate) const fn opt(name: &'static str, kind: Kind) -> Arg {
    Arg {
        name,
        kind,
        required: false,
    }
}

/// A required argument.
pub(crate) const fn req(name: &'static str, kind: Kind) -> Arg {
    Arg {
        name,
        kind,
        required: true,
    }
}

/// The names a view shows, as its schemas offer them.
#[derive(Debug, Clone, Default)]
pub(crate) struct Names {
    pub types: Vec<String>,
    pub attributes: Vec<String>,
    /// The view shows every name (ADR-0023 rule 12): a name is offered and
    /// taken by its pattern alone.
    pub every: bool,
}

impl Names {
    /// The view's names, each kept only when it matches its pattern: a name
    /// that does not is reachable through the NGSI-LD view root alone, and
    /// never shown to a model (MCP 3.2).
    pub(crate) fn new(types: Vec<String>, attributes: Vec<String>) -> Self {
        Self {
            types: types.into_iter().filter(|t| is_type_name(t)).collect(),
            attributes: attributes.into_iter().filter(|a| is_attr_name(a)).collect(),
            every: false,
        }
    }

    /// Every name its pattern admits (ADR-0023 rule 12).
    pub(crate) fn every() -> Self {
        Self {
            every: true,
            ..Self::default()
        }
    }

    /// Whether `t` is a type this view offers.
    pub(crate) fn has_type(&self, t: &str) -> bool {
        if self.every {
            is_type_name(t)
        } else {
            self.types.iter().any(|n| n == t)
        }
    }
}

fn is_type_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn is_attr_name(s: &str) -> bool {
    let mut chars = s.chars();
    s.len() <= 256
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || "_-.:/#".contains(c))
}

fn is_entity_id(s: &str) -> bool {
    s.len() <= 512
        && s.strip_prefix("urn:ngsi-ld:")
            .is_some_and(|rest| !rest.is_empty() && !rest.chars().any(char::is_whitespace))
}

/// A name schema: the view's names as an enum while they are few, else
/// the pattern alone.
fn named(names: &[String], every: bool, pattern: &str, max_len: usize) -> Value {
    if !every && names.len() <= MAX_ENUM {
        json!({"type": "string", "enum": names})
    } else {
        json!({"type": "string", "pattern": pattern, "maxLength": max_len})
    }
}

fn schema_of(kind: Kind, names: &Names) -> Value {
    match kind {
        Kind::Id => json!({"type": "string", "pattern": ENTITY_URN, "maxLength": 512}),
        Kind::Ids => {
            json!({"type": "array", "maxItems": MAX_ITEMS, "items": schema_of(Kind::Id, names)})
        }
        Kind::Type => named(&names.types, names.every, TYPE_NAME, 256),
        Kind::Types => {
            json!({"type": "array", "minItems": 1, "maxItems": MAX_TYPES, "items": schema_of(Kind::Type, names)})
        }
        Kind::Attr => named(&names.attributes, names.every, ATTR_NAME, 256),
        Kind::Attrs => {
            json!({"type": "array", "minItems": 1, "maxItems": MAX_ITEMS, "items": schema_of(Kind::Attr, names)})
        }
        Kind::Text(max) => json!({"type": "string", "maxLength": max}),
        Kind::Word(words) => json!({"type": "string", "enum": words}),
        Kind::Int(min, max) => json!({"type": "integer", "minimum": min, "maximum": max}),
        Kind::Flag => json!({"type": "boolean"}),
        Kind::Coordinates => json!({"type": "array", "maxItems": 1000}),
        Kind::Entity => json!({
            "type": "object",
            "required": ["id", "type"],
            "properties": {"id": schema_of(Kind::Id, names), "type": schema_of(Kind::Type, names)},
            "propertyNames": member_names(names, &["id", "type"]),
        }),
        Kind::Entities => {
            json!({"type": "array", "minItems": 1, "maxItems": MAX_ITEMS, "items": schema_of(Kind::Entity, names)})
        }
        Kind::Fragment => json!({
            "type": "object",
            "minProperties": 1,
            "properties": {"type": schema_of(Kind::Type, names)},
            "propertyNames": member_names(names, &["type"]),
        }),
        Kind::Attribute => json!({"type": "object"}),
        Kind::Object(args) => Value::Object(input_schema(args, names)),
        Kind::ListOf(args, max) => {
            json!({"type": "array", "maxItems": max, "items": input_schema(args, names)})
        }
        Kind::Endpoint => json!({"type": "string", "maxLength": 2048}),
    }
}

/// The member names of an Entity body: `core` and the view's Attributes.
fn member_names(names: &Names, core: &[&str]) -> Value {
    if names.attributes.len() <= MAX_ENUM {
        let mut all: Vec<&str> = core.to_vec();
        all.extend(names.attributes.iter().map(String::as_str));
        json!({"enum": all})
    } else {
        json!({"pattern": ATTR_NAME, "maxLength": 256})
    }
}

/// An object without `@context`: the surface sets the view's (MCP 3.2).
fn object<'v>(arg: &str, v: &'v Value) -> Result<&'v Map<String, Value>, Broken> {
    let o = v
        .as_object()
        .ok_or_else(|| broken(arg, "must be an object"))?;
    if o.contains_key("@context") {
        return Err(broken(
            arg,
            "carries no @context; the endpoint sets the view's",
        ));
    }
    Ok(o)
}

/// An Entity body (`whole`) or an Attribute fragment of one, each member
/// a name the view shows.
fn entity(arg: &str, v: &Value, names: &Names, whole: bool) -> Result<(), Broken> {
    let o = object(arg, v)?;
    for (k, member) in o {
        match k.as_str() {
            "id" if whole => {
                if !member.as_str().is_some_and(is_entity_id) {
                    return Err(broken(arg, "id must be an NGSI-LD Entity id"));
                }
            }
            "id" => return Err(broken(arg, "names no id; the id argument does")),
            "type" => {
                one_name(
                    arg,
                    member,
                    (&names.types, names.every),
                    is_type_name,
                    "types",
                )?;
            }
            name => {
                one_name(
                    arg,
                    &Value::String(name.to_owned()),
                    (&names.attributes, names.every),
                    is_attr_name,
                    "attributes",
                )?;
            }
        }
    }
    if whole && !(o.contains_key("id") && o.contains_key("type")) {
        return Err(broken(arg, "must have an id and a type"));
    }
    if o.is_empty() {
        return Err(broken(arg, "must name at least one Attribute"));
    }
    Ok(())
}

/// `inner`, a refusal of a member of `arg`, named as one.
fn nested(arg: &str, inner: &Broken) -> Broken {
    inner.replacen("argument ", &format!("argument {arg}."), 1)
}

/// The input schema of a tool with `args` under `names`.
pub(crate) fn input_schema(args: &[Arg], names: &Names) -> Map<String, Value> {
    let properties: Map<String, Value> = args
        .iter()
        .map(|a| (a.name.to_owned(), schema_of(a.kind, names)))
        .collect();
    let required: Vec<&str> = args.iter().filter(|a| a.required).map(|a| a.name).collect();
    let mut schema = Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), json!(required));
    }
    schema.insert("additionalProperties".into(), json!(false));
    schema
}

/// Why an argument was refused: the argument and the broken rule.
pub(crate) type Broken = String;

fn broken(arg: &str, rule: &str) -> Broken {
    format!("argument {arg}: {rule}")
}

/// One name of `names` under `pattern`, refused alike when hidden or absent.
fn one_name(
    arg: &str,
    v: &Value,
    (names, every): (&[String], bool),
    valid: fn(&str) -> bool,
    what: &str,
) -> Result<String, Broken> {
    let s = v.as_str().ok_or_else(|| broken(arg, "must be a string"))?;
    let known = if !every && names.len() <= MAX_ENUM {
        names.iter().any(|n| n == s)
    } else {
        valid(s)
    };
    if known {
        Ok(s.to_owned())
    } else {
        Err(broken(
            arg,
            &format!("not one of the {what} this endpoint serves"),
        ))
    }
}

fn list<'v>(arg: &str, v: &'v Value, min: usize, max: usize) -> Result<&'v [Value], Broken> {
    let items = v
        .as_array()
        .ok_or_else(|| broken(arg, "must be an array"))?;
    if items.len() < min || items.len() > max {
        return Err(broken(arg, &format!("takes {min} to {max} items")));
    }
    Ok(items)
}

/// One checked argument value, as the request is built from it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Checked {
    One(String),
    Many(Vec<String>),
    Int(u64),
    Flag(bool),
    Json(Value),
}

fn check(arg: &Arg, v: &Value, names: &Names) -> Result<Checked, Broken> {
    let name = arg.name;
    Ok(match arg.kind {
        Kind::Id => {
            let s = v.as_str().filter(|s| is_entity_id(s));
            Checked::One(
                s.ok_or_else(|| broken(name, "must be an NGSI-LD Entity id"))?
                    .to_owned(),
            )
        }
        Kind::Ids => Checked::Many(
            list(name, v, 1, MAX_ITEMS)?
                .iter()
                .map(|i| {
                    i.as_str()
                        .filter(|s| is_entity_id(s))
                        .map(str::to_owned)
                        .ok_or_else(|| broken(name, "must hold NGSI-LD Entity ids"))
                })
                .collect::<Result<_, _>>()?,
        ),
        Kind::Type => Checked::One(one_name(
            name,
            v,
            (&names.types, names.every),
            is_type_name,
            "types",
        )?),
        Kind::Types => Checked::Many(
            list(name, v, 1, MAX_TYPES)?
                .iter()
                .map(|t| one_name(name, t, (&names.types, names.every), is_type_name, "types"))
                .collect::<Result<_, _>>()?,
        ),
        Kind::Attr => Checked::One(one_name(
            name,
            v,
            (&names.attributes, names.every),
            is_attr_name,
            "attributes",
        )?),
        Kind::Attrs => Checked::Many(
            list(name, v, 1, MAX_ITEMS)?
                .iter()
                .map(|a| {
                    one_name(
                        name,
                        a,
                        (&names.attributes, names.every),
                        is_attr_name,
                        "attributes",
                    )
                })
                .collect::<Result<_, _>>()?,
        ),
        Kind::Text(max) => {
            let s = v.as_str().ok_or_else(|| broken(name, "must be a string"))?;
            if s.len() > max {
                return Err(broken(name, &format!("takes at most {max} bytes")));
            }
            Checked::One(s.to_owned())
        }
        Kind::Word(words) => {
            let s = v.as_str().filter(|s| words.contains(s));
            Checked::One(
                s.ok_or_else(|| broken(name, &format!("must be one of {}", words.join(", "))))?
                    .to_owned(),
            )
        }
        Kind::Int(min, max) => {
            let n = v.as_u64().filter(|n| (min..=max).contains(n));
            Checked::Int(
                n.ok_or_else(|| broken(name, &format!("must be an integer from {min} to {max}")))?,
            )
        }
        Kind::Flag => Checked::Flag(
            v.as_bool()
                .ok_or_else(|| broken(name, "must be true or false"))?,
        ),
        Kind::Coordinates => {
            list(name, v, 1, 1000)?;
            Checked::Json(v.clone())
        }
        Kind::Entity
        | Kind::Entities
        | Kind::Fragment
        | Kind::Attribute
        | Kind::Object(_)
        | Kind::ListOf(..)
        | Kind::Endpoint => body(arg, v, names)?,
    })
}

/// [`check`] of a body a write tool sends: kept as given once checked.
fn body(arg: &Arg, v: &Value, names: &Names) -> Result<Checked, Broken> {
    let name = arg.name;
    Ok(match arg.kind {
        Kind::Entity => {
            entity(name, v, names, true)?;
            Checked::Json(v.clone())
        }
        Kind::Entities => {
            for e in list(name, v, 1, MAX_ITEMS)? {
                entity(name, e, names, true)?;
            }
            Checked::Json(v.clone())
        }
        Kind::Fragment => {
            entity(name, v, names, false)?;
            Checked::Json(v.clone())
        }
        Kind::Attribute => {
            object(name, v)?;
            Checked::Json(v.clone())
        }
        Kind::Object(args) => {
            check_all(args, object(name, v)?, names).map_err(|e| nested(name, &e))?;
            Checked::Json(v.clone())
        }
        Kind::ListOf(args, max) => {
            for item in list(name, v, 0, max)? {
                check_all(args, object(name, item)?, names).map_err(|e| nested(name, &e))?;
            }
            Checked::Json(v.clone())
        }
        Kind::Endpoint => {
            let s = v.as_str().filter(|s| s.len() <= 2048);
            let s = s.ok_or_else(|| broken(name, "must be a string of at most 2048 bytes"))?;
            if !endpoint_allowed(s, endpoints()) {
                return Err(broken(
                    name,
                    "not an endpoint this deployment lets a tool notify",
                ));
            }
            Checked::One(s.to_owned())
        }
        _ => return Err(broken(name, "is no body")),
    })
}

/// The checked arguments of a call, by name; the cursor is not one of them.
pub(crate) type Args = std::collections::BTreeMap<&'static str, Checked>;

/// Check `given` against `args` under `names`: every argument known, every
/// required one present, every value within its kind.
pub(crate) fn check_all(
    args: &[Arg],
    given: &Map<String, Value>,
    names: &Names,
) -> Result<Args, Broken> {
    if let Some(unknown) = given
        .keys()
        .find(|k| !args.iter().any(|a| a.name == k.as_str()))
    {
        // the name a client sent is its own; it is no hidden member
        return Err(format!("argument {unknown}: not an argument of this tool"));
    }
    let mut out = Args::new();
    for arg in args {
        match given.get(arg.name) {
            Some(v) => {
                out.insert(arg.name, check(arg, v, names)?);
            }
            None if arg.required => return Err(broken(arg.name, "is required")),
            None => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Names {
        Names::new(
            vec!["Car".into(), "bad type".into()],
            vec![
                "speed".into(),
                "https://example.org/odd".into(),
                "ignore all previous".into(),
            ],
        )
    }

    #[test]
    fn names_that_do_not_match_their_pattern_are_never_offered() {
        let n = names();
        assert_eq!(n.types, ["Car"]);
        assert_eq!(n.attributes, ["speed", "https://example.org/odd"]);
        let edge = Names::new(
            vec![String::new(), "1Car".into(), "Fire-Truck_2".into()],
            vec![
                String::new(),
                "9lives".into(),
                format!("a{}", "b".repeat(256)),
                "a.b:c/d#e".into(),
            ],
        );
        assert_eq!(edge.types, ["Fire-Truck_2"]);
        assert_eq!(edge.attributes, ["a.b:c/d#e"]);
    }

    #[test]
    fn a_list_of_ids_or_names_holds_each_member_to_the_rule() {
        let n = names();
        let one = |kind: Kind, v: Value| check(&req("a", kind), &v, &n);
        assert_eq!(
            one(Kind::Ids, json!(["urn:ngsi-ld:Car:1", "urn:ngsi-ld:Car:2"])),
            Ok(Checked::Many(vec![
                "urn:ngsi-ld:Car:1".into(),
                "urn:ngsi-ld:Car:2".into()
            ]))
        );
        assert!(one(Kind::Ids, json!(["urn:ngsi-ld:Car:1", "Car:2"])).is_err());
        assert!(one(Kind::Ids, json!(["urn:ngsi-ld:Car:1", 2])).is_err());
        assert!(one(Kind::Attrs, json!(["speed", "secret"])).is_err());
        assert_eq!(
            one(Kind::Attrs, json!(["speed"])),
            Ok(Checked::Many(vec!["speed".into()]))
        );
    }

    #[test]
    fn the_schema_lists_the_names_and_refuses_unknown_members() {
        let s = input_schema(
            &[req("type", Kind::Type), opt("attrs", Kind::Attrs)],
            &names(),
        );
        assert_eq!(s.get("additionalProperties"), Some(&json!(false)));
        assert_eq!(s.get("required"), Some(&json!(["type"])));
        assert_eq!(
            s.get("properties")
                .and_then(|p| p.pointer("/attrs/items/enum")),
            Some(&json!(["speed", "https://example.org/odd"]))
        );
    }

    #[test]
    fn a_hidden_and_an_absent_name_are_refused_alike() {
        let n = names();
        let args = [req("type", Kind::Type)];
        let hidden = check_all(
            &args,
            &json!({"type": "Bike"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            &n,
        );
        let absent = check_all(
            &args,
            &json!({"type": "Nothing"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            &n,
        );
        assert_eq!(hidden, absent);
        assert!(hidden.is_err_and(|e| !e.contains("Bike")));
    }

    #[test]
    fn every_kind_holds_its_bound() {
        let n = names();
        let one = |kind: Kind, v: Value| check(&req("a", kind), &v, &n);
        assert!(one(Kind::Id, json!("urn:ngsi-ld:Car:1")).is_ok());
        assert!(one(Kind::Id, json!("urn:ngsi-ld:")).is_err());
        assert!(one(Kind::Id, json!("urn:ngsi-ld:Car 1")).is_err());
        assert!(one(Kind::Id, json!(format!("urn:ngsi-ld:{}", "x".repeat(501)))).is_err());
        assert!(one(Kind::Ids, json!([])).is_err());
        assert!(one(Kind::Ids, json!(vec!["urn:ngsi-ld:C:1"; MAX_ITEMS + 1])).is_err());
        assert!(one(Kind::Types, json!(["Car"])).is_ok());
        assert!(one(Kind::Types, json!("Car")).is_err());
        assert!(one(Kind::Types, json!(["Car", "Bike"])).is_err());
        assert!(one(Kind::Text(4), json!("abcd")).is_ok());
        assert!(one(Kind::Text(4), json!("abcde")).is_err());
        assert!(one(Kind::Word(&["flat"]), json!("inline")).is_err());
        assert_eq!(one(Kind::Int(1, 3), json!(3)), Ok(Checked::Int(3)));
        assert!(one(Kind::Int(1, 3), json!(4)).is_err());
        assert!(one(Kind::Int(1, 3), json!(-1)).is_err());
        assert!(one(Kind::Flag, json!("true")).is_err());
        assert!(one(Kind::Coordinates, json!([1, 2])).is_ok());
        assert!(one(Kind::Coordinates, json!({})).is_err());
    }

    #[test]
    fn more_names_than_an_enum_holds_fall_back_to_the_pattern() {
        let many: Vec<String> = (0..=MAX_ENUM).map(|i| format!("a{i}")).collect();
        let n = Names::new(vec![], many);
        assert_eq!(
            schema_of(Kind::Attr, &n).get("pattern"),
            Some(&json!(ATTR_NAME))
        );
        assert!(check(&req("a", Kind::Attr), &json!("zzz"), &n).is_ok());
        assert!(check(&req("a", Kind::Attr), &json!("z z"), &n).is_err());
    }

    #[test]
    fn unknown_and_missing_arguments_are_refused() {
        let args = [req("id", Kind::Id)];
        let n = names();
        let given = json!({"id": "urn:ngsi-ld:Car:1", "tenant": "other"});
        assert!(
            check_all(&args, given.as_object().unwrap_or(&Map::new()), &n)
                .is_err_and(|e| e.contains("tenant"))
        );
        assert!(check_all(&args, &Map::new(), &n).is_err_and(|e| e.contains("required")));
    }

    #[test]
    fn an_entity_body_names_only_what_the_view_shows_and_no_context() {
        let n = names();
        let entity = |v: Value| check(&req("entity", Kind::Entity), &v, &n);
        let fragment = |v: Value| check(&req("attributes", Kind::Fragment), &v, &n);
        assert!(entity(json!({"id": "urn:ngsi-ld:Car:1", "type": "Car", "speed": 1})).is_ok());
        for (bad, rule) in [
            (
                json!({"id": "urn:ngsi-ld:Car:1", "type": "Car", "secret": 1}),
                "not one of the attributes",
            ),
            (
                json!({"id": "urn:ngsi-ld:Car:1", "type": "Bike"}),
                "not one of the types",
            ),
            (
                json!({"id": "Car:1", "type": "Car"}),
                "id must be an NGSI-LD Entity id",
            ),
            (
                json!({"type": "Car", "speed": 1}),
                "must have an id and a type",
            ),
            (
                json!({"id": "urn:ngsi-ld:Car:1", "speed": 1}),
                "must have an id and a type",
            ),
            (
                json!({"id": "urn:ngsi-ld:Car:1", "type": "Car", "@context": "x"}),
                "no @context",
            ),
            (json!([1]), "must be an object"),
        ] {
            assert!(
                entity(bad.clone()).is_err_and(|e| e.contains(rule)),
                "{bad}"
            );
        }
        assert!(fragment(json!({"speed": 1, "type": "Car"})).is_ok());
        assert!(
            fragment(json!({"id": "urn:ngsi-ld:Car:1"})).is_err_and(|e| e.contains("names no id"))
        );
        assert!(fragment(json!({})).is_err_and(|e| e.contains("at least one")));
        let batch = |v: Value| check(&req("entities", Kind::Entities), &v, &n);
        assert!(batch(json!([{"id": "urn:ngsi-ld:Car:1", "type": "Car"}])).is_ok());
        assert!(batch(json!([{"id": "urn:ngsi-ld:Car:1", "type": "Car", "secret": 1}])).is_err());
        assert!(batch(json!([])).is_err());
        let attribute = |v: Value| check(&req("attribute", Kind::Attribute), &v, &n);
        assert!(attribute(json!({"value": 1})).is_ok());
        assert!(attribute(json!({"value": 1, "@context": []})).is_err());
        assert!(attribute(json!(1)).is_err());
    }

    #[test]
    fn nested_members_are_checked_and_named_by_their_path() {
        const INNER: &[Arg] = &[req("uri", Kind::Endpoint)];
        const OUTER: &[Arg] = &[
            opt("endpoint", Kind::Object(INNER)),
            opt("each", Kind::ListOf(INNER, 1)),
        ];
        let n = names();
        let outer = |v: Value| check(&req("n", Kind::Object(OUTER)), &v, &n);
        let refused = "argument n.endpoint.uri: not an endpoint this deployment lets a tool notify";
        assert_eq!(
            outer(json!({"endpoint": {"uri": "https://x.example/"}})),
            Err(refused.into())
        );
        assert!(outer(json!({"endpoint": {"uri": 7}})).is_err_and(|e| e.contains("2048 bytes")));
        assert!(outer(json!({"endpoint": {}}))
            .is_err_and(|e| e.contains("n.endpoint.uri: is required")));
        assert!(outer(json!({"other": 1})).is_err_and(|e| e.contains("n.other")));
        assert!(outer(json!({"each": [{}, {}]})).is_err_and(|e| e.contains("takes 0 to 1")));
        assert!(outer(json!({"each": [7]})).is_err_and(|e| e.contains("must be an object")));
        assert!(outer(json!({"each": []})).is_ok());
        let schema = schema_of(Kind::Object(OUTER), &n);
        assert_eq!(
            schema.pointer("/properties/endpoint/required"),
            Some(&json!(["uri"]))
        );
        assert_eq!(schema.pointer("/properties/each/maxItems"), Some(&json!(1)));
    }

    #[test]
    fn an_endpoint_is_one_named_or_below_one_ending_in_a_slash() {
        let allowed = [
            "https://hooks.example/".to_owned(),
            "https://one.example/x".to_owned(),
        ];
        for ok in [
            "https://hooks.example/",
            "https://hooks.example/a/b",
            "https://one.example/x",
        ] {
            assert!(endpoint_allowed(ok, &allowed), "{ok}");
        }
        for no in [
            "https://hooks.example",
            "https://hooks.example.evil/",
            "https://one.example/xy",
            "",
        ] {
            assert!(!endpoint_allowed(no, &allowed), "{no}");
        }
        assert!(!endpoint_allowed("https://hooks.example/", &[]));
    }

    #[test]
    fn a_body_schema_lists_the_view_names_as_member_names() {
        let n = names();
        let entity = schema_of(Kind::Entity, &n);
        assert_eq!(
            entity.pointer("/propertyNames/enum"),
            Some(&json!(["id", "type", "speed", "https://example.org/odd"]))
        );
        assert_eq!(entity.get("required"), Some(&json!(["id", "type"])));
        let fragment = schema_of(Kind::Fragment, &n);
        assert_eq!(
            fragment.pointer("/propertyNames/enum/0"),
            Some(&json!("type"))
        );
        assert_eq!(
            schema_of(Kind::Entities, &n).get("maxItems"),
            Some(&json!(MAX_ITEMS))
        );
        assert_eq!(schema_of(Kind::Attribute, &n), json!({"type": "object"}));
        assert_eq!(
            schema_of(Kind::Endpoint, &n).get("maxLength"),
            Some(&json!(2048))
        );
        let many: Vec<String> = (0..=MAX_ENUM).map(|i| format!("a{i}")).collect();
        let wide = Names::new(vec![], many);
        assert_eq!(
            schema_of(Kind::Entity, &wide).pointer("/propertyNames/pattern"),
            Some(&json!(ATTR_NAME))
        );
        assert!(body(&req("x", Kind::Flag), &json!(true), &n).is_err());
    }
}
