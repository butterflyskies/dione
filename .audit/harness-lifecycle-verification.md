# R5 harness lifecycle verification

## Scope and ownership

Base is PR #510 at `7399a302943a911ff4b8fe1b4569019e843d116f`.
R4's exact-head gate passed 1648 feature tests, four default oneshot tests,
and 34 Python tests, with one opt-in live test skipped. Its CLI help comparison
also passed. R5 moves existing lifecycle ownership without rewriting the
Codex reconnect engine, queue format, or MCP contracts.

The selected design is B, a concrete prepared session consumed into a running
session. Parent comparison scored A/B/C as 22/24/21; the separate judge scored
22/23/20 without receiving parent scores. The parent saved its scores after
the judge summary was visible, so these are not two independent rankings. The running owner retains the complete
connection, incorporating A's lock-lifetime constraint. The design rejects an
additional lifecycle trait and a worker-file relocation. Artifacts are retained
under `C:/Users/Jaycee/code/dione/.git/codex-handoffs/dione-r4-r7/r5-*`.

Preparation opens one queue and allocates matching binding endpoints. MCP,
Teams delivery, and the worker use clones of that owner. Startup requires the
attention guard, resolves socket and thread configuration, persists the initial
binding, publishes it, and starts the existing worker. The running owner keeps
the connection alive through a worker failure and until shutdown finishes.

Delivery cancellation is independent of process cancellation. Main preserves
Teams restoration before worker startup, listener startup afterward, and
accepted request and reply drains before delivery cancellation and join.
Consuming start and shutdown prevent duplicate transitions through this API.
They do not encode the entire application's listener ordering or provide async
drop cleanup. That ordering remains explicit in main for R6 composition work.

## Work separation

R5 design and isolated implementation overlapped R4's slow release build.
The primary R4 branch and native verification checkout remained fixed. R4 was
published before integrating R5. The writer owns a separate Windows worktree;
the parent owns native Cargo runs. Native source snapshots stay immutable while
builds run. Publication remains sequential.

## Review

Two read-only reviewers covered Safety/Security/Privacy and
Design/Idiomacy/Tests, respectively. No production regression was found.
The second reviewer requested documentation for public error variants and a
source-chain assertion that checks the actual inbox-lock failure instead of a
fixed hop count. The writer corrected both. The comment review recommended
zero deletions and reported no mandatory removals.

The new socket tests use an accepting attention guard and ordinary events.
They do not independently prove forwarding of attention guard checks or
receipts. Source inspection confirms the required guard assignment; existing
worker tests cover the engine's attention behavior. Configuration and initial
binding failure precedence are preserved in source order but are not separately
fault-injected by the new lifecycle tests.

## Focused verification

Run the lifecycle tests, existing socket/reconnect tests, and R4 protocol
characterization. Prove that replacing the independent delivery token with a
child of process cancellation fails the intended delivery assertion. Restore
source bytes, run the lexical boundary checker, compare CLI help, and run the
full exact-head gate against the base above before publication.

The boundary checker adds specific lifecycle ownership names for main while
allowing the CLI thread-ID type. It is a lexical regression check, not Rust name
resolution or proof against all possible aliases. The offline attention CLI
still has its separate selection-to-policy mapping for R6.

No live service, deployment, merge, or construct test is claimed. R7 still
requires external Oh My Pi source/docs inspection before implementation.

The focused native run passed 51 tests with 1603 excluded by the filter.
Feature Clippy passed with warnings denied. The first run's fixture failure and
its correction are recorded in [the review record](harness-lifecycle-reviews.md).
The cancellation baseline passed one test. Replacing the independent token with
`process_shutdown.child_token()` failed the intended five-second delivery
assertion. The driver restored exact source bytes with SHA256
`0e64935fa183055f5bbdd01f04d37899e6be8d0627bcbc1f67bfa02985a3bde4`.

Evidence lives in `/home/syn/code/dione-r5-focused.log`,
`/home/syn/code/dione-r5-checkpoint.log`,
`/home/syn/code/dione-r5-cancel-baseline.log`, and
`/home/syn/code/dione-r5-cancel-mutant.log`. The rerunnable mutation driver is
retained beside the design artifacts as `mutate-session-cancel.py`.
The lexical boundary checker passed on six shared files and main's named
lifecycle ownership tokens. Full exact-commit gate and CLI comparison are still
required before PR publication; this document does not claim they passed.
The cross-model audit reproduced the boundary checker. Its output is retained
outside the repository as `r5-boundary.log` beside the design artifacts.
The focused native checkout also had unrelated `cargo xfmt` import regroupings;
these were not copied into the PR. The final gate script preserves that snapshot
with a stash and checks out the signed bundle into a clean native branch.

The first fixture-failure log was overwritten by the corrected run. Its failure
is retained in this thread at transcript position 4679 and summarized in the
review record. The cancellation mutant uses a generic receive deadline; the log
does not label which post-cancellation receive failed. The mutation changes only
the token's cancellation relationship. No repeated flake study or lock-lifetime
mutation is claimed. The 50 ms no-connect checks are bounded observations.

The trail audit's duplicated-context finding was reproduced by a new assertion
on the capability error's source chain. It failed before the correction and
passed after making the outer session error transparent. The same test checks
that queue-open context occurs once and retains the concrete lock-conflict source.
Logs are `/home/syn/code/dione-r5-error-before.log` and
`/home/syn/code/dione-r5-error-after.log`. The focused lifecycle run predates this
small diagnostic correction; the final gate must cover the corrected source.

## Full-gate scheduling correction

The first gate at `9b5c41c` passed 1652 feature tests but failed the existing
`uncertain_deferral_keeps_attention_backoff_after_token_reconciliation` test.
It observed 1.439 seconds remaining against a greater-than-1.5-second threshold.
The unchanged queue writes a two-second deadline before persistence and the test
subtracts its later observation time. The same test passed in isolation.

A read-only reviewer confirmed the production worker and queue are unchanged
from R4 and retain the doubled delay through uncertainty reconciliation.
The observed remaining value is consistent with late observation of the correct
delay. Contention is inferred; storage latency or clock movement could also
consume the margin. The exact test now joins the existing nextest override that
reserves all runner slots for two other timing-sensitive worker tests. Production
code and the assertion are unchanged. This reduces suite contention but does not
promise immunity to external load. The full gate must pass on the new commit.

The failed gate log is retained as `/home/syn/code/dione-r5-pr-ready.log`.
The isolated check is `/home/syn/code/dione-r5-backoff-isolated.log`.
