# R1 rendering verification

Base 7ad17021e026f3e88b7f1accce4233c3f4e0aef3 is PR505. Branch refactor/discord-render targets refactor/discord-introspection. This is R1 of the user-fixed R1-R7 scope. Optional unrelated cleanups remain outside this project.

## Contract and design

The compiler and retained PNG file operation move to a neutral owner. Discord owns the complete permission/render/caption/delivery operation. MCP retains parsing and exact wire projection. Private PNG and retained-file receipts encode successful construction. Compiler and file failures remain distinct, with concrete upstream causes. Native paths remain lossless until MCP formatting. No cache, scheduling change, retention change, new authorization rule or harness support is part of R1.

Three independent GPT-6-Astra contexts proposed designs. Parent scored A24/B24/C25 before reading the independent same-model judge, who scored A23/B22/C24. Both selected C. Scores evaluate proposed designs, not demonstrated equivalence. Grafts retain full PathPersistError display/cleanup ownership, the exact wire checklist and the distinction between locked png0.17 and png0.18. No stateless service, ordinary-count wrapper or public API solely for tests is added.

## Baseline evidence

The unchanged base passed 16 tests selected by test(render), with1595 skipped. Log /home/syn/code/dione-render-baseline.log. Two temporary native integration probe runs produced identical eight successful PNG outputs and two failure observations. The probe used cargo test --test r1_render_probe -- --nocapture solely to capture printed observations; ordinary verification uses Nextest. Logs /home/syn/code/dione-render-oracles.log and dione-render-oracles-r2.log. Temporary probe source was removed from native checkout and retained outside the repo at C:/Users/Jaycee/AppData/Local/Temp/dione-render-design/probe.rs.

Committed fixture tests/fixtures/render-oracles.json records eight successful PNG lengths and SHA256 values for simple math, square root, four matrix variants, escaped dollar and empty input. They are exact-byte oracles for the native dependency environment, not a cross-platform stability promise. Malformed fraction checks the diagnostic stage and missing-denominator message without pinning incidental diagnostic span identity.

The first characterization compile failed because the existing cfg(test) with_pre_send_pipeline helper was private. Its visibility is now pub(crate) only under cfg(test); no production visibility changed. Log /home/syn/code/dione-render-characterization.log. The corrected run passed26 rendering-related tests with1595 skipped, Nextest b22d256f-e40e-4634-91eb-f11717ffaeca. Log /home/syn/code/dione-render-characterization-r2.log. Default all-target Clippy passed in /home/syn/code/dione-render-characterization-clippy.log. Custom formatting was applied to the three changed test-bearing files before that Clippy run. Parent read the characterization diff, including formatting and the later expansion of one dense assertion. Signed baseline commit7f710e8bc4d1622e5fd5e3eb966e6234bf59ba70 contains the tests, fixture and cfg(test) visibility change.

The pin covers parser priority, required/optional grammar, operation versus argument error envelopes, exact persisted-file responses, override/permission/compiler ordering, empty and absent captions, actual loopback Serenity multipart requests, ignored typing failure, caption hook surface/halt/bypass, successful sent-ID registration and failed-send nonregistration. No real Discord service is contacted.

## Evidence limits

Compiler unsafe-converted-dollar, no-page, encode failure and native tempfile create/write/keep failure paths are not naturally induced. Pure error projection tests, if added, do not establish reachability of those native failures. Caption redirect/rewrite remain existing shared messaging behavior rather than newly covered end to end. There is no attachment held-queue claim. Synchronous compiler and filesystem work is an inherited standards exception deliberately retained to preserve execution and cancellation semantics.

No live Discord, construct, harness or external Rust consumer validation has occurred. Before R7 begins, notify the user and inspect Oh My Pi source/docs outside the Dione repo. Construct testing remains required before considering main.

## Migration and source review

Signed source commit a1c61f4136083e1de3fde96c1fc5028eb247a430 moves complete operations and deletes the old public module without aliases. Version 0.58.0 records the deliberate Rust API break. Parent read the implementation, wiring, dependency diff, and formatted new production modules. The implementation writer worked in an isolated checkout and ran no builds. It disclosed that it read its poteto role instructions after its initial edits; parent supplied the grounding and design brief before work and owned the required workflow steps.

The first migrated run passed all 30 focused tests, with 1595 skipped, Nextest e946198d-6d34-4f38-8b01-2d60b35cffac. Log /home/syn/code/dione-render-migrated.log. These are the baseline 26 selected tests plus four new receipt/error tests. Compiler tests moved from the external test binary to the private owner. The JSON comparator retains eight contract tests and 34 JSON expressions. It also compares the compiler template, scale, ordered replacements and fixture bytes against signed baseline 7f710e8. Run python .audit/compare-render-fixtures.py 7f710e8bc4d1622e5fd5e3eb966e6234bf59ba70 from the repository.

Comment review recommended five deletions, which parent applied. Required module/API documentation and dependency explanations remain; the comment reviewer verified the mitex and Typst builder claims against installed sources. No new suppressions, workaround flags, restoration, encoding proposals or unresolved comment constraints were found. Source search found no old Rust rendering entry points under src or tests.

Three fresh GPT-6-Astra contexts reviewed paired Safety/Tests, Design/Idiomacy, and Security/Privacy lenses and reported no new scoped findings. Their reports identify the working tree on baseline 7f710e8 and explicitly separate source reasoning from execution. They are in C:/Users/Jaycee/AppData/Local/Temp/dione-render-design/review-*.md. This is same-model separate-context review, not model diversity or official Forgejo approval. Full briefs are retained in review-brief.md and parent tool calls.

The first candidate Clippy run failed result_large_err because the combined channel error was at least 128 bytes. Parent boxed only the existing MessagingError payload and dereferenced it at MCP projection, retaining its concrete cause and feedback. Log /home/syn/code/dione-render-clippy.log. The second default all-target Clippy run passed in /home/syn/code/dione-render-clippy-r2.log. The 30-test run predates this boxing change; final gate and mutation restoration will verify the committed candidate. A fresh source delta review covers the three-line boxing correction after the initial reviews.

Standalone rustfmt with all repository custom options passed the three new modules and changed dispatch file before the successful Clippy run. The broader inherited custom-format differences documented in the previous slice remain outside R1. No repository-wide formatting sweep is claimed. The full gate still runs the repository's cargo fmt --all -- --check.

## Mutation and final source review

At a1c61f4, .audit/prove-render-preflight.py temporarily removed the early check_outbound statement while holding the same exclusive native source lock used by synchronization. The target failed its literal assertion with LaTeX rejection instead of denied channel42. This proves detection of one authorization-order regression. It does not prove exhaustive mutation coverage. The helper required a clean checkout and exact HEAD source bytes, restored those bytes in finally, checked the source hash and clean status, and reran the target. Restored Nextest 7a501dd5-1c98-4eb5-9b1a-c17a29085653 passed one test with1406 skipped. Before/after SHA256 d4711b251dfc5ce4ec2c7d613cdce777a2134a0ca9088134b47e9698a6206b1b. Logs /home/syn/code/dione-render-preflight-proof.log, dione-render-preflight-mutant.log and dione-render-preflight-restored.log.

The outer bundle/shell command emitted a Git-path error from its command-substitution status check before the native cd. The mutation helper's own clean-tree assertion ran in the native checkout and passed before any source mutation. Future shell commands omit that redundant substitution. Source synchronization was not run while the mutation lock was held.

A fresh GPT-6-Astra reviewer checked the boxing correction through all six lenses at a1c61f4. Reversing exactly the three boxing substitutions reproduced the prior four reviewed source hashes; there were no other source differences. All six source lenses passed without new findings. Report C:/Users/Jaycee/AppData/Local/Temp/dione-render-design/review-final-delta.md. The final full local gate, remote CI and official approval are separate from these source reviews. Full-gate result and submitted head will be recorded in the PR body after the trail audit.

## Cross-model trail corrections

Claude Sonnet 5.5 audited this run against the scoped transcript and artifacts. Its report found one P2 wording issue in the unpublished PR draft and five P3 provenance, bookkeeping and coverage notes; none requires a production change. Report C:/Users/Jaycee/AppData/Local/Temp/dione-render-design/trail-r1.md. The PR description now reserves its pass claim for the exact-head gate and carries the native-environment, live-service, fault-path and inherited synchronous-work limits.

The 26-test characterization run preceded the final assertion layout expansion and formatting that were committed as 7f710e8. That exact baseline commit was compiled under all-target Clippy but was not separately test-executed. The 30-test migration run preceded five comment deletions and the later three-line boxing correction. The full gate is the first full test execution of the exact submitted production bytes. Source comparison and the reviewed three-line delta support continuity, but do not replace that exact-head run.

The optional RenderLatexCaption-to-SendFileCaption mutation suggested by the design and implementation reports was not run. The early-authorization mutation supplies the required concrete regression-detection proof; the caption surface remains covered by its literal runtime assertion and source review, without a separate mutation claim. Additional mutation work is not needed to close a remaining identified defect.

TSV timestamps are record-write times, sometimes batched, rather than reconstructed event times. The appended correction row points at the probe capture, five comment deletions and paired source reviews described above. The retrospective start row identifies the prior-slice range and this R1 run; it does not move or rewrite earlier rows. Existing line-ending differences are retained to keep the trail append-only. The design checklist now explicitly means implementation completion.

The first full gate began concurrently with the read-only trail audit after source freeze, contrary to the earlier brief's intended serial order. Any documentation correction produces a new commit and requires a full exact-head gate before publishing. The final result belongs in the PR body. The native PNG fingerprints remain a CI watch item because remote CI is the first additional environment. A failure there requires investigation; do not weaken the fixture silently or regenerate it without recording the dependency or environment reason.
