# Scoped scheduler

The scheduler now keys queues and immediate-delivery budgets by complete provider, configured module slot, and conversation identity. Numeric and opaque local identities remain distinct. Discord owns event classification and passes retained notifications through the neutral scheduler. Configured delays cross the adapter boundary as Duration.

Production scheduler callers and integration tests use the new API. NotificationEvent no longer uses non_exhaustive; affected matches are exhaustive. This breaks the old public DeliveryBuffer constructor/event API and intentionally requires a minor release while the package is pre-1.0.

Review corrected unchecked deadline arithmetic: a new unrepresentable deadline returns a typed failure and retained payload. Existing queues preserve their deadline; unused delay does not reject control or immediate events. Ordinary forwarding retains rejected data for the existing shutdown path. The pre-fix test failed with the literal Tokio overflow panic; its source snapshot and hash are preserved in the local handoff.

ProviderId and ModuleSlotId are distinct types. A Nextest-discovered trybuild negative fixture rejects their interchange, paired with a valid-construction fixture.

Independent six-lens source reviews found no remaining introduced issue after the correction. Linux final evidence and signed-head gate will be recorded in the PR. Local agent reviews are not official Forgejo approval.

The known preexisting forward_ordinary retry-order bug remains assigned to an immediate, separate behavior fix. Typed live delivery, shared authority contracts, platform ownership, and library documentation enforcement remain later units. This scheduler extraction does not establish complete platform independence or Codex atomic idle delivery.
Native feature Clippy and all 117 focused tests passed before the version-only bump to 0.64.0. The generated negative fixture was inspected: E0308 names the exchanged ProviderId and ModuleSlotId arguments; its valid companion passed. Logs: /home/syn/code/dione-core-r2-clippy.log and /home/syn/code/dione-core-r2-focused.log. The scope mutation, normal-mode fixture rerun, and signed-head full gate remain required before publication.

The first signed-head full gate exposed a trybuild harness race: two integration binaries rewrote the same generated crate, causing the identity suite to run a verified-action fixture. The correction registers all17 existing verified-action negatives and both identity fixtures in one contracts_compile_fail runner. No production change was needed. Independent correction review was clean, and the combined runner passed normally (without overwrite) in /home/syn/code/dione-core-runner-focused-r2.log. The unpublished commit was amended; its full gate must run again.
