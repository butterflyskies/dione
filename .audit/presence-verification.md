# Typed presence verification record

This slice targets `refactor/discord-typed-messaging` at
`8a55c251d480737cd7156ae9cedaae8d310855b8`, the head of PR #494.
Version 0.53.0 deliberately changes the public Rust contract while preserving
admitted MCP inputs and outputs.

## Design and ownership

Three independent designs compared a combined bot-state owner, a dedicated
presence owner, and an opaque service with gateway-owned state. The independent
judge scored them 23, 24, and 21 out of 25. The dedicated owner removes broad
context forwarding and keeps requests, snapshots, replay, and queues together.
Private request fields and MCP-owned raw argument parsing were grafted onto it.
The judge preferred concrete HTTP fixtures on architectural grounds. The parent
retained the minimal private typing trait because the project standards require
trait-based service mocks.

One implementation owner worked in an isolated checkout. The parent alone ran
native Linux builds. The private typing transport trait follows the coding
standards' explicit requirement for trait-based external-service mocks. Domain
presence contains neither Serenity nor JSON. Public API migration and remaining
debt are recorded in the changelog and `docs/design/vertical-slices.md`.

## Behavior evidence

| Check | Evidence | Result |
| --- | --- | --- |
| Existing baseline | Nextest `0e0912df-2cf3-4e0e-bcb7-9add66bfd87d` | 12 passed. |
| Added characterization before production edits | Nextest `64e96f33-6f4f-4e45-a196-8326f296ba9f` | 12 existing tests plus 3 new characterization tests passed. |
| Literal fixture comparison | `C:/Users/Jaycee/AppData/Local/Temp/dione-presence-design/compare_wire_fixtures.py` | All 49 JSON expressions in the 3 new tests match the baseline byte for byte. |
| Final focused suite | Nextest `f6e7a2d0-72f5-4048-8a91-46f1a4b42595` | 17 passed. |
| Remove reconnect replay | Nextest `79ad3a74-7d3d-4e78-a8a0-052006fbe94d` | Failed the intended empty replacement-sink assertion. |
| Restore reconnect replay | Nextest `0e6d62b5-2f25-4c89-8cde-d02bc65497bd` | Passed; source restored. |

The first compile attempts exposed ambiguous empty-vector equality and two
unnecessary test clones. The first focused run passed 16 tests and failed the
new SDK-source assertion. That assertion expected an unboxed error, while
thiserror retained the boxed SDK error. The corrected test checks the exact
boxed variant and message. Production error handling did not change.

The baseline filter was `test(presence) | test(send_typing) | test(bot_state)`.
The final focused filter was `test(presence) | test(typing)`. Existing bot-state
snapshot and validation coverage moved into MCP dispatch; authority and queue
coverage moved from events into presence. The preexisting round-trip dispatch
test migrated its construction and typed snapshot assertion. Only the three new
characterization tests received the byte comparison.

The initial Clippy log is partial because a queued retry reused its output
file. `clippy-final.log` is the clean successful run before the final test-only
error-source correction. Final lint evidence must come from the submission gate.

Logs are under `/home/syn/code/` with the `dione-presence-` prefix. The native
verification checkout is `/home/syn/code/dione-pr-presence`.

## Independent review

Safety, Tests, Design, Idiomacy, Security, and Privacy reviews returned no
actionable source findings. A fresh review accepted the corrected error-source
test. The comment review removed 15 narrative lines, retained public contracts,
and found no required code reshaping or suppressions. No comments were restored.

Three separate GPT-6-Astra agents each reviewed two lenses. A fourth fresh
GPT-6-Astra agent reviewed the test correction. They received the contract brief
and inspected source independently. These are independent agents on the same
model, not six separate reviewers or six model families. The trail review used
Claude Sonnet 5.5. The committed [review summary](presence-reviews.md) records
their findings and limits.

Detailed review reports and the design comparison are retained locally in
`C:/Users/Jaycee/AppData/Local/Temp/dione-presence-design/`. The optional queue
consumer keeps its existing detached lifecycle; production now creates no such
consumer. Future queue-only integration needs an explicit shutdown owner.

The source review confirmed the unchanged effects mutex and typing lock release.
Tests do not directly prove that a second writer cannot overtake a blocked sink
callback or that a typing transport can acquire the state write lock during I/O.
Those optional concurrency checks remain accepted coverage gaps. The deliberate
mutation proves reconnect replay only.

## Submission gate

The full `scripts/pr-ready.sh origin/refactor/discord-typed-messaging` gate must
pass from a clean native Linux checkout of the submitted commit. The PR body
records that commit and final result, which cannot be embedded in the commit it
identifies. Independent agent review does not replace official Forgejo approval.
This task authorizes a ready PR, not merge or deployment.

The verification worktree was created for this task. The gate rejects tracked or
untracked changes before starting. Builds share a target cache but run with one
build owner; this is a clean-worktree check, not a cold-cache build.
