# Typed live delivery

Live delivery and all real and test harness adapters consume scoped typed
records. Ordinary deliveries require a first event, managed deliveries retain
one created message, and operational controls remain distinct. The core
contracts contain no platform SDK or host wire representation.

Host presentation owns batching and text formatting. It groups by complete
conversation identity and retains complete actor identity. Codex owns a private
legacy encoder which validates nested scopes and numeric identities before
projecting the unchanged durable payload format. Existing decoding, replay,
and queue semantics remain separate from this migration.

Evidence values and ordered locators are separate from Discord author binding.
Parser errors preserve concrete causes. Coordinator ingress validates numeric
identifiers before publication, and typed events avoid reparsing them later.
Version 0.70.0 reflects the changed delivery, coordinator, and parser APIs.

Native Linux formatting, feature Clippy with warnings denied, and 465 focused
Nextest tests passed on this checkout's exclusive target. Coverage includes
actual Claude and OMP writers, Codex enqueue/reopen compatibility, nested scope
rejection, managed directives, opaque fake-provider batches, retained evidence
and observations, and malformed coordinator pushes followed by valid delivery.
Independent contract, persistence, and runtime reviews found no remaining
actionable source findings after correction.

Legacy host projections still omit origin and targeting. Four narrowly scoped
non-test dead-code expectations retain these observations under the documented
coding-standards policy. The full-payload projection unit must consume them,
remove all four expectations, and test actual emitted values. This candidate
does not claim complete payload delivery or a neutral durable record format.

The coordinator baseline reproduced the intended invalid-responder failure;
the fixed exact test and coordinator suite passed after byte-identical restoration.
Final candidate review and the full signed-head gate remain pending. Local review is not official Forgejo approval. The Codex
idle-read/start race, generic durable admission, native DSH bridge, integration,
deployment, and real-host acceptance remain separate work.
