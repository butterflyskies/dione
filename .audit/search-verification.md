# Discord search verification record

This slice starts at `cbf141b7bf398e079c53945df3537469edc6ad93`, PR #499,
on `refactor/discord-search`. It targets `refactor/discord-typed-queries`.

## Contract and design

The original search operation owns HTTP, live channel filtering, and MCP JSON
in one MCP module. The selected design gives the complete operation to
`discord::messaging::search`. A private prepared request binds the canonical
query to its pagination facts. MCP owns argument parsing and result projection.
Input parsing failures remain dispatch failures; missing filters and date
conversion failures remain tool error payloads.

Three independently scoped GPT-6-Astra agents produced designs. A separate
GPT-6-Astra judge scored A21, B23, and C23 and selected C for the smaller
completed request. The parent scored A21, B23, and C24. These are design
judgments, not measurements. The synthesis takes B's nested ownership,
separate preparation/execution failures, and optional raw retry header.
It rejects extra filter/order wrappers, a transport trait, a service object,
and a continuation enum wrapping a private boolean.

The original PR #148 explains that visible count replaced misleading Discord
total-results metadata. This migration must preserve continuation based on raw
outer groups, including denied and malformed groups. It must also preserve
transport order, duplicates, first-member selection, string-only channel IDs,
and live DM/thread authorization after HTTP. Epoch date zero and truncating
far-future date conversion remain admitted behavior.

## Evidence sequence

The untouched parent passed 52 selected search tests, with 1533 skipped.
The command was `cargo nextest run --workspace --locked -E 'test(search)'`.
Its log is `/home/syn/code/dione-search-baseline.log`.

The old pagination test calculated the pagination expression in its own body.
It did not prove the operation's response. Before typed migration, the owner
extracted a private concrete RequestBuilder execution seam and characterized
the existing JSON-returning implementation with loopback HTTP. That extraction
is a production edit; it is not untouched-operation HTTP evidence. It constructs
the inert client/request builder before semantic validation but still sends
only after validation. The tests use a loopback builder; production endpoint
and credential construction are separately source-inspected.

Ten `search_contract_` tests passed in Nextest run
`395586aa-aed8-42a9-a61a-f1fc528a23ad`, with 1584 tests skipped. The log is
`/home/syn/code/dione-search-characterization.log`. They cover complete page
payloads, raw group pagination, ordering and duplicates, first-member selection,
numeric channel omission, missing/non-array envelopes, HTTP error branches,
raw/missing/non-UTF8 retry headers, query order and repeated values, date
wrapping, parser errors, and dispatch error placement. A response barrier
changes live DM and thread grants and revocations after receiving the request,
before returning the response. No sleeps establish permission timing.

Transport and truncated-body failures assert their stable category prefix;
their platform-dependent complete error text is not pinned. The tests do not
establish production Discord connectivity. Gate-before-message-decode remains
a source-review contract, separate from the live permission timing test.

The uncommitted pre-format copies under the local `characterization` directory
have SHA-256 hashes `274d79e291f248ceacefbbe07b24ca5b94758e1ea643d54cb9c25b95e7870b76`
for `search.rs` and `2405307791dc5b478ee5a5f81135018784bf0a2b64b204ea45f7f716248002dd`
for `dispatch.rs`. The fixture comparison found 47 unchanged JSON expressions
across ten tests after formatting. This compares test expressions, not outputs
by itself. Those scratch hashes do not identify committed files. The separate
characterization commit preserves the executable baseline in Git. That signed commit is
`8c329b071bd49be6171011c60884f04c20142b93`.
Its search blob is `94408ec3a0ae847179aaf09d1ec3f05aa9e1ff3f`; its dispatch
blob is `2975846ded5d85acb0082885d104b91a3102e48d`.

Comment review recommended five narrative line deletions, which the parent
applied. No MUST KILL flag, suppression, restored comment, or reshape occurred.
Default-feature Clippy passed with all targets and warnings denied after those
deletions. The broader search run passed all 61 selected tests, with 1533 skipped,
in Nextest run `fe8caddb-3bce-4d55-82a3-44855abeb330`. Its log is
`/home/syn/code/dione-search-characterization-all.log`.

One isolated writer owns the coupled migration. The parent owns integration,
native Linux builds, signed commits, and Forgejo writes. Design and review
reports have disjoint files under
`C:/Users/Jaycee/AppData/Local/Temp/dione-search-design/`.

## Limits

This slice does not change timeout, retry, fresh-client creation, response body
buffering, raw error body disclosure, or the broad MessagingCtx. It does not
change the MCP tool schema or add a guild-wide authorization preflight. A valid
identifier is not proof of current channel authorization.

The Rust API removal will be versioned deliberately. The only in-repository
production caller found is MCP dispatch; that does not prove external consumer
compatibility. Local agent review is separate from official Forgejo approval.

## Candidate verification

Signed candidate `1f0b6cada6c8b51b2e89220f1afbb852df0ad0a2` removes the
old public Rust search API in version 0.55.0. The replacement is crate-private.
All 47 JSON fixture expressions remain unchanged after migration. The focused
run passed 61 tests (1533 skipped), Nextest run
`16d0d4cc-9d2d-45a6-8e80-ad7b5e249a56`. Default all-target Clippy passed.
The native focused run preceded the six repetitive test comment deletions.
The full gate will establish the exact committed source. No suppression or reshape was
needed. Logs are `/home/syn/code/dione-search-focused.log` and
`/home/syn/code/dione-search-clippy.log`.

Three independent GPT-6-Astra reviewers inspected the signed candidate against
the exact parent. They used the same model as the implementer and inspected
the parent's execution evidence without running tests themselves. All six
lenses passed without actionable P1, P2, or P3 source findings. A trailing
blank line in this record was a housekeeping finding, corrected afterward.

| Lenses | Evidence and remaining limits |
| --- | --- |
| Safety, Tests | The focused run covers live grants and revocations after the request and a literal raw-page response fixture. Source inspection found no lock across HTTP. Production endpoint/token and gate-before-decode are source-inspected. |
| Design, Idiomacy | Private prepared request, derived visible count, separate preparation and execution errors, complete Discord ownership, old caller migrated and API deleted. |
| Security, Privacy | Current channel filtering precedes SDK decoding; no new state writes or credential path. Existing raw error body and request URL disclosure remain unchanged. |

Reports are `safety-tests-r1.md`, `design-idiomacy-r1.md`, and
`security-privacy-r1.md` in the local design directory above. These are local
agent reviews, not official Forgejo approvals.

The pagination mutation replaced raw-group continuation with visible-count
continuation. Nextest run `9ea246ff-bb21-46bd-a9af-a98c3240251d` failed the
literal response assertion: actual `has_more: false`, expected `true` with
three visible messages from eight raw groups. After restoring the original
bytes, run `ca33a4cc-d5cd-4c62-86ad-c9f9ae4b6d79` passed. Logs are
`/home/syn/code/dione-search-mutation.log` and
`/home/syn/code/dione-search-restored.log`. The helper restored bytes in a
finally block and checked equality. This mutation proves only the pagination
continuation test's sensitivity. Other contracts have passing tests and source
review, without mutation evidence. The cross-model auditor compared committed
test bodies after whitespace normalization. Seven were identical; three changed
only at adapter call sites, with assertions unchanged.

Builds and mutation ran in `/home/syn/code/dione-pr-queries`, reused from the
prior slice. The native index was compared with the candidate before moving
native HEAD; it was clean before the gate. Git normalized CRLF working files
to LF index blobs, so this is Git-tree equality, not working-file byte equality.

The refused-port test releases an ephemeral port before connecting, leaving a
theoretical port-reuse race. Raw-group `has_more` can reveal that hidden groups
exist. Both are retained and explicitly identified by the source reviewers.

## Trail review

Claude Sonnet 5.5 reviewed transcript positions 2431 through 2646 and the
referenced files. The report lists truncated tool items and which artifacts
substituted for their full text. It is `trail-r1.md` in the local design
directory. The result was pass with documentation corrections, no source
finding. This record clarifies scratch hashes, review independence, mutation
scope, comment-cleanup timing, native checkout reuse, and retained limitations.
The append-only log supersedes the weak tooling claim instead of rewriting it.
Log timestamps record when rows were written, not when earlier events occurred.

## Full gate

The clean candidate passed `sh scripts/pr-ready.sh
cbf141b7bf398e079c53945df3537469edc6ad93`. The log is
`/home/syn/code/dione-search-pr-ready-1f0b6ca.log` and ends with the explicit
candidate-head pass message. Release hygiene, formatting, both Clippy feature
sets, Rust 1.98, packaging, public package privacy, dependency policy, release
binaries, and the `dione 0.55.0` launch check passed.

The feature suite passed 1605 tests, with one skipped, in run
`0742db4c-1108-4c86-af2f-f3bb78d7fe3d`. Four default one-shot tests passed in
run `81f67b77-9fa7-4dac-a752-9d8648da11b5`; all 34 Python checks passed.
The existing yanked `yoke-derive 0.8.3` and duplicate dependency warnings
remain. This slice changes only the root package version in Cargo.lock.

Only this evidence record and appended log rows changed after source review.
The final documentation commit requires a clean exact-head gate before push;
its commit ID and result belong in the PR description. CI and an independent
official approval remain required before merge.
