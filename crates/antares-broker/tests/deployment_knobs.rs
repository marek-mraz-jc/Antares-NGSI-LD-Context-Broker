// SPDX-License-Identifier: EUPL-1.2
//! The DoS-bound deployment knobs (bounds.rs: ANTARES_MAX_BATCH_ITEMS,
//! ANTARES_MAX_LIMIT, ANTARES_DEFAULT_LIMIT, ANTARES_TEMPORAL_INSTANCE_LIMIT,
//! ANTARES_TEMPORAL_DEFAULT_INSTANCES, ANTARES_FED_FANOUT), proven against
//! the real binary: a value set in the environment is the one `/q/health`
//! publishes and the one a request meets, and a value that is no positive
//! number keeps the default.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

mod common;
#[path = "common/process.rs"]
mod process;
use common::free_port;

struct Broker(Child);

impl Drop for Broker {
    fn drop(&mut self) {
        process::stop(&mut self.0);
    }
}

fn http(port: u16, method: &str, path: &str, body: Option<&str>) -> String {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else {
        return String::new();
    };
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n");
    match body {
        Some(b) => req.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}",
            b.len()
        )),
        None => req.push_str("\r\n"),
    }
    let _ = s.write_all(req.as_bytes());
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

/// The JSON body of a `Connection: close` answer, chunked or not.
fn json(answer: &str) -> serde_json::Value {
    let (head, mut rest) = answer.split_once("\r\n\r\n").unwrap_or((answer, ""));
    let mut body = String::new();
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        // each chunk: hex size line, the bytes, CRLF; a zero size ends it
        while let Some((size, tail)) = rest.split_once("\r\n") {
            let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            if n == 0 || tail.len() < n {
                break;
            }
            body.push_str(&tail[..n]);
            rest = tail[n..].trim_start_matches("\r\n");
        }
    } else {
        body.push_str(rest);
    }
    serde_json::from_str(&body).unwrap_or_else(|err| panic!("not JSON ({err}): {answer}"))
}

fn spawn(envs: &[(&str, &str)]) -> (Broker, u16) {
    let port = free_port();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_antares"));
    cmd.env("ANTARES_HTTP_PORT", port.to_string());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let broker = Broker(cmd.spawn().expect("spawn antares"));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !http(port, "GET", "/q/health", None).starts_with("HTTP/1.1 200") {
        assert!(Instant::now() < deadline, "broker never got healthy");
        std::thread::sleep(Duration::from_millis(200));
    }
    (broker, port)
}

fn entity(n: usize) -> String {
    format!(
        r#"{{"id":"urn:ngsi-ld:Knob:{n}","type":"Knob","p":{{"type":"Property","value":{n}}}}}"#
    )
}

#[test]
fn the_bound_knobs_set_in_the_environment_are_published_and_enforced() {
    let (_broker, port) = spawn(&[
        ("ANTARES_MAX_BATCH_ITEMS", "2"),
        ("ANTARES_MAX_LIMIT", "5"),
        ("ANTARES_DEFAULT_LIMIT", "3"),
        ("ANTARES_TEMPORAL_INSTANCE_LIMIT", "7"),
        ("ANTARES_TEMPORAL_DEFAULT_INSTANCES", "4"),
        ("ANTARES_FED_FANOUT", "2"),
    ]);
    let limits = json(&http(port, "GET", "/q/health", None))["limits"].clone();
    assert_eq!(limits["maxBatchItems"], 2, "{limits}");
    assert_eq!(limits["maxLimit"], 5, "{limits}");
    assert_eq!(limits["defaultLimit"], 3, "{limits}");
    assert_eq!(limits["temporalInstanceLimit"], 7, "{limits}");
    assert_eq!(limits["temporalDefaultInstances"], 4, "{limits}");
    assert_eq!(limits["maxFedFanout"], 2, "{limits}");

    // a batch over the ceiling is refused whole, one at the ceiling passes
    let three = format!("[{},{},{}]", entity(1), entity(2), entity(3));
    let over = http(
        port,
        "POST",
        "/ngsi-ld/v1/entityOperations/create",
        Some(&three),
    );
    assert!(
        over.starts_with("HTTP/1.1 400"),
        "batch of 3 over 2: {over}"
    );
    for pair in [[1, 2], [3, 4]] {
        let two = format!("[{},{}]", entity(pair[0]), entity(pair[1]));
        let ok = http(
            port,
            "POST",
            "/ngsi-ld/v1/entityOperations/create",
            Some(&two),
        );
        assert!(ok.starts_with("HTTP/1.1 201"), "batch of 2: {ok}");
    }

    // a page without `limit` holds the default, one past the maximum is 403
    let page = http(port, "GET", "/ngsi-ld/v1/entities?type=Knob", None);
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert_eq!(json(&page).as_array().map(Vec::len), Some(3), "{page}");
    let too_many = http(port, "GET", "/ngsi-ld/v1/entities?type=Knob&limit=6", None);
    assert!(
        too_many.starts_with("HTTP/1.1 403"),
        "limit 6 over 5: {too_many}"
    );
}

#[test]
fn a_knob_that_is_no_positive_number_keeps_the_default() {
    let (_broker, port) = spawn(&[
        ("ANTARES_MAX_BATCH_ITEMS", "0"),
        ("ANTARES_MAX_LIMIT", "many"),
        ("ANTARES_DEFAULT_LIMIT", "-1"),
    ]);
    let limits = json(&http(port, "GET", "/q/health", None))["limits"].clone();
    assert_eq!(limits["maxBatchItems"], 1_000, "{limits}");
    assert_eq!(limits["maxLimit"], 1_000, "{limits}");
    assert_eq!(limits["defaultLimit"], 100, "{limits}");
}
