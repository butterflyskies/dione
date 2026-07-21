# GAIE archive atom 1

GAIE archive support is disabled by default and runs only through the explicit
`gaie_archive` one-shot binary. Normal Dione daemon startup never backfills.

This atom archives one configured parent channel through Discord's raw HTTP
message response, emits `message_create` plus aggregate `reaction_snapshot`
events, downloads attachments into `attachments/<sha256>.<safe-extension>`, and
advances the parent checkpoint only after the corresponding event batch has
been fsynced. A first run safely walks full history with `before`; a resumed run
uses the durable message ID with `after`. Repeated or malformed cursors fail
closed. Owned-thread enumeration remains outside this bounded atom, so
`allow_partial = true` is required for parent-only backfill.

The latest-state replayer additionally understands message edit/delete and
reaction add/remove events, but this collector does not claim live delta
capture or reactor attribution.

Semantic parity is checked with the corrected synthetic fixtures from gaoie PR
#2, validated by the Python v11 oracle at commit `4a6f8e4`. This is not a claim
of byte-for-byte equality for newly collected archives: generated UUIDs and
observation timestamps differ. Format framing, compact event bytes used for
batch SHA-256, and oracle-defined raw-payload hashing are tested separately.

Run the focused acceptance slice with `cargo test gaie_archive`; the filter
executes recovery, corruption, checkpoint-ordering, retry, producer, replay,
and CAS parity tests.
