// SPDX-License-Identifier: EUPL-1.2
//! `$apply` (OData Data Aggregation 4.0 section 3, Q19, T-3937): `identity`, `filter`,
//! `orderby`, `top`, `skip`, `aggregate` and `groupby` over the whole answer of the view, read
//! through one bounded scan: past `RESIDUAL_SCAN` Entities the read is refused, never aggregated
//! in part (ADR-0028). Each expression is a residual value of every Entity, as a `$compute` item
//! is: a member the view hides is an unknown name (400), one the engine hides reads as absent,
//! and an Entity either hides is not in the answer, so nothing hidden is counted or summed.
//! After `aggregate` or `groupby` the rows are no Entities: a later transformation reads each
//! as an Entity whose Properties are the row's names.
//!
//! Decided (worker-5, owner may override): `sum`, `average`, `min` and `max` fold the non-null
//! values and are `null` without one, `countdistinct` counts the distinct non-null values,
//! `$count` the instances (Data Aggregation 4.0 section 3.1.3); `sum` and `average` take
//! numbers only (400 otherwise). Beside `$apply`, `$filter`, `$orderby`, `$select`, `$expand`,
//! `$search` and `$compute` are 501, and so are `/$ref` and `/$count`; `$top`, `$skip`,
//! `$count` and the `ngsild.*` options apply as on any collection read.

use crate::edm::{Member, Set, Shape, Written};
use crate::filter::{compute, numeric, whole, Translated};
use crate::read::{
    attach, compare, digest, entity_value, ngsild, not_implemented, now, paged, position, pushdown,
    refusal, resource, scanned, set_of, token, Reads, COMPUTED,
};
use antares_ops::{Caller, Page as ScanPage};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use odata_axum::{json_response, ODataErrorResponse};
use odata_json::{
    context_url, write_collection, ContextTarget, Ctx, EntityValue, MetadataLevel, ODataVersion,
    Page,
};
use odata_url::{
    Aggregate, AggregateWith, Compute, Expr, MemberPath, ODataUri, QueryOptions, Segment,
    Transformation,
};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// A collection read with `$apply`.
pub(crate) async fn collection(
    reads: &Reads,
    caller: &Caller,
    ctx: &Ctx<'_>,
    (set_name, path): (&str, &[Segment]),
    options: &QueryOptions,
    headers: &HeaderMap,
) -> Result<Response, ODataErrorResponse> {
    let version = ctx.version;
    let beside = options.filter.is_some()
        || options.orderby.is_some()
        || options.select.is_some()
        || options.expand.is_some()
        || options.search.is_some()
        || options.compute.is_some();
    // `/$ref` and `/$count` of the rows
    if beside || path.len() > 1 {
        return Err(not_implemented(version));
    }
    let set = set_of(reads, set_name, version)?;
    let steps = options.apply.as_deref().unwrap_or_default();
    // the transformations up to the first aggregation read the Entities
    let split = steps
        .iter()
        .position(|s| grouped(s).is_some())
        .unwrap_or(steps.len());
    let (before, after) = steps.split_at(split);
    let mut t = Translated::default();
    let items: Vec<Compute> = before
        .iter()
        .chain(after.first())
        .enumerate()
        .flat_map(|(at, step)| items(at, step))
        .collect();
    compute(&items, set, &options.aliases, &mut t, now())
        .map_err(|refused| refusal(refused, version))?;
    ngsild(&reads.view, options, &mut t, version)?;
    let program = whole(&t, now()).map_err(|refused| refusal(refused, version))?;
    let pushdown = pushdown(set, None, &[], &mut t);
    let all = ScanPage {
        start: 0,
        size: 0,
        whole: true,
    };
    let (mut docs, _) = scanned(reads, caller, &pushdown, &program, all, version).await?;
    attach(&mut docs, &t, version)?;
    for (at, step) in before.iter().enumerate() {
        docs = kept(step, at, docs, |doc| doc);
    }
    let digest = digest(&reads.state.service_root, path, options);
    let window = position(reads, options, digest, headers, version)?;
    let link = |cursor: u64, skip: u64, top: Option<u64>| {
        let rest = QueryOptions {
            skip: None,
            top: None,
            skiptoken: Some(token(cursor, skip, top, digest)),
            ..options.clone()
        };
        let uri = ODataUri::Resource {
            path: path.to_vec(),
            options: rest,
        };
        format!("{}{uri}", reads.state.service_root)
    };
    let counted = options.count == Some(true);
    let aggregation = after
        .split_first()
        .and_then(|(first, later)| Some((grouped(first)?, later)));
    let Some((first, later)) = aggregation else {
        // no aggregation: the Entities, as a collection read writes them
        let (docs, total, next) = paged(docs, window, &link);
        let plain = Translated {
            computed: Vec::new(),
            ..t
        };
        let page = Page {
            count: counted.then_some(total),
            next_link: next.as_deref(),
        };
        return entities(reads, ctx, (set_name, set), &docs, &plain, &page);
    };
    let mut rows = aggregated(first, split, &docs, &types(&t), version)?;
    for (at, step) in later.iter().enumerate() {
        rows = over_rows(
            step,
            split.saturating_add(1).saturating_add(at),
            rows,
            options,
            version,
        )?;
    }
    let (values, total, next) = paged(rows.values, window, &link);
    let names: Vec<&str> = rows.names.iter().map(|(n, _)| n.as_str()).collect();
    Ok(json_response(
        version,
        ctx.level,
        written(
            ctx,
            set_name,
            &names,
            &values,
            counted.then_some(total),
            next.as_deref(),
        ),
    ))
}

/// `docs` as the Entities of `set`, written as a collection read writes them.
fn entities(
    reads: &Reads,
    ctx: &Ctx<'_>,
    (set_name, set): (&str, &Set),
    docs: &[Value],
    filter: &Translated,
    page: &Page<'_>,
) -> Result<Response, ODataErrorResponse> {
    let entities: Vec<EntityValue> = docs
        .iter()
        .map(|doc| entity_value(reads, set_name, set, doc, None, &[], filter))
        .collect();
    let resource = resource(reads, set_name, ctx.version)?;
    let mut body = Vec::new();
    // A Vec takes every byte: the write cannot fail.
    let _ = write_collection(ctx, &resource, &entities, page, &mut body);
    Ok(json_response(ctx.version, ctx.level, body))
}

/// The Edm type of each computed value of `t`, by name.
fn types(t: &Translated) -> BTreeMap<&str, &'static str> {
    t.computed
        .iter()
        .map(|(name, _)| name.as_str())
        .zip(t.computed_types.iter().copied())
        .collect()
}

/// The grouping properties and the aggregates of a transformation that aggregates (what
/// follows reads its rows), `None` for one that keeps the instances.
fn grouped(step: &Transformation) -> Option<(&[String], &[Aggregate])> {
    match step {
        Transformation::GroupBy {
            properties,
            aggregate,
        } => Some((properties, aggregate.as_deref().unwrap_or_default())),
        Transformation::Aggregate(list) => Some((&[], list)),
        _ => None,
    }
}

/// Where the `item`th value of the transformation at `step` is kept among an instance's
/// computed values: `@` starts no member's name.
fn slot(step: usize, item: usize) -> String {
    format!("@{step}.{item}")
}

/// The values the transformation at `at` reads of each instance, as `$compute` items.
fn items(at: usize, step: &Transformation) -> Vec<Compute> {
    let item = |(i, expr): (usize, &Expr)| Compute {
        expr: expr.clone(),
        alias: slot(at, i),
    };
    let member = |name: &String| {
        Expr::Member(MemberPath {
            variable: None,
            path: vec![name.clone()],
        })
    };
    let aggregate = |list: &[Aggregate], from: usize| -> Vec<Compute> {
        list.iter()
            .enumerate()
            .filter_map(|(i, a)| match a {
                Aggregate::With { expr, .. } => Some(item((from.saturating_add(i), expr))),
                Aggregate::Count { .. } => None,
            })
            .collect()
    };
    match step {
        Transformation::Filter(expr) => vec![item((0, expr))],
        Transformation::OrderBy(keys) => {
            keys.iter().map(|k| &k.expr).enumerate().map(item).collect()
        }
        Transformation::Aggregate(list) => aggregate(list, 0),
        Transformation::GroupBy {
            properties,
            aggregate: list,
        } => {
            let keys: Vec<Expr> = properties.iter().map(member).collect();
            let mut out: Vec<Compute> = keys.iter().enumerate().map(item).collect();
            out.extend(aggregate(list.as_deref().unwrap_or_default(), keys.len()));
            out
        }
        Transformation::Identity | Transformation::Top(_) | Transformation::Skip(_) => Vec::new(),
    }
}

/// The non-null computed value `name` of an instance.
fn value<'d>(doc: &'d Value, name: &str) -> Option<&'d Value> {
    doc.get(COMPUTED)?.get(name).filter(|v| !v.is_null())
}

/// The instances a transformation that does not aggregate keeps, in its order, each read
/// through `doc`. A sort puts null first ascending and keeps the order of ties (URL 5.1.5).
fn kept<T>(
    step: &Transformation,
    at: usize,
    mut docs: Vec<T>,
    doc: impl Fn(&T) -> &Value,
) -> Vec<T> {
    let count = |n: &u64| usize::try_from(*n).unwrap_or(usize::MAX);
    match step {
        Transformation::Filter(_) => {
            docs.retain(|d| value(doc(d), &slot(at, 0)) == Some(&Value::Bool(true)));
        }
        Transformation::OrderBy(keys) => {
            docs.sort_by(|a, b| {
                keys.iter()
                    .enumerate()
                    .map(|(i, k)| {
                        let name = slot(at, i);
                        let o = compare(value(doc(a), &name), value(doc(b), &name));
                        if k.descending {
                            o.reverse()
                        } else {
                            o
                        }
                    })
                    .find(|o| o.is_ne())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        Transformation::Top(n) => docs.truncate(count(n)),
        Transformation::Skip(n) => {
            docs.drain(..count(n).min(docs.len()));
        }
        // `identity`; an aggregation is never handed here
        _ => {}
    }
    docs
}

/// Rows: each name with the Edm type of its values, and the values of each row in that order.
struct Rows {
    names: Vec<(String, &'static str)>,
    values: Vec<Vec<Value>>,
}

/// The rows of the aggregation at `at` over `docs`, whose computed values have the Edm `types`.
fn aggregated(
    (properties, list): (&[String], &[Aggregate]),
    at: usize,
    docs: &[Value],
    types: &BTreeMap<&str, &'static str>,
    version: ODataVersion,
) -> Result<Rows, ODataErrorResponse> {
    let edm_of = |name: &str| types.get(name).copied().unwrap_or("Edm.Untyped");
    let mut names: Vec<(String, &'static str)> = properties
        .iter()
        .enumerate()
        .map(|(i, p)| (p.clone(), edm_of(&slot(at, i))))
        .collect();
    for (i, a) in list.iter().enumerate() {
        let edm = match a {
            Aggregate::Count { .. } => "Edm.Int64",
            Aggregate::With { with, .. } => {
                let input = edm_of(&slot(at, properties.len().saturating_add(i)));
                match with {
                    AggregateWith::Sum | AggregateWith::Average if !numeric(input) => {
                        return Err(ODataErrorResponse::new(
                            StatusCode::BAD_REQUEST,
                            "BadRequest",
                            "sum and average aggregate numbers",
                        )
                        .with_version(version))
                    }
                    AggregateWith::Sum | AggregateWith::Average => "Edm.Decimal",
                    AggregateWith::CountDistinct => "Edm.Int64",
                    AggregateWith::Min | AggregateWith::Max => input,
                }
            }
        };
        names.push((a.alias().to_owned(), edm));
    }
    // the groups in the order their first instance comes
    let mut groups: Vec<(Vec<Value>, Vec<&Value>)> = Vec::new();
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    if properties.is_empty() {
        groups.push((Vec::new(), docs.iter().collect()));
    }
    for doc in docs.iter().filter(|_| !properties.is_empty()) {
        let key: Vec<Value> = (0..properties.len())
            .map(|i| value(doc, &slot(at, i)).cloned().unwrap_or(Value::Null))
            .collect();
        let text = Value::Array(key.clone()).to_string();
        let at_group = *index.entry(text).or_insert(groups.len());
        match groups.get_mut(at_group) {
            Some((_, members)) => members.push(doc),
            None => groups.push((key, vec![doc])),
        }
    }
    let values = groups
        .into_iter()
        .map(|(mut row, members)| {
            for (i, a) in list.iter().enumerate() {
                let name = slot(at, properties.len().saturating_add(i));
                row.push(folded(
                    a,
                    members.iter().filter_map(|d| value(d, &name)),
                    members.len(),
                ));
            }
            row
        })
        .collect();
    Ok(Rows { names, values })
}

/// One aggregate over the non-null `values` of a group of `instances`.
fn folded<'v>(a: &Aggregate, values: impl Iterator<Item = &'v Value>, instances: usize) -> Value {
    let with = match a {
        Aggregate::Count { .. } => return Value::from(instances),
        Aggregate::With { with, .. } => *with,
    };
    let values: Vec<&Value> = values.collect();
    match with {
        AggregateWith::CountDistinct => {
            let distinct: BTreeSet<String> = values.iter().map(ToString::to_string).collect();
            Value::from(distinct.len())
        }
        AggregateWith::Min | AggregateWith::Max => {
            let pick = values.into_iter().reduce(|best, v| {
                let o = compare(Some(v), Some(best));
                let better = if with == AggregateWith::Min {
                    o.is_lt()
                } else {
                    o.is_gt()
                };
                if better {
                    v
                } else {
                    best
                }
            });
            pick.cloned().unwrap_or(Value::Null)
        }
        AggregateWith::Sum | AggregateWith::Average => {
            let numbers: Vec<&serde_json::Number> =
                values.iter().filter_map(|v| v.as_number()).collect();
            if numbers.is_empty() {
                return Value::Null;
            }
            let float: f64 = numbers.iter().filter_map(|n| n.as_f64()).sum();
            if with == AggregateWith::Average {
                #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
                let n = numbers.len() as f64;
                return serde_json::Number::from_f64(float / n).map_or(Value::Null, Value::Number);
            }
            // integers stay integers while their sum fits
            let int = numbers
                .iter()
                .try_fold(0i64, |sum, n| sum.checked_add(n.as_i64()?));
            int.map_or_else(
                || serde_json::Number::from_f64(float).map_or(Value::Null, Value::Number),
                Value::from,
            )
        }
    }
}

/// The transformation at `at` over `rows`: each row read as an Entity whose Properties are the
/// row's names.
fn over_rows(
    step: &Transformation,
    at: usize,
    rows: Rows,
    options: &QueryOptions,
    version: ODataVersion,
) -> Result<Rows, ODataErrorResponse> {
    let set = Set {
        type_iri: "urn:antares:odata:apply:Row".into(),
        members: rows
            .names
            .iter()
            .map(|(name, edm)| {
                let member = Member {
                    iri: format!("urn:antares:odata:apply:{name}"),
                    shape: Shape::Value {
                        edm,
                        collection: false,
                    },
                    list: false,
                    written: Written::Never,
                };
                (name.clone(), member)
            })
            .collect(),
        identifier: None,
        writable: crate::edm::Writable::default(),
        references: BTreeMap::new(),
    };
    let mut t = Translated::default();
    compute(&items(at, step), &set, &options.aliases, &mut t, now())
        .map_err(|refused| refusal(refused, version))?;
    let terms: Vec<String> = rows
        .names
        .iter()
        .map(|(name, _)| {
            crate::filter::mint(&mut t.context, &format!("urn:antares:odata:apply:{name}"))
        })
        .collect();
    let mut docs: Vec<Value> = rows
        .values
        .iter()
        .map(|row| pseudo(&rows.names, &terms, row))
        .collect();
    attach(&mut docs, &t, version)?;
    if let Some(grouping) = grouped(step) {
        return aggregated(grouping, at, &docs, &types(&t), version);
    }
    let pairs: Vec<(Value, Vec<Value>)> = docs.into_iter().zip(rows.values).collect();
    let values = kept(step, at, pairs, |(doc, _)| doc)
        .into_iter()
        .map(|(_, row)| row)
        .collect();
    Ok(Rows {
        names: rows.names,
        values,
    })
}

/// One row as a normalized NGSI-LD Entity: each non-null value a Property under its term, a
/// date, time or instant as its typed JSON-LD value.
fn pseudo(names: &[(String, &'static str)], terms: &[String], row: &[Value]) -> Value {
    let mut doc = Map::new();
    doc.insert("id".into(), Value::from("urn:ngsi-ld:Row:0"));
    doc.insert("type".into(), Value::from("Row"));
    for (((_, edm), term), v) in names.iter().zip(terms).zip(row) {
        let typed = match *edm {
            "Edm.DateTimeOffset" => Some("DateTime"),
            "Edm.Date" => Some("Date"),
            "Edm.TimeOfDay" => Some("Time"),
            _ => None,
        };
        let v = match (typed, v) {
            (_, Value::Null) => continue,
            (Some(ty), Value::String(_)) => serde_json::json!({"@type": ty, "@value": v}),
            _ => v.clone(),
        };
        doc.insert(
            term.clone(),
            serde_json::json!({"type": "Property", "value": v}),
        );
    }
    Value::Object(doc)
}

/// The rows as a collection: the context URL names the set and the row's names, each row a
/// JSON object of its names (Data Aggregation 4.0 section 3, JSON Format 4.01 section 12).
fn written(
    ctx: &Ctx<'_>,
    set: &str,
    names: &[&str],
    values: &[Vec<Value>],
    count: Option<u64>,
    next: Option<&str>,
) -> Vec<u8> {
    let prefix = match ctx.version {
        ODataVersion::V4_0 => "@odata.",
        ODataVersion::V4_01 => "@",
    };
    let mut members: Vec<(String, Value)> = Vec::new();
    if ctx.level != MetadataLevel::None {
        let target = ContextTarget::Collection {
            set,
            select: Some(names),
        };
        members.push((
            format!("{prefix}context"),
            Value::from(context_url(ctx, &target)),
        ));
    }
    if let Some(count) = count {
        members.push((format!("{prefix}count"), Value::from(count)));
    }
    let rows: Vec<Value> = values
        .iter()
        .map(|row| {
            Value::Object(
                names
                    .iter()
                    .map(|n| (*n).to_owned())
                    .zip(row.iter().cloned())
                    .collect(),
            )
        })
        .collect();
    members.push(("value".into(), Value::Array(rows)));
    if let Some(next) = next {
        members.push((format!("{prefix}nextLink"), Value::from(next)));
    }
    // the members in this order: the control information before `value`, the link after it
    let mut out = b"{".to_vec();
    for (i, (name, v)) in members.into_iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend(Value::from(name).to_string().into_bytes());
        out.push(b':');
        out.extend(v.to_string().into_bytes());
    }
    out.push(b'}');
    out
}
