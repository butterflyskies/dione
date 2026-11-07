# Dione GAIE archive

This crate owns the complete format-v2 GAIE archive, event model, retained origin
bytes, Discord backfill client, and latest-state replay. Its opaque archive and
capture values retain their existing constructors and validation. Public event
fields remain supplied data.

Archive opening acquires an exclusive lock, validates paths, and recovers only a
final torn or uncommitted tail. Appending validates a complete message batch and
retained origin references before committing exact bytes. Invalid claimed origin
evidence is quarantined separately from accepted events during reads. Historical
events without origin evidence retain their existing behavior. These checks
establish consistency with retained bytes, not external authenticity.

Backfill keeps scoped capture discovery, independent stream checkpoints, durable
commit ordering, duplicate suppression, byte limits, and no-op behavior. The HTTP
client preserves existing retry, origin, redaction, and attachment policies.
Dione retains configuration and credential loading, channel allowlists, and the
archive command-line entry point.

The two default attachment budgets live here and are also used by application
configuration. Existing tests move with the implementation. Oracle fixtures and
their four application integration tests remain in Dione. Existing Kani proof
modules remain present; the normal workspace gate does not execute them.

This crate is part of the Dione workspace and uses its MIT OR Apache-2.0 license.
