// SPDX-License-Identifier: EUPL-1.2
//! The store half of request cost (T-6615, docs/src/request-cost.md): every
//! statement sqlx runs ends in one `sqlx::query` event carrying its wall time
//! and the rows it returned, emitted on the task that awaited it. This layer
//! adds each one to the request that task serves
//! ([`antares_store::cost::current`]). The broker installs it only with
//! `ANTARES_REQUEST_COST=on`: without the layer sqlx builds no such event.

use std::time::Duration;
use tracing_subscriber::filter::{Filtered, Targets};
use tracing_subscriber::layer::{Context, Layer};

/// Adds each `sqlx::query` event to the current request's cost.
#[derive(Clone, Copy, Debug, Default)]
pub struct StatementCost;

/// [`StatementCost`] under the filter that turns sqlx's statement events on
/// (they are DEBUG, slow ones WARN) for this layer alone.
#[must_use]
pub fn statement_layer<S: tracing::Subscriber>() -> Filtered<StatementCost, Targets, S> {
    StatementCost.with_filter(Targets::new().with_target("sqlx::query", tracing::Level::TRACE))
}

#[derive(Default)]
struct Fields {
    elapsed_secs: f64,
    rows_returned: u64,
}

impl tracing::field::Visit for Fields {
    fn record_f64(&mut self, f: &tracing::field::Field, v: f64) {
        if f.name() == "elapsed_secs" {
            self.elapsed_secs = v;
        }
    }
    fn record_u64(&mut self, f: &tracing::field::Field, v: u64) {
        if f.name() == "rows_returned" {
            self.rows_returned = v;
        }
    }
    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

impl<S: tracing::Subscriber> Layer<S> for StatementCost {
    fn on_event(&self, e: &tracing::Event<'_>, _: Context<'_, S>) {
        if e.metadata().target() != "sqlx::query" {
            return;
        }
        let Some(cost) = antares_store::cost::current() else {
            return;
        };
        let mut f = Fields::default();
        e.record(&mut f);
        let elapsed = Duration::try_from_secs_f64(f.elapsed_secs).unwrap_or_default();
        cost.add_statement(elapsed, f.rows_returned);
    }
}
