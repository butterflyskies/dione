# Discord management verification

Base `cc7ee6adc46d6dc898da69737a826f12cbd8c0b8` is PR #502.
This slice uses `refactor/discord-management` and targets `refactor/discord-search`.
Log timestamps are write times, not reconstructed event times.

## Contract and design

Before this slice, pin, unpin, delete, and thread creation mixed Discord policy, HTTP,
state effects, and MCP JSON. A narrow Discord owner now completes operations.
MCP parses existing arguments and projects typed results. Three message
mutations share one typed action and unit result; thread creation has a separate
result. This keeps one authorization sequence without an all-purpose outcome
union. No service object, transport trait, or caller-managed authorization token
is needed. Unrestricted thread names remain text under the strict type policy.

Three independently scoped GPT-6-Astra designs and a same-model judge compared
alternatives. The parent selected B, scoring A23/B24/C23; the judge scored
A23/B25/C23. These are design judgments. The synthesis rejects C's extra target,
origin, and operation wrappers. It retains A's explicit standalone-thread gate
and internal own-send lookup. The judge incorrectly named src/discord.rs as the
facade; the actual file is src/discord/mod.rs. The parent corrected this before
implementation.

History in commit 0f2b58c and PR #455 establishes that management's stricter
IngressOrOwnSend policy is intentional. A canonical Discord lookup cannot
substitute for admission. Preserve gate, own-send, ledger, and mutation order.
Unknown targets can still trigger a configured canary alert. Channel mismatch
still blocks own-send targets; existing Expired and Unavailable handling is
unchanged. Ledger verification can perform expiry housekeeping.

Thread success must return Discord's ID and name while recording that ID
against the requested parent. The cache write precedes the result. The response
parent is not the authority for this mapping. Thread names are forwarded without
new bounds. Optional nonstring malformed message IDs retain their legacy absent
interpretation. Parser failures stay dispatch errors; operation failures stay
tool results.

## Characterization sequence

The untouched base passed six selected management tests, with 1588 skipped,
in `/home/syn/code/dione-management-baseline.log`. The selection includes a
management-related test outside the management module. One old test merely
imitated the thread-cache write. Six new contract tests replace that gap with
real operation calls and literal results through Serenity's existing loopback
proxy. No production execution seam is introduced.

The initial test build failed because the fixture compared a nested optional
cache value with a plain integer reference. The expected value was corrected
to Some(&Some(42)). Production behavior was unchanged. The native sync helper
initially tried Linux Git against a Windows worktree pointer and failed before
copying anything. Windows Git now writes its changed-path manifest for the
native copy helper. These are tooling corrections, not implementation results.

Comment review proposed eight deletions. Seven narrative lines were removed;
the public context doc remains under the public API exception and repository
documentation requirement. Three additional narrative lines disappear with the
replaced simulated test. No suppressions, MUST KILL flags, or reshape remain.

Default all-target Clippy passed in
`/home/syn/code/dione-management-characterization-clippy.log`.
The characterization run passed all 11 selected tests, with 1588 skipped, in Nextest run `4e2caf9f-6558-421a-afc6-1d7652f51612`. Its log is `/home/syn/code/dione-management-characterization-r2.log`.

Signed characterization commit `1589237d668e90dee4bddf109ad22c90440ab31a`
preserves that state. Its management blob is
`36543ad5e7a66881df999de76a5e1759aec813da`; its dispatch blob is
`d19738342ef6a04ab704c8bdf51ee0e703cd0323`.
Run `python3 .audit/compare-management-fixtures.py 1589237` from a checkout
containing that commit to compare the six contract tests' 36 JSON expressions
with current source. This checks expressions, not runtime outputs by itself.

## Limits and later work

Characterization currently covers HTTP rejection, not a malformed successful
thread response. Shared verifier tests cover deeper expiry and canary policy.
The new fixtures disable canary configuration, so their no-HTTP result must not
be generalized to configured alerts. No live Discord or external Rust consumer
compatibility is established. Existing per-call config snapshots, separate state
reads, SDK lifecycle/retry behavior, raw SDK error disclosure, bounded own-send
and thread caches, and shared verifier policies remain unchanged.

Oh My Pi support and testing on a construct are required later, before
considering any merge to main. This slice does not implement those gates.

Local artifacts are under
`C:/Users/Jaycee/AppData/Local/Temp/dione-management-design/`.
Native checks run in `/home/syn/code/dione-pr-queries`, with one parent build
owner. Design files, grounding, characterization, comment review, and the
synthesis are separately named reports. Later sections record implementation
and review results. Local agent review does not constitute official Forgejo approval.

## Implementation and review

Signed candidate `efe50dc3a92231645df8e6c5368ffb05c1d8c81b` moves complete
operations into `discord::management`. MCP keeps parsing and pure JSON projection.
The former public management module is removed in version 0.56.0. The replacement
is crate-private. External Rust source compatibility is intentionally not retained.

Initial migrated Clippy found redundant field names and a large SDK error variant.
The fixes use field shorthand and boxed SDK error storage without changing the
source or its wire display. Corrected Clippy passed in
`/home/syn/code/dione-management-clippy-r2.log`. All 12 selected management tests
passed, with 1588 skipped, in `/home/syn/code/dione-management-focused.log`.
The committed fixture comparator retained all 36 JSON expressions across six tests.
Two proposed module-doc deletions were rejected because repository standards
require module purpose documentation.

Three independent source reviewers covered all six lenses at efe50dc against
cc7ee6a. Design, Idiomacy, Security, and Privacy passed. Safety and Tests identified
P3-1 in the new HTTP fixture. Its accept timeout did not bound header/body reads,
and premature header EOF could loop forever. The correction bounds the entire
exchange and rejects EOF. Final review and execution of this correction remain
pending at this checkpoint.

A temporary native mutation removed the real thread-parent registration. The
thread contract test failed at the literal expected mapping, reporting None
instead of Some(Some(42)). Restoring the original bytes passed the same test.
Evidence is `/home/syn/code/dione-management-mutation.log` and
`/home/syn/code/dione-management-restored.log`. During this experiment the parent
accidentally copied the mutated native file to the primary worktree. The diff
exposed the missing production block immediately; it was restored before any
commit. The mutation helper restored its native file in a finally block.
No copy or source sync may run while a native mutation is in progress.

## Fixture correction

Signed source candidate `10051036b2514420a95369ec7668cecddd0d4670` changes only
the management test module relative to efe50dc. The fixture uses a five-second
whole-exchange deadline and rejects request/header EOF. Two real TCP tests
exercise incomplete headers followed by shutdown and an incomplete declared body
held open. Separate ten-second watchdogs distinguish fixture failure from a hung
helper, and assertions identify the expected panic causes.

Clippy passed in `/home/syn/code/dione-management-clippy-r4.log`. The corrected
focused run passed all 14 tests, with 1588 skipped, in
`/home/syn/code/dione-management-focused-r2.log`. The stalled-body test completed
in 5.044 seconds; the whole selected run took 5.049 seconds. This measures the
fixture deadline, not production Discord latency. The fixture comparator again
confirmed six tests and 36 unchanged JSON expressions. The full native gate is
running at source candidate 1005103; completion is not claimed yet.

The reader now follows one mutation authorization sequence instead of three
copies. Thread creation retains its distinct result and state effect. JSON
construction resides in a 29-line MCP projector. The explicit domain result and
error types replace operation-owned JSON; no compatibility forwarding layer or
new transport trait remains.

## Final source review

A fresh independent GPT-6-Astra reviewer inspected source candidate
`10051036b2514420a95369ec7668cecddd0d4670` against base
`cc7ee6adc46d6dc898da69737a826f12cbd8c0b8`. The reviewer confirmed that the
production prefix matches efe50dc byte for byte. The report is
`C:/Users/Jaycee/AppData/Local/Temp/dione-management-design/source-r2.md`.
P3-1 is closed by the source correction and the parent's executed fault tests.
No actionable P1, P2, or P3 source findings remain.

| Lens | Result | Evidence |
| --- | --- | --- |
| Safety | Pass | Gate and verified target precede HTTP; locks end before network calls; only parsed thread success writes the requested parent. The fixture now bounds reads and rejects EOF. |
| Design | Pass | Discord owns complete effects; MCP parses and projects. All callers migrated and the old public module is deleted. |
| Security | Pass | Authenticated own-send evidence stays internal; fixed management policy preserves mismatch rejection and prevents unknown-target canonical fallback. |
| Privacy | Pass | No new audience, log, persistence, or retention change. Wire fields and existing error text remain unchanged. |
| Idiomacy | Pass | Typed IDs, exhaustive actions/errors, context plus source errors, crate-private APIs, and unrestricted text follow the strict standards. |
| Tests | Pass | Literal HTTP and wire contracts pair failures with successes. Real creation exercises parent registration and a later authorized mutation; the mutation proof fails when registration is removed. EOF and stalled-body tests execute the real fixture. |

These are local agent review results, not official Forgejo approval. CI and an
eligible independent reviewer must cover the submitted head before merge.

## Cross-model trail review

Claude Sonnet 5.5 reviewed the scoped parent transcript through position 2967,
resolved truncated items, inspected logs and commits, and reran the read-only
fixture comparator. The report is
`C:/Users/Jaycee/AppData/Local/Temp/dione-management-design/trail-r1.md`.
It found no false completion claims. Its flags and dispositions follow.

1. Only thread registration had mutation evidence. A second experiment against
   signed source 1005103 will replace the authenticated own-send lookup with true
   and require the existing literal ingress-refusal test to fail. Its result
   remains pending here. Mismatch, policy selection, and gate ordering have normal
   behavior assertions and source review; separate mutations of those paths have
   not been run.
2. The registration mutation and Clippy r3 used an uncommitted intermediate
   containing the parent's first fixture correction. They did not run at an exact
   signed head. Its production code matches efe50dc and 1005103, but the full
   intermediate source was not hashed. Only the clean full gate is evidence for
   the exact signed candidate. The own-send experiment will assert HEAD, clean
   status, committed source equality, and the original source hash before edits.
3. Commit 1005103 was signed after Clippy r4 passed but before focused-r2 completed.
   The later 14-test pass verifies that same source. Signing was not presented as
   a test result. Clippy evidence is `/home/syn/code/dione-management-clippy-r4.log`;
   fixture comparison is reproducible with the committed helper and was rerun by
   the trail reviewer. Earlier pending statements describe their checkpoints.
4. The fixture helper extraction and two fault tests intentionally exercise the
   same server used by the operation tests. They are the execution proof requested
   by P3-1. The stalled-body test costs about five seconds and deliberately checks
   its panic cause. Its Tokio debug-text expectation is coupled to the locked
   dependency and may need an update when Tokio changes. Wall-clock deadlines
   remain sensitive to a heavily loaded host. Concurrent unrelated Cargo work
   used a separate checkout and target directory; this parent was the sole Cargo
   owner of the native verification checkout, not of the whole machine.
5. The design judge and source reviewers all used GPT-6-Astra. Their independence
   is separate context and source inspection, not model diversity. Their briefs
   included the contracts to inspect. All six lens results are source-review
   results; the parent separately executed tests. Claude supplied cross-model
   trail review, not an official approval or a second source-review model.
6. The parent read the judge report before writing its design scores. They were
   not blind scores. They express the parent's final judgment after comparison.
7. The implementation writer worktree remains dirty and is retained as a local
   artifact, not another candidate. `migrate-management.py` generated the initial
   move only; do not rerun it over the later manual typed-error test, boxing, or
   fixture fixes. `management_typed_errors_retain_channel_category_and_source`
   is the twelfth selected test. The historical PR body is saved as `pr455.md`
   in the artifact directory. The construct-gate documentation accompanied the
   characterization commit; that commit therefore includes this small scope note.
   A Git bundle transferred the source candidate to the existing native checkout;
   staged-tree equality was checked before a soft reset and clean-state gate.
8. TSV times are write times, including batched rows. The production-lens evidence
   also includes `design-idiomacy-r1.md` and `security-privacy-r1.md` in the artifact
   directory, not only the Safety report cited in the early row.

The review also highlighted retained API, cancellation, SDK error, canary, and
malformed-response limits already recorded above. These remain explicit limits,
not newly established guarantees.

## Complete source-candidate gate

The clean native command
`CARGO_TARGET_DIR=/home/syn/code/dione-vertical-native/target sh scripts/pr-ready.sh cc7ee6adc46d6dc898da69737a826f12cbd8c0b8`
passed at `10051036b2514420a95369ec7668cecddd0d4670`. Its log is
`/home/syn/code/dione-management-pr-ready-1005103.log`.

The gate passed release hygiene, formatting, both Clippy configurations, all
1613 feature tests with one skipped, four default integration tests, 34 Python
checks, Rust 1.98 compatibility, package verification and privacy, dependency
policy, both release binaries, and the `dione 0.56.0` version launch. One compile
failure test was slow but passed. Existing yoke-derive 0.8.3 yank and duplicate
version warnings remain; dependency versions did not change. The optimized
release build took 9 minutes 44 seconds on this host. That is a build duration,
not a runtime performance measurement.

This result covers the source candidate. The submitted documentation head and
its separate gate result must accompany the PR before publication.

## Own-send mutation result and final handoff

The own-send experiment started from clean native HEAD
`10051036b2514420a95369ec7668cecddd0d4670`. It checked the source against the Git
blob after normalizing CRLF to LF and recorded the CRLF working-file SHA256
`fb65980240553e83cd3b2c0c00cb50d985c2b03f093cc9ce208b756edee28ae7`.
The LF Git blob SHA256 is
`b062d3765a25eda07ebb7b958041151c33f92b74c52c5a333a8af157d653883c`.
Replacing the authenticated lookup with `true` made the existing denial test
fail on `{"ok":true}` versus the literal `ingress_required` payload. The helper
restored the original bytes in a finally block. The same test then passed in
Nextest run `0427875d-3a12-4775-8aac-17e74c566a93`, and Git status was clean.

Logs are `/home/syn/code/dione-management-own-send-mutation.log` and
`/home/syn/code/dione-management-own-send-restored.log`. The local reproducible
helper is `C:/Users/Jaycee/AppData/Local/Temp/dione-management-design/prove-own-send.py`.
This closes the trail review's primary mutation-evidence gap. No primary source
copy or native sync occurred during this experiment.

Only these two audit files change after the reviewed source candidate. The
final documentation head must run the same clean-native gate before publication.
Its exact SHA and result will accompany the PR. Official CI and independent
Forgejo approval remain required. No deployment or merge is part of this work.

## Follow-up trail audit

Claude Sonnet 5.5 reviewed the later transcript through position 3059 and the
new evidence in `trail-r2.md`. It confirmed the exact-source own-send failure
and restoration and found no false completion claims. This follow-up corrected
the unlabelled pending sentence above and distinguished the CRLF working-file
hash from the LF Git blob hash. The earlier scripted pending-text replacement
had silently missed LF text because it matched CRLF; the direct patch fixes it.

The final PR body must name its exact head and gate result and identify source
review as local GPT-6-Astra review. The audit file cannot contain the result of
its own later gate. `final-head-review.md` separately checked the bdf64a7 audit
delta and carried forward the unchanged six-lens source result. That was another
local GPT-6-Astra context, not cross-model source review or official approval.

The gate's printed `Six lenses: failure` and `Could not refresh PR #1` lines
are expected negative-case output from `.forgejo/scripts/test_review_receipt.py`.
`test_event_posts_status_for_exact_head` mocks invalid and missing reviews;
the push-refresh test injects IncompleteRead and asserts the failure. All 34
checker tests pass. These lines are not live Forgejo review statuses.

The own-send helper uses `--lib` because the selected test is in the library;
the full gate still uses the workspace. Bundle transfer used a staged-tree
comparison for the source candidate and a fast-forward merge for the later
audit commit. These operations did not change the tested source. Separate
mutations for mismatch, policy selection, and gate ordering remain unexecuted
and disclosed; the normal contract tests and source review cover those paths.
