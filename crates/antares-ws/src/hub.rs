// SPDX-License-Identifier: EUPL-1.2
//! The per-process table of live connections (ADR-0026 rule 9), each
//! connection's bounded queue and resume ring (rules 6 and 7), and the
//! admission bounds (rule 10).

use crate::envelope;
use crate::limits::{Limits, MAX_NOTIFICATION_BYTES};
use antares_ops::{Caller, Ops, Outbound};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::Notify;

/// The endpoint URI prefix of a connection (rule 4).
pub const ENDPOINT_PREFIX: &str = "urn:antares:ws:";

/// The digits of `u64::MAX`, the longest seq a notification can carry.
const U64_MAX_DIGITS: usize = 20;

/// The decimal digits of `n`.
fn digits(n: u64) -> usize {
    usize::try_from(n.checked_ilog10().unwrap_or(0))
        .unwrap_or(U64_MAX_DIGITS)
        .saturating_add(1)
}

/// Where a connection's operations run: the NGSI-LD root of its Tenant, or
/// one view, looked up again on every use so a removed view is noticed.
#[derive(Clone)]
pub(crate) enum Runner {
    Root(Ops),
    View(Ops, String),
}

/// One connection, live or within its grace period.
pub(crate) struct Conn {
    pub id: String,
    token: String,
    pub tenant: String,
    pub view: Option<String>,
    pub caller: Caller,
    subject: Vec<(String, Vec<u8>)>,
    pub runner: Runner,
    state: Mutex<State>,
    pub wake: Notify,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Queued>,
    ring: VecDeque<(u64, String)>,
    /// The next `x-seq`; the first notification is 1.
    next_seq: u64,
    /// The notifications lost to an overflow, first to last.
    lost: Option<(u64, u64)>,
    /// The Subscriptions this connection made, with their `showChanges`.
    subs: BTreeMap<String, bool>,
    /// The `expiresAt` the client asked of a Subscription, earlier or later
    /// than its lease (rule 7a).
    wishes: BTreeMap<String, String>,
    /// The socket serving it now, by generation; `None` within grace.
    attached: Option<u64>,
    generation: u64,
    /// The queue overflowed while a socket was attached: it closes.
    overflowed: bool,
}

struct Queued {
    seq: u64,
    sub: String,
    entity: Option<String>,
    coalescible: bool,
    text: String,
}

/// What a socket writes next.
pub(crate) enum Next {
    /// The queue overflowed: `x-gap` over the lost range, `x-closing`, close.
    Overflow(u64, u64),
    /// Notifications, in order.
    Send(Vec<String>),
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Conn {
    pub fn endpoint(&self) -> String {
        format!("{ENDPOINT_PREFIX}{}", self.id)
    }

    pub fn subscriptions(&self) -> Vec<String> {
        lock(&self.state).subs.keys().cloned().collect()
    }

    pub fn owns(&self, sub: &str) -> bool {
        lock(&self.state).subs.contains_key(sub)
    }

    pub fn subscription_count(&self) -> usize {
        lock(&self.state).subs.len()
    }

    pub fn record(&self, sub: &str, show_changes: bool) {
        lock(&self.state).subs.insert(sub.to_owned(), show_changes);
    }

    pub fn forget(&self, sub: &str) {
        let mut st = lock(&self.state);
        st.subs.remove(sub);
        st.wishes.remove(sub);
    }

    /// The `expiresAt` the client asked of `sub`, `None` for no expiry.
    pub fn want(&self, sub: &str, wish: Option<String>) {
        let mut st = lock(&self.state);
        match wish {
            Some(w) => st.wishes.insert(sub.to_owned(), w),
            None => st.wishes.remove(sub),
        };
    }

    /// What [`Conn::want`] last recorded for `sub`.
    pub fn wish(&self, sub: &str) -> Option<String> {
        lock(&self.state).wishes.get(sub).cloned()
    }

    pub fn is_attached(&self, generation: u64) -> bool {
        lock(&self.state).attached == Some(generation)
    }

    /// Offer one notification (rule 6): coalesced into a queued one of the
    /// same Subscription and single Entity when neither shows changes,
    /// queued when there is room, and otherwise lost with everything still
    /// queued, which closes an attached socket.
    fn offer(&self, n: Forwarded, limit: usize) -> Answer {
        let mut st = lock(&self.state);
        let Some(&show) = st.subs.get(&n.subscription) else {
            return Answer::Unknown;
        };
        let coalescible = !show && n.entity.is_some() && !n.changes;
        let into = coalescible
            .then(|| {
                st.queue
                    .iter()
                    .position(|q| q.coalescible && q.sub == n.subscription && q.entity == n.entity)
            })
            .flatten();
        let seq = into
            .and_then(|i| st.queue.get(i))
            .map_or(st.next_seq.max(1), |q| q.seq);
        // rendered once, with the seq it takes; the cap is judged as if the
        // seq had the most digits a u64 has, so a text at the cap stays
        // under it whatever seq it gets
        let text = envelope::notification(&n.rendered, seq, &n.subscription);
        if text
            .len()
            .saturating_add(U64_MAX_DIGITS.saturating_sub(digits(seq)))
            > MAX_NOTIFICATION_BYTES
        {
            return Answer::TooLarge;
        }
        if let Some(q) = into.and_then(|i| st.queue.get_mut(i)) {
            q.text = text;
            return Answer::Queued;
        }
        st.next_seq = seq.saturating_add(1);
        if st.queue.len() >= limit {
            let from = st.queue.front().map_or(seq, |q| q.seq);
            let from = st.lost.map_or(from, |(f, _)| f.min(from));
            st.lost = Some((from, seq));
            st.queue.clear();
            if st.attached.is_some() {
                st.overflowed = true;
            }
            drop(st);
            self.wake.notify_one();
            return Answer::Overflow;
        }
        st.queue.push_back(Queued {
            seq,
            sub: n.subscription,
            entity: n.entity,
            coalescible,
            text,
        });
        drop(st);
        self.wake.notify_one();
        Answer::Queued
    }

    /// What the socket of `generation` writes next; written notifications
    /// enter the ring, which keeps the last `limit`.
    pub fn next(&self, generation: u64, limit: usize) -> Option<Next> {
        let mut st = lock(&self.state);
        if st.attached != Some(generation) {
            return None;
        }
        if st.overflowed {
            st.overflowed = false;
            let (from, to) = st.lost.unwrap_or((0, 0));
            return Some(Next::Overflow(from, to));
        }
        let mut out = Vec::with_capacity(st.queue.len());
        while let Some(q) = st.queue.pop_front() {
            out.push(q.text.clone());
            st.ring.push_back((q.seq, q.text));
            while st.ring.len() > limit {
                st.ring.pop_front();
            }
        }
        Some(Next::Send(out))
    }

    /// Attach a socket: the next generation, which the previous socket of
    /// this connection, if any, sees as its end.
    pub fn attach(&self) -> u64 {
        let mut st = lock(&self.state);
        st.generation = st.generation.saturating_add(1);
        st.attached = Some(st.generation);
        st.overflowed = false;
        st.generation
    }

    /// Whether no socket attached since the one of `generation` detached.
    pub fn detached_since(&self, generation: u64) -> bool {
        let st = lock(&self.state);
        st.attached.is_none() && st.generation == generation
    }

    /// Detach the socket of `generation`; false when another took over.
    pub fn detach(&self, generation: u64) -> bool {
        let mut st = lock(&self.state);
        if st.attached != Some(generation) {
            return false;
        }
        st.attached = None;
        true
    }

    /// The messages a resumed socket starts with (rule 7): `x-gap` for
    /// what the ring no longer holds after `seq`, the ring after `seq`, and
    /// `x-gap` for what an overflow lost.
    pub fn replay(&self, seq: u64) -> Vec<String> {
        let st = lock(&self.state);
        let mut out = Vec::new();
        let after = seq.saturating_add(1);
        let oldest = st.ring.front().map(|(s, _)| *s);
        let lost_from = st.lost.map(|(f, _)| f);
        // the first seq the ring or the queue still holds
        let first_kept = oldest
            .or_else(|| st.queue.front().map(|q| q.seq))
            .or(lost_from)
            .unwrap_or(st.next_seq.max(1));
        if first_kept > after {
            out.push(envelope::gap(after, first_kept.saturating_sub(1)));
        }
        out.extend(
            st.ring
                .iter()
                .filter(|(s, _)| *s > seq)
                .map(|(_, t)| t.clone()),
        );
        if let Some((from, to)) = st.lost.filter(|(_, to)| *to > seq) {
            out.push(envelope::gap(from.max(after), to));
        }
        out
    }
}

/// One notification as the sink hands it to the connection: rendered, with
/// what the queue decides on. It is also the payload one pod sends another
/// for a connection it does not hold (rule 9).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Forwarded {
    /// The Tenant the notification is of.
    pub tenant: String,
    /// `subscriptionId`.
    pub subscription: String,
    /// The one Entity's id when the notification carries exactly one.
    pub entity: Option<String>,
    /// Whether any Attribute carries a previous value (`showChanges`).
    pub changes: bool,
    /// The TS 104 243 message the MQTT binding would publish.
    pub rendered: Value,
}

/// What a connection answers an offered notification.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Answer {
    /// Queued or coalesced.
    Queued,
    /// The queue was full: lost, the socket closes.
    Overflow,
    /// Larger than [`MAX_NOTIFICATION_BYTES`]: never written.
    TooLarge,
    /// No such connection here, or not one of its Subscriptions, or of
    /// another Tenant: never written.
    Unknown,
}

impl Forwarded {
    /// Read a notification for delivery; `None` when it names no
    /// Subscription.
    #[must_use]
    pub fn of(out: &Outbound) -> Option<Self> {
        let subscription = out.body.get("subscriptionId")?.as_str()?.to_owned();
        let data = out.body.get("data").and_then(Value::as_array);
        let entity = data
            .filter(|d| d.len() == 1)
            .and_then(|d| d.first())
            .and_then(|e| e.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let changes = data.is_some_and(|d| d.iter().any(carries_previous));
        let tenant = out
            .receiver_info
            .iter()
            .rev()
            .find(|(k, _)| k.eq_ignore_ascii_case("NGSILD-Tenant"))
            .map_or_else(|| "default".to_owned(), |(_, v)| v.clone());
        Some(Self {
            tenant,
            subscription,
            entity,
            changes,
            rendered: antares_ops::build_message(
                &out.body,
                &out.accept,
                Some(&out.link),
                &out.receiver_info,
            ),
        })
    }
}

fn carries_previous(entity: &Value) -> bool {
    entity.as_object().is_some_and(|o| {
        o.values().any(|attr| {
            let one = |a: &Value| {
                a.as_object()
                    .is_some_and(|a| a.keys().any(|k| k.starts_with("previous")))
            };
            match attr {
                Value::Array(instances) => instances.iter().any(one),
                a => one(a),
            }
        })
    })
}

/// Cross-pod delivery (rule 9, `bus = nats`): a request on the connection's
/// own subject, answered by the pod that holds it. The broker implements it
/// over its bus; the hub only asks and answers.
pub trait Relay: Send + Sync {
    /// Send `payload` (a serialized [`Forwarded`]) to whichever pod holds
    /// `connection`; its serialized [`Answer`], or `None` when no pod
    /// answered.
    fn request<'a>(
        &'a self,
        connection: &'a str,
        payload: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<Vec<u8>>> + Send + 'a>>;
    /// Start answering requests for `connection` on this pod, through
    /// [`Hub::answer`].
    fn listen(&self, hub: std::sync::Weak<Hub>, connection: &str);
    /// Stop answering for `connection`.
    fn unlisten(&self, connection: &str);
}

/// The connections of this process.
pub struct Hub {
    pub(crate) limits: Limits,
    conns: Mutex<HashMap<String, Arc<Conn>>>,
    relay: Mutex<Option<Arc<dyn Relay>>>,
    overflows: std::sync::atomic::AtomicU64,
}

/// Why an upgrade was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// `ANTARES_WS_MAX_CONNECTIONS`.
    Process,
    /// `ANTARES_WS_MAX_CONNECTIONS_PER_TENANT`.
    Tenant,
}

impl Hub {
    /// An empty table under `limits`.
    ///
    /// ```
    /// let hub = antares_ws::Hub::new(antares_ws::Limits::default());
    /// assert_eq!(hub.connections(), 0);
    /// ```
    #[must_use]
    pub fn new(limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            limits,
            conns: Mutex::default(),
            relay: Mutex::default(),
            overflows: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// The live connections and those within their grace period.
    #[must_use]
    pub fn connections(&self) -> usize {
        lock(&self.conns).len()
    }

    /// The connections a socket holds now: [`Hub::connections`] less those
    /// waiting out their grace period.
    ///
    /// ```
    /// let hub = antares_ws::Hub::new(antares_ws::Limits::default());
    /// assert_eq!(hub.attached(), 0);
    /// ```
    #[must_use]
    pub fn attached(&self) -> usize {
        let conns: Vec<Arc<Conn>> = lock(&self.conns).values().cloned().collect();
        conns
            .iter()
            .filter(|c| lock(&c.state).attached.is_some())
            .count()
    }

    /// Connections closed for overflow since start (`/q/metrics`).
    #[must_use]
    pub fn overflows(&self) -> u64 {
        self.overflows.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Deliver to connections other pods hold through `relay`.
    pub fn set_relay(&self, relay: Arc<dyn Relay>) {
        *lock(&self.relay) = Some(relay);
    }

    fn relay(&self) -> Option<Arc<dyn Relay>> {
        lock(&self.relay).clone()
    }

    pub(crate) fn get(&self, id: &str) -> Option<Arc<Conn>> {
        lock(&self.conns).get(id).cloned()
    }

    /// Admit and register a new connection (rule 10, W2).
    pub(crate) fn open(
        self: &Arc<Self>,
        tenant: String,
        view: Option<String>,
        caller: Caller,
        subject: Vec<(String, Vec<u8>)>,
        runner: Runner,
    ) -> Result<Arc<Conn>, Refused> {
        let mut conns = lock(&self.conns);
        if conns.len() >= self.limits.max_connections {
            return Err(Refused::Process);
        }
        if conns.values().filter(|c| c.tenant == tenant).count()
            >= self.limits.max_connections_per_tenant
        {
            return Err(Refused::Tenant);
        }
        let mut id = crate::random::id();
        while conns.contains_key(&id) {
            id = crate::random::id();
        }
        let conn = Arc::new(Conn {
            id: id.clone(),
            token: crate::random::token(),
            tenant,
            view,
            caller,
            subject,
            runner,
            state: Mutex::new(State {
                next_seq: 1,
                ..State::default()
            }),
            wake: Notify::new(),
        });
        conns.insert(id.clone(), conn.clone());
        drop(conns);
        if let Some(relay) = self.relay() {
            relay.listen(Arc::downgrade(self), &id);
        }
        Ok(conn)
    }

    /// The token a client resumes `conn` with.
    pub(crate) fn token(conn: &Conn) -> &str {
        &conn.token
    }

    /// Remove a connection from the table.
    pub(crate) fn close(&self, id: &str) {
        lock(&self.conns).remove(id);
        if let Some(relay) = self.relay() {
            relay.unlisten(id);
        }
    }

    /// The connection a resume names, when every check of rule 7 holds:
    /// the token (constant time), the Tenant, the view and the subject.
    pub(crate) fn resumable(
        &self,
        connection: &str,
        token: &str,
        fresh: &Conn,
    ) -> Option<Arc<Conn>> {
        let conn = self.get(connection)?;
        let same_token = crate::random::equal(conn.token.as_bytes(), token.as_bytes());
        (same_token
            && conn.id != fresh.id
            && conn.tenant == fresh.tenant
            && conn.view == fresh.view
            && conn.subject == fresh.subject)
            .then_some(conn)
    }

    /// Offer a notification to a connection this pod holds.
    fn offer_here(&self, id: &str, n: Forwarded) -> Answer {
        let Some(conn) = self.get(id) else {
            return Answer::Unknown;
        };
        // rule 9: a write compares the Tenants
        if conn.tenant != n.tenant {
            return Answer::Unknown;
        }
        let answer = conn.offer(n, self.limits.queue);
        if answer == Answer::Overflow {
            self.overflows
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        answer
    }

    /// Answer a request another pod sent for a connection on this one.
    #[must_use]
    pub fn answer(&self, connection: &str, payload: &[u8]) -> Vec<u8> {
        let answer = serde_json::from_slice::<Forwarded>(payload)
            .map_or(Answer::Unknown, |n| self.offer_here(connection, n));
        serde_json::to_vec(&answer).unwrap_or_default()
    }

    /// Deliver to the connection `id`: here when this pod holds it, through
    /// the relay otherwise.
    pub(crate) async fn deliver(&self, id: &str, n: Forwarded) -> Answer {
        if self.get(id).is_some() {
            return self.offer_here(id, n);
        }
        let Some(relay) = self.relay() else {
            return Answer::Unknown;
        };
        // a Forwarded always serializes; an empty payload would be Unknown
        // on the other side
        let payload = serde_json::to_vec(&n).unwrap_or_default();
        relay
            .request(id, payload)
            .await
            .and_then(|a| serde_json::from_slice(&a).ok())
            .unwrap_or(Answer::Unknown)
    }
}
