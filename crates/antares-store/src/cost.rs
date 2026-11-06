// SPDX-License-Identifier: EUPL-1.2
//! What one request cost (T-6615, docs/src/request-cost.md): a per-request
//! accumulator the API's request-cost layer puts in a tokio task-local and
//! the store seam adds to. Nothing here runs unless a deployment turns
//! `ANTARES_REQUEST_COST` on: with it off no accumulator exists, and every
//! `add_*` below is one failed task-local lookup on a path that never calls
//! it (the store's hooks are installed only when it is on).
//!
//! Native only: `std::time::Instant` panics on wasm32, and the browser build
//! serves no network to answer a cost to.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

tokio::task_local! {
    static CURRENT: Arc<Cost>;
}

/// The running totals of one request. Atomics, because a request's work can
/// run on a task of its own (`AppState::call_on_task`) that adds to the same
/// totals while the request waits on it.
#[derive(Debug, Default)]
pub struct Cost {
    app_ns: AtomicU64,
    db_ns: AtomicU64,
    pool_ns: AtomicU64,
    statements: AtomicU64,
    rows: AtomicU64,
}

/// A read-out of [`Cost`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    /// Time spent inside the request future's `poll` (busy time on the runtime).
    pub app: Duration,
    /// Wall time of the store statements the request awaited.
    pub db: Duration,
    /// Time spent waiting for a pool connection (plus its BEGIN).
    pub pool: Duration,
    /// Store statements run.
    pub statements: u64,
    /// Rows the store returned.
    pub rows: u64,
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

impl Cost {
    /// Adds busy time spent polling the request.
    pub fn add_app(&self, d: Duration) {
        self.app_ns.fetch_add(nanos(d), Ordering::Relaxed);
    }

    /// Adds one statement: its wall time and the rows it returned.
    pub fn add_statement(&self, elapsed: Duration, rows: u64) {
        self.db_ns.fetch_add(nanos(elapsed), Ordering::Relaxed);
        self.statements.fetch_add(1, Ordering::Relaxed);
        self.rows.fetch_add(rows, Ordering::Relaxed);
    }

    /// Adds time spent waiting for a pool connection.
    pub fn add_pool(&self, d: Duration) {
        self.pool_ns.fetch_add(nanos(d), Ordering::Relaxed);
    }

    /// The totals so far.
    #[must_use]
    pub fn totals(&self) -> Totals {
        let d = |a: &AtomicU64| Duration::from_nanos(a.load(Ordering::Relaxed));
        Totals {
            app: d(&self.app_ns),
            db: d(&self.db_ns),
            pool: d(&self.pool_ns),
            statements: self.statements.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
        }
    }
}

/// The accumulator of the request this task serves, if one is measured.
#[must_use]
pub fn current() -> Option<Arc<Cost>> {
    CURRENT.try_with(Arc::clone).ok()
}

/// Runs `fut` with `cost` as the task's accumulator.
pub async fn scope<F: Future>(cost: Arc<Cost>, fut: F) -> F::Output {
    CURRENT.scope(cost, fut).await
}

/// Runs `fut` with `cost` as the task's accumulator and adds the time each
/// of its polls takes to the app total.
pub async fn measured<F: Future>(cost: Arc<Cost>, fut: F) -> F::Output {
    let timer = Arc::clone(&cost);
    let mut fut = std::pin::pin!(fut);
    let polled = std::future::poll_fn(move |cx| {
        let t0 = Instant::now();
        let out = fut.as_mut().poll(cx);
        timer.add_app(t0.elapsed());
        out
    });
    scope(cost, polled).await
}

/// Adds one statement to the current request, if one is measured.
pub fn add_statement(elapsed: Duration, rows: u64) {
    let _ = CURRENT.try_with(|c| c.add_statement(elapsed, rows));
}

/// Adds a pool wait to the current request, if one is measured.
pub fn add_pool(d: Duration) {
    let _ = CURRENT.try_with(|c| c.add_pool(d));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn adds_reach_only_the_scoped_request() {
        add_statement(Duration::from_millis(5), 3);
        assert!(current().is_none());
        let cost = Arc::new(Cost::default());
        measured(Arc::clone(&cost), async {
            add_statement(Duration::from_millis(2), 10);
            add_statement(Duration::from_millis(1), 0);
            add_pool(Duration::from_millis(4));
            assert!(current().is_some());
        })
        .await;
        let t = cost.totals();
        assert_eq!((t.statements, t.rows), (2, 10));
        assert_eq!(t.db, Duration::from_millis(3));
        assert_eq!(t.pool, Duration::from_millis(4));
        assert!(current().is_none());
    }
}
