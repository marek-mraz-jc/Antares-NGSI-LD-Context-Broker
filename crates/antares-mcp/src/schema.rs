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
}

/// One argument of a tool.
#[derive(Debug, Clone, Copy)]
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
    }
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
}
