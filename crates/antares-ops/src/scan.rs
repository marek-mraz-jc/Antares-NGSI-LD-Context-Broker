// SPDX-License-Identifier: EUPL-1.2
//! [`ViewOps::scan`]: a standard's query as the store's pushdown and the residual the store
//! cannot answer (ADR-0028 rules 3 to 5). The residual runs only on what [`ViewOps::run`]
//! answered for the caller, so it drops Entities and never adds one; a page is filled from an
//! upstream cursor within [`RESIDUAL_SCAN`]; a count or a residual sort reads the whole answer
//! when it fits the budget and is refused before any evaluation when it does not.

use std::fmt::Write as _;

use axum::http::Method;
use axum::response::Response;
use serde_json::Value;

use crate::residual::{Program, Truth};
use crate::{Caller, OperationId, OpsRequest, Refusal, ViewOps};

/// The most upstream Entities one [`ViewOps::scan`] reads (ADR-0028 rule 4).
// ponytail: one constant for every deployment; an environment variable (ADR-0028) when a
// deployment measures that it needs another value.
pub const RESIDUAL_SCAN: usize = 10_000;

/// The Query Entities the store answers, as a surface built it.
#[derive(Debug, Clone, Default)]
pub struct Pushdown {
    /// The encoded query parameters (`type=…&q=…`), without `limit`, `offset`, `count`,
    /// `orderBy`, `attrs` and `options=sysAttrs`: [`ViewOps::scan`] sets those.
    pub query: String,
    /// The `orderBy` keys the store applies (TS 104 176 7.2.3.1); with a residual the scan ends
    /// them with `id`, so the upstream order is total.
    pub order: Vec<String>,
    /// The Attributes the standard selects; `None` selects every one.
    pub attrs: Option<Vec<String>>,
    /// Whether the standard asks for `createdAt` and `modifiedAt`.
    pub sys_attrs: bool,
    /// The @context the names are read under ([`OpsRequest::context`]).
    pub context: Option<Value>,
}

/// Which part of the answer a [`ViewOps::scan`] serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    /// The upstream offset to read from: 0, or a previous page's `next`.
    pub start: usize,
    /// How many Entities the page holds at most.
    pub size: usize,
    /// The whole answer, counted, for a count or a sort the store cannot apply: `start` and
    /// `size` are then the surface's to apply after its own sort.
    pub whole: bool,
}

/// What a [`ViewOps::scan`] answered.
#[derive(Debug)]
pub enum Scanned {
    /// Entities the residual admitted, in upstream order.
    Page {
        /// The Entities, each as the standard selected it.
        entities: Vec<Value>,
        /// The upstream offset just past the last Entity served, when more may remain.
        next: Option<usize>,
        /// The exact count of the answer, when the page asked for the whole of it.
        count: Option<usize>,
    },
    /// The store's own answer to the pushdown when it was no success (a broken `q`: 400), for
    /// the surface to render as it renders [`ViewOps::run`]'s.
    Answer(Response),
}

/// Why a filter needs more of the store than one request may read.
pub const TOO_COMPLEX: &str = "the filter is larger than the residual evaluator allows";

impl ViewOps {
    /// Query Entities through the view with `residual` applied to what the store answered.
    ///
    /// Without a residual it is one [`ViewOps::run`] of the pushdown with `limit`, `offset`
    /// and, for a whole answer, `count`. With one, the scan pushes `orderBy` ending in `id`,
    /// reads upstream pages of the broker's `max_limit` from `page.start` and keeps the
    /// Entities `residual` holds true for until the page is full, the answer ends or
    /// [`RESIDUAL_SCAN`] Entities were read; `next` is the offset just past the last one
    /// served. A whole answer reads `NGSILD-Results-Count` first and every Entity after it.
    /// The pushdown's `attrs` gains what the residual reads, and `options=sysAttrs` when it
    /// reads a system member; neither leaves the scan unless the standard selected it.
    ///
    /// # Errors
    /// [`Refusal::TooManyResults`] when a whole answer holds more than [`RESIDUAL_SCAN`]
    /// Entities, [`Refusal::Malformed`] when the residual's evaluation passes its step bound,
    /// and everything [`ViewOps::run`] refuses.
    pub async fn scan(
        &self,
        caller: &Caller,
        pushdown: &Pushdown,
        residual: Option<&Program>,
        page: Page,
    ) -> Result<Scanned, Refusal> {
        match residual {
            None => self.direct(caller, pushdown, page).await,
            Some(program) => {
                let upstream = self.inner.state.max_limit;
                let filled = self
                    .fill(caller, pushdown, program, page, (RESIDUAL_SCAN, upstream))
                    .await?;
                Ok(filled.0)
            }
        }
    }

    /// The pushdown alone: one [`ViewOps::run`].
    async fn direct(
        &self,
        caller: &Caller,
        pushdown: &Pushdown,
        page: Page,
    ) -> Result<Scanned, Refusal> {
        let count = if page.whole { "&count=true" } else { "" };
        let query = format!(
            "{}{}&limit={}&offset={}{count}",
            pushdown.query,
            params(pushdown, &pushdown.order, &[], false),
            page.size,
            page.start
        );
        let (batch, total) = match read(self.query(caller, pushdown, &query).await?).await? {
            Ok(read) => read,
            Err(answer) => return Ok(Scanned::Answer(answer)),
        };
        let next = (!page.whole && batch.len() == page.size && page.size > 0)
            .then(|| page.start.saturating_add(batch.len()));
        Ok(Scanned::Page {
            entities: batch,
            next,
            count: total.filter(|_| page.whole),
        })
    }

    /// The pushdown with `program` applied, upstream pages of `upstream` Entities and at most
    /// `budget` read, with how many it read.
    pub(crate) async fn fill(
        &self,
        caller: &Caller,
        pushdown: &Pushdown,
        program: &Program,
        page: Page,
        (budget, upstream): (usize, usize),
    ) -> Result<(Scanned, usize), Refusal> {
        let mut order = pushdown.order.clone();
        if order.last().map(String::as_str) != Some("id") {
            order.push("id".to_owned());
        }
        let extra: Vec<&str> = match &pushdown.attrs {
            Some(selected) => program
                .attributes()
                .into_iter()
                .filter(|a| !selected.iter().any(|s| s == a))
                .collect(),
            None => Vec::new(),
        };
        let system = program.reads_system_members() && !pushdown.sys_attrs;
        let fixed = format!(
            "{}{}",
            pushdown.query,
            params(pushdown, &order, &extra, system)
        );
        // a whole answer reads what the count announced, a page what the budget allows
        let mut cap = budget;
        if page.whole {
            let counted = format!("{fixed}&limit=0&count=true");
            match read(self.query(caller, pushdown, &counted).await?).await? {
                Ok((_, total)) => {
                    // no count is no promise the answer fits
                    cap = total.unwrap_or(usize::MAX);
                    if cap > budget {
                        return Err(Refusal::TooManyResults { budget, size: cap });
                    }
                }
                Err(answer) => return Ok((Scanned::Answer(answer), 0)),
            }
        }
        let size = if page.whole { usize::MAX } else { page.size };
        let mut entities = Vec::new();
        let (mut offset, mut read_so_far) = (page.start, 0usize);
        if size == 0 {
            return Ok(served(entities, None, None, read_so_far));
        }
        loop {
            let limit = upstream.min(cap.saturating_sub(read_so_far));
            if limit == 0 {
                // A whole answer was read as counted; a page ran out of budget before it was
                // full and is served short with its continuation.
                let count = page.whole.then_some(entities.len());
                let next = (!page.whole).then_some(offset);
                return Ok(served(entities, next, count, read_so_far));
            }
            let query = format!("{fixed}&limit={limit}&offset={offset}");
            let batch = match read(self.query(caller, pushdown, &query).await?).await? {
                Ok((batch, _)) => batch,
                Err(answer) => return Ok((Scanned::Answer(answer), read_so_far)),
            };
            let last = batch.len();
            read_so_far = read_so_far.saturating_add(last);
            let ended = last < limit;
            for (i, mut entity) in batch.into_iter().enumerate() {
                offset = offset.saturating_add(1);
                match program.eval(&entity) {
                    Ok(Truth::True) => {}
                    Ok(_) => continue,
                    Err(_) => return Err(Refusal::Malformed(TOO_COMPLEX)),
                }
                strip(&mut entity, &extra, system);
                entities.push(entity);
                if entities.len() == size {
                    let more = !(ended && i.saturating_add(1) == last);
                    return Ok(served(entities, more.then_some(offset), None, read_so_far));
                }
            }
            if ended {
                let count = page.whole.then_some(entities.len());
                return Ok(served(entities, None, count, read_so_far));
            }
        }
    }

    async fn query(
        &self,
        caller: &Caller,
        pushdown: &Pushdown,
        query: &str,
    ) -> Result<Response, Refusal> {
        let mut req = OpsRequest::new(Method::GET, format!("/ngsi-ld/v1/entities?{query}"));
        if let Some(context) = &pushdown.context {
            req = req.context(context.clone());
        }
        self.run(OperationId::QueryEntities, caller, req).await
    }
}

/// A page of `entities` and the count of upstream Entities read for it.
fn served(
    entities: Vec<Value>,
    next: Option<usize>,
    count: Option<usize>,
    read: usize,
) -> (Scanned, usize) {
    (
        Scanned::Page {
            entities,
            next,
            count,
        },
        read,
    )
}

/// `orderBy`, `attrs` and `options` of a pushdown, each `&`-prefixed.
fn params(pushdown: &Pushdown, order: &[String], extra: &[&str], system: bool) -> String {
    let mut out = String::new();
    if !order.is_empty() {
        out.push_str("&orderBy=");
        out.push_str(&join(order.iter().map(String::as_str)));
    }
    if let Some(selected) = &pushdown.attrs {
        out.push_str("&attrs=");
        out.push_str(&join(
            selected
                .iter()
                .map(String::as_str)
                .chain(extra.iter().copied()),
        ));
    }
    if pushdown.sys_attrs || system {
        out.push_str("&options=sysAttrs");
    }
    out
}

/// Comma-joined and percent-encoded but for RFC 3986 unreserved characters.
fn join<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for (i, name) in names.enumerate() {
        if i > 0 {
            out.push(',');
        }
        for b in name.bytes() {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                out.push(char::from(b));
            } else {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// The Entities of a successful Query Entities answer and its `NGSILD-Results-Count`, or the
/// answer itself when the store answered with anything but a success.
async fn read(answer: Response) -> Result<Result<(Vec<Value>, Option<usize>), Response>, Refusal> {
    if !answer.status().is_success() {
        return Ok(Err(answer));
    }
    let total = answer
        .headers()
        .get("NGSILD-Results-Count")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
        .await
        .ok();
    match bytes.and_then(|b| serde_json::from_slice(&b).ok()) {
        Some(Value::Array(entities)) => Ok(Ok((entities, total))),
        _ => Err(Refusal::Malformed(
            "the store's answer is no list of Entities",
        )),
    }
}

/// The members of an Attribute that hold its value, never an Attribute of their own.
const VALUES: [&str; 7] = [
    "value",
    "object",
    "json",
    "languageMap",
    "valueList",
    "objectList",
    "vocab",
];

/// Removes what only the residual asked for: the Attributes it added to `attrs`, and
/// `createdAt`/`modifiedAt` of the Entity, its Attributes and their sub-Attributes when the
/// standard did not ask for them (a value's own members stay; the scan reads
/// `application/json`, so no `@context` member is there to keep).
fn strip(entity: &mut Value, extra: &[&str], system: bool) {
    // a Query Entities answer holds objects only
    entity.as_object_mut().into_iter().for_each(|members| {
        for name in extra {
            members.remove(*name);
        }
        if system {
            unsystem(members);
        }
    });
}

fn unsystem(members: &mut serde_json::Map<String, Value>) {
    members.remove("createdAt");
    members.remove("modifiedAt");
    for (name, value) in members.iter_mut() {
        if VALUES.contains(&name.as_str()) {
            continue;
        }
        match value {
            Value::Object(m) => unsystem(m),
            Value::Array(items) => items
                .iter_mut()
                .filter_map(Value::as_object_mut)
                .for_each(unsystem),
            _ => {}
        }
    }
}
