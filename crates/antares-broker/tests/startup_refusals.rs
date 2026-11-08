// SPDX-License-Identifier: EUPL-1.2
//! Configuration the binary refuses at startup, proven against the real
//! binary: each refusal is a failed start whose message names the setting,
//! never a broker serving with something other than what was asked for.

use std::process::{Command, Output};
use std::time::Duration;

mod common;
#[path = "common/process.rs"]
mod process;
use common::free_port;

/// Start the binary with `envs` and wait for it to give up.
fn refused(envs: &[(&str, &str)]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_antares"));
    cmd.env("ANTARES_HTTP_PORT", free_port().to_string());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out: Output = cmd.output().expect("run antares");
    assert!(!out.status.success(), "the start must fail: {envs:?}");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn an_unknown_store_backend_is_a_failed_start_that_lists_the_built_ones() {
    let log = refused(&[("ANTARES_STORE", "mongo")]);
    assert!(log.contains("unknown ANTARES_STORE"), "{log}");
    assert!(
        log.contains("memory"),
        "the message lists what was built: {log}"
    );
}

/// The collector URL may carry a credential (RFC 3986 3.2.1): a URL the
/// exporter cannot use fails the start, and the message names the URL with
/// the credential stripped.
#[test]
fn an_unusable_collector_url_fails_the_start_without_its_credential() {
    let log = refused(&[
        ("ANTARES_TELEMETRY", "1"),
        ("ANTARES_OTLP_ENDPOINT", "http://user:s3cret@[::1/v1/traces"),
    ]);
    assert!(
        !log.contains("s3cret"),
        "the collector password in the log: {log}"
    );
}

/// ANTARES_REQUIRE_RLS turns a database role that bypasses row-level
/// security (the test role is the superuser) from a warning into a refusal.
#[test]
fn a_role_that_bypasses_rls_is_refused_when_rls_is_required() {
    let Ok(url) = std::env::var("ANTARES_TEST_DATABASE_URL") else {
        eprintln!("SKIP: ANTARES_TEST_DATABASE_URL not set");
        return;
    };
    let log = refused(&[
        ("ANTARES_STORE", "postgres"),
        ("ANTARES_ALLOW_SHARED_LOCAL", "1"),
        ("ANTARES_DATABASE_URL", &url),
        ("ANTARES_REQUIRE_RLS", "1"),
    ]);
    assert!(log.contains("ANTARES_REQUIRE_RLS"), "{log}");
}

/// Naming the default policy engine explicitly selects the same engine
/// from the shelf: the broker starts, answers, and drains on SIGTERM.
#[test]
fn naming_the_allow_all_engine_starts_the_broker() {
    let port = free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_antares"))
        .env("ANTARES_HTTP_PORT", port.to_string())
        .env("ANTARES_POLICY", "allow-all")
        .env("ANTARES_DRAIN_DELAY_MS", "0")
        .spawn()
        .expect("spawn antares");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let healthy = loop {
        let probe = Command::new(env!("CARGO_BIN_EXE_antares"))
            .arg("--health")
            .env("ANTARES_HTTP_PORT", port.to_string())
            .status()
            .is_ok_and(|s| s.success());
        if probe || std::time::Instant::now() > deadline {
            break probe;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    process::stop(&mut child);
    assert!(healthy, "the broker never answered its health probe");
    assert!(
        child.try_wait().ok().flatten().is_some_and(|s| s.success()),
        "SIGTERM drains to a clean exit"
    );
}
