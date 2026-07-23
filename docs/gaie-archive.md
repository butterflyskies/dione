# GAIE archive atoms 1 and 1b

GAIE archive support is disabled by default and runs only through the explicit
`gaie_archive` one-shot binary. Normal Dione daemon startup never backfills.

Every successful non-empty Discord message-page response is retained
byte-for-byte in `origin-evidence/<sha256>` before any normalized event derived
from that page is committed. Empty terminal pages are not retained because they
yield no event or evidence reference. New events carry an optional typed evidence reference: message
creates select `/<page-index>`, and reaction snapshots select
`/<page-index>/reactions/<reaction-index>`. Discord evidence names the
`discord-http-message-page` adapter and deliberately omits harness identity.
Adapter version `1` versions this response-array-to-JSON-Pointer contract; it
is independent of the collector version.
The existing `raw_payload_sha256` remains the Python-v11-compatible semantic
hash; it is not replaced by the exact-byte evidence digest.
Archive append and read both dispatch by the adapter version recorded in the
receipt and revalidate that contract, canonical
digest-derived location, regular non-symlink CAS object, exact-byte digest,
event-kind selector shape, source message ID, and selected semantic hash.
Collector version remains independent ingest metadata. Each read pass caches a
validated parsed page by digest so multiple selectors do not repeatedly read
and parse the same evidence object.
Evidence-directory durability is synchronized again immediately before a
referencing batch is appended.
The shared adapter projection additionally binds all normalized fields derived
from the selected JSON, including message actor/timestamps/content/attachment
metadata/reply and reaction emoji-key/count details. Downloaded attachment
content hashes and capture-context fields—corpus, guild, channel, context-only
thread values, observation time, sequence, and event ID—are outside this
response-body receipt's authentication claim when absent from the response.
When Discord supplies `channel_id` or `guild_id`, the projection authenticates
that value against the normalized source. Symlink and ancestor checks run
again at validation and fsync boundaries; residual pathname check/use races
remain until a future fd-relative storage redesign.

Append fails closed on any invalid receipt. Reads first recover the structural
committed prefix, then partition origin failures into typed quarantined events.
Quarantined events are excluded from the ordinary event list and carry a loud
validation error; unaffected verified and legacy unreceipted events remain
available. The CLI reports every quarantine rather than silently treating it as
verified data.

Atom 1 archives the configured parent channel through Discord's raw HTTP
message response, emits `message_create` plus aggregate `reaction_snapshot`
events, downloads attachments into `attachments/<sha256>.<safe-extension>`, and
advances the parent checkpoint only after the corresponding event batch has
been fsynced. A first run safely walks full history with `before`; a resumed run
uses the durable message ID with `after`. Repeated or malformed cursors fail
closed.

Attachment capture has two typed, startup-validated byte limits:
`archive.max_attachment_bytes` (25 MiB by default) and
`archive.max_run_download_bytes` (250 MiB by default). Both must be nonzero,
and the cumulative limit must be at least the per-attachment limit. Before any
attachment from a message is downloaded, the collector reserves the declared
Discord sizes of every attachment on that message. It rejects an oversized file
or cumulative overflow before publishing any attachment from that message.
Response bodies are then read incrementally and checked against both limits, so
an understated Discord size or HTTP length cannot bypass the guard. The
attachment that crosses the actual-byte limit is never renamed into the
canonical content-addressed namespace.

The cumulative download budget counts logical attachment occurrences actually
downloaded during the run, not newly allocated CAS bytes. Two uncommitted
attachments with identical content therefore each consume download budget, as
does an uncommitted transfer whose blob already exists. Messages already
committed to the archive are skipped before budget reservation, so a stale
checkpoint can be repaired across bounded retries instead of turning the
per-run limit into an absolute stream-size ceiling. Unique-CAS disk budgeting
is a separate concern and is not claimed here. Attachment errors report sizes
and limits but never the credential-bearing CDN URL.

Atom 1b expands one allowlisted capture root to the parent plus every active or
archived thread visible to the authenticated Discord principal. Completeness is
therefore **principal-visible and non-atomic**, not global or historical: a
thread the principal cannot enumerate is outside the claim, and threads may
change while the two discovery snapshots are taken. Discovery uses the official
Discord routes for guild-active threads, parent public archives, and private
archives. A 403 from the all-private route falls back to joined-private. Public
and private archive routes paginate with an ISO-8601 `before` cursor;
joined-private uses a snowflake cursor. `has_more`, never page length, decides
whether another page is required. The target set is the union of active
snapshot A, all archive pages, and active snapshot B.

Every discovered child is validated against the configured guild, exact parent
ID, and admitted thread types before it can become a verified capture target.
Message retrieval accepts only verified targets, not arbitrary child IDs. The
default path completes enumeration before mutating the archive and fails closed
if enumeration or validation fails. `allow_partial = true` remains an explicit
parent-only break-glass mode; Atom 1b does not claim durable partial-thread
semantics.

Capture order is deterministic: parent first, then threads by numeric snowflake;
messages within each stream are numeric ascending. Message identity remains the
Discord-global `message_id`. A forum or thread starter observed once through
the parent and once through the child stream produces one message, while its
embedded thread relationship is retained. Reaction ordering remains unchanged.

## Checkpoint v2

Atom 1b replaces the single parent cursor with a versioned checkpoint:

```json
{
  "version": 2,
  "corpus_id": "example",
  "guild_id": "10",
  "parent_channel_id": "100",
  "streams": {
    "100": { "after_message_id": "150" },
    "200": { "after_message_id": null }
  },
  "updated_at": "2026-07-21T00:00:00Z"
}
```

`streams` is a deterministic `BTreeMap`; nullable cursors represent discovered
but empty streams. The exact Atom 1 v1 object migrates to v2 with one parent
stream. Unknown versions, mixed v1/v2 fields, foreign corpus/guild/parent IDs,
and corrupt shapes fail closed. Each stream advances independently, and only
after its corresponding message batch is committed and fsynced. Reprocessing a
committed batch after a checkpoint-write fault must deduplicate globally and
repair only that stream cursor. A semantic no-op rerun appends no events and
does not rewrite the checkpoint merely to churn `updated_at`.

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

Run the Atom 1b acceptance slice with:

```console
cargo nextest run -E 'test(/atom_1b|capture_root_accepts_optional_category_parent|incremental_backfill|incremental_short_after|fresh_backfill_short_before|pagination_cursor|repairs_stale_stream_cursor|default_backfill|wrong_parent|checkpoint|discovery_permutations|root_thread_route_matrix|incompatible_thread_type|http_trace_endpoint|archive_cursors/)'
```

The expression selects the production discovery/backfill transcripts, root
validation matrix, checkpoint migration/corruption cases, and deterministic
planning contracts without relying on a broad substring shared by unrelated
tests.

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
- discovery validates the principal-visible thread set before archive mutation,
  unions two active snapshots with archived pages, and produces deterministic
  verified targets;
- checkpoint v1 migrates exactly to a v2 parent-only stream map, independent
  stream cursors recover commit-before-checkpoint faults, and no-op reruns do
  not churn durable bytes;
- Discord-global message identity deduplicates parent/thread starter aliases
  without discarding their embedded thread relationship;
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
