# Dione mutes

The store replays guild_mute_receipts.jsonl into an ArcSwap projection and
serializes mutations through one Tokio writer mutex. It appends a receipt,
flushes and syncs the file, then publishes the new projection. A failed append
leaves the projection unchanged, but can leave disk bytes behind. Replay
rejects corrupt nonblank lines with their line number. No torn-tail recovery,
cross-process lock, parent-directory creation, or directory synchronization is
added.

Historical optional receipt fields and omitted GuildMute cutoff identities
remain readable. IDs, actors, reasons, and operation/optional-field combinations
retain their existing representation. Listing order remains unspecified.
Reads filter expiry without recording receipts. Mutations and reconciliation
keep their existing error types and replay behavior.

The expiry worker retains its notifier, nearest-deadline scheduling, overdue
reconciliation, and five-second retry backoff. Callers own when it starts and
abort its JoinHandle. Dropping the handle does not stop the worker.

Store TTL zero remains accepted and creates an immediately expired receipt.
TTL above 43200 and shortening an active mute are rejected. The application
separately rejects zero TTL and authenticates mutation callers.

Process-global installation belongs to Dione composition, not this crate.
The application owns current access checks, tool responses, startup ordering,
and shutdown wiring. The store records supplied decisions without proving actor
identity or authorization.

## Inherited limitations

The unchanged public raw u64 guild/TTL arguments, mutable record/state fields,
derived deserialization, and operation plus optional-field combinations retain
their preexisting type-policy violations. String mutation errors, boxed
load/reconciliation errors and text conversion of replay failures retain their
preexisting error-policy violations. This extraction does not establish
validated construction or stronger durability. Semantic changes require an
explicit caller and historical-data migration.
