// SPDX-License-Identifier: EUPL-1.2
//! Group commit for the store calls of in-process deliveries (T-5159).
//!
//! A fan-out of one change to 1 000 WebSocket subscriptions on one view used
//! to cost 1 000 reads of the same Entities and 1 000 bookkeeping
//! transactions, each waiting for a pool connection. Here a delivery that
//! reaches the store joins the NEXT statement of its tenant: while one
//! statement runs, the deliveries arriving behind it queue, and the next
//! statement serves all of them. A delivery never joins a statement already
//! running, so the Entities it reads are read after it asked (ADR-0023
//! rule 9: the view as the table serves it now). A tenant has at most one
//! booking and one read statement in flight, whatever its fan-out.

use super::*;
use antares_store::Delivery;

/// What waits for a tenant's next statement: each question with the channel
/// its answer goes back on.
#[cfg(not(target_arch = "wasm32"))]
type Waiting<Q, A> = Vec<(Q, tokio::sync::oneshot::Sender<A>)>;

/// One queue per tenant. An entry exists exactly while a runner serves it, so
/// a caller that finds none starts one, and the map holds only tenants with a
/// statement in flight.
#[cfg(not(target_arch = "wasm32"))]
pub(super) struct Coalescer<Q, A> {
    queues: std::sync::Mutex<HashMap<String, Waiting<Q, A>>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl<Q: Send + 'static, A: Send + 'static> Coalescer<Q, A> {
    pub(super) fn new() -> Self {
        Self {
            queues: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// `q`'s answer from the next statement `run` makes for `tenant`, one
    /// answer per question in order. `None` when the runner died before
    /// answering (a panic inside `run`).
    ///
    /// A caller that finds its tenant idle runs its own statement on its own
    /// task, so an uncontended delivery costs what it did before; only the
    /// questions that queued behind that statement get a runner task.
    pub(super) async fn call<F, Fut>(
        &'static self,
        st: &AppState,
        tenant: &TenantId,
        q: Q,
        run: F,
    ) -> Option<A>
    where
        F: Fn(AppState, TenantId, Vec<Q>) -> Fut + Clone + Send + 'static,
        Fut: std::future::Future<Output = Vec<A>> + Send,
    {
        let waiting = {
            // A poisoned map is a panic in some past runner; its guard
            // already removed that runner's entry.
            let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
            match queues.get_mut(tenant.as_str()) {
                Some(waiting) => {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    waiting.push((q, tx));
                    Err(rx)
                }
                None => {
                    queues.insert(tenant.as_str().to_owned(), Vec::new());
                    Ok(q)
                }
            }
        };
        let q = match waiting {
            Ok(q) => q,
            Err(rx) => return rx.await.ok(),
        };
        // Dropped mid-statement (its delivery's deadline) or done, the
        // caller hands what queued behind it to a runner: a question that
        // waited is never lost because the caller ahead of it went away.
        let respawn = {
            let (st, tenant, run) = (st.clone(), tenant.clone(), run.clone());
            move || {
                tokio::spawn(async move { self.serve(&st, &tenant, run).await });
            }
        };
        let _handoff = Retire {
            queues: &self.queues,
            tenant: tenant.as_str(),
            armed: true,
            respawn: Some(Box::new(respawn)),
        };
        run(st.clone(), tenant.clone(), vec![q]).await.pop()
    }

    /// Serve `tenant`'s queue until it is empty, one statement per round.
    async fn serve<F, Fut>(&self, st: &AppState, tenant: &TenantId, run: F)
    where
        F: Fn(AppState, TenantId, Vec<Q>) -> Fut,
        Fut: std::future::Future<Output = Vec<A>>,
    {
        let mut guard = Retire {
            queues: &self.queues,
            tenant: tenant.as_str(),
            armed: true,
            respawn: None,
        };
        loop {
            let batch = {
                let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
                match queues.get_mut(tenant.as_str()) {
                    Some(waiting) if !waiting.is_empty() => std::mem::take(waiting),
                    _ => {
                        queues.remove(tenant.as_str());
                        guard.armed = false;
                        return;
                    }
                }
            };
            let (qs, txs): (Vec<Q>, Vec<_>) = batch.into_iter().unzip();
            let answers = run(st.clone(), tenant.clone(), qs).await;
            for (tx, answer) in txs.into_iter().zip(answers) {
                // the caller gave up (its delivery deadline): nothing to tell
                let _gone = tx.send(answer);
            }
        }
    }
}

/// What a runner leaves behind. With `respawn` (the caller that ran its own
/// statement), questions still waiting go to a new runner, whether the caller
/// finished or was dropped. Without it, or when unwinding from a panic, the
/// entry goes, so the tenant's next caller starts afresh instead of queueing
/// behind a dead runner; the questions still queued then get `None`.
#[cfg(not(target_arch = "wasm32"))]
struct Retire<'a, Q, A> {
    queues: &'a std::sync::Mutex<HashMap<String, Waiting<Q, A>>>,
    tenant: &'a str,
    armed: bool,
    respawn: Option<Box<dyn FnOnce() + Send + 'a>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl<Q, A> Drop for Retire<'_, Q, A> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut queues = self.queues.lock().unwrap_or_else(|p| p.into_inner());
        let waiting = queues.get(self.tenant).is_some_and(|w| !w.is_empty());
        match self.respawn.take() {
            Some(respawn) if waiting && !std::thread::panicking() => {
                drop(queues);
                respawn();
            }
            _ => {
                queues.remove(self.tenant);
            }
        }
    }
}

/// The 5.2.14.2 stamp of one in-process delivery, written with every other
/// in-process delivery of its tenant waiting for the same statement. `None`:
/// the subscription is gone or the write failed, and nothing may be sent.
pub(super) async fn book_in_process(
    st: &AppState,
    tenant: &TenantId,
    sub_id: &str,
    now: &str,
) -> Option<Delivery> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        static BOOKINGS: std::sync::LazyLock<Coalescer<(String, String), Option<Delivery>>> =
            std::sync::LazyLock::new(Coalescer::new);
        BOOKINGS
            .call(
                st,
                tenant,
                (sub_id.to_owned(), now.to_owned()),
                |st, tenant, attempts| async move { book(&st, &tenant, &attempts).await },
            )
            .await
            .flatten()
    }
    #[cfg(target_arch = "wasm32")]
    {
        book(st, tenant, &[(sub_id.to_owned(), now.to_owned())])
            .await
            .pop()
            .flatten()
    }
}

/// One `record_deliveries` per round of distinct subscriptions. Two waiting
/// attempts on one subscription (two notifications in flight for it) go in
/// consecutive rounds, so each books exactly one notification and gets back
/// the `lastSuccess` it overwrote.
pub(super) async fn book(
    st: &AppState,
    tenant: &TenantId,
    attempts: &[(String, String)],
) -> Vec<Option<Delivery>> {
    let mut out: Vec<Option<Delivery>> = vec![None; attempts.len()];
    let mut left: Vec<usize> = (0..attempts.len()).collect();
    while !left.is_empty() {
        let mut seen = std::collections::HashSet::new();
        let (round, later): (Vec<usize>, Vec<usize>) = left.into_iter().partition(|i| {
            attempts
                .get(*i)
                .is_some_and(|(id, _)| seen.insert(id.as_str()))
        });
        let stamps: Vec<(&str, &str)> = round
            .iter()
            .filter_map(|i| attempts.get(*i))
            .map(|(id, now)| (id.as_str(), now.as_str()))
            .collect();
        match st
            .store
            .record_deliveries(tenant, Kind::Subscription, &stamps)
            .await
        {
            Ok(found) => {
                for (i, d) in round.into_iter().zip(found) {
                    if let Some(slot) = out.get_mut(i) {
                        *slot = d;
                    }
                }
            }
            Err(e) => tracing::warn!("bookkeeping writeback failed: {e}"),
        }
        left = later;
    }
    out
}

/// The failure write-back of one in-process delivery
/// ([`super::delivery::record_failure`]), queued per tenant behind the one in
/// flight, so overflowing sockets of one fan-out never hold more than one
/// pool connection for it. Rounds write in arrival order, so two failures of
/// one subscription roll `lastSuccess` back in the order they were booked.
pub(super) async fn fail_in_process(
    st: &AppState,
    tenant: &TenantId,
    sub_id: &str,
    prev_success: Option<Value>,
    ts: &str,
) -> Option<Value> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        type Failure = (String, Option<Value>, String);
        static FAILURES: std::sync::LazyLock<Coalescer<Failure, Option<Value>>> =
            std::sync::LazyLock::new(Coalescer::new);
        FAILURES
            .call(
                st,
                tenant,
                (sub_id.to_owned(), prev_success, ts.to_owned()),
                |st, tenant, failures: Vec<Failure>| async move {
                    let mut docs = Vec::with_capacity(failures.len());
                    for (id, prev, ts) in failures {
                        docs.push(
                            super::delivery::record_failure(
                                &st,
                                &tenant,
                                Kind::Subscription,
                                &id,
                                prev,
                                &ts,
                            )
                            .await,
                        );
                    }
                    docs
                },
            )
            .await
            .flatten()
    }
    #[cfg(target_arch = "wasm32")]
    {
        super::delivery::record_failure(st, tenant, Kind::Subscription, sub_id, prev_success, ts)
            .await
    }
}

/// The stored Entities `ids` names, read in one statement with every other
/// delivery of the tenant waiting for a read. `None`: the read failed (logged
/// once for the whole statement).
pub(super) async fn read_entities(
    st: &AppState,
    tenant: &TenantId,
    ids: Vec<String>,
) -> Option<Arc<HashMap<String, Value>>> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        type Read = Option<Arc<HashMap<String, Value>>>;
        static READS: std::sync::LazyLock<Coalescer<Vec<String>, Read>> =
            std::sync::LazyLock::new(Coalescer::new);
        READS
            .call(
                st,
                tenant,
                ids,
                |st, tenant, asks: Vec<Vec<String>>| async move {
                    let found = read(&st, &tenant, asks.iter().flatten()).await;
                    vec![found; asks.len()]
                },
            )
            .await
            .flatten()
    }
    #[cfg(target_arch = "wasm32")]
    {
        read(st, tenant, ids.iter()).await
    }
}

/// One `get_entities` over the union of `ids`.
async fn read<'a>(
    st: &AppState,
    tenant: &TenantId,
    ids: impl Iterator<Item = &'a String>,
) -> Option<Arc<HashMap<String, Value>>> {
    let union: std::collections::BTreeSet<&str> = ids.map(String::as_str).collect();
    let union: Vec<&str> = union.into_iter().collect();
    match st.store.get_entities(tenant, &union).await {
        Ok(found) => Some(Arc::new(found)),
        Err(e) => {
            tracing::warn!("notification path: reading the notified Entities failed: {e}");
            None
        }
    }
}
