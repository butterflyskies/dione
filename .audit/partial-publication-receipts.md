# Partial publication receipt verification

The MCP projection previously discarded acknowledged message IDs from partial
Discord reply and DM outcomes, and from failed consent release/rephrase attempts.
The domain already retained those IDs. The projection now adds
`partially_delivered: true` and ordered numeric `message_ids` while preserving
existing error, rewrite, live-handle, and expiry fields. The prefix describes the
current attempt only. No destination, unacknowledged ID, or retry policy is inferred.
Complete, held, and zero-progress failure shapes remain unchanged.

The source-only change is based on `a6c50ab356564de8220624f0b593c991d0073b56`.
The publication candidate applies that same projector change to accepted core
`43d2e5bdadc4c8f6fdf664dbe69f4902829536a6`, where the original projector is
unchanged. This secure composition adds the reviewed TLS prerequisite and uses
version 0.67.2. The earlier 0.67.1 candidate is retained as historical evidence.
The new signed composition still requires its complete verification gate. Historical candidate
`117153b7b95c21072ad28f5c92ef069cd57a0dee` remains intact. The independent
Discord ownership sibling moves this same projector; reconcile the committed
changes through Git when combining those units, rather than replacing the moved
file with a copy.

## Verification evidence

The native checkout is `/home/syn/code/dione-partial-receipts`, using its own
`target` directory. The baseline applied only the new and updated tests to the
exact a6c50ab production source. Its ten projector tests produced four intended
assertion failures for missing partial fields and six passing controls. The
test-only artifact's production prefix was checked against the original before
the run. This baseline does not rely on the earlier shared build directory.

The corrected candidate passed feature-enabled workspace/all-target Clippy with
warnings denied. Its focused projector and MCP protocol Nextest run passed all 85 selected tests. Native formatting was copied back for the one source file.
The version, changelog, and audit files were prepared afterward and have not yet
been verified as part of a signed candidate.

Literal projector regressions cover ordered partial IDs, the maximum u64 ID,
rewrite retention, direct/redirected complete DM results, complete and held
replies, outbound denial, and partial versus zero-progress consent failures.
An independent six-lens source review found no actionable findings. That review
does not substitute for official Forgejo approval or executed verification.

Local evidence files:

- `/home/syn/code/dione-partial-receipts-baseline.log`
- `/home/syn/code/dione-partial-receipts-clippy.log`
- `/home/syn/code/dione-partial-receipts-focused.log`

Complete required checks on the clean signed head and PR publication remain pending. No full-gate or completion claim is made here.
