// SPDX-License-Identifier: EUPL-1.2
//! Stopping a spawned broker the way an orchestrator does.

use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// SIGTERM, then wait for the drain (shutdown.rs) to end the process, and
/// SIGKILL only when it is still running after 30 s. A SIGKILL straight
/// away skips the drain the binary ships, and an instrumented binary writes
/// its coverage profile only on a clean exit.
pub fn stop(child: &mut Child) {
    let pid = child.id().to_string();
    let termed = Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .is_ok_and(|s| s.success());
    let deadline = Instant::now() + Duration::from_secs(30);
    while termed && Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
