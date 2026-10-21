// SPDX-License-Identifier: EUPL-1.2
//! The query options and the resource path of a SensorThings request, bounded before they are
//! read: `$filter` compiled to the NGSI-LD `q` (and, on a series, the temporal window), the page
//! options, and the path split without cutting a key literal.

use serde_json::{json, Map, Value};

/// The longest `$filter` read, in bytes.
pub const MAX_FILTER: usize = 4096;

/// The default and the largest `$top`.
pub const MAX_TOP: usize = 100;

/// The largest `$skip`: what one view request may offset to.
pub const MAX_SKIP: usize = 10_000;

/// The serialized size a page is cut at, between whole resources.
pub const PAGE_BYTES: usize = 1 << 20;

/// The deepest `$expand` path.
const MAX_EXPAND: usize = 2;

/// A query option this surface cannot honour, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bad(pub &'static str, pub String);

/// The query options of one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// `$top`, 1 to [`MAX_TOP`].
    pub top: usize,
    /// `$skip`.
    pub skip: usize,
    /// `$count=true`.
    pub count: bool,
    /// `$select`: the members each resource keeps; empty keeps every one.
    pub select: Vec<String>,
    /// The first segment of each `$expand` path.
    pub expand: Vec<String>,
    /// `$orderby` names `phenomenonTime desc`.
    pub newest_first: bool,
    /// `$filter`, as sent.
    pub filter: Option<String>,
    /// `$skiptoken` of a set whose Entity holds several resources, as its `@iot.nextLink` writes
    /// it: the Entity offset and how many of that Entity's resources were served.
    pub resume: Option<(usize, usize)>,
}

/// The query options of `pairs`.
///
/// # Errors
/// [`Bad`] naming the option that is no number, too deep or too large.
pub fn options(pairs: &[(String, String)]) -> Result<Options, Bad> {
    let mut out = Options {
        top: MAX_TOP,
        skip: 0,
        count: false,
        select: Vec::new(),
        expand: Vec::new(),
        newest_first: false,
        filter: None,
        resume: None,
    };
    for (name, value) in pairs {
        match name.as_str() {
            "$top" => out.top = number("$top", value, usize::MAX)?.clamp(1, MAX_TOP),
            "$skip" => out.skip = number("$skip", value, MAX_SKIP)?,
            "$count" => out.count = value == "true",
            "$select" => out.select = list(value).map(str::to_owned).collect(),
            "$expand" => {
                if list(value).any(|p| p.split('/').count() > MAX_EXPAND) {
                    return Err(Bad(
                        "$expand",
                        format!("a path is at most {MAX_EXPAND} deep"),
                    ));
                }
                // `Observations($top=1)`: the set before its nested options.
                out.expand = list(value)
                    .filter_map(|p| p.split(['/', '(']).next())
                    .map(str::to_owned)
                    .collect();
            }
            "$orderby" => {
                let lowered = value.to_ascii_lowercase();
                out.newest_first = lowered.contains("phenomenontime") && lowered.contains("desc");
            }
            "$filter" if value.len() > MAX_FILTER => {
                return Err(Bad("$filter", format!("at most {MAX_FILTER} bytes")));
            }
            "$filter" => out.filter = Some(value.clone()),
            "$skiptoken" => {
                let usage = || Bad("$skiptoken", "the value an @iot.nextLink carries".into());
                let (entity, served) = value.split_once('.').ok_or_else(usage)?;
                out.resume = Some((
                    number("$skiptoken", entity, MAX_SKIP)?,
                    number("$skiptoken", served, MAX_SKIP)?,
                ));
            }
            _ => {}
        }
    }
    Ok(out)
}

/// The comma-separated items of an option, trimmed, empty ones left out.
fn list(value: &str) -> impl Iterator<Item = &str> {
    value.split(',').map(str::trim).filter(|s| !s.is_empty())
}

/// A whole number of at most `max`.
fn number(name: &'static str, value: &str, max: usize) -> Result<usize, Bad> {
    value
        .parse::<usize>()
        .ok()
        .filter(|n| *n <= max)
        .ok_or_else(|| Bad(name, format!("a whole number, at most {max}")))
}

/// `$filter` as the NGSI-LD `q`: comparisons over an Attribute, `and`, `or`, parentheses and
/// `substringof`, the subset the gateway compiled; anything else refused by name, since a
/// filter dropped silently answers rows the caller asked not to see.
///
/// # Errors
/// [`Bad`] naming the construct.
pub fn filter_to_q(filter: &str) -> Result<String, Bad> {
    let mut out = String::new();
    let mut rest = filter.trim();
    let mut operand = true;
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix('(') {
            out.push('(');
            (rest, operand) = (tail.trim_start(), true);
            continue;
        }
        if let Some(tail) = rest.strip_prefix(')') {
            out.push(')');
            (rest, operand) = (tail.trim_start(), false);
            continue;
        }
        let (token, tail) = match rest.find([' ', '(', ')']) {
            Some(at) => rest.split_at(at),
            None => (rest, ""),
        };
        rest = tail.trim_start();
        if !operand {
            out.push_str(connective(token)?);
            operand = true;
        } else if token.eq_ignore_ascii_case("substringof") {
            out.push_str(&substring(&mut rest)?);
            operand = false;
        } else if token.eq_ignore_ascii_case("not") {
            return Err(bad(
                "not is not supported; write the negated comparison instead",
            ));
        } else {
            out.push_str(&term(token)?);
            operand = false;
        }
    }
    if out.is_empty() {
        return Err(bad("the filter is empty"));
    }
    Ok(out)
}

/// A `$filter` refusal.
fn bad(detail: &str) -> Bad {
    Bad("$filter", detail.to_owned())
}

/// A comparison or logical operator as NGSI-LD writes it.
fn connective(token: &str) -> Result<&'static str, Bad> {
    Ok(match token.to_ascii_lowercase().as_str() {
        "eq" => "==",
        "ne" => "!=",
        "gt" => ">",
        "ge" => ">=",
        "lt" => "<",
        "le" => "<=",
        "and" => ";",
        "or" => "|",
        other => return Err(Bad("$filter", format!("{other} is not supported"))),
    })
}

/// `substringof('text',attribute)` as a pattern match; `rest` starts at its parenthesis.
fn substring(rest: &mut &str) -> Result<String, Bad> {
    let usage = || bad("substringof takes ('text', attribute)");
    let (inner, after) = rest
        .strip_prefix('(')
        .and_then(|open| open.split_once(')'))
        .ok_or_else(usage)?;
    let (needle, attribute) = inner.split_once(',').ok_or_else(usage)?;
    let needle = needle.trim().trim_matches('\'');
    let attribute = attribute.trim();
    if ID.contains(&attribute) {
        return Err(bad("id takes eq or ne on the top-level and"));
    }
    if needle.is_empty() || attribute.is_empty() {
        return Err(usage());
    }
    *rest = after.trim_start();
    Ok(format!("{attribute}~=\".*{needle}.*\""))
}

/// The names a resource's id goes by: no Attribute, so never a `q` operand.
const ID: [&str; 2] = ["id", "@iot.id"];

/// An operand: a quoted literal in NGSI-LD's quotes, a path its last segment (NGSI-LD has one
/// flat Attribute namespace). The id is refused: [`by_id`] reads its top-level comparisons.
fn term(token: &str) -> Result<String, Bad> {
    if ID.contains(&token) {
        return Err(bad("id takes eq or ne on the top-level and"));
    }
    Ok(
        match token.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')) {
            Some(text) => format!("\"{text}\""),
            None => token.rsplit('/').next().unwrap_or(token).to_owned(),
        },
    )
}

/// A set's `$filter` split (T-4928): the resource ids its top-level `and` compares with `eq`
/// and `ne`, which no `q` can name, and the `q` of the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ById {
    /// `id eq` literals: the resource is each of them.
    pub is: Vec<String>,
    /// `id ne` literals: the resource is none of them.
    pub not: Vec<String>,
    /// The NGSI-LD `q` of every other conjunct.
    pub q: Option<String>,
}

impl ById {
    /// Whether the resource of `id` passes the id comparisons.
    #[must_use]
    pub fn admits(&self, id: Option<&str>) -> bool {
        self.is.iter().all(|i| Some(i.as_str()) == id)
            && !self.not.iter().any(|i| Some(i.as_str()) == id)
    }
}

/// The `$filter` of an entity set as [`ById`].
///
/// # Errors
/// [`Bad`] naming the construct: an id compared other than by `eq` or `ne` with a quoted literal,
/// or anything [`filter_to_q`] refuses.
pub fn by_id(filter: &str) -> Result<ById, Bad> {
    let mut out = ById::default();
    let mut rest = Vec::new();
    for conjunct in conjuncts(filter) {
        let mut words = conjunct.splitn(3, ' ');
        let (first, operator, literal) = (words.next(), words.next(), words.next());
        if !first.is_some_and(|f| ID.contains(&f)) {
            rest.push(conjunct);
            continue;
        }
        let literal = literal
            .map(str::trim)
            .and_then(|l| l.strip_prefix('\'')?.strip_suffix('\''))
            .filter(|l| !l.is_empty())
            .ok_or_else(|| bad("id compares with a quoted literal"))?
            .replace("''", "'");
        match operator.map(str::to_ascii_lowercase).as_deref() {
            Some("eq") => out.is.push(literal),
            Some("ne") => out.not.push(literal),
            _ => return Err(bad("id takes eq or ne on the top-level and")),
        }
    }
    if !rest.is_empty() {
        out.q = Some(filter_to_q(&rest.join(" and "))?);
    }
    Ok(out)
}

/// A series `$filter`: the temporal window from its `phenomenonTime` predicates and the `q` of
/// the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Series {
    /// `timerel`, `timeAt` and `endTimeAt`.
    pub window: Vec<(&'static str, String)>,
    /// The NGSI-LD `q` of every other predicate.
    pub q: Option<String>,
}

/// The `$filter` of a Datastream's Observations. `phenomenonTime` is an instance's instant, read
/// by NGSI-LD as a window on the whole request, so a predicate on it stands on the top-level
/// `and` alone; inside a parenthesis or an `or` it is refused. The broker checks the instants.
///
/// # Errors
/// [`Bad`] naming the predicate.
pub fn series_filter(filter: &str) -> Result<Series, Bad> {
    const TIME: &str = "phenomenontime";
    let (mut start, mut end) = (None, None);
    let mut rest = Vec::new();
    for conjunct in conjuncts(filter) {
        let lowered = conjunct.to_ascii_lowercase();
        if !lowered.contains(TIME) {
            rest.push(conjunct);
            continue;
        }
        let words: Vec<&str> = conjunct.split_whitespace().collect();
        let [first, operator, instant] = words.as_slice() else {
            return Err(bad(
                "phenomenonTime takes `phenomenonTime <op> <instant>` on the top-level and",
            ));
        };
        if !first.eq_ignore_ascii_case(TIME) {
            return Err(bad(
                "phenomenonTime takes `phenomenonTime <op> <instant>` on the top-level and",
            ));
        }
        let instant = instant.trim_matches('\'').to_owned();
        match operator.to_ascii_lowercase().as_str() {
            "gt" | "ge" => start = Some(instant),
            "lt" | "le" => end = Some(instant),
            "eq" => (start, end) = (Some(instant.clone()), Some(instant)),
            other => {
                return Err(Bad(
                    "$filter",
                    format!("{other} is not supported on phenomenonTime"),
                ))
            }
        }
    }
    let window = match (start, end) {
        (Some(s), Some(e)) if s > e => return Err(bad("the window ends before it starts")),
        (Some(s), Some(e)) => vec![
            ("timerel", "between".into()),
            ("timeAt", s),
            ("endTimeAt", e),
        ],
        (Some(s), None) => vec![("timerel", "after".into()), ("timeAt", s)],
        (None, Some(e)) => vec![("timerel", "before".into()), ("timeAt", e)],
        (None, None) => Vec::new(),
    };
    let q = if rest.is_empty() {
        None
    } else {
        Some(filter_to_q(&rest.join(" and "))?)
    };
    Ok(Series { window, q })
}

/// The top-level ` and `-separated terms of a filter, what a parenthesis holds left whole; split
/// on character boundaries only (T-4323).
fn conjuncts(filter: &str) -> Vec<&str> {
    let mut terms = Vec::new();
    let (mut depth, mut start, mut resume) = (0usize, 0usize, 0usize);
    for (at, c) in filter.char_indices() {
        if at < resume {
            continue;
        }
        match c {
            '(' => depth = depth.saturating_add(1),
            ')' => depth = depth.saturating_sub(1),
            ' ' if depth == 0
                && filter
                    .get(at..at.saturating_add(5))
                    .is_some_and(|w| w.eq_ignore_ascii_case(" and ")) =>
            {
                terms.extend(filter.get(start..at));
                (start, resume) = (at.saturating_add(5), at.saturating_add(5));
            }
            _ => {}
        }
    }
    terms.extend(filter.get(start..));
    terms
        .into_iter()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect()
}

/// One step of a resource path: the set and the key it addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The set name.
    pub set: String,
    /// The key literal, unquoted.
    pub key: Option<String>,
}

/// The steps of a resource path, a slash inside a quoted key left in it (a Datastream id is
/// `{urn}/{attribute}`).
#[must_use]
pub fn steps(path: &str) -> Vec<Step> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in path.chars() {
        match c {
            '/' if !quoted => out.extend(step(&std::mem::take(&mut current))),
            '\'' => {
                quoted = !quoted;
                current.push(c);
            }
            _ => current.push(c),
        }
    }
    out.extend(step(&current));
    out
}

/// One step: `Set` or `Set('key')`, a doubled quote in the key read as one.
fn step(raw: &str) -> Option<Step> {
    if raw.is_empty() {
        return None;
    }
    Some(match raw.split_once('(') {
        Some((set, rest)) => {
            let key = rest.strip_suffix(')').unwrap_or(rest);
            let key = key.strip_prefix('\'').unwrap_or(key);
            let key = key.strip_suffix('\'').unwrap_or(key);
            Step {
                set: set.to_owned(),
                key: Some(key.replace("''", "'")),
            }
        }
        None => Step {
            set: raw.to_owned(),
            key: None,
        },
    })
}

/// The resources of a page cut at [`PAGE_BYTES`] between Entities, never inside one; the
/// Entities served, for the next page's `$skip`. A first Entity larger than the budget is
/// served alone.
#[must_use]
pub fn fit(groups: Vec<Vec<Value>>) -> (Vec<Value>, usize) {
    let mut out = Vec::new();
    let (mut bytes, mut served) = (0usize, 0usize);
    for group in groups {
        let size = group
            .iter()
            .map(|v| v.to_string().len().saturating_add(1))
            .fold(0usize, usize::saturating_add);
        bytes = bytes.saturating_add(size);
        if bytes > PAGE_BYTES && served > 0 {
            break;
        }
        out.extend(group);
        served = served.saturating_add(1);
    }
    (out, served)
}

/// A collection: `@iot.count` when known, `value`, `@iot.nextLink` when more remain. `select`
/// keeps those members of each resource.
#[must_use]
pub fn collection(
    items: Vec<Value>,
    select: &[String],
    count: Option<usize>,
    next: Option<String>,
) -> Value {
    let mut out = Map::new();
    if let Some(count) = count {
        out.insert("@iot.count".into(), json!(count));
    }
    let items = items.into_iter().map(|i| selected(i, select)).collect();
    out.insert("value".into(), Value::Array(items));
    if let Some(next) = next {
        out.insert("@iot.nextLink".into(), json!(next));
    }
    Value::Object(out)
}

/// The members of `item` that `select` names; every one for an empty `select`.
#[must_use]
pub fn selected(mut item: Value, select: &[String]) -> Value {
    if let (Some(map), false) = (item.as_object_mut(), select.is_empty()) {
        map.retain(|k, _| select.iter().any(|s| s == k));
    }
    item
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn options_are_bounded_and_named_when_refused() {
        let o = options(&pairs(&[
            ("$top", "500"),
            ("$skip", "3"),
            ("$count", "true"),
            ("$select", "@iot.id, name,"),
            ("$expand", "Locations,Datastreams/Observations"),
            ("$orderby", "phenomenonTime DESC"),
            ("$filter", "a eq 1"),
            ("other", "x"),
        ]));
        assert_eq!(
            o,
            Ok(Options {
                top: 100,
                skip: 3,
                count: true,
                select: vec!["@iot.id".into(), "name".into()],
                expand: vec!["Locations".into(), "Datastreams".into()],
                newest_first: true,
                filter: Some("a eq 1".into()),
                resume: None,
            })
        );
        let nested = options(&pairs(&[
            ("$expand", "Observations($top=1)"),
            ("$skiptoken", "7.2"),
        ]));
        assert_eq!(
            nested.as_ref().map(|o| (o.expand.clone(), o.resume)),
            Ok((vec!["Observations".to_owned()], Some((7, 2))))
        );
        for token in ["7", "x.1", "10001.0", "1.-1"] {
            assert_eq!(
                options(&pairs(&[("$skiptoken", token)])).map_err(|b| b.0),
                Err("$skiptoken"),
                "{token}"
            );
        }
        assert_eq!(options(&pairs(&[("$top", "0")])).map(|o| o.top), Ok(1));
        assert_eq!(
            options(&pairs(&[("$orderby", "name")])).map(|o| o.newest_first),
            Ok(false)
        );
        assert!(options(&pairs(&[("$top", "x")])).is_err());
        assert_eq!(
            options(&pairs(&[("$skip", "10001")])).map_err(|b| b.0),
            Err("$skip")
        );
        assert_eq!(
            options(&pairs(&[("$expand", "A/B/C")])).map_err(|b| b.0),
            Err("$expand")
        );
        let huge = "a".repeat(MAX_FILTER + 1);
        assert_eq!(
            options(&pairs(&[("$filter", &huge)])).map_err(|b| b.0),
            Err("$filter")
        );
    }

    /// T-4928: a resource id is no Attribute; a `q` naming it scanned the whole store on prod.
    #[test]
    fn an_id_comparison_is_read_by_id_and_never_sent_as_q() {
        assert_eq!(
            by_id("id eq 'urn:a/x/2026' and speed gt 3 and @iot.id ne 'it''s'"),
            Ok(ById {
                is: vec!["urn:a/x/2026".into()],
                not: vec!["it's".into()],
                q: Some("speed>3".into()),
            })
        );
        assert_eq!(by_id("speed gt 3").map(|b| b.q), Ok(Some("speed>3".into())));
        for refused in [
            "id gt 'a'",
            "id eq 3",
            "id eq ''",
            "(id eq 'a')",
            "speed gt 3 or id eq 'a'",
            "substringof('a', id)",
        ] {
            assert_eq!(by_id(refused).map_err(|b| b.0), Err("$filter"), "{refused}");
        }
        let split = by_id("id eq 'a' and id ne 'b'").unwrap_or_default();
        assert!(split.admits(Some("a")));
        assert!(!split.admits(Some("b")));
        assert!(!split.admits(None));
        assert!(ById::default().admits(None));
    }

    #[test]
    fn a_filter_compiles_to_q_or_names_what_it_refuses() {
        assert_eq!(
            filter_to_q("(name eq 'Žilina' or speed gt 5) and Datastreams/x le 2"),
            Ok("(name==\"Žilina\"|speed>5);x<=2".into())
        );
        assert_eq!(
            filter_to_q("a ne 1 and b ge 2 and c lt 3"),
            Ok("a!=1;b>=2;c<3".into())
        );
        assert_eq!(
            filter_to_q("substringof('Ži', name)"),
            Ok("name~=\".*Ži.*\"".into())
        );
        assert!(filter_to_q("substringof name").is_err());
        assert!(filter_to_q("substringof('x')").is_err());
        assert!(filter_to_q("substringof('', name)").is_err());
        assert!(filter_to_q("substringof('x', )").is_err());
        assert!(filter_to_q("not a eq 1").is_err());
        assert_eq!(
            filter_to_q("a has 1").map_err(|b| b.1),
            Err("has is not supported".into())
        );
        assert!(filter_to_q("  ").is_err());
    }

    #[test]
    fn a_series_filter_splits_into_its_window_and_its_q() {
        let s = series_filter("phenomenonTime ge '2026-01-01T00:00:00Z' and phenomenonTime lt 2026-02-01T00:00:00Z and result gt 3");
        assert_eq!(
            s,
            Ok(Series {
                window: vec![
                    ("timerel", "between".into()),
                    ("timeAt", "2026-01-01T00:00:00Z".into()),
                    ("endTimeAt", "2026-02-01T00:00:00Z".into())
                ],
                q: Some("result>3".into()),
            })
        );
        let after = series_filter("phenomenonTime gt 2026-01-01T00:00:00Z");
        assert_eq!(
            after.map(|s| s.window.first().map(|w| w.1.clone())),
            Ok(Some("after".into()))
        );
        let before = series_filter("phenomenonTime le 2026-01-01T00:00:00Z");
        assert_eq!(
            before.map(|s| s.window.first().map(|w| w.1.clone())),
            Ok(Some("before".into()))
        );
        let at = series_filter("phenomenonTime eq 2026-01-01T00:00:00Z");
        assert_eq!(at.map(|s| s.window.len()), Ok(3));
        assert_eq!(
            series_filter("(a eq 1 and b eq 2)").map(|s| s.q),
            Ok(Some("(a==1;b==2)".into()))
        );
        assert_eq!(
            series_filter("Žilina eq 'x'").map(|s| s.window),
            Ok(Vec::new())
        );
        assert!(series_filter("a eq 1 or phenomenonTime gt 2026-01-01T00:00:00Z").is_err());
        assert!(series_filter("phenomenonTime gt").is_err());
        assert!(series_filter("a eq phenomenonTime").is_err());
        assert!(series_filter("phenomenonTime ne 2026-01-01T00:00:00Z").is_err());
        assert!(series_filter(
            "phenomenonTime gt 2026-02-01T00:00:00Z and phenomenonTime lt 2026-01-01T00:00:00Z"
        )
        .is_err());
        assert!(series_filter("x has 1").is_err());
    }

    #[test]
    fn a_path_keeps_the_slash_inside_a_key() {
        assert_eq!(
            steps("Datastreams('urn:a/speed')/Observations/"),
            vec![
                Step {
                    set: "Datastreams".into(),
                    key: Some("urn:a/speed".into())
                },
                Step {
                    set: "Observations".into(),
                    key: None
                }
            ]
        );
        assert_eq!(
            steps("Things('urn:a''b')")
                .first()
                .and_then(|s| s.key.clone()),
            Some("urn:a'b".into())
        );
        assert_eq!(
            steps("Things(urn:a)").first().and_then(|s| s.key.clone()),
            Some("urn:a".into())
        );
        assert_eq!(steps("").len(), 0);
    }

    #[test]
    fn a_page_is_cut_between_entities() {
        let big = json!("x".repeat(PAGE_BYTES / 2));
        let (items, served) = fit(vec![
            vec![big.clone()],
            vec![big.clone(), json!(1)],
            vec![json!(2)],
        ]);
        assert_eq!((items.len(), served), (1, 1));
        let (items, served) = fit(vec![
            vec![json!("x".repeat(PAGE_BYTES * 2))],
            vec![json!(1)],
        ]);
        assert_eq!((items.len(), served), (1, 1));
        let (items, served) = fit(vec![vec![json!(1), json!(2)], vec![]]);
        assert_eq!((items.len(), served), (2, 2));
    }

    #[test]
    fn a_collection_selects_counts_and_links() {
        let c = collection(
            vec![json!({"a": 1, "b": 2}), json!(3)],
            &["a".into()],
            Some(2),
            Some("n".into()),
        );
        assert_eq!(
            c,
            json!({"@iot.count": 2, "value": [{"a": 1}, 3], "@iot.nextLink": "n"})
        );
        assert_eq!(collection(vec![], &[], None, None), json!({"value": []}));
    }
}
