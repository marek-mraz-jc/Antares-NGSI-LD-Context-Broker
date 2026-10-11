// SPDX-License-Identifier: EUPL-1.2
//! `$filter` to NGSI-LD `q` and `id` (odata-linkml-mapping.md 5.2), and
//! `$orderby` to `orderBy` (Q9: `id` and scalar Attributes). The
//! subset of this round: comparisons of a scalar member with a literal
//! (F1 to F6, F26), `and`/`or` (F7, F8), `null` (F2, F4, F10), `in` (F12),
//! `contains`/`startswith`/`endswith` and their `not` (F9, F15), and `id`
//! as a top-level conjunct (F28). Everything else is `Later` (501) or
//! `Bad` (400) before any operation runs.
//!
//! A `q` names Attributes by term, and a term is a letter followed by
//! letters, digits or `_` (7.2.3.2), and `orderBy` reads a `.` as a path
//! (4.23): a schema IRI is neither. Each IRI the request names gets the
//! term `t{n}`, and the request carries the
//! `@context` that defines them (`OpsRequest::context`).

use crate::edm::{Member, Set, Shape};
use odata_url::{CompareOp, Expr, Literal, MemberPath, Method, OrderBy};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The broker's bound on a `q` (`antares_ql::MAX_Q_BYTES`): a longer
/// translation is refused here with the OData error a bound gets.
pub const MAX_Q_BYTES: usize = 4096;

/// The largest integer an `f64` holds exactly: NGSI-LD compares numbers as
/// `f64` (F26).
const MAX_EXACT: i64 = 1 << 53;

/// Why a filter is not translated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// A construct a later round maps or no round does: 501.
    Later,
    /// A filter that cannot hold for this model: 400.
    Bad,
    /// Past the broker's bound on a `q`: 400 `TooComplexQuery`.
    TooComplex,
}

/// A translated filter.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Translated {
    /// The `q`, if any term needs one.
    pub(crate) q: Option<String>,
    /// The ids of an `id` conjunct.
    pub(crate) ids: Option<Vec<String>>,
    /// The `orderBy`, if the request orders.
    pub(crate) order: Option<String>,
    /// The terms `q` and `orderBy` use, term to IRI.
    pub(crate) context: Map<String, Value>,
    /// Whether `q` follows a Relationship (F24): the broker evaluates a
    /// hop only under a `join` (5.7.2.4).
    pub(crate) linked: bool,
}

/// Translates `filter` against the members of `set` and, through a
/// navigation, of the `sets` it targets, with the parameter aliases of the
/// request bound.
pub(crate) fn translate(
    filter: &Expr,
    set: &Set,
    sets: &BTreeMap<String, Set>,
    aliases: &[(String, Literal)],
) -> Result<Translated, Refused> {
    let mut out = Translated::default();
    let mut terms = Terms {
        set,
        sets,
        aliases,
        context: &mut out.context,
        linked: false,
    };
    let mut conjuncts = Vec::new();
    for conjunct in conjuncts_of(filter) {
        match ids_of(conjunct, aliases)? {
            Some(ids) if out.ids.is_none() => out.ids = Some(ids),
            // ponytail: one id conjunct; two would intersect, no client sends it
            Some(_) => return Err(Refused::Later),
            None => conjuncts.push(format!("({})", terms.term(conjunct)?)),
        }
    }
    out.linked = terms.linked;
    if !conjuncts.is_empty() {
        let q = conjuncts.join(";");
        if q.len() > MAX_Q_BYTES {
            return Err(Refused::TooComplex);
        }
        out.q = Some(q);
    }
    Ok(out)
}

/// Adds `$orderby` to `out` as `orderBy` (Q9) over the same terms: `id`, or
/// a scalar Attribute; a member the model marks not sortable (C12) is 400,
/// the base members and the other shapes are a later round's.
pub(crate) fn order(items: &[OrderBy], set: &Set, out: &mut Translated) -> Result<(), Refused> {
    // `orderBy` names Attributes of the Entity itself: no navigation.
    let sets = BTreeMap::new();
    let mut terms = Terms {
        set,
        sets: &sets,
        aliases: &[],
        context: &mut out.context,
        linked: false,
    };
    let mut keys = Vec::new();
    for item in items {
        let single = match &item.expr {
            Expr::Member(MemberPath {
                variable: None,
                path,
            }) => match path.as_slice() {
                [name] => Some(name.as_str()),
                _ => None,
            },
            _ => None,
        };
        let key = match single {
            None => return Err(Refused::Later),
            Some("id") => "id".to_owned(),
            Some(name) if set.members.get(name).is_some_and(|m| !m.shape.sortable()) => {
                return Err(Refused::Bad)
            }
            _ => terms.scalar(&item.expr)?,
        };
        keys.push(if item.descending {
            format!("{key};desc")
        } else {
            key
        });
    }
    out.order = Some(keys.join(","));
    Ok(())
}

/// The operands of the top-level `and`s.
fn conjuncts_of(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::And(l, r) => {
            let mut all = conjuncts_of(l);
            all.extend(conjuncts_of(r));
            all
        }
        other => vec![other],
    }
}

/// F28: `id eq 'u'` or `id in ('u', ...)`, `None` for any other conjunct.
fn ids_of(e: &Expr, aliases: &[(String, Literal)]) -> Result<Option<Vec<String>>, Refused> {
    let is_id = |m: &Expr| matches!(m, Expr::Member(MemberPath { variable: None, path }) if path.len() == 1 && path.first().is_some_and(|p| p == "id"));
    let literals: Vec<&Literal> = match e {
        Expr::Compare(l, CompareOp::Eq, r) if is_id(l) => vec![literal(r, aliases)?],
        Expr::Compare(l, CompareOp::Eq, r) if is_id(r) => vec![literal(l, aliases)?],
        Expr::In(l, list) if is_id(l) => list.iter().collect(),
        _ => return Ok(None),
    };
    literals
        .into_iter()
        .map(|l| match l {
            Literal::String(s) => Ok(s.clone()),
            _ => Err(Refused::Bad),
        })
        .collect::<Result<_, _>>()
        .map(Some)
}

/// The literal an operand is, through an alias.
fn literal<'a>(e: &'a Expr, aliases: &'a [(String, Literal)]) -> Result<&'a Literal, Refused> {
    match e {
        Expr::Literal(l) => Ok(l),
        Expr::Alias(name) => aliases
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, l)| l)
            .ok_or(Refused::Bad),
        _ => Err(Refused::Later),
    }
}

struct Terms<'a> {
    set: &'a Set,
    sets: &'a BTreeMap<String, Set>,
    aliases: &'a [(String, Literal)],
    context: &'a mut Map<String, Value>,
    linked: bool,
}

impl<'a> Terms<'a> {
    /// One boolean expression as `q`.
    fn term(&mut self, e: &Expr) -> Result<String, Refused> {
        match e {
            Expr::And(l, r) => Ok(format!("({});({})", self.term(l)?, self.term(r)?)),
            Expr::Or(l, r) => Ok(format!("({})|({})", self.term(l)?, self.term(r)?)),
            Expr::Compare(l, op, r) => self.compare(l, *op, r),
            Expr::In(l, list) => {
                let name = self.scalar(l)?;
                let values = list.iter().map(value).collect::<Result<Vec<_>, _>>()?;
                Ok(format!("{name}=={}", values.join(",")))
            }
            Expr::Call(method, args) => self.call(*method, args, false),
            Expr::Not(inner) => match inner.as_ref() {
                Expr::Call(method, args) => self.call(*method, args, true),
                Expr::Compare(l, CompareOp::Eq, r)
                    if matches!(literal(r, self.aliases), Ok(Literal::Null)) =>
                {
                    Ok(self.scalar(l)?)
                }
                // F11: `q` has no general negation.
                _ => Err(Refused::Later),
            },
            // `$filter=flag`: a Boolean member as the predicate.
            Expr::Member(_) => Ok(format!("{}==true", self.scalar(e)?)),
            Expr::Literal(_) | Expr::Alias(_) | Expr::Any { .. } => Err(Refused::Later),
        }
    }

    /// F1 to F6, F2/F4 on `null`, F25 refused.
    fn compare(&mut self, l: &Expr, op: CompareOp, r: &Expr) -> Result<String, Refused> {
        let (member, op, lit) = match (literal(l, self.aliases), literal(r, self.aliases)) {
            (Err(_), Ok(lit)) => (l, op, lit),
            (Ok(lit), Err(_)) => (r, mirrored(op), lit),
            // F25: two members, or two literals.
            _ => return Err(Refused::Later),
        };
        let name = self.scalar(member)?;
        Ok(match (op, lit) {
            (CompareOp::Eq, Literal::Null) => format!("!{name}"),
            (CompareOp::Ne, Literal::Null) => name,
            (_, Literal::Null) => return Err(Refused::Later),
            (CompareOp::Eq, lit) => format!("{name}=={}", value(lit)?),
            // F3: OData `ne` holds on an absent member, NGSI-LD `!=` does not.
            (CompareOp::Ne, lit) => format!("!{name}|{name}!={}", value(lit)?),
            (CompareOp::Gt, lit) => format!("{name}>{}", value(lit)?),
            (CompareOp::Ge, lit) => format!("{name}>={}", value(lit)?),
            (CompareOp::Lt, lit) => format!("{name}<{}", value(lit)?),
            (CompareOp::Le, lit) => format!("{name}<={}", value(lit)?),
        })
    }

    /// F9, F15: the literal regex-escaped, anchored for starts and ends.
    fn call(&mut self, method: Method, args: &[Expr], negated: bool) -> Result<String, Refused> {
        let [member, operand] = args else {
            return Err(Refused::Later);
        };
        let (Ok(Literal::String(s)), Method::Contains | Method::StartsWith | Method::EndsWith) =
            (literal(operand, self.aliases), method)
        else {
            return Err(Refused::Later);
        };
        let name = self.scalar(member)?;
        // URL 5.1.1.5: the string functions take a string; NGSI-LD would
        // match no other value without saying why.
        let string = matches!(member, Expr::Member(MemberPath { path, .. })
            if self.leaf(path).is_some_and(|(_, m)| matches!(m.shape, Shape::Value { edm: "Edm.String", .. })));
        if !string {
            return Err(Refused::Bad);
        }
        let escaped = escape(s);
        let pattern = match method {
            Method::StartsWith => format!("^{escaped}"),
            Method::EndsWith => format!("{escaped}$"),
            _ => escaped,
        };
        let op = if negated { "!~=" } else { "~=" };
        Ok(format!("{name}{op}\"{pattern}\""))
    }

    /// The member a path ends in, and the navigation it goes through: a
    /// member of the set, or (F24) a member of the set a single-valued
    /// navigation targets.
    fn leaf(&self, path: &[String]) -> Option<(Option<(&'a Member, &'a Set)>, &'a Member)> {
        match path {
            [name] => Some((None, self.set.members.get(name)?)),
            [nav, name] => {
                let via = self.set.members.get(nav)?;
                let Shape::Nav {
                    target,
                    collection: false,
                } = &via.shape
                else {
                    return None;
                };
                let target = self.sets.get(target)?;
                Some((Some((via, target)), target.members.get(name)?))
            }
            _ => None,
        }
    }

    /// The term of a scalar member: filterable, not a collection (a
    /// collection's `eq` is membership in NGSI-LD and not in OData, F22),
    /// of the set or one hop away. A hop is `rel{Type:attr}` (7.2.3.2
    /// EXAMPLE 15); the broker resolves its target through the caller's
    /// narrowing, a hidden one like a dangling link (T-3474, O26).
    fn scalar(&mut self, e: &Expr) -> Result<String, Refused> {
        let Expr::Member(MemberPath {
            variable: None,
            path,
        }) = e
        else {
            return Err(Refused::Later);
        };
        // The base members (`id` outside a top-level conjunct, `scope`,
        // `_types`, the system members), complex and annotation paths
        // (F20, F21, F22, F28, F33) are a later round's.
        let (via, member) = self.leaf(path).ok_or(Refused::Later)?;
        match &member.shape {
            // The same test the model's NonFilterableProperties and
            // NonSortableProperties are written from (C11, C12).
            s if s.scalar() => {}
            Shape::Json => return Err(Refused::Bad),
            _ => return Err(Refused::Later),
        }
        let attr = self.term_of(&member.iri);
        Ok(match via {
            None => attr,
            Some((rel, target)) => {
                self.linked = true;
                let rel = self.term_of(&rel.iri);
                let ty = self.term_of(&target.type_iri);
                format!("{rel}{{{ty}:{attr}}}")
            }
        })
    }

    /// The term `t{n}` of an IRI, minted on first use.
    fn term_of(&mut self, iri: &str) -> String {
        let found = self
            .context
            .iter()
            .find(|(_, defined)| defined.as_str() == Some(iri))
            .map(|(term, _)| term.clone());
        found.unwrap_or_else(|| {
            let next = format!("t{}", self.context.len());
            self.context.insert(next.clone(), Value::from(iri));
            next
        })
    }
}

/// `v op p` read as `p op' v`.
fn mirrored(op: CompareOp) -> CompareOp {
    match op {
        CompareOp::Gt => CompareOp::Lt,
        CompareOp::Ge => CompareOp::Le,
        CompareOp::Lt => CompareOp::Gt,
        CompareOp::Le => CompareOp::Ge,
        same => same,
    }
}

/// A literal as a `q` value (F26, F27).
fn value(lit: &Literal) -> Result<String, Refused> {
    match lit {
        Literal::Bool(b) => Ok(b.to_string()),
        Literal::Int(n) if (-MAX_EXACT..=MAX_EXACT).contains(n) => Ok(n.to_string()),
        Literal::Int(_) => Err(Refused::Bad),
        Literal::Decimal(raw) => match raw.parse::<f64>() {
            Ok(n) if n.is_finite() => Ok(Value::from(n).to_string()),
            _ => Err(Refused::Bad),
        },
        Literal::String(s) => Ok(Value::from(s.as_str()).to_string()),
        Literal::Date(d) => Ok(d.clone()),
        Literal::DateTimeOffset(t) if t.ends_with('Z') => Ok(t.clone()),
        Literal::TimeOfDay(t) => Ok(format!("{t}Z")),
        // An offset other than Z, null in a list, a geography: later rounds.
        Literal::DateTimeOffset(_) | Literal::Null | Literal::Geography(_) => Err(Refused::Later),
    }
}

/// The characters of `s` with every regular-expression metacharacter
/// escaped, and `"` as `\x22`: the quoted `RegExp` of `q` is taken as
/// written up to the next quote.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '"' {
            out.push_str(r"\x22");
            continue;
        }
        if r"\.^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edm::Member;
    use std::collections::BTreeMap;

    fn set() -> Set {
        let member = |iri: &str, shape| Member {
            iri: iri.into(),
            shape,
        };
        let value = |edm, collection| Shape::Value { edm, collection };
        Set {
            type_iri: "https://e.org/Car".into(),
            members: BTreeMap::from([
                (
                    "speed".into(),
                    member("https://e.org/speed", value("Edm.Double", false)),
                ),
                (
                    "name".into(),
                    member("https://e.org/name", value("Edm.String", false)),
                ),
                (
                    "on".into(),
                    member("https://e.org/on", value("Edm.Boolean", false)),
                ),
                (
                    "tags".into(),
                    member("https://e.org/tags", value("Edm.String", true)),
                ),
                ("doc".into(), member("https://e.org/doc", Shape::Json)),
                (
                    "owner".into(),
                    member("https://e.org/owner", Shape::Url { collection: false }),
                ),
            ]),
        }
    }

    fn m(name: &str) -> Expr {
        Expr::Member(MemberPath {
            variable: None,
            path: vec![name.into()],
        })
    }

    fn lit(l: Literal) -> Expr {
        Expr::Literal(l)
    }

    fn cmp(l: Expr, op: CompareOp, r: Expr) -> Expr {
        Expr::Compare(Box::new(l), op, Box::new(r))
    }

    fn q(e: &Expr) -> Result<String, Refused> {
        translate(
            e,
            &set(),
            &BTreeMap::new(),
            &[("a".into(), Literal::Int(3))],
        )
        .map(|t| t.q.unwrap_or_default())
    }

    #[test]
    fn comparisons_mirror_and_name_their_terms() {
        let speed = |op, n| cmp(m("speed"), op, lit(Literal::Int(n)));
        assert_eq!(q(&speed(CompareOp::Eq, 1)), Ok("(t0==1)".into()));
        assert_eq!(q(&speed(CompareOp::Gt, 1)), Ok("(t0>1)".into()));
        assert_eq!(q(&speed(CompareOp::Ge, 1)), Ok("(t0>=1)".into()));
        assert_eq!(q(&speed(CompareOp::Lt, 1)), Ok("(t0<1)".into()));
        assert_eq!(q(&speed(CompareOp::Le, 1)), Ok("(t0<=1)".into()));
        assert_eq!(
            q(&speed(CompareOp::Ne, 1)),
            Ok("(!t0|t0!=1)".into()),
            "F3: ne holds on the absent member"
        );
        for (op, want) in [
            (CompareOp::Gt, "<"),
            (CompareOp::Ge, "<="),
            (CompareOp::Lt, ">"),
            (CompareOp::Le, ">="),
            (CompareOp::Eq, "=="),
        ] {
            let e = cmp(lit(Literal::Int(1)), op, m("speed"));
            assert_eq!(q(&e), Ok(format!("(t0{want}1)")), "{op:?}");
        }
        let alias = cmp(m("speed"), CompareOp::Lt, Expr::Alias("a".into()));
        assert_eq!(q(&alias), Ok("(t0<3)".into()));
        let unbound = cmp(m("speed"), CompareOp::Lt, Expr::Alias("b".into()));
        assert_eq!(
            q(&unbound),
            Err(Refused::Later),
            "an unbound alias is no literal"
        );
    }

    #[test]
    fn and_or_group_and_one_iri_is_one_term() {
        let e = Expr::And(
            Box::new(cmp(m("speed"), CompareOp::Gt, lit(Literal::Int(1)))),
            Box::new(Expr::Or(
                Box::new(cmp(
                    m("name"),
                    CompareOp::Eq,
                    lit(Literal::String("a\"b".into())),
                )),
                Box::new(cmp(
                    m("speed"),
                    CompareOp::Lt,
                    lit(Literal::Decimal("0.5".into())),
                )),
            )),
        );
        let t = translate(&e, &set(), &BTreeMap::new(), &[]).unwrap_or_default();
        assert_eq!(t.q.as_deref(), Some(r#"(t0>1);((t1=="a\"b")|(t0<0.5))"#));
        assert_eq!(t.context.len(), 2);
        assert_eq!(t.context["t1"], "https://e.org/name");
        assert_eq!(t.ids, None);
        let nested = Expr::Or(
            Box::new(m("on")),
            Box::new(Expr::And(Box::new(m("on")), Box::new(m("on")))),
        );
        assert_eq!(
            q(&nested),
            Ok("((t0==true)|((t0==true);(t0==true)))".into())
        );
    }

    #[test]
    fn null_in_and_flags() {
        let null = |op| cmp(m("name"), op, lit(Literal::Null));
        assert_eq!(q(&null(CompareOp::Eq)), Ok("(!t0)".into()));
        assert_eq!(q(&null(CompareOp::Ne)), Ok("(t0)".into()));
        assert_eq!(
            q(&Expr::Not(Box::new(null(CompareOp::Eq)))),
            Ok("(t0)".into())
        );
        assert_eq!(q(&null(CompareOp::Gt)), Err(Refused::Later));
        let within = Expr::In(
            Box::new(m("speed")),
            vec![Literal::Int(1), Literal::Bool(true)],
        );
        assert_eq!(q(&within), Ok("(t0==1,true)".into()));
        assert_eq!(
            q(&Expr::In(Box::new(m("speed")), vec![Literal::Null])),
            Err(Refused::Later)
        );
        assert_eq!(q(&m("on")), Ok("(t0==true)".into()));
    }

    #[test]
    fn a_path_past_one_hop_is_later() {
        let deep = Expr::Member(MemberPath {
            variable: None,
            path: vec!["nav".into(), "name".into(), "x".into()],
        });
        assert_eq!(
            q(&cmp(deep, CompareOp::Eq, lit(Literal::Int(1)))),
            Err(Refused::Later)
        );
    }

    #[test]
    fn patterns_are_escaped_and_anchored() {
        let call =
            |method, s: &str| Expr::Call(method, vec![m("name"), lit(Literal::String(s.into()))]);
        assert_eq!(
            q(&call(Method::Contains, "a.b")),
            Ok(r#"(t0~="a\.b")"#.into())
        );
        assert_eq!(
            q(&call(Method::StartsWith, "(x")),
            Ok(r#"(t0~="^\(x")"#.into())
        );
        assert_eq!(q(&call(Method::EndsWith, "$")), Ok(r#"(t0~="\$$")"#.into()));
        let not = Expr::Not(Box::new(call(Method::Contains, "x")));
        assert_eq!(q(&not), Ok(r#"(t0!~="x")"#.into()), "F9");
        assert_eq!(
            q(&call(Method::Contains, "a\"b\\")),
            Ok(r#"(t0~="a\x22b\\")"#.into())
        );
        assert_eq!(q(&call(Method::GeoIntersects, "x")), Err(Refused::Later));
        let one_arg = Expr::Call(Method::Contains, vec![m("name")]);
        assert_eq!(q(&one_arg), Err(Refused::Later));
        let number = Expr::Call(Method::Contains, vec![m("name"), lit(Literal::Int(1))]);
        assert_eq!(q(&number), Err(Refused::Later));
        let on_a_number = Expr::Call(
            Method::Contains,
            vec![m("speed"), lit(Literal::String("1".into()))],
        );
        assert_eq!(
            q(&on_a_number),
            Err(Refused::Bad),
            "a string function on a Double"
        );
    }

    #[test]
    fn ids_are_a_top_level_conjunct() {
        let id = |l: &str| cmp(m("id"), CompareOp::Eq, lit(Literal::String(l.into())));
        let t = translate(&id("urn:a"), &set(), &BTreeMap::new(), &[]).unwrap_or_default();
        assert_eq!((t.q, t.ids), (None, Some(vec!["urn:a".to_owned()])));
        let mirrored = cmp(lit(Literal::String("urn:b".into())), CompareOp::Eq, m("id"));
        let both = Expr::And(
            Box::new(Expr::In(
                Box::new(m("id")),
                vec![Literal::String("urn:a".into())],
            )),
            Box::new(cmp(m("speed"), CompareOp::Gt, lit(Literal::Int(0)))),
        );
        let t = translate(&both, &set(), &BTreeMap::new(), &[]).unwrap_or_default();
        assert_eq!(
            (t.q.as_deref(), t.ids),
            (Some("(t0>0)"), Some(vec!["urn:a".to_owned()]))
        );
        let twice = Expr::And(Box::new(id("urn:a")), Box::new(mirrored));
        assert_eq!(
            translate(&twice, &set(), &BTreeMap::new(), &[]),
            Err(Refused::Later)
        );
        let number = cmp(m("id"), CompareOp::Eq, lit(Literal::Int(1)));
        assert_eq!(
            translate(&number, &set(), &BTreeMap::new(), &[]),
            Err(Refused::Bad)
        );
        let nested = Expr::Or(Box::new(id("urn:a")), Box::new(id("urn:b")));
        assert_eq!(
            translate(&nested, &set(), &BTreeMap::new(), &[]),
            Err(Refused::Later),
            "F28: top level only"
        );
        let unbound = cmp(m("id"), CompareOp::Eq, Expr::Alias("x".into()));
        assert_eq!(
            translate(&unbound, &set(), &BTreeMap::new(), &[]),
            Err(Refused::Bad)
        );
    }

    #[test]
    fn what_this_round_does_not_map_is_refused() {
        let later = [
            cmp(m("speed"), CompareOp::Eq, m("speed")),
            cmp(lit(Literal::Int(1)), CompareOp::Eq, lit(Literal::Int(1))),
            Expr::Not(Box::new(cmp(
                m("speed"),
                CompareOp::Gt,
                lit(Literal::Int(1)),
            ))),
            cmp(m("tags"), CompareOp::Eq, lit(Literal::String("a".into()))),
            cmp(m("owner"), CompareOp::Eq, lit(Literal::String("a".into()))),
            cmp(m("scope"), CompareOp::Eq, lit(Literal::String("/a".into()))),
            cmp(
                Expr::Member(MemberPath {
                    variable: None,
                    path: vec!["speed".into(), "x".into()],
                }),
                CompareOp::Eq,
                lit(Literal::Int(1)),
            ),
            cmp(
                Expr::Member(MemberPath {
                    variable: Some("x".into()),
                    path: vec!["speed".into()],
                }),
                CompareOp::Eq,
                lit(Literal::Int(1)),
            ),
            cmp(
                m("name"),
                CompareOp::Eq,
                lit(Literal::DateTimeOffset("2026-01-01T00:00:00+01:00".into())),
            ),
            Expr::Literal(Literal::Bool(true)),
            Expr::Any {
                collection: MemberPath {
                    variable: None,
                    path: vec!["tags".into()],
                },
                lambda: None,
            },
        ];
        for e in later {
            assert_eq!(q(&e), Err(Refused::Later), "{e:?}");
        }
        for e in [
            cmp(m("doc"), CompareOp::Eq, lit(Literal::Int(1))),
            cmp(m("speed"), CompareOp::Eq, lit(Literal::Int(MAX_EXACT + 1))),
            cmp(
                m("speed"),
                CompareOp::Eq,
                lit(Literal::Decimal("INF".into())),
            ),
        ] {
            assert_eq!(q(&e), Err(Refused::Bad), "{e:?}");
        }
    }

    #[test]
    fn literals_are_q_values() {
        for (lit, want) in [
            (Literal::Bool(false), "false"),
            (Literal::Int(-MAX_EXACT), "-9007199254740992"),
            (Literal::Decimal("1e2".into()), "100.0"),
            (Literal::Date("2026-10-11".into()), "2026-10-11"),
            (
                Literal::DateTimeOffset("2026-10-11T08:00:00Z".into()),
                "2026-10-11T08:00:00Z",
            ),
            (Literal::TimeOfDay("08:00:00".into()), "08:00:00Z"),
        ] {
            assert_eq!(value(&lit), Ok(want.to_owned()), "{lit:?}");
        }
    }

    #[test]
    fn orderby_names_id_and_scalar_terms() {
        let by = |name: &str, descending| OrderBy {
            expr: m(name),
            descending,
        };
        let mut t = translate(
            &cmp(m("name"), CompareOp::Eq, lit(Literal::Int(1))),
            &set(),
            &BTreeMap::new(),
            &[],
        )
        .unwrap_or_default();
        assert_eq!(
            order(
                &[by("speed", true), by("id", false), by("name", false)],
                &set(),
                &mut t
            ),
            Ok(())
        );
        assert_eq!(
            t.order.as_deref(),
            Some("t1;desc,id,t0"),
            "one IRI, one term, shared with q"
        );
        let mut t = Translated::default();
        for (items, refused) in [
            (vec![by("tags", false)], Refused::Bad),
            (vec![by("doc", false)], Refused::Bad),
            (vec![by("owner", false)], Refused::Later),
            (vec![by("scope", false)], Refused::Later),
            (
                vec![OrderBy {
                    expr: lit(Literal::Int(1)),
                    descending: false,
                }],
                Refused::Later,
            ),
            (
                vec![OrderBy {
                    expr: Expr::Member(MemberPath {
                        variable: None,
                        path: vec!["speed".into(), "x".into()],
                    }),
                    descending: false,
                }],
                Refused::Later,
            ),
        ] {
            assert_eq!(order(&items, &set(), &mut t), Err(refused), "{items:?}");
        }
    }

    #[test]
    fn a_translation_past_the_broker_bound_is_too_complex() {
        let long = "x".repeat(MAX_Q_BYTES);
        let e = cmp(m("name"), CompareOp::Eq, lit(Literal::String(long)));
        assert_eq!(
            translate(&e, &set(), &BTreeMap::new(), &[]),
            Err(Refused::TooComplex)
        );
    }
}
