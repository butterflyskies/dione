# Access-request verification

## Scope

R2 begins at `07baad4c667c15967574395c19670de48deb4fbd`, the R1 rendering head on `refactor/discord-render`. The implementation branch is `refactor/discord-access`. This slice covers typed listing, approval, denial, and MCP projection. The queue format, configuration mutation runtime, ingress, pruning, and authorization policy stay within their existing contracts.

## Grounding

The old MCP handler combines orchestration with JSON. Approval snapshots a pending request, releases the queue lock, and awaits `ConfigRuntime::mutate`. Unknown config durability retains the request. Durable config mutation precedes attempted queue removal. Queue writes are synchronous and best effort. The response does not prove durable queue removal. Cancellation can leave an applied config and pending request, and concurrent denial can race the approval snapshot. This slice does not claim transactional approval.

Persisted queue records allow a zero user ID. Listing must preserve it without constructing a nonzero Serenity ID. Command targets retain the dispatch parser's nonzero ID requirement. The `admin_only_mutations` switch disables approve and deny before argument parsing; it is not per-request administrator authentication. Listing remains ungated.

## Baseline

The unchanged base passed `cargo nextest run --workspace --locked --features oneshot-test-seam -E "test(access) | test(queue::tests)"` in the native Linux checkout. The selection ran 32 tests, all passed, with 1605 skipped. This broad name filter also selects tests outside access requests. The log is `/home/syn/code/dione-access-baseline.log`.

## Characterization

Signed commit `83a630eb2838d041e26430c2ac0465306b2e592d` contains seven characterization tests and the literal historical queue fixture. Production code before each test module was compared to the base and was unchanged. The final run passed all seven tests with 1406 skipped, followed by all-target feature Clippy with warnings denied. Logs: `/home/syn/code/dione-access-characterization-r3.log` and `/home/syn/code/dione-access-characterization-clippy.log`.

The first compile could not find the new fixture because the parent copied only the two Rust files. The complete manifest fixed this. The next run passed six tests but failed the dispatch gate fixture: direct file writes did not refresh Dione's cached configuration. The fixture now explicitly reloads configuration before each gate mode. No production behavior changed. Earlier logs are `dione-access-characterization.log` and `dione-access-characterization-r2.log` in the same native log directory.

The literal expectations cover historical zero and maximum IDs, mismatched stored keys, timezone/tie ordering, durable and idempotent approval, missing lookup before config failure, uncertain durability and successful retry, denial, best-effort queue write failure, and MCP gate/parser/error envelopes. Generation values are checked as positive then normalized. The uncertain-durability warning is checked exactly against its observed generation before normalization.

## Selected design

Three same-model independent sketches compared a Discord owner, operations beside the queue, and context methods. The parent tied A/C at24 and preferred A initially; the blind judge preferred C25 over A24 and B22. The parent selected C's complete context methods and flat private receipts after considering both rationales. Queue storage remains separate, with no forwarding service. The review artifacts are under `C:/Users/Jaycee/AppData/Local/Temp/dione-access-design/`.

## Migrated implementation

Signed source candidate `bc8b98ae809ed3e8466d1710e9bcf9de203f7c94` moves the three operations into `discord::access`. The seven original behavioral tests now exercise the typed owner and pure projector. An eighth test checks the retained concrete error through the source chain. All eight passed with 1406 skipped in `/home/syn/code/dione-access-migrated.log`, run `1171f40d-91b9-4537-8e70-426e57e08e3a`. The native checkout was an uncommitted source copy during that run; the production and test text matches this signed source commit after CRLF normalization. Documentation edits after copying do not affect the tested source.

`python .audit/compare-access-fixtures.py 83a630e` checks that the seven tests retain their 23 JSON expressions and that the historical queue fixture is unchanged. It checks literal expressions, not complete semantic equivalence by itself. Real before/after test runs provide the behavior evidence. The source-chain test constructs a synthetic error; the config-failure test separately executes a real ConfigRuntime failure.

The implementation worker reported a temporary script failure from Windows default text encoding before dispatch edits. It resumed using explicit UTF-8. Parent review found the intended source/integration diff and unchanged fixture. Repository custom formatting merged an existing adjacent server import. No dependency versions changed; only Dione moves to 0.59.0.

The scoped comment review found no deletion candidates, no actionable constraints, and no new suppressions. Required module and API contract documentation remains. The parent read the full new modules and integration diff and checked private fields, source retention, ID handling, exact projection, and unchanged queue/config files.

## Source review

Three separately scoped GPT-6-Astra reviewers checked source candidate `bc8b98ae809ed3e8466d1710e9bcf9de203f7c94` against the full base. Safety, Tests, Design, Idiomacy, Security, and Privacy each passed without an introduced P1-P3 finding. Reports are `review-safety-tests.md`, `review-design-idiomacy.md`, and `review-security-privacy.md` in the temporary review directory. These are source verdicts, not official approvals or runtime results. The complete migrated source also passed feature all-target Clippy in `/home/syn/code/dione-access-clippy.log`.

## Verification limits

The invalid allow-list fixture causes an edit failure before writing; it does not inject every pre-rename OS I/O error. The real directory-fsync and queue-temporary-file faults cover their named failure points only. Successful decisions are exercised through the owner and projector, with dispatch success wiring inspected. Competing actors, cancellation across config publication, corrupt queue recovery, and queue rename failure were not newly injected. Existing config/queue tests remain separate evidence, not proof that this slice fixes their inherited limitations. Nextest process isolation matters for process-wide config state.

No remote CI, official review, live Discord, or construct test result is claimed. The existing yoke-derive 0.8.3 yanked warning is unchanged; this slice updates no dependency version. Harness adapters and Oh My Pi remain later R-items.

## Deliberate regression proof

`.audit/prove-access-retention.py` temporarily removes the request in the unknown-durability branch. The targeted retry test fails because `peek(42).is_some()` is false. The script restores exact original bytes in `finally`, checks the SHA256 and clean tree, then reruns the target successfully. The restored run passed one test with 1413 skipped, run `311880a2-ce58-4e17-bda4-c54a0d31c7b2`. This experiment used clean candidate `fe45d24fe7cf604399712d48a73cee13c7ca2b22`, whose source is unchanged from the reviewed implementation.

Logs are `/home/syn/code/dione-access-retention-proof-r4.log`, `/home/syn/code/dione-access-retention-mutant.log`, and `/home/syn/code/dione-access-retention-restored.log`. Earlier proof attempts stopped before mutation: the copied source had CRLF bytes instead of Git's canonical LF, a normal restore left those bytes unchanged, and explicit normalization required refreshing the index metadata. The parent verified the difference was only line endings, wrote the committed bytes, staged the unchanged blob to refresh metadata, and checked the tree clean. The script's exact-byte and clean-tree guards remained intact.

## Final submission gate

Run `CARGO_TARGET_DIR=/home/syn/code/dione-vertical-native/target sh scripts/pr-ready.sh 07baad4c667c15967574395c19670de48deb4fbd` from the clean native Linux checkout at the final commit. The final PR body records the submitted head, gate completion, and any remaining limitations. The gate log is `/home/syn/code/dione-access-pr-ready.log`. It covers formatting, both feature configurations, test suites, package/privacy checks, dependency policy, release build, and binary launch. Source and dependency changes after `bc8b98a` require renewed review.

The final trail review checks this record against the scoped T3 thread after user request position3831, run47. It is separate from source review and official approval. Reports remain outside the repository in the temporary review directory.
## Trail audit dispositions

The first Claude Sonnet 5.5 trail audit verified the baseline, characterization,
migration, deliberate regression, restored hash, and source-review provenance.
Its five P3 entries were assessed as follows.

1. Added the missing blank line between the access and management architecture
   paragraphs. The submitted head changes, so the complete gate runs again.
2. Kept the source unchanged. The deleted inline comment restated the existing
   `ConfigDurability::Unknown` API contract in `config.rs`, which explains the
   applied rename, failed directory fsync, and possible crash loss. The access
   method contract states retention, and the named retry test executes the
   re-persistence path. The design documentation explains the retry reason.
   Duplicating that comment is not a correctness or documentation requirement.
3. The PR body identifies same-model source review, copied-source provenance,
   and the completed mutation proof. Its evidence link is pinned to the final
   submitted commit. The gate result replaces pending text only after exit zero.
4. The preliminary feature Clippy overlapped a native sync of equivalent source
   text and audit changes. Treat the clean exact-head gate's two Clippy runs as
   authoritative. This overlap did not authorize another concurrent sync.
5. Retained the explicit test limits. One mutation checks loss of the retry
   request. Dispatch success wiring is inspected. The source-review brief
   included earlier results and compatibility constraints; it was not blind.
   The baseline unknown-durability test was replaced by the stronger complete
   receipt/reload/retry characterization test. Temporary reports and raw logs
   are local evidence; the committed tests and scripts are the reproducible
   evidence available to PR readers.

The source, fixture, and dependency versions remain identical to `bc8b98a`.
Only documentation changed after the first trail audit. Source review is not
model diversity or official approval. The final gate log supersedes preliminary
copied-checkout lint and test runs for the submitted head.