//! One socket: the welcome, the client's operations through the operation
//! layer, the queue drained in order, Ping/Pong, idle, lifetime and resume
//! (ADR-0026 rules 4 to 8).

use crate::envelope::{self, ClientOp, Unread};
use crate::hub::{Conn, Hub, Next, Runner};
use antares_ops::{Caller, OperationId, OpsRequest};
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::http::{header, HeaderValue, Method, StatusCode};
use serde_json::{Map, Value};
use std::sync::Arc;
use tokio::time::{interval_at, sleep_until, Instant, MissedTickBehavior};

tokio::task_local! {
    /// The connection a Subscription is being made for (rule 4, W6): the
    /// sink accepts `urn:antares:ws:<id>` only inside this scope.
    pub(crate) static CREATING: String;
}

/// The close codes this surface uses (RFC 6455 7.4.1).
mod code {
    pub const NORMAL: u16 = 1000;
    pub const UNSUPPORTED: u16 = 1003;
    pub const POLICY: u16 = 1008;
    pub const TOO_BIG: u16 = 1009;
    pub const TRY_AGAIN: u16 = 1013;
}

/// The largest answer body read back from the operation layer.
const ANSWER_BYTES: usize = 1024 * 1024;

/// Why a socket ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// The client closed it, or it broke.
    Gone,
    /// Another socket resumed the connection.
    TakenOver,
    /// The server closed it with `x-closing` and this reason.
    Closed(&'static str),
}

fn not_found() -> (u16, Option<Value>) {
    (StatusCode::NOT_FOUND.as_u16(), Some(envelope::not_found()))
}

fn bad(detail: &str) -> (u16, Option<Value>) {
    (
        StatusCode::BAD_REQUEST.as_u16(),
        Some(envelope::bad_request(detail)),
    )
}

/// A path segment: everything but the unreserved characters and `:`
/// percent-encoded, so a Subscription id is one segment whatever it holds.
fn segment(id: &str) -> String {
    id.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~:".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Run one operation where the connection runs, inside its marker.
async fn run(conn: &Conn, op: OperationId, req: OpsRequest) -> (u16, Option<Value>) {
    let caller: &Caller = &conn.caller;
    let answer = CREATING
        .scope(conn.id.clone(), async {
            match &conn.runner {
                Runner::Root(ops) => ops.run(op, caller, req).await,
                Runner::View(ops, id) => match ops.view(id) {
                    Some(view) => view.run(op, caller, req).await,
                    None => Err(antares_ops::Refusal::NotAllowed { allow: Vec::new() }),
                },
            }
        })
        .await
        .unwrap_or_else(|refusal| refusal.ngsi_ld());
    let status = answer.status().as_u16();
    let body = axum::body::to_bytes(answer.into_body(), ANSWER_BYTES)
        .await
        .ok()
        .filter(|b| !b.is_empty())
        .and_then(|b| serde_json::from_slice(&b).ok());
    (status, body)
}

fn json_request(method: Method, path: String, body: &Map<String, Value>) -> OpsRequest {
    let bytes = Value::Object(body.clone()).to_string();
    let media = if body.contains_key("@context") {
        "application/ld+json"
    } else {
        "application/json"
    };
    OpsRequest::new(method, path)
        .header(header::CONTENT_TYPE, HeaderValue::from_static(media))
        .header(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()))
        .body(bytes)
}

/// The endpoint member a client message names, when it names one.
fn named_endpoint(body: &Map<String, Value>) -> Option<&Value> {
    body.get("notification")?.get("endpoint")?.get("uri")
}

fn show_changes(body: &Map<String, Value>) -> Option<bool> {
    body.get("notification")?.get("showChanges")?.as_bool()
}

async fn retrieve(conn: &Conn, id: &str) -> Option<Value> {
    let req = OpsRequest::new(
        Method::GET,
        format!("/ngsi-ld/v1/subscriptions/{}", segment(id)),
    );
    run(conn, OperationId::RetrieveSubscription, req).await.1
}

/// `x-subscribe`: Create Subscription (10.5.2) with this connection as the
/// endpoint; a body naming another endpoint is refused (W6), past
/// `max_subscriptions` too (W9).
async fn subscribe(conn: &Conn, max: usize, mut body: Map<String, Value>) -> (u16, Option<Value>) {
    if conn.subscription_count() >= max {
        return bad(&format!("a connection makes at most {max} Subscriptions"));
    }
    let own = conn.endpoint();
    match named_endpoint(&body) {
        None => {}
        Some(Value::String(uri)) if *uri == own => {}
        Some(_) => {
            return bad("the endpoint of a Subscription made over this socket is the socket")
        }
    }
    let notification = body
        .entry("notification")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(notification) = notification.as_object_mut() else {
        return bad("notification is a JSON object");
    };
    let endpoint = notification
        .entry("endpoint")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(endpoint) = endpoint.as_object_mut() else {
        return bad("notification.endpoint is a JSON object");
    };
    endpoint.insert("uri".into(), Value::String(own));
    // the id is known before the Subscription exists, so a notification
    // sent the moment it is created already finds it
    let id = match body.get("id") {
        None => {
            let id = format!("urn:ngsi-ld:Subscription:ws-{}", crate::random::id());
            body.insert("id".into(), Value::String(id.clone()));
            id
        }
        Some(Value::String(id)) => id.clone(),
        Some(_) => return bad("id is a URI"),
    };
    if conn.owns(&id) {
        return (
            StatusCode::CONFLICT.as_u16(),
            Some(serde_json::json!({
                "type": "https://uri.etsi.org/ngsi-ld/errors/AlreadyExists",
                "title": "AlreadyExists",
                "status": 409,
            })),
        );
    }
    conn.record(&id, show_changes(&body).unwrap_or(false));
    let req = json_request(Method::POST, "/ngsi-ld/v1/subscriptions".into(), &body);
    let (status, answer) = run(conn, OperationId::CreateSubscription, req).await;
    if status != StatusCode::CREATED.as_u16() {
        conn.forget(&id);
        return (status, answer);
    }
    (status, retrieve(conn, &id).await)
}

/// `x-update`: Update Subscription (10.5.3) of one this connection made.
async fn update(conn: &Conn, id: &str, fragment: Map<String, Value>) -> (u16, Option<Value>) {
    if !conn.owns(id) {
        return not_found();
    }
    match named_endpoint(&fragment) {
        None => {}
        Some(Value::String(uri)) if *uri == conn.endpoint() => {}
        Some(_) => {
            return bad("the endpoint of a Subscription made over this socket is the socket")
        }
    }
    let path = format!("/ngsi-ld/v1/subscriptions/{}", segment(id));
    let (status, answer) = run(
        conn,
        OperationId::UpdateSubscription,
        json_request(Method::PATCH, path, &fragment),
    )
    .await;
    if status != StatusCode::NO_CONTENT.as_u16() {
        return (status, answer);
    }
    if let Some(show) = show_changes(&fragment) {
        conn.record(id, show);
    }
    (status, retrieve(conn, id).await)
}

/// `x-unsubscribe` and the end of the grace period: Delete Subscription.
pub(crate) async fn unsubscribe(conn: &Conn, id: &str) -> (u16, Option<Value>) {
    if !conn.owns(id) {
        return not_found();
    }
    let req = OpsRequest::new(
        Method::DELETE,
        format!("/ngsi-ld/v1/subscriptions/{}", segment(id)),
    );
    let (status, answer) = run(conn, OperationId::DeleteSubscription, req).await;
    if status == StatusCode::NO_CONTENT.as_u16() || status == StatusCode::NOT_FOUND.as_u16() {
        conn.forget(id);
    }
    (status, answer)
}

async fn send(socket: &mut WebSocket, text: String) -> bool {
    socket.send(Message::Text(text.into())).await.is_ok()
}

async fn close(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _closed = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

/// One attached socket and what it has seen.
struct Session {
    hub: Arc<Hub>,
    conn: Arc<Conn>,
    generation: u64,
    socket: WebSocket,
    heard: Instant,
    ponged: Instant,
    first: bool,
}

/// Write each message in order; false when the socket broke.
async fn send_all(socket: &mut WebSocket, texts: Vec<String>) -> bool {
    for t in texts {
        if !send(socket, t).await {
            return false;
        }
    }
    true
}

impl Session {
    /// The server closes: `x-closing`, then the close frame.
    async fn closing(&mut self, code: u16, reason: &'static str) -> End {
        let _closing = send(&mut self.socket, envelope::closing(reason)).await;
        close(&mut self.socket, code, reason).await;
        End::Closed(reason)
    }

    /// A frame from the client.
    async fn incoming(&mut self, frame: Option<Result<Message, axum::Error>>) -> Option<End> {
        let text = match frame {
            Some(Ok(Message::Text(t))) => t,
            Some(Ok(Message::Pong(_))) => {
                self.ponged = Instant::now();
                return None;
            }
            Some(Ok(Message::Ping(_))) => return None,
            Some(Ok(Message::Binary(_))) => {
                close(
                    &mut self.socket,
                    code::UNSUPPORTED,
                    "binary frames are not read",
                )
                .await;
                return Some(End::Closed("binary"));
            }
            Some(Ok(Message::Close(_)) | Err(_)) | None => return Some(End::Gone),
        };
        self.heard = Instant::now();
        let first = std::mem::take(&mut self.first);
        let message = match envelope::read(text.as_str()) {
            Ok(m) => m,
            Err(Unread::Close(why)) => {
                close(&mut self.socket, code::TOO_BIG, why).await;
                return Some(End::Closed("bound"));
            }
            Err(Unread::Bad(why)) => {
                let r = envelope::result(None, 400, Some(envelope::bad_request(&why)));
                return (!send(&mut self.socket, r).await).then_some(End::Gone);
            }
        };
        let id = message.id;
        let (status, body) = match message.op {
            ClientOp::Subscribe(body) => {
                subscribe(&self.conn, self.hub.limits.max_subscriptions, body).await
            }
            ClientOp::Update {
                subscription,
                fragment,
            } => update(&self.conn, &subscription, fragment).await,
            ClientOp::Unsubscribe { subscription } => unsubscribe(&self.conn, &subscription).await,
            ClientOp::Resume {
                connection,
                token,
                seq,
            } => {
                return self
                    .resume(id.as_deref(), first, &connection, &token, seq)
                    .await
            }
        };
        let r = envelope::result(id.as_deref(), status, body);
        (!send(&mut self.socket, r).await).then_some(End::Gone)
    }

    /// `x-resume` (rule 7): the first message of a new socket takes over a
    /// connection within its grace period; every failure is the same 404.
    async fn resume(
        &mut self,
        id: Option<&str>,
        first: bool,
        connection: &str,
        token: &str,
        seq: u64,
    ) -> Option<End> {
        let resumed = first
            .then(|| self.hub.resumable(connection, token, &self.conn))
            .flatten();
        let Some(old) = resumed else {
            let r = envelope::result(id, 404, Some(envelope::not_found()));
            return (!send(&mut self.socket, r).await).then_some(End::Gone);
        };
        self.hub.close(&self.conn.id);
        self.conn = old;
        self.generation = self.conn.attach();
        let mut replay = vec![envelope::result(id, 200, None)];
        replay.extend(self.conn.replay(seq));
        if !send_all(&mut self.socket, replay).await {
            return Some(End::Gone);
        }
        // what was queued while the socket was away follows
        self.conn.wake.notify_one();
        None
    }

    /// The queue has something (or overflowed, or another socket took the
    /// connection).
    async fn drain(&mut self) -> Option<End> {
        match self.conn.next(self.generation, self.hub.limits.queue) {
            None => Some(End::TakenOver),
            Some(Next::Overflow(from, to)) => {
                let _gap = send(&mut self.socket, envelope::gap(from, to)).await;
                Some(self.closing(code::TRY_AGAIN, "overflow").await)
            }
            Some(Next::Send(texts)) => {
                (!send_all(&mut self.socket, texts).await).then_some(End::Gone)
            }
        }
    }

    /// The heartbeat tick: takeover, a removed view, a missing Pong, idle,
    /// then the Ping.
    async fn tick(&mut self) -> Option<End> {
        let now = Instant::now();
        if !self.conn.is_attached(self.generation) {
            return Some(End::TakenOver);
        }
        if let Runner::View(ops, view) = &self.conn.runner {
            if ops.view(view).is_none() {
                return Some(self.closing(code::POLICY, "view-removed").await);
            }
        }
        let limits = &self.hub.limits;
        if now.duration_since(self.ponged) > limits.pong_within {
            return Some(End::Gone);
        }
        if self.conn.subscription_count() == 0 && now.duration_since(self.heard) > limits.idle {
            return Some(self.closing(code::NORMAL, "idle").await);
        }
        let ping = self.socket.send(Message::Ping(Vec::new().into())).await;
        ping.is_err().then_some(End::Gone)
    }

    async fn run(&mut self) -> End {
        if !send(
            &mut self.socket,
            envelope::welcome(&self.conn.id, Hub::token(&self.conn)),
        )
        .await
        {
            return End::Gone;
        }
        let limits = self.hub.limits.clone();
        let opened = Instant::now();
        let deadline = opened.checked_add(limits.max_lifetime).unwrap_or(opened);
        let mut ping = interval_at(
            opened.checked_add(limits.ping_every).unwrap_or(opened),
            limits.ping_every,
        );
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            let conn = self.conn.clone();
            let end = tokio::select! {
                frame = self.socket.recv() => self.incoming(frame).await,
                () = conn.wake.notified() => self.drain().await,
                _ = ping.tick() => self.tick().await,
                () = sleep_until(deadline) => Some(self.closing(code::NORMAL, "lifetime").await),
            };
            if let Some(end) = end {
                return end;
            }
        }
    }
}

/// Serve `conn` on `socket` until it ends, then detach it.
pub(crate) async fn serve(hub: Arc<Hub>, conn: Arc<Conn>, socket: WebSocket) {
    let generation = conn.attach();
    tracing::info!(
        connection = %conn.id,
        tenant = %conn.tenant,
        view = conn.view.as_deref().unwrap_or(""),
        "websocket opened"
    );
    let now = Instant::now();
    let mut session = Session {
        hub,
        conn,
        generation,
        socket,
        heard: now,
        ponged: now,
        first: true,
    };
    let end = session.run().await;
    let Session {
        hub,
        conn,
        generation,
        ..
    } = session;
    let reason = match end {
        End::Gone => "gone",
        End::TakenOver => "resumed elsewhere",
        End::Closed(why) => why,
    };
    tracing::info!(
        connection = %conn.id,
        tenant = %conn.tenant,
        view = conn.view.as_deref().unwrap_or(""),
        reason,
        "websocket closed"
    );
    if end != End::TakenOver && conn.detach(generation) {
        linger(hub, conn, generation);
    }
}

/// Rule 7: a connection without Subscriptions goes at once; one with
/// Subscriptions waits `resume_grace` for a resume, and then its
/// Subscriptions are deleted like any client delete.
fn linger(hub: Arc<Hub>, conn: Arc<Conn>, generation: u64) {
    if conn.subscription_count() == 0 {
        hub.close(&conn.id);
        return;
    }
    let grace = hub.limits.resume_grace;
    tokio::spawn(async move {
        tokio::time::sleep(grace).await;
        // a resume in the meantime attached a later generation and took it
        if !conn.detached_since(generation) {
            return;
        }
        for sub in conn.subscriptions() {
            let _deleted = unsubscribe(&conn, &sub).await;
        }
        hub.close(&conn.id);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subscription_id_is_one_path_segment() {
        assert_eq!(
            segment("urn:ngsi-ld:Subscription:1"),
            "urn:ngsi-ld:Subscription:1"
        );
        assert_eq!(segment("urn:x:a/b?c#d e"), "urn:x:a%2Fb%3Fc%23d%20e");
    }

    #[test]
    fn the_answer_codes_are_the_http_ones() {
        assert_eq!(not_found().0, 404);
        assert_eq!(bad("x").0, 400);
    }
}
