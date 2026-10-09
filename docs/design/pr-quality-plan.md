# PR quality plan

This plan covers Dione changes proposed against `main`. A passing test suite
means the checked behaviors passed at one commit. It does not prove that no
defects exist.

## Definition of done

For each proposed PR head:

1. The Linux format, both Clippy feature sets, both Nextest feature sets,
   package boundary, minimum Rust version, release binary build,
   dependency audit, and release hygiene checks pass at that exact commit. The
   aggregate Forgejo status must depend on all of them. The branch rule must
   also require the individual statuses and the trusted six-lens receipt
   status once its workflow is on `main`.
2. Every changed behavior has a test at its observable boundary. Changes to
   persisted formats also prove that representative data written by the prior
   release loads and retains its meaning after restart. Startup and background
   tasks have bounded completion or readiness checks. A test must fail for a
   concrete broken version of the behavior it claims to protect.
3. The PR review records zero unresolved, actionable P1, P2, or P3 findings.
   Before opening the PR, consult the group's
   [design principles and review lenses](../pr-ready.md#review-before-opening-a-pr).
   Any member of `lacuna-constructs`, `friends-of-lacuna`, or
   `lacuna-blinkers` is an official reviewer and may review and approve the
   final commit; the PR author cannot review or approve their own work. A member
   of `lacuna-blinkers` performs the final merge after checking the required
   approvals and automated results.
4. The PR description names the changed behavior, its failure modes, the tests
   that prove them, and any deployment or rollback condition.

## Risk levels

| Level | Meaning | Merge rule |
| --- | --- | --- |
| P1 | Data loss, unauthorized access, or a service-wide outage | Block |
| P2 | A supported workflow fails or can silently duplicate or drop work | Block |
| P3 | A narrower correctness, recovery, or maintainability defect with a concrete failure path | Resolve or document a reason it is not actionable |

Style preferences without a failure path are review suggestions, not defects.
Zero findings means zero known actionable findings in the reviewed scope. It is
not a claim about undiscovered defects elsewhere in the repository.

## Work units

1. Capture the baseline on unmodified `main`. Record the exact commit and each
   command result. Compare the local command list with the Forgejo and GitHub
   workflows.
2. Add a change-impact checklist that maps a diff to the tests it needs:
   old persisted data, startup, concurrency, permissions, external delivery,
   retry and replay, malformed input, and shutdown. The inbox incident is one
   example, not a special rule for every PR.
3. Add one documented local PR gate that runs the same relevant checks as CI.
   A missing tool or failing check must fail the gate rather than skip silently.
4. Review the final diff and its callers, data writers, recovery paths, and
   external boundaries. Record each P1 through P3 candidate with a
   reproduction or dismiss it with evidence. Recheck the exact final commit
   after fixes.

The first falsifiability sweep and its remaining gaps are recorded in
[test-oracle-audit.md](test-oracle-audit.md).

## Later coverage work

Fix the existing rustdoc private-link errors on `main`, then add a warning-free
documentation build to CI. The first historical inbox fixture comes from
`v0.48.0`; extend that pattern to each durable state format. Add a fixture
before changing a format, then test old data, partial writes, restart, and
recovery. Exercise external delivery with local fake services so tests do
not need production tokens. Keep a small manual release drill for deployment,
state snapshots, rollback boundaries, and a live canary. Review any incident
against the change-impact checklist and add the missing automated case.
