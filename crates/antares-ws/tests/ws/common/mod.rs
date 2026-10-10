// SPDX-License-Identifier: EUPL-1.2
//! One broker on a real socket with both WebSocket surfaces and the sink,
//! a client that speaks the envelope, and the HTTP calls the tests make
//! beside it.

use antares_ops::test_kit::TestBroker;
use antares_ws::{Hub, Limits, Sink, ViewWs, Ws};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// How long a test waits for a message it expects.
pub const WAIT: Duration = Duration::from_secs(10);

/// A broker serving both surfaces on `addr`.
pub struct Broker {
    /// Kept alive: its reload loop serves `/q/views`.
    pub _kept: TestBroker,
    /// The hub the surfaces and the sink share.
    pub hub: Arc<Hub>,
    /// Where it listens.
    pub addr: SocketAddr,
    /// An HTTP client for the NGSI-LD API beside the sockets.
    pub http: Http,
}

/// The subject header the tests' policy engine reads.
pub const USER: &str = "x-user";

/// Start a broker under `limits`.
pub async fn broker(limits: Limits) -> Result<Broker, String> {
    broker_asking(limits, None).await
}

/// Start a broker under `limits` asking `engine`.
pub async fn broker_asking(
    limits: Limits,
    engine: Option<Arc<dyn antares_ops::policy::PolicyEngine>>,
) -> Result<Broker, String> {
    // read once by the broker at first use, before any broker of this
    // binary starts
    static SUBJECT: std::sync::Once = std::sync::Once::new();
    SUBJECT.call_once(|| std::env::set_var("ANTARES_POLICY_SUBJECT_HEADERS", USER));
    let hub = Hub::new(limits);
    let broker = TestBroker::with_sinks(vec![Box::new(Sink::new(hub.clone()))], engine).await?;
    let app = broker.serve_mounted(vec![
        antares_ops::mount(Ws::new(hub.clone())),
        antares_ops::mount(ViewWs::new(hub.clone())),
    ])?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        let _served = axum::serve(listener, app).await;
    });
    Ok(Broker {
        _kept: broker,
        hub,
        addr,
        http: Http { addr },
    })
}

/// The HTTP side: one request per call, over its own connection.
#[derive(Clone, Copy)]
pub struct Http {
    addr: SocketAddr,
}

impl Http {
    /// `method path` with `headers` and a JSON `body`; the status and the
    /// body as JSON (`Null` when empty).
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<(u16, Value), String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let payload = body.map(Value::to_string).unwrap_or_default();
        let mut req =
            format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
        for (k, v) in headers {
            let _w = write!(req, "{k}: {v}\r\n");
        }
        if body.is_some() {
            req.push_str("Content-Type: application/json\r\n");
        }
        let _w = write!(req, "Content-Length: {}\r\n\r\n{payload}", payload.len());
        let mut s = TcpStream::connect(self.addr)
            .await
            .map_err(|e| e.to_string())?;
        s.write_all(req.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).await.map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&raw);
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("no status in {head:?}"))?;
        let chunked = head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked");
        let body = if chunked {
            dechunk(body)?
        } else {
            body.to_owned()
        };
        let json = if body.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(body.trim()).map_err(|e| format!("{e}: {body}"))?
        };
        Ok((status, json))
    }

    /// Create a `Car` in `tenant` (`None`: the default Tenant).
    pub async fn car(&self, tenant: Option<&str>, id: &str, speed: i64) -> Result<(), String> {
        let headers: Vec<(&str, &str)> = tenant.map(|t| ("NGSILD-Tenant", t)).into_iter().collect();
        let body = json!({"id": id, "type": "Car", "speed": {"type": "Property", "value": speed}});
        let (status, answer) = self
            .call("POST", "/ngsi-ld/v1/entities", &headers, Some(&body))
            .await?;
        (status == 201)
            .then_some(())
            .ok_or(format!("create {id}: {status} {answer}"))
    }

    /// Set `speed` of a Car.
    pub async fn speed(&self, tenant: Option<&str>, id: &str, speed: i64) -> Result<(), String> {
        let headers: Vec<(&str, &str)> = tenant.map(|t| ("NGSILD-Tenant", t)).into_iter().collect();
        let body = json!({"speed": {"type": "Property", "value": speed}});
        let path = format!("/ngsi-ld/v1/entities/{id}/attrs");
        let (status, answer) = self.call("PATCH", &path, &headers, Some(&body)).await?;
        (status == 204)
            .then_some(())
            .ok_or(format!("speed {id}: {status} {answer}"))
    }
}

/// The payload of a chunked HTTP/1.1 body.
fn dechunk(mut rest: &str) -> Result<String, String> {
    let mut out = String::new();
    loop {
        let (size, tail) = rest
            .split_once("\r\n")
            .ok_or("a chunk without a size line")?;
        let size = usize::from_str_radix(size.trim(), 16)
            .map_err(|e| format!("chunk size {size:?}: {e}"))?;
        if size == 0 {
            return Ok(out);
        }
        out.push_str(tail.get(..size).ok_or("a chunk shorter than its size")?);
        rest = tail
            .get(size..)
            .and_then(|t| t.strip_prefix("\r\n"))
            .ok_or("a chunk without its CRLF")?;
    }
}

/// One client socket.
pub struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    /// `x-connection` of its welcome.
    pub connection: String,
    /// `x-resume-token` of its welcome.
    pub token: String,
}

/// The upgrade's answer when it was refused.
pub type Refusal = Response<Option<Vec<u8>>>;

/// Open `path` with `headers`; the refusal when the upgrade is not
/// answered with 101.
pub async fn open(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> Result<Result<Client, Refusal>, String> {
    let mut req = format!("ws://{addr}{path}")
        .into_client_request()
        .map_err(|e| e.to_string())?;
    for (k, v) in headers {
        req.headers_mut().append(
            HeaderName::from_bytes(k.as_bytes()).map_err(|e| e.to_string())?,
            HeaderValue::from_str(v).map_err(|e| e.to_string())?,
        );
    }
    let ws = match tokio_tungstenite::connect_async(req).await {
        Ok((ws, _)) => ws,
        Err(tokio_tungstenite::tungstenite::Error::Http(refused)) => return Ok(Err(*refused)),
        Err(e) => return Err(e.to_string()),
    };
    let mut client = Client {
        ws,
        connection: String::new(),
        token: String::new(),
    };
    let welcome = client.next().await?;
    if welcome.pointer("/metadata/x-op") != Some(&json!("x-welcome")) {
        return Err(format!("not a welcome: {welcome}"));
    }
    client.connection = text(&welcome, "/metadata/x-connection");
    client.token = text(&welcome, "/metadata/x-resume-token");
    Ok(Ok(client))
}

/// Open `path`, which must be admitted.
pub async fn connect(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> Result<Client, String> {
    open(addr, path, headers)
        .await?
        .map_err(|r| format!("upgrade refused: {}", r.status()))
}

/// The string at `pointer`, empty when absent.
#[must_use]
pub fn text(v: &Value, pointer: &str) -> String {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

impl Client {
    /// Send one text frame.
    pub async fn send_text(&mut self, text: String) -> Result<(), String> {
        self.ws
            .send(Message::Text(text.into()))
            .await
            .map_err(|e| e.to_string())
    }

    /// Send one envelope.
    pub async fn send(&mut self, metadata: Value, body: Option<Value>) -> Result<(), String> {
        let mut m = json!({"metadata": metadata});
        if let (Some(b), Some(o)) = (body, m.as_object_mut()) {
            o.insert("body".into(), b);
        }
        self.send_text(m.to_string()).await
    }

    /// The next text message as JSON, within [`WAIT`].
    pub async fn next(&mut self) -> Result<Value, String> {
        loop {
            let frame = tokio::time::timeout(WAIT, self.ws.next())
                .await
                .map_err(|_| "no message within the wait".to_owned())?
                .ok_or("the socket closed")?
                .map_err(|e| e.to_string())?;
            match frame {
                Message::Text(t) => {
                    return serde_json::from_str(t.as_str()).map_err(|e| e.to_string())
                }
                Message::Close(c) => return Err(format!("closed: {c:?}")),
                _ => {}
            }
        }
    }

    /// The close frame's code once the server closes, skipping messages.
    pub async fn closed(&mut self) -> Result<(u16, Vec<Value>), String> {
        let mut seen = Vec::new();
        loop {
            let frame = tokio::time::timeout(WAIT, self.ws.next())
                .await
                .map_err(|_| "not closed within the wait".to_owned())?;
            match frame {
                Some(Ok(Message::Text(t))) => {
                    seen.push(serde_json::from_str(t.as_str()).map_err(|e| e.to_string())?);
                }
                Some(Ok(Message::Close(Some(c)))) => return Ok((c.code.into(), seen)),
                Some(Ok(Message::Close(None))) | None => return Ok((1005, seen)),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(format!("{e}; seen {seen:?}")),
            }
        }
    }

    /// `x-subscribe` with `body`; the `x-result`.
    pub async fn subscribe(&mut self, body: Value) -> Result<Value, String> {
        self.send(json!({"x-op": "x-subscribe", "x-id": "s"}), Some(body))
            .await?;
        self.result().await
    }

    /// The next `x-result`.
    pub async fn result(&mut self) -> Result<Value, String> {
        let m = self.next().await?;
        if m.pointer("/metadata/x-op") == Some(&json!("x-result")) {
            Ok(m)
        } else {
            Err(format!("not a result: {m}"))
        }
    }

    /// The next `x-notification`.
    pub async fn notification(&mut self) -> Result<Value, String> {
        let m = self.next().await?;
        if m.pointer("/metadata/x-op") == Some(&json!("x-notification")) {
            Ok(m)
        } else {
            Err(format!("not a notification: {m}"))
        }
    }

    /// The raw stream, for frames the envelope helpers do not send.
    pub fn ws_mut(&mut self) -> &mut WebSocketStream<MaybeTlsStream<TcpStream>> {
        &mut self.ws
    }

    /// Close the socket from the client side.
    pub async fn close(mut self) {
        let _closed = self.ws.close(None).await;
    }
}

/// A Subscription to Cars' `speed`.
#[must_use]
pub fn cars() -> Value {
    json!({"type": "Subscription", "entities": [{"type": "Car"}], "watchedAttributes": ["speed"], "notification": {"attributes": ["speed"]}})
}

/// The status a result carries.
#[must_use]
pub fn status(result: &Value) -> u64 {
    result
        .pointer("/metadata/x-status")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// `v` with `x` at `path`, making the objects on the way.
#[must_use]
pub fn with(mut v: Value, path: &[&str], x: Value) -> Value {
    let Some((last, parents)) = path.split_last() else {
        return x;
    };
    let mut cur = &mut v;
    for k in parents {
        let Some(o) = cur.as_object_mut() else {
            return v;
        };
        cur = o.entry(*k).or_insert_with(|| json!({}));
    }
    if let Some(o) = cur.as_object_mut() {
        o.insert((*last).to_owned(), x);
    }
    v
}

/// Wait until `what` of the hub holds, within [`WAIT`].
pub async fn until(hub: &Hub, what: impl Fn(&Hub) -> bool) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !what(hub) {
        if tokio::time::Instant::now() > deadline {
            return Err(format!(
                "not within the wait: {} connections, {} attached, {} overflows",
                hub.connections(),
                hub.attached(),
                hub.overflows()
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

impl Client {
    /// `x-resume` of `connection` with `token` after `seq`; the `x-result`.
    pub async fn resume(
        &mut self,
        connection: &str,
        token: &str,
        seq: u64,
    ) -> Result<Value, String> {
        let m = json!({"x-op": "x-resume", "x-id": "r", "x-connection": connection, "x-resume-token": token, "x-seq": seq});
        self.send(m, None).await?;
        self.result().await
    }
}
