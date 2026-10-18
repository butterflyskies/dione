# Typed query verification record

This slice targets `refactor/discord-presence` at
`5fd89ddd3322582d3170508b711298ee84d15d4c`, the head of PR #495.
The public Rust migration preserves the admitted MCP inputs and outputs.

## Design and ownership

Three independently scoped GPT-6-Astra agents compared operations left in
`messaging.rs`, a nested query owner, and private message views with
operation-specific results. A separate GPT-6-Astra judge recommended the nested
owner after a 23-to-23 tie with the private-view design; complete operation
ownership broke the tie. The parent's separate scores were 24 and 23. These
scores are design judgments, not verification. The parent selected that owner and private result construction, with
derived page metadata and typed paths for downloads. These are independent
agents on the same model, not different model families.

The selected shape places the five complete read and download operations in
`discord/messaging/queries.rs`. MCP owns JSON projection in `query_wire.rs`.
Search shares the message projection while retaining its current transport,
authorization, filtering, and ordering. No forwarding service or duplicate
query context is required. The implementation reuses the existing semantic
`timestamp::Timestamp` instead of adding the timestamp wrapper proposed in
the initial sketches.

One writer uses an isolated worktree. The parent owns native Linux builds,
integration, signed commits, and Forgejo writes. Existing local HTTP fixtures
move with their tests. The slice introduces no new external mock seam.

## Behavior evidence

The existing baseline at the parent commit passed 124 selected tests.
The command selected new-since, fetch and pin builders, pagination, reply
projection, receipt, evidence projection, and search tests. Its log is
`/home/syn/code/dione-queries-baseline.log`.

Before production edits, five new `query_contract_` tests passed in Nextest
run `51209db7-c2fa-498f-92fa-dc0ca6902eb9`. They pin complete collection
payloads, MIME field omission and nulls, independent reply metadata,
authorization before cursor conflict rejection, and search filtering with
transport ordering. Their log is
`/home/syn/code/dione-queries-characterization.log`.

The characterization source copies and comparison script are retained under
`C:/Users/Jaycee/AppData/Local/Temp/dione-queries-design/`. The script compares
29 JSON expressions across the five tests, ignoring formatting whitespace
outside string literals. It verifies fixture preservation, not runtime output
equivalence by itself. The before and after test runs supply that evidence.

The trail reviewer compared the saved characterization files with the parent
commit. They contain only test additions: 85 lines in `messaging.rs` and 21 in
`search.rs`, with no production changes or deletions. Their raw-file SHA-256
hashes are respectively
`711bd4393d061dee35195c57d722cac2a7478fd7168a74d0d2376cb865742234` and
`bd6acb34e786183ffefe7085702d867475c8597c2277dc8927d5ce010e6a54aa`.
The characterization state was not committed separately.

## Coverage boundaries

Search's raw-group-count pagination expression remains outside the pure
filtering tests. Source review confirmed that this slice preserved that
expression. That inspection is separate from executed coverage.

Zero-limit tests exercise result construction and projection. Source inspection
confirms that the HTTP call still forwards `Some(limit)`, but this slice adds no
zero-limit HTTP trace. The live Discord smoke test remains ignored. The moved
download helper tests do not establish cancellation cleanup or bounded buffering
for oversized responses without a content length.

The existing direct Rust cursor options, permissive limits, best-effort
attachment failure handling, and broad `MessagingCtx` are retained contracts.
This slice does not establish repository-wide type compliance. Official
Forgejo approval remains separate from local review and verification.

Other unchanged debt includes no explicit download timeout, default redirect
behavior, attachment URLs in warning logs, trust in the inbox parent directory,
and raw Discord error bodies in search. Earlier slice sections in the design
document describe debt at those historical checkpoints. This query slice
supersedes their statements that query success JSON remains.

## Final candidate

The signed implementation commit is
`c6320a4e7e6256f8b1321b4b0e55f7149b462916`. Default-feature Clippy passed with
all targets and warnings denied. The first run found three imports left in the
production module after relocation. A second run found that two of those imports
were unnecessary in tests too. The parent removed them and kept `Value` in the
test module. Logs retain each attempt under `dione-queries-clippy` in
`/home/syn/code/`; `dione-queries-clippy-final.log` is the successful run.

The focused suite passed 151 tests in Nextest run
`a562288e-dca9-4a8b-806d-cdde60ce2e31`. The filter was
`test(discord::messaging::queries) | test(search) | test(evidence) | test(sentex)`.
The log is `/home/syn/code/dione-queries-focused.log`. The final test selector
differs from the initial baseline selector, so these totals are not a count of
tests added by this PR.

Three independent GPT-6-Astra reviewers each ran two separate lens passes at
the implementation commit. Safety, Tests, Design, Idiomacy, Security, and Privacy
returned no actionable P1, P2, or P3 findings. Their source-review reports are in
the local design directory. They ran no builds and did not grant official
Forgejo approval. The parent read all three reports. They received a
parent-written brief that included expected contracts and scope limits. Its
import-cleanup wording was corrected after review; the earlier local brief was
not retained. These are independent agents, not a blind or cross-model code
review.

The review evidence is summarized here so the result does not depend on local
temporary report files.

| Lens | Inspected evidence and verdict |
| --- | --- |
| Safety | The five operations preserve authorization and SDK calls; private collections preserve ordering; receipt failure does not fail retrieval; download helpers match the base. No actionable finding. |
| Design | One owner contains complete operations; MCP projects results; search shares observations; old builders and legacy adapters are deleted with their callers. No actionable finding. |
| Security | Live outbound checks precede I/O; search checks channels before decoding; evidence authority remains the message author; atomic download creation and collision handling are unchanged. No actionable finding. |
| Privacy | Summary responses still omit MIME; only explicit retrieval writes a configured receipt; the receipt writer receives identities, not message content. No new persistence or logging. No actionable finding. |
| Idiomacy | Identities, timestamps, byte units, and paths retain semantic types; private fields prevent mutation; receipt failures retain their typed cause; matching is exhaustive. No actionable finding. |
| Tests | Literal payload assertions cover omissions, nulls, ordering, and independent reply metadata; local HTTP receipt tests and byte-level download tests moved and were adapted to typed results and wire projection; authorization tests include both allowed and denied targets. No actionable finding. |

Reviewers compared the preserved search pagination expression and download
helpers against the base. These comparisons are source evidence. They did not
run live Discord, hostile-network, cancellation, or concurrent-revocation tests.

The comment review identified 21 narrative lines in two files. The parent
removed them, kept public contracts, and found no suppressions or required code
reshapes. No comments were restored. The final adapter constructs attachment
projections directly with a private summary/detail enum rather than mutating a
serialized array. Five existing test fixture helpers became `pub(super)` for
the nested test module. The removed zero-limit comment's rationale remains in
the tests and this record: a zero limit must not imply another page merely
because `count == limit`.

The authorization mutation moved cursor conflict rejection before the outbound
permission check. Nextest run `722589af-9d00-4641-a5a3-0e8811de2758` failed the
intended literal error assertion. The helper restored the original source bytes,
and run `878bfe89-6518-4146-b71e-81dce8523a4b` passed the same test. Logs are
`/home/syn/code/dione-queries-mutation.log` and
`/home/syn/code/dione-queries-restored.log`. This proves the authorization
precedence assertion detects that change; it is not mutation coverage of every
preserved contract.

The implementation gate ran from a clean native Linux checkout at the signed
implementation commit. Before this record was committed, both Clippy feature
sets passed, Nextest run `9e637d00-9327-4c92-8ebe-efa1e3d1d5a0` passed 1596
tests with one ignored test, and run `9960c467-1cf1-4b1b-a734-5c45681097a7`
passed all four default-feature one-shot tests. All 34 Python checks, the Rust
1.98 check, package verification, package privacy, and dependency policy passed.
The release build was still running. Its complete result and the final-head
rerun belong in the PR body. The implementation log is
`/home/syn/code/dione-queries-pr-ready-c6320a4.log`.

Packaging retains the existing `yoke-derive 0.8.3` yanked-version warning and
dependency policy retains duplicate-version warnings. This change updates only
the Dione version in the lockfile. A successful gate does not mean warning-free
dependency output.

## Trail review

Claude Sonnet 5.5 checked this task's transcript through position 2355 and the
local evidence, including full recovery of selected truncated tool results. It
did not read every later transcript item or re-run the gate. It found no
blocking or high-severity issue. Its low and low-to-medium flags concerned
characterization provenance, attribution, review provenance, log timing,
retained debt, wording, and the final submitted commit. The clarifications above
address those findings. TSV timestamps record batched write times, not exact
operation start or completion times; later rows close earlier pending states.
The reviewer independently repeated the 29-expression fixture comparison.

The final documentation commit must change only `.audit/`. The full gate will
run again from a clean native checkout of that final head, whose SHA and result
belong in the PR body. Local reviews do not replace official approval.
