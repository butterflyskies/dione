<!-- design-meta
status: draft
last-updated: 2026-07-22
depth: standard
-->

# GAIE byte-exact origin evidence

## Problem

The GAIE Discord backfill normalizes a parsed message into schema-v1 events and
records `raw_payload_sha256`. That digest intentionally matches the Python-v11
oracle's canonical JSON reserialization. It is not a digest of retained source
bytes, and the source response is discarded after parsing.

This is adequate for producer parity, but not for later provenance analysis.
Normalization can erase object-key order, whitespace, duplicate-key evidence,
and the distinction between fields that a typed adapter omitted and fields that
were present with `null`. A normalized event therefore cannot be its own origin
receipt.

## Goal

Retain the exact successful Discord HTTP message-page bytes before normalized
events derived from that page are committed. Bind each new normalized event to
the retained observation through a typed, content-addressed reference.

## Non-goals

- Classifying an observation as human, scheduler, tool, or phantom.
- Treating retained evidence or a classification as authorization.
- Changing the Python-v11 `raw_payload_sha256` compatibility contract.
- Claiming byte-exact live gateway capture. Serenity exposes deserialized
  gateway events at its raw-handler seam; transport-frame capture belongs in a
  later atom below that seam.
- Retaining thread-discovery or channel-metadata responses in this atom.

## Requirements

- **OE-R1 — exact bytes:** Store the exact successful message-page response
  body, without parse/reserialize, below the configured GAIE data directory.
- **OE-R2 — content addressing:** Name observations by SHA-256. Reusing an
  existing object must verify its bytes match its name.
- **OE-R3 — durable-before-derived:** Persist and fsync the observation before
  appending any normalized event that references it.
- **OE-R4 — typed reference:** A normalized event may carry an optional origin
  evidence reference containing the adapter identity/version, exact-byte
  digest, archive-relative location, media type, and JSON Pointer selector.
- **OE-R5 — honest absence:** Discord observations omit harness identity. They
  must not serialize an invented harness or coerce absence into a value.
- **OE-R6 — compatibility:** Existing archives and Python-v11 fixtures continue
  to deserialize and serialize unchanged when no evidence reference is present.
- **OE-R7 — hardened storage:** Evidence storage uses the archive's path,
  symlink, permission, atomic-write, read-back verification, and directory-fsync
  discipline.
- **OE-R8 — fail closed:** A storage, integrity, shape, or selector error aborts
  the affected backfill before its derived event batch commits.
- **OE-R9 — scoped claim:** This atom claims exact successful HTTP response-body
  retention only. It does not claim TLS-frame, HTTP-header, gateway-frame, or
  failed-response retention.

## Architecture

The Discord archive client returns an observed message page containing both the
unmodified response bytes and parsed messages with their original array indexes.
Parsing validates that the top-level value is an array but does not replace the
retained byte string.

`Archive` owns a hardened `origin-evidence/` content-addressed store. Storing a
page returns an internal stored-object handle; the Discord adapter combines that
handle with a canonical selector to construct the serialized
`OriginEvidenceRef`. The backfill persists every fetched page once, then sorts
the derived message records by snowflake as before. Each message event
references `/<page-index>`; reaction snapshots reference
`/<page-index>/reactions/<reaction-index>`.

The archive treats serialized evidence references as untrusted input. Before an
event is appended, and again when committed events are read, it validates the
exact adapter contract, lowercase digest grammar, digest-derived location,
regular non-symlink object, byte digest, event-kind-specific selector, source
message ID, and Python-v11 semantic hash of the selected JSON. Public schema
types remain serializable for compatibility; validity is established only at
the archive boundary.

The existing canonicalized `raw_payload_sha256` remains in `Ingest`. The new
optional evidence reference answers a different question:

- `raw_payload_sha256`: does Rust reproduce the Python-v11 semantic-record hash?
- `origin_evidence.sha256`: which exact retained response body did this event
  come from?

The stable GAIE model owns the evidence-reference types. Discord/HTTP supplies
one adapter implementation; future harness adapters may reuse the contract
without importing Discord types into the core.

The adapter contract also owns the raw-JSON projection used by both event
construction and archive validation. For message creates it authenticates the
raw-derived message ID, actor and timestamps, content and content hash,
attachment metadata (excluding the separately downloaded attachment content
digest), reply relation, embedded-thread behavior, lineage, and constants. For
reaction snapshots it authenticates the emoji name/ID/key, count details,
  raw-derived empty/default fields, lineage, and constants. A response-supplied
  `channel_id` or `guild_id` is also authenticated; when either field is absent,
  its event value remains capture context. Corpus, context-only thread values,
  observation time, archive sequence, and event ID are likewise capture context
  and are explicitly not authenticated by this receipt.

## Failure and recovery

- A crash before evidence rename leaves only a hidden temporary file; opening
  or retrying does not treat it as evidence.
- A crash after evidence rename but before event commit may leave an unreferenced
  CAS object. This is safe and preferable to a committed event with missing
  evidence.
- A retry verifies and reuses the CAS object, fsyncs the evidence directory
  again before success, then relies on existing message-ID deduplication and
  checkpoints. Any append carrying evidence revalidates the object and fsyncs
  that directory immediately before writing the event batch.
- A pre-existing object whose bytes do not match its filename is an integrity
  failure, not an overwrite opportunity.

## Threat model

- **Path traversal / symlink substitution:** callers never provide evidence
  paths; the archive derives them from validated SHA-256 digests and rejects
  symlinks at the directory, its ancestors, and the destination immediately
  before validation and directory fsync. These pathname checks plus
  `O_NOFOLLOW` on evidence-object opens substantially narrow substitution, but
  do not eliminate every check/use race; an fd-relative directory-handle design
  is deferred beyond this atom.
- **Evidence substitution:** content addressing plus read-back verification
  binds the reference to bytes. Append and read also re-project the selected
  source JSON and reject any mismatch in fields the adapter derives.
- **Normalization laundering:** normalized fields are explicitly downstream of
  retained evidence and cannot replace it as provenance.
- **Secret leakage:** authorization headers are not part of response bodies and
  are never stored. Response bodies may contain Discord content already within
  the configured corpus consent boundary.
- **Unbounded storage:** this atom retains one CAS object per distinct fetched
  page. Existing corpus/channel allowlists and explicit one-shot invocation are
  the current bounds; quotas and tiering remain later operational work.

## Test plan

- A page with unusual whitespace, key order, one absent field, and one explicit
  `null` is retained byte-for-byte and hashes to its filename.
- Message and reaction events point to selectors that resolve to their exact
  source objects in the retained page.
- Re-storing identical bytes is idempotent; a corrupt existing object fails.
- A symlinked evidence directory or destination is rejected.
- Injected evidence-directory sync failure blocks both verified-object reuse
  and event append; a later successful retry commits exactly once.
- Fabricated locations and selectors are rejected on append, and forged
  committed archives are rejected on read.
- Existing Python-v11 oracle parity fixtures remain byte-for-byte unchanged.
- A no-op rerun appends neither events nor duplicate evidence bytes.

## Rollout and rollback

The evidence reference is optional and omitted for historical events, so current
archives remain readable. Rollback is code-only: older readers ignore the new
field because the event model is not `deny_unknown_fields`; evidence CAS objects
remain inert. No automatic deletion is part of rollback.
