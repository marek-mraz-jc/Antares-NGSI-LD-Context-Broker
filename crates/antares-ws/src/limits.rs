//! The bounds of ADR-0026 rule 10, rule 5's heartbeat and rule 7's grace,
//! read from the environment once and reported in `/q/health`.

use serde_json::{json, Value};
use std::time::Duration;

/// Every bound a connection is held to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// `ANTARES_WS_QUEUE`: queued notifications per connection, and the
    /// length of its resume ring (W4).
    pub queue: usize,
    /// `ANTARES_WS_RESUME_GRACE_SECS`: how long a closed connection's
    /// Subscriptions and ring wait for a resume.
    pub resume_grace: Duration,
    /// `ANTARES_WS_MAX_LIFETIME_SECS`: a socket ends at this age (W7).
    pub max_lifetime: Duration,
    /// `ANTARES_WS_MAX_CONNECTIONS` (W2).
    pub max_connections: usize,
    /// `ANTARES_WS_MAX_CONNECTIONS_PER_TENANT` (W2).
    pub max_connections_per_tenant: usize,
    /// `ANTARES_WS_MAX_SUBSCRIPTIONS`: Subscriptions per connection (W9).
    pub max_subscriptions: usize,
    /// `ANTARES_WS_ALLOWED_ORIGINS`: the browser `Origin`s an upgrade may
    /// carry; empty refuses every browser (W1).
    pub allowed_origins: Vec<String>,
    /// A connection with no client message and no Subscription this long
    /// is closed (W2).
    pub idle: Duration,
    /// How often a Ping is sent (RFC 6455 5.5.2).
    pub ping_every: Duration,
    /// A socket with no Pong this long is closed.
    pub pong_within: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            queue: 256,
            resume_grace: Duration::from_secs(30),
            max_lifetime: Duration::from_secs(300),
            max_connections: 10_000,
            max_connections_per_tenant: 1_000,
            max_subscriptions: 64,
            allowed_origins: Vec::new(),
            idle: Duration::from_secs(60),
            ping_every: Duration::from_secs(30),
            pong_within: Duration::from_secs(60),
        }
    }
}

impl Limits {
    /// The limits the environment `var` names, each on its default when
    /// unset.
    ///
    /// ```
    /// let l = antares_ws::Limits::from_env(|k| (k == "ANTARES_WS_QUEUE").then(|| "8".to_owned())).unwrap();
    /// assert_eq!(l.queue, 8);
    /// ```
    ///
    /// # Errors
    /// The variable and its value when one is not a positive whole number:
    /// a bound the operator mistyped stops the broker rather than falling
    /// back to one the operator did not choose.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let d = Self::default();
        let set = |name: &str| -> Result<Option<u64>, String> {
            var(name).map_or(Ok(None), |v| match v.trim().parse::<u64>() {
                Ok(n) if n > 0 => Ok(Some(n)),
                _ => Err(format!("{name}={v:?} is not a positive whole number")),
            })
        };
        let count = |name: &str, default: usize| -> Result<usize, String> {
            Ok(set(name)?.map_or(default, |n| usize::try_from(n).unwrap_or(usize::MAX)))
        };
        let secs = |name: &str, default: Duration| -> Result<Duration, String> {
            Ok(set(name)?.map_or(default, Duration::from_secs))
        };
        Ok(Self {
            queue: count("ANTARES_WS_QUEUE", d.queue)?,
            resume_grace: secs("ANTARES_WS_RESUME_GRACE_SECS", d.resume_grace)?,
            max_lifetime: secs("ANTARES_WS_MAX_LIFETIME_SECS", d.max_lifetime)?,
            max_connections: count("ANTARES_WS_MAX_CONNECTIONS", d.max_connections)?,
            max_connections_per_tenant: count(
                "ANTARES_WS_MAX_CONNECTIONS_PER_TENANT",
                d.max_connections_per_tenant,
            )?,
            max_subscriptions: count("ANTARES_WS_MAX_SUBSCRIPTIONS", d.max_subscriptions)?,
            allowed_origins: var("ANTARES_WS_ALLOWED_ORIGINS")
                .map(|v| {
                    v.split(',')
                        .map(str::trim)
                        .filter(|o| !o.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            ..d
        })
    }

    /// What `/q/health` reports (rule 10: every limit beside the bounds).
    #[must_use]
    pub fn health(&self) -> Value {
        json!({
            "queue": self.queue,
            "resumeGraceSecs": self.resume_grace.as_secs(),
            "maxLifetimeSecs": self.max_lifetime.as_secs(),
            "maxConnections": self.max_connections,
            "maxConnectionsPerTenant": self.max_connections_per_tenant,
            "maxSubscriptions": self.max_subscriptions,
            "allowedOrigins": self.allowed_origins.len(),
            "idleSecs": self.idle.as_secs(),
            "pingSecs": self.ping_every.as_secs(),
            "pongSecs": self.pong_within.as_secs(),
            "maxMessageBytes": crate::envelope::MAX_MESSAGE_BYTES,
            "maxFrameBytes": crate::envelope::MAX_FRAME_BYTES,
            "maxNotificationBytes": crate::hub::MAX_NOTIFICATION_BYTES,
            "maxJsonDepth": antares_ops::bounds::MAX_JSON_DEPTH,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_is_the_default_and_a_mistyped_bound_stops_the_broker() -> Result<(), String> {
        assert_eq!(Limits::from_env(|_| None)?, Limits::default());
        let set = Limits::from_env(|k| match k {
            "ANTARES_WS_RESUME_GRACE_SECS" => Some("5".into()),
            "ANTARES_WS_ALLOWED_ORIGINS" => Some(" https://a.example , ,https://b.example".into()),
            _ => None,
        })?;
        assert_eq!(set.resume_grace, Duration::from_secs(5));
        assert_eq!(
            set.allowed_origins,
            ["https://a.example", "https://b.example"]
        );
        for bad in ["0", "-1", "x", ""] {
            let e = Limits::from_env(|k| (k == "ANTARES_WS_QUEUE").then(|| bad.to_owned()));
            assert!(e.is_err(), "{bad}");
        }
        assert_eq!(Limits::default().health().get("queue"), Some(&json!(256)));
        Ok(())
    }
}
