# Typed attention authority

Version 0.67.0 changes the final attention guard from a JSON-shaped interface to a typed descriptor containing the validated decision identity, scheduling directive, complete message scope, actor scope, and exact original text. These observations do not grant authority: the runtime still checks current recipient, expiry, delivery state, ingress authority, freshness, and stored source evidence. The descriptor directive must match the recorded admission. Already-admitted work retains its existing behavior across later mode changes.

Discord composition supplies one module instance to the scheduler, attention runtime, and Codex legacy scope. Source invalidation uses the complete configured module, conversation, and message identity. Foreign instances, other conversations, and opaque numeric-looking identities cannot invalidate a numeric Discord source. Record selection remains available through an exhaustive alternative. Removal, lease revocation, and deduplication updates retain the existing transaction.

RecordId now rejects blank values and values longer than 512 UTF-8 bytes through one constructor and validating deserializer. Valid whitespace and Unicode are preserved. Durable readers retain path-aware decode causes, reject trailing input, and leave malformed files unchanged. The Codex-owned compatibility decoder preserves ordinary payloads and unknown fields while validating managed markers and source fields. Absent-method historical creates remain supported; explicitly non-channel methods reject. Earlier synthetic authorless managed records were queue-admissible but could not pass the existing runtime guard; they now fail enqueue/load explicitly rather than acquiring reconstructed authority.

Receipt persistence uses one shared blocking store operation with concrete initialization, worker, lock, and store errors. Accepted transport is not replayed when only receipt persistence fails. Unrelated control/learning String-returning APIs remain a separately tracked standards unit. Ordinary delivery remains JSON until A2. This change does not prove atomic Codex idle-to-start admission or complete platform independence.

## Compatibility

This is an intentional pre-1.0 minor API change. Public guard, runtime, queue, server, and session callers were migrated together. RecordId no longer has unchecked From or Default construction. AttentionInvalidation replaces two optional selectors. New decoding/encoding errors replace the removed untyped metadata/selector errors; the downstream exhaustive enum fixture lists every variant without a wildcard. Core actor and message identities remain opaque and SDK-free.

## Verification status before signing

The parent observed the original scoped-invalidation regression fail against the old API: a foreign conversation removed the queued record instead of retaining it. The test-only patch and failing-run log are retained in the local handoff.

The fresh isolated-target r4 feature Clippy and all 210 focused tests passed, including the compile-fail contract suite. Logs are retained as /home/syn/code/dione-attention-a1-clippy-r4.log and /home/syn/code/dione-attention-a1-focused-r4.log. The passing tests include accepted_live_turn_is_not_replayed_after_real_receipt_write_failure: a real AttentionRuntime and live worker encounter a concrete store I/O failure after transport acceptance, settle the queue, and deliver an ordinary follow-up with exactly one managed turn.

Independent source reviews of the corrected authority and persistence boundaries found no further issues; the required comment corrections were applied. These are local source reviews, not official PR approval. Earlier shared-target compiler output is not evidence that the complete candidate or all callers passed. Fresh isolated deliberate mutation evidence, exact signed-head repository checks, CI, and official review remain pending. This audit must be updated with those results before any completion claim.

Implementation base: core 7ff65db87b1510a37b1ef14589f26f9cc30c598e. No A1 release commit has been signed at this checkpoint.
