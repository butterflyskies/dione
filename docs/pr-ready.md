# Check a Dione PR before submission

Run these steps from a clean native Linux worktree at the exact commit you plan
to submit. On WSL, keep the checkout under the Linux home directory rather
than using a Windows Git worktree under `/mnt/c`: Cargo can assemble a different
package from the latter. The application uses Unix APIs, so Windows Rust
cannot run this gate.

1. Fetch the current base with `git fetch origin main`.
2. Commit the proposed change. The gate rejects an uncommitted worktree so its
   result identifies one reproducible commit.
3. Run `sh scripts/pr-ready.sh origin/main`.
4. Review that commit with the group's pinned design principles and review
   lenses linked below. Resolve actionable findings before opening the PR.
5. Attach the commit ID and command result to the PR. Wait for CI at the same
   head commit, then obtain independent review and
   approval of the final commit.
   A member of `lacuna-blinkers` performs the final merge.

The command requires Rust 1.98, `cargo-nextest`, `cargo-deny`, and Python 3. It
fails if a tool is missing. It runs release hygiene, formatting, both Clippy feature
sets, both Nextest runs, the minimum Rust version check, the public package
boundary, review receipt tests, dependency policy, a release build, and a binary launch check. CI
remains the authoritative check because its PR base and build
environment may differ from the local worktree.

The Forgejo `main` branch rule must require each individual PR job, the
`Forgejo Linux CI / Required checks (pull_request)` aggregate, and the
`Dione review / Six lenses` receipt status once that trusted workflow is on
`main`. Any member of `lacuna-constructs`, `friends-of-lacuna`, or
`lacuna-blinkers` is an official reviewer and may review and approve a PR.
Require one approval from one of these teams, never from the PR author, and
allow only `lacuna-blinkers` to merge. The author of a PR cannot review or
approve it. Once others have reviewed and approved, any member of
`lacuna-blinkers` may merge, including the author or the reviewer.
Keep blocking on rejected reviews enabled. The six-lens receipt status checks
the official approval body for a complete receipt, but Forgejo lets repository
writers publish a status with the same context. The status alone cannot prove
who ran the check. Before merging, the blinker verifies the official approval
body, the trusted `main` workflow run, and the status at the PR head.
The aggregate rejects failed or skipped prerequisites, but its workflow
and script come from the PR checkout. It must not be the sole required status.
Keep the existing required statuses while adding the release build and receipt
statuses. Prove that a failed or skipped job, a changed aggregate script, and a
missing or stale receipt block disposable PRs before removing any older status.
Keep the legacy `Release Hygiene / Version + changelog accompany source changes`
workflow until its required branch-rule context has been retired.
The disposable PR must also prove that the workflow token can read official PR
reviews and publish a status, and that `main` advancing revokes a stale
receipt. Keep `block_on_outdated_branch` and stale-approval dismissal enabled.
Repository files cannot prove the server rule is enabled.

## Forgejo review status integration

Configure this before merging the receipt workflow. Forgejo does not apply
GitHub-style `permissions:` keys; [Forgejo v16 introduced Authorized Integrations][forgejo-v16]
as the alternative. The review job requests a short-lived JWT
for a Forgejo Actions (Local) Authorized Integration instead of using the
automatic workflow token for its API calls. Forgejo still supplies that
automatic token to the job; this change does not remove it from the runner.
The checker validates the public API URL and sends both its OIDC and API
requests through Forgejo's internal service with the public Host header.
The disposable PR must still prove that the runner can reach this service and
validate its TLS certificate.

1. Use a dedicated review-status account with access to `lacuna/dione`. Do not put this account in
   `lacuna-blinkers` or allow it to merge `main` or create protected release tags.
2. Under that account's Settings → Authorized Integrations, create a Forgejo
   Actions (Local) integration. Limit its source repository to `lacuna/dione`,
   workflow file to `review-receipt.yml`, and reference to `refs/heads/main`.
   Leave Event unselected: Forgejo 16.0.3 does not offer `issue_comment` in
   this form, and an empty selection permits a PR comment to refresh the
   receipt status after an approval review is submitted.
3. Select **Specific repositories**, add only `lacuna/dione`, and grant
   `read:issue` and `write:repository` API scopes. The job reads Forgejo's
   `official` PR review flag, which reflects whether an approval counts under
   the protected branch's `lacuna-constructs`, `friends-of-lacuna`, or
   `lacuna-blinkers` approval whitelist. It does not need access to the teams'
   member lists.
4. Copy the integration's generated Audience into the repository Actions
   variable `DIONE_REVIEW_AUDIENCE`. It is a public identifier. Check that no
   organization variable with the same name overrides it.
5. After the workflow reaches `main`, use a disposable PR to verify that a
   qualifying approval from each of the three teams with a receipt in its review
   body produces a success status after a PR comment refresh. Confirm that
   Forgejo refuses the PR author's own approval. An outsider's approval, a
   receipt in a PR comment instead of an official review, an official request
   for changes, and a missing or stale receipt must produce a failure status. Check that an
   unrelated workflow or reference cannot use this integration. Then add the
   status to the required branch rule. The source tests mock Forgejo's
   `official` flag; this live check must prove that the updated approval
   whitelist marks each eligible team's submitted review official. Also prove
   that a forged success status cannot substitute for an official approval or
   the blinker's live check of the trusted workflow run and review body.

The release-tag job uses a separate integration and account described in
[Forgejo release tag writer](release-tagger.md). Its audience is already a
different repository variable. Neither integration gives the review-status
account merge authority.

## Review before opening a PR

Consult the group's pinned [design principles][principles] and
[six review lenses][lenses] when reviewing the proposed commit. The principle
index identifies the rules relevant to the change. Keep review findings and
their evidence with the PR; the [risk levels](design/pr-quality-plan.md#risk-levels)
define which findings block merge. Review again if the head changes.

An independent member of any of the three eligible teams submits a Forgejo
**Approve** review with this receipt as its review body. The body is tied to
the official review, rather than to an issue comment another repository writer
could edit. Each lens needs specific evidence or a reason it does not apply;
the explanation after `pass -` or `n/a -` must have at least eight characters.
The `Head` and `Base` fields are the full commit IDs reviewed. Replace every
placeholder and paste the lines without the Markdown fence. Then post a PR
comment to refresh the status; the comment's text is not used as the receipt.

```text
DIONE-SIX-LENS-RECEIPT v1
Head: <40-character PR head SHA>
Base: <40-character base SHA>
Decision: approve
Safety: pass - <evidence or finding disposition>
Design: pass - <evidence or finding disposition>
Security: pass - <evidence or finding disposition>
Privacy: pass - <evidence or finding disposition>
Idiomacy: pass - <evidence or finding disposition>
Tests: pass - <evidence or finding disposition>
Unresolved P1-P3: none
```

Use `n/a - <specific reason>` for a lens with no applicable path. The workflow
on `main` reads the latest official approval review from each reviewer. At least
one current official approval is required, and every current official approver
must include a valid receipt in that approval body. A current-head official
request for changes fails the status even without a receipt; the branch rule
also blocks rejected reviews from earlier heads. A reviewer with a blocking finding must use
Forgejo's **Request changes** review action. On this Forgejo instance, COMMENT
reviews are not marked official, so their bodies do not control the receipt
status. One reviewer's later approval cannot erase another's unresolved
finding. Forgejo refuses a PR author's own approval. The workflow then writes
the `Dione review / Six lenses` commit status. It checks the receipt's structure
and commit IDs; it does not perform the six-lens review or judge the evidence.
A new head or base needs a new approval review with a new receipt body. Each
refresh first marks the current head pending; failed review reads leave that
required context pending instead of preserving an
earlier success. After writing success, the checker reads the PR and review
state again and retries a pending status if either changed during the write.
If Forgejo cannot issue a token, read the PR, or accept any status write, an
earlier success can remain visible. A failed receipt workflow on a `main` push
means open PR statuses need a fresh check before merge; the blinker must check
the live review state and workflow run. Forgejo's review-list total also counts
drafts hidden from the integration, so the checker cannot independently inspect
those omitted records. The status
refreshes when the PR or any PR comment changes and when
`main` advances. [Forgejo v16's review notifier][forgejo-review-notifier]
addresses the reviewed commit, and [workflow selection][forgejo-workflow-selection]
does not move this event to the default branch. This privileged status workflow
cannot safely use the review event
to refresh after a review alone. A later rejected review may leave the earlier
receipt status green until the next comment or push. The `main` branch rule's
**block rejected reviews** setting is the merge authority for that transition;
the merger must check the live review state. A reviewer can post a PR comment
after changing a review to refresh the status. Because `Base` is
the current tip of `main`, every merge makes
open PR receipts stale; obtain fresh reviews before the next merge. Keep
`block_on_outdated_branch` enabled so a base update
cannot be merged before the new status runs. After a reviewer leaves an eligible
team, dismiss that person's pending approvals and refresh affected PR statuses
with a new comment before another merge. The `lacuna-blinkers` merger confirms
the reviewer is still authorized and all required approvals and checks cover
the final commit.
This workflow cannot enforce the current PR until it lands on
`main`; [issue #479](https://forgejo.svc.echoes/lacuna/dione/issues/479)
tracks that rollout.

## Choose behavior tests from the change

Use this table when writing or reviewing the PR. Include both a normal case
and a failure case when the changed path can fail. Before counting a regression
test as coverage, temporarily break the protected behavior and verify the
targeted test fails for that reason. Restore the implementation afterward.

| If the change touches | Prove this behavior |
| --- | --- |
| Stored state or config | Load data written by the prior release; preserve meaning after a write and restart; reject corrupt data without silent loss. |
| Startup or shutdown | Reach readiness or return a clear error within a deadline; stop owned tasks and release resources. |
| Queue or concurrency | Preserve ordering and ownership across restart; retry partial commits without loss or duplicate delivery; test competing actors. |
| External delivery | Use a fake peer to test success, timeout, rejection, retry, and ambiguous acknowledgment. |
| Permissions or identity | Accept the intended principal and reject a nearby unauthorized principal at the public boundary. |
| Wire format or public API | Check the exact serialized shape or a consumer build, including the previous supported format. |
| Packaging or release | Check the package contents, version and changelog, and a reversible deployment plan. |

For every PR, inspect the diff's callers and data writers. Record any P1, P2,
or P3 finding with a reproducible failure path. Fix actionable findings and
review the final commit again. A green command alone does not close a review
finding.

## Startup release drill

When a change touches startup, the Codex inbox, or live delivery, run this
before deployment with a dedicated test bot and Codex thread. Put a copy of
prior-release state in an isolated `DIONE_STATE_DIR`; never point the drill at
the running bot's state. Start the candidate binary in Codex mode with that
state directory and the test app-server socket and thread binding.

Within a fixed 30-second window, observe the `Discord gateway ready` log and
send one new test message through the configured test channel. Confirm the
message reaches the Codex thread and the old inbox entries remain readable or
are consumed only by an intentional pull/acknowledgement. If readiness or
delivery misses the deadline, treat the drill as failed and retain the state
copy and logs for diagnosis. Stop the process with SIGTERM and reopen the state
before considering deployment. Record the candidate commit, prior-release
fixture/version, timings, and observed message IDs with the release evidence.

The automated historical-inbox tests cover the loader and local delivery
worker. This drill covers the Discord gateway and real Codex process boundary
that those tests do not start.

[principles]: https://forgejo.svc.echoes/lacuna/lacuna-marketplace/src/commit/d999379d1e62d6dfc9ff24086149503f51313bac/plugins/lacuna/design-principles/v2/INDEX.md
[lenses]: https://forgejo.svc.echoes/lacuna/lacuna-marketplace/src/commit/d999379d1e62d6dfc9ff24086149503f51313bac/plugins/lacuna-core/skills/elbow-grease/references/lenses.md
[forgejo-v16]: https://forgejo.org/2026-07-release-v16-0/
[forgejo-review-notifier]: https://codeberg.org/forgejo/forgejo/src/tag/v16.0.3/services/actions/notifier.go
[forgejo-workflow-selection]: https://codeberg.org/forgejo/forgejo/src/tag/v16.0.3/modules/actions/github.go
