// SPDX-License-Identifier: EUPL-1.2
//! `residual_equals_brute_force` (ADR-0028 Confirmation, the pure part): compiling an expression
//! and evaluating it on a random Entity answers what a direct reference evaluator, written
//! here without the crate's code, answers on the same Entity.

use antares_residual::{compile, eval, Cmp, DateTime, Expr, Func, Lit, Path, Truth};
use proptest::prelude::*;
use serde_json::{json, Map, Value};

const ATTRS: [&str; 3] = ["a", "b", "c"];

/// A leaf value an Attribute holds, or nothing.
#[derive(Debug, Clone, PartialEq)]
enum Leaf {
    Int(i64),
    Str(&'static str),
    Bool(bool),
}

/// The reference form: what the generated expression means, evaluated with `Option<bool>`.
#[derive(Debug, Clone)]
enum Ref {
    Cmp(Cmp, usize, Leaf),
    AddCmp(Cmp, usize, i64, i64),
    IsNull(usize),
    In(usize, Vec<Leaf>),
    And(Vec<Ref>),
    Or(Vec<Ref>),
    Not(Box<Ref>),
}

fn leaf() -> impl Strategy<Value = Leaf> {
    prop_oneof![
        prop_oneof![Just(i64::MIN), Just(i64::MAX), -3i64..=3].prop_map(Leaf::Int),
        prop_oneof![Just("x"), Just("y")].prop_map(Leaf::Str),
        any::<bool>().prop_map(Leaf::Bool),
    ]
}

fn op() -> impl Strategy<Value = Cmp> {
    prop_oneof![
        Just(Cmp::Eq),
        Just(Cmp::Ne),
        Just(Cmp::Lt),
        Just(Cmp::Le),
        Just(Cmp::Gt),
        Just(Cmp::Ge)
    ]
}

fn reference() -> impl Strategy<Value = Ref> {
    let attr = 0..ATTRS.len();
    let base = prop_oneof![
        (op(), attr.clone(), leaf()).prop_map(|(o, a, l)| Ref::Cmp(o, a, l)),
        (
            op(),
            attr.clone(),
            prop_oneof![Just(i64::MAX), -2i64..=2],
            -3i64..=3
        )
            .prop_map(|(o, a, k, v)| Ref::AddCmp(o, a, k, v)),
        attr.clone().prop_map(Ref::IsNull),
        (attr, prop::collection::vec(leaf(), 0..3)).prop_map(|(a, l)| Ref::In(a, l)),
    ];
    base.prop_recursive(4, 32, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(Ref::And),
            prop::collection::vec(inner.clone(), 0..3).prop_map(Ref::Or),
            inner.prop_map(|r| Ref::Not(Box::new(r))),
        ]
    })
}

fn entity() -> impl Strategy<Value = Vec<Option<Leaf>>> {
    prop::collection::vec(prop::option::of(leaf()), ATTRS.len())
}

fn lit(l: &Leaf) -> Lit {
    match l {
        Leaf::Int(i) => Lit::Int(*i),
        Leaf::Str(s) => Lit::Str((*s).to_owned()),
        Leaf::Bool(b) => Lit::Bool(*b),
    }
}

fn path(a: usize) -> Expr {
    Expr::Path(Path::value(ATTRS.get(a).copied().unwrap_or("a")))
}

fn expr(form: &Ref) -> Expr {
    match form {
        Ref::Cmp(op, attr, leaf) => {
            Expr::Cmp(*op, Box::new(path(*attr)), Box::new(Expr::Lit(lit(leaf))))
        }
        Ref::AddCmp(op, attr, addend, against) => Expr::Cmp(
            *op,
            Box::new(Expr::Call(
                Func::Add,
                vec![path(*attr), Expr::Lit(Lit::Int(*addend))],
            )),
            Box::new(Expr::Lit(Lit::Int(*against))),
        ),
        Ref::IsNull(attr) => Expr::IsNull(Box::new(path(*attr))),
        Ref::In(attr, list) => Expr::In(Box::new(path(*attr)), list.iter().map(lit).collect()),
        Ref::And(forms) => Expr::And(forms.iter().map(expr).collect()),
        Ref::Or(forms) => Expr::Or(forms.iter().map(expr).collect()),
        Ref::Not(inner) => Expr::Not(Box::new(expr(inner))),
    }
}

fn holds(o: Cmp, ord: std::cmp::Ordering) -> bool {
    match o {
        Cmp::Eq => ord.is_eq(),
        Cmp::Ne => ord.is_ne(),
        Cmp::Lt => ord.is_lt(),
        Cmp::Le => ord.is_le(),
        Cmp::Gt => ord.is_gt(),
        Cmp::Ge => ord.is_ge(),
    }
}

/// SQL comparison: unknown for an absent side, a type mismatch, or an ordering of booleans.
fn compare(o: Cmp, x: Option<&Leaf>, y: &Leaf) -> Option<bool> {
    match (x?, y) {
        (Leaf::Int(x), Leaf::Int(y)) => Some(holds(o, x.cmp(y))),
        (Leaf::Str(x), Leaf::Str(y)) => Some(holds(o, x.cmp(y))),
        (Leaf::Bool(x), Leaf::Bool(y)) if matches!(o, Cmp::Eq | Cmp::Ne) => {
            Some(holds(o, x.cmp(y)))
        }
        _ => None,
    }
}

/// Kleene folding of the operands' answers: `decisive` wins, then unknown, else `!decisive`.
fn fold(answers: &[Option<bool>], decisive: bool) -> Option<bool> {
    if answers.contains(&Some(decisive)) {
        Some(decisive)
    } else if answers.contains(&None) {
        None
    } else {
        Some(!decisive)
    }
}

fn reference_eval(form: &Ref, entity: &[Option<Leaf>]) -> Option<bool> {
    let at = |attr: usize| entity.get(attr).and_then(Option::as_ref);
    match form {
        Ref::Cmp(op, attr, leaf) => compare(*op, at(*attr), leaf),
        Ref::AddCmp(op, attr, addend, against) => match at(*attr) {
            Some(Leaf::Int(value)) => value
                .checked_add(*addend)
                .map(|sum| holds(*op, sum.cmp(against))),
            _ => None,
        },
        Ref::IsNull(attr) => Some(at(*attr).is_none()),
        // An absent value is unknown even against an empty list; a present one is false there.
        Ref::In(attr, list) => at(*attr).and_then(|_| {
            let hits: Vec<Option<bool>> = list
                .iter()
                .map(|leaf| compare(Cmp::Eq, at(*attr), leaf))
                .collect();
            fold(&hits, true)
        }),
        Ref::And(forms) => fold(
            &forms
                .iter()
                .map(|f| reference_eval(f, entity))
                .collect::<Vec<_>>(),
            false,
        ),
        Ref::Or(forms) => fold(
            &forms
                .iter()
                .map(|f| reference_eval(f, entity))
                .collect::<Vec<_>>(),
            true,
        ),
        Ref::Not(inner) => reference_eval(inner, entity).map(|answer| !answer),
    }
}

fn json_entity(e: &[Option<Leaf>]) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), json!("urn:ngsi-ld:T:1"));
    m.insert("type".into(), json!("T"));
    for (name, v) in ATTRS.iter().zip(e) {
        let value = match v {
            Some(Leaf::Int(i)) => json!(i),
            Some(Leaf::Str(s)) => json!(s),
            Some(Leaf::Bool(b)) => json!(b),
            None => continue,
        };
        m.insert((*name).into(), json!({"type": "Property", "value": value}));
    }
    Value::Object(m)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: if cfg!(miri) { 8 } else { 512 }, ..ProptestConfig::default() })]

    #[test]
    fn residual_equals_brute_force(r in reference(), e in entity()) {
        let program = compile(&expr(&r), DateTime::MIN);
        prop_assert!(program.is_ok(), "{r:?} did not compile");
        let got = program.ok().map(|p| eval(&p, &json_entity(&e)));
        let want = match reference_eval(&r, &e) {
            Some(true) => Truth::True,
            Some(false) => Truth::False,
            None => Truth::Unknown,
        };
        prop_assert_eq!(got, Some(Ok(want)), "{:?} on {:?}", r, e);
    }
}
