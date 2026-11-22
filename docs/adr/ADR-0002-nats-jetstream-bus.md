# ADR-0002: NATS JetStream as the change bus (with a local mode)

Date: 2026-08-03 · Status: accepted

One `ANTARES_CHANGES` interest stream, durable pull consumers per concern,
JetStream KV for the subscription mirror — this replaces Scorpio's whole
SUB_ALIVE/SUB_SYNC instance-sync protocol. Delivery is at-least-once +
idempotent consumers; entity `version` gives order tolerance. `bus = local`
(in-process) is a first-class single-node mode: no infrastructure beyond
Postgres. Kafka rejected: operational cost without a matching benefit at
our scale.

At-least-once means a change can arrive late. Each event carries `written_at`
and the matcher does not notify it to a Subscription created after that
instant (5.8.6; 5 s are allowed for clock skew between api pods). A pod told
to stop takes no new change, finishes and acks the batch it holds and hands
the rest back before its pools close. A crash still redelivers: a change
whose notification left before the ack is then notified again.

## Confirmation

`crates/antares-broker/tests/nats_e2e.rs sigkill_between_commit_and_publish_republishes_from_outbox` `nats_outage_flips_health_and_recovers`, `a_late_change_does_not_reach_a_subscription_made_after_it` and `a_rolled_matcher_notifies_the_change_it_held_once`; the postgres-nats and timescale-nats cells of the full ETSI matrix.
