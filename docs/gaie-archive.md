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
capture or reactor attribution. Python-v11 replay parity applies only while
reaction counts are representable by `u64`; Rust fails closed on overflow
rather than pretending to match Python's arbitrary-precision integers.

Semantic parity is checked with the corrected synthetic fixtures from gaoie PR
#2, validated by the Python v11 oracle at commit `4a6f8e4`. This is not a claim
of byte-for-byte equality for newly collected archives: generated UUIDs and
observation timestamps differ. Format framing, compact event bytes used for
batch SHA-256, and oracle-defined raw-payload hashing are tested separately.

Run the focused acceptance slice with `cargo test gaie_archive`; the filter
executes recovery, corruption, checkpoint-ordering, retry, producer, replay,
and CAS parity tests.

## Tested invariants

The example and property suites make these contracts explicit:

- every accepted batch contains one message, strictly increasing archive
  sequences, and archive-unique event IDs;
- compact event lines plus their exact trailing newlines are the bytes covered
  by each batch hash, and generated small ASCII per-message batches round-trip
  semantically;
- readers expose only committed prefixes, final torn/uncommitted bytes are
  recoverable without reserializing the prefix, and interior corruption fails
  closed;
- replay reaction add/remove behavior agrees with an independent nonnegative
  counter model, and checked overflow fails closed;
- corpus identifiers and archive paths cannot introduce traversal;
- pagination derives strict numeric snowflake bounds and rejects repeated or
  malformed cursors;
- corrected Python-v11 fixtures remain the differential replay and CAS oracle
  anchors.

Five filesystem-heavy properties use 32 generated cases each; five pure replay,
validation, and pagination properties use 128 cases each, for 800 generated
cases per run.

Two bounded Kani harnesses prove the corpus acceptance grammar for ASCII inputs
of 0–8 bytes and the reaction-counter contract exhaustively over `u64 × bool`.
They were executed with Kani 0.67.0 using the exact commands:

- `cargo kani --harness gaie::model::proofs::corpus_acceptance_matches_bounded_ascii_grammar --exact`
  — verification successful, 0 of 548 checks failed (26 unreachable).
- `cargo kani --harness gaie::replay::proofs::reaction_transition_satisfies_checked_counter_contract --exact`
  — verification successful, 0 of 60 checks failed.

These proofs deliberately do not cover I/O, fsync, locking, or HTTP behavior.
The crate's declared minimum Rust version is 1.93, matching the nightly bundled
with Kani 0.67.0. CI enforces that boundary with Rust 1.93.0 and
`cargo check --locked --all-targets`; Kani is not installed or run in CI.
