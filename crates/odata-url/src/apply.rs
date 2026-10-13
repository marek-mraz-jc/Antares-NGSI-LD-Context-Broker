// SPDX-License-Identifier: EUPL-1.2
//! `$apply` (OData Data Aggregation 4.0 section 3): `identity`, `filter`, `orderby`, `top`,
//! `skip`, `aggregate` and `groupby` (T-3937). The other transformations, rollup, custom
//! aggregates and custom methods are not implemented (T-3985).

use crate::expr::{common_expr, Scope};
use crate::model::{Member, Model};
use crate::query::{member, unsigned, Query};
use crate::{Aggregate, AggregateWith, Feature, Transformation, UrlError, UrlErrorKind};

/// Data Aggregation 4.0 transformations the parser does not serve, and `rollup`.
const OTHER: [&str; 18] = [
    "topcount",
    "bottomcount",
    "topsum",
    "bottomsum",
    "toppercent",
    "bottompercent",
    "concat",
    "compute",
    "search",
    "expand",
    "nest",
    "addnested",
    "join",
    "outerjoin",
    "ancestors",
    "descendants",
    "traverse",
    "rollup",
];

const METHODS: [(&str, AggregateWith); 5] = [
    ("sum", AggregateWith::Sum),
    ("min", AggregateWith::Min),
    ("max", AggregateWith::Max),
    ("average", AggregateWith::Average),
    ("countdistinct", AggregateWith::CountDistinct),
];

impl<'m, M: Model> Query<'_, '_, '_, 'm, M> {
    /// `applyExpr`: transformations separated by `/`, and the scope after them. After one that
    /// aggregates, the scope holds no member and `budget.computed` names what it produced.
    pub(crate) fn apply(
        &mut self,
        scope: Scope<'m>,
    ) -> Result<(Vec<Transformation>, Scope<'m>), UrlError> {
        let mut steps = Vec::new();
        let mut here = scope;
        loop {
            steps.push(self.transformation(&mut here)?);
            if !self.c.slash() {
                return Ok((steps, here));
            }
        }
    }

    fn transformation(&mut self, here: &mut Scope<'m>) -> Result<Transformation, UrlError> {
        let start = self.c.pos();
        let Some(word) = self.c.identifier()? else {
            return Err(self.c.syntax("applyTrafo"));
        };
        let word = word.to_ascii_lowercase();
        if word == "identity" {
            return Ok(Transformation::Identity);
        }
        if self.c.peek() == Some(b'.') || OTHER.contains(&word.as_str()) {
            // a custom transformation (a function), or one T-3985 serves
            self.c.reset(start);
            return Err(self.c.not_implemented(Feature::Apply));
        }
        if !self.c.open() {
            return Err(self.c.syntax("applyTrafo"));
        }
        self.c.bws();
        let step = match word.as_str() {
            "filter" => {
                Transformation::Filter(common_expr(self.c, self.model, *here, &mut self.budget)?)
            }
            "orderby" => Transformation::OrderBy(self.orderby(*here)?),
            "top" => Transformation::Top(unsigned(self.c)?),
            "skip" => Transformation::Skip(unsigned(self.c)?),
            "aggregate" => {
                let items = self.aggregates(*here, &[])?;
                self.produced(here, &[], &items);
                Transformation::Aggregate(items)
            }
            "groupby" => self.groupby(here)?,
            _ => {
                self.c.reset(start);
                return Err(self.c.syntax("applyTrafo"));
            }
        };
        self.c.bws();
        if !self.c.close() {
            return Err(self.c.syntax("CLOSE"));
        }
        Ok(step)
    }

    /// After an aggregation only `properties` and the aliases of `items` resolve.
    fn produced(&mut self, here: &mut Scope<'m>, properties: &[String], items: &[Aggregate]) {
        *here = Scope::Primitive;
        self.budget.computed = properties
            .iter()
            .cloned()
            .chain(items.iter().map(|i| i.alias().to_owned()))
            .collect();
    }

    /// `groupby` after its `OPEN`: `(groupingProperty, ...)`, then `aggregate(...)` or nothing.
    fn groupby(&mut self, here: &mut Scope<'m>) -> Result<Transformation, UrlError> {
        if !self.c.open() {
            return Err(self.c.syntax("groupbyList"));
        }
        let mut properties: Vec<String> = Vec::new();
        loop {
            self.c.bws();
            let at = self.c.pos();
            let Some(name) = self.c.identifier()? else {
                return Err(self.c.syntax("groupingProperty"));
            };
            if self.c.slash() || self.c.open() || self.c.peek() == Some(b'.') {
                // a path, `rollup(...)`, or a type cast
                self.c.reset(at);
                return Err(self.c.not_implemented(Feature::Apply));
            }
            let computed = self.budget.computed.iter().any(|c| c == name);
            let found = member(self.model, *here, name)
                .or_else(|| computed.then_some(Member::Primitive { collection: false }));
            match found {
                Some(Member::Primitive { collection: false }) => {}
                Some(_) => {
                    self.c.reset(at);
                    return Err(self.c.not_implemented(Feature::Apply));
                }
                None => {
                    self.c.reset(at);
                    return Err(self.c.error(UrlErrorKind::Unresolved));
                }
            }
            if properties.iter().any(|p| p == name) {
                self.c.reset(at);
                return Err(self.c.syntax("groupingProperty"));
            }
            properties.push(name.to_owned());
            self.c.bws();
            if !self.c.comma() {
                break;
            }
        }
        if !self.c.close() {
            return Err(self.c.syntax("CLOSE"));
        }
        self.c.bws();
        let mut aggregate = None;
        if self.c.comma() {
            self.c.bws();
            let at = self.c.pos();
            let word = self.c.identifier()?;
            if !word.is_some_and(|w| w.eq_ignore_ascii_case("aggregate")) {
                // any other transformation of the groups
                self.c.reset(at);
                return Err(self.c.not_implemented(Feature::Apply));
            }
            if !self.c.open() {
                return Err(self.c.syntax("OPEN"));
            }
            self.c.bws();
            let items = self.aggregates(*here, &properties)?;
            self.c.bws();
            if !self.c.close() {
                return Err(self.c.syntax("CLOSE"));
            }
            aggregate = Some(items);
        }
        self.produced(here, &properties, aggregate.as_deref().unwrap_or_default());
        Ok(Transformation::GroupBy {
            properties,
            aggregate,
        })
    }

    /// `aggregateExpr *( COMMA aggregateExpr )`; an alias given twice, or one of `taken`, is
    /// refused.
    fn aggregates(
        &mut self,
        scope: Scope<'m>,
        taken: &[String],
    ) -> Result<Vec<Aggregate>, UrlError> {
        let mut items: Vec<Aggregate> = Vec::new();
        loop {
            let item = if self.c.eat("$count") {
                self.aggregate_alias(|alias| Aggregate::Count { alias })?
            } else {
                let expr = common_expr(self.c, self.model, scope, &mut self.budget)?;
                let at = self.c.pos();
                if !(self.c.rws() && self.c.eat_ci("with") && self.c.rws()) {
                    self.c.reset(at);
                    // a custom aggregate (`aggregate(Forecast)`) or `from`
                    return Err(self.c.not_implemented(Feature::Apply));
                }
                let with = self.method()?;
                self.aggregate_alias(|alias| Aggregate::With { expr, with, alias })?
            };
            let alias = item.alias();
            if taken.iter().any(|t| t == alias) || items.iter().any(|i| i.alias() == alias) {
                return Err(self.c.syntax("alias"));
            }
            items.push(item);
            self.c.bws();
            if !self.c.comma() {
                return Ok(items);
            }
            self.c.bws();
        }
    }

    /// An aggregation method: a standard one, a custom (qualified) one is not implemented.
    fn method(&mut self) -> Result<AggregateWith, UrlError> {
        let at = self.c.pos();
        let word = self.c.identifier()?.unwrap_or_default();
        if self.c.peek() == Some(b'.') {
            self.c.reset(at);
            return Err(self.c.not_implemented(Feature::Apply));
        }
        if let Some((_, with)) = METHODS.iter().find(|(n, _)| n.eq_ignore_ascii_case(word)) {
            return Ok(*with);
        }
        self.c.reset(at);
        Err(self.c.syntax("aggregateMethod"))
    }

    /// `RWS "as" RWS alias`, made into an item by `item`.
    fn aggregate_alias(
        &mut self,
        item: impl FnOnce(String) -> Aggregate,
    ) -> Result<Aggregate, UrlError> {
        if !(self.c.rws() && self.c.eat_ci("as") && self.c.rws()) {
            return Err(self.c.syntax("aggregateExpr"));
        }
        match self.c.identifier()? {
            Some(alias) => Ok(item(alias.to_owned())),
            None => Err(self.c.syntax("alias")),
        }
    }
}
