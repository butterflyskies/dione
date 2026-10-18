# Typed messaging checkpoint and review record

## PR preparation

The user resumed this slice on 2026-10-10. The published shutdown commit is
`1f3a9a27144dcd788df717f444c7ae1fb4682c05`. Its history is retained; a follow-up
commit carries the test corrections and review record. The PR targets
`refactor/vertical-slices-discord` at
`86048ce5a24c764cc012cdc07071e9946a17c09b`, PR #493.

Independent reviewers checked Safety, Design, Security, Privacy, Idiomacy, and
Tests in two passes. They found one test gap: the partial-diary fixture had no
matched Log or Celebrate records, so its Auto-only assertion could not prove
exclusion. The fixture now matches all three actions and includes a complete
delivery control. Independent review of the final test delta found no remaining
source findings. Comment and deslop review also passed.

The initial focused run exposed an incorrect new test assumption. The baseline
chunker sends one empty chunk. The corrected fixture verifies the captured empty
POST and the fake transport's returned ID. It does not claim that Discord accepts
empty messages. No production behavior changed to satisfy this test. Dated
corrections were appended to the original design and acceptance artifacts.

Mutation evidence from the isolated native Linux checkout:

| Protected behavior | Mutation result | Evidence |
| --- | --- | --- |
| Retry receipts retain earlier posted IDs | Dropping accumulated IDs failed the expected ID assertion; restoration passed. | Nextest `fe517341-d812-4b95-98d2-7ebf2fd7251f`, restored `7ef62011-01d5-4a0b-a9bd-8c2518f790df` |
| Partial initial sends settle only Auto diary records | Removing the Auto filter failed the exclusion assertion. | Nextest `77c72f16-016c-4866-96cd-817c4bc68e75` |

Both mutations were restored. All 184 focused tests passed in Nextest run
`0146ec3e-e20e-405c-b42f-9b8d9f6b22ff`, including the strengthened diary control.
Both moved bounce snapshot payloads match the base byte for byte after line-ending
normalization; only source metadata changed.

Formatting and default workspace/all-target Clippy passed the checkpoint.
The full `scripts/pr-ready.sh origin/refactor/vertical-slices-discord` gate must
pass the final clean commit before PR submission. The PR body records that exact
commit and the final command result, which cannot be embedded in the commit it
identifies. Independent agent reviews do not substitute for official Forgejo
approval. This work does not authorize merge or deployment.

Verification checkout: `/home/syn/code/dione-pr-typed-final`.
Shared build directory: `/home/syn/code/dione-vertical-native/target`.
Logs: `/home/syn/code/dione-typed-retry-mutant.log`,
`/home/syn/code/dione-typed-retry-restored.log`,
`/home/syn/code/dione-typed-diary-mutant.log`, and
`/home/syn/code/dione-typed-final-focused.log`.

## Historical shutdown checkpoint

Stopped at the user's shutdown request. This branch is a work-in-progress checkpoint, not PR-ready.

- Branch: refactor/discord-typed-messaging.
- Intended PR base: refactor/vertical-slices-discord at 86048ce5a24c764cc012cdc07071e9946a17c09b, PR #493.
- Version: 0.52.0 for public Rust API changes; preserve MCP wire behavior.
- Implementation: operation-specific typed reply/DM/consent outcomes, total/partial delivery errors with nonempty MessageIds, shared typed failures, MCP-only projections, typed ping/preview policies, caller/test migrations.
- Design: docs/design/vertical-slices.md. Alternatives and acceptance matrix are retained in C:/Users/Jaycee/AppData/Local/Temp/dione-typed-messaging-design/.
- One writer completed implementation but final changes are not verified. Clippy --lib --tests -D warnings passed before the final redirected-DM test, docs, and import/reexport cleanup. No final focused test result exists.
- Focused Nextest stopped during compilation on shutdown request. No mutation run, independent six-lens review, cross-model trail review, full gate, or new PR completed.
- Comment review on the earlier delta found no actionable findings; reconfirm final delta.
- Both moved snapshot payloads were independently byte-compared against base, differing only in source metadata.

### Original resume instructions

1. Read the current diff against 86048ce and this checkpoint. Do not treat the checkpoint commit as a verified candidate. Consolidate local checkpoint history into verified commits before PR submission.
2. Synchronize the final source to a native Linux checkout. Existing task-only /home/syn/code/dione-pr-typed-messaging is detached at base with an older applied patch; it does NOT contain the final cleanup. Preserve/inspect that state before replacing it, or create a fresh native worktree from a Git bundle of this branch.
3. Use CARGO_TARGET_DIR=/home/syn/code/dione-vertical-native/target with one build owner. Run formatting, Clippy, and focused Nextest for Discord messaging, consent/queue, MCP messaging_wire, management/render, attention runtime, and contradictionary_flow.
4. Prove mixed partial/total/partial/success retry test sensitivity by removing accumulated IDs in an isolated checkout, observe the intended failure, restore, and rerun green. Mutation has NOT been performed.
5. Review typed errors/exhaustive matches, API visibility, unchanged wire/error strings, numeric versus string IDs, no retry-content leakage, DM route asymmetry, rewrite stage, consent diary/fence semantics, and complete caller migration. Audit unchanged debt honestly.
6. Independent six-lens review and cross-model trail review, then exact-commit scripts/pr-ready.sh against origin/refactor/vertical-slices-discord. Full release build is required. Only then push final candidate and open ready stacked PR; link it through T3. Do not merge.

Logs: /home/syn/code/dione-typed-check.log, /home/syn/code/dione-typed-clippy.log, /home/syn/code/dione-typed-focused.log. Last focused run was interrupted and is not a pass. Temp synchronization patch: C:/Users/Jaycee/AppData/Local/Temp/dione-typed.patch.
