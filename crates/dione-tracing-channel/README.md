# Dione tracing channel

Owns trace event capture, two independent reload controls, and the detached
forwarding task. Process subscriber installation stays with the application.

Capture uses a bounded 256-event queue and drops on full or closed queues.
Forwarding awaits downstream capacity, preserves order, and exits on closure.
It supplies no joined shutdown or cancellation guarantee.

The inherited controller returns `Result<(), String>` with unchanged invalid
filter and reload failure messages. This extraction preserves that contract;
it does not redesign errors to satisfy the newer typed-error standard.

Events use the existing descriptive `dione-discord-events` contract. The
transitive native/provider dependency graph remains. The workspace's vendored
native TLS feature supports standalone compilation without system OpenSSL.

Run `cargo nextest run -p dione-tracing-channel --locked` for scoped subscriber
and forwarding effect tests.
