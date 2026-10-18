# Typed Teams reply handles

ReplyHandle validates the historical teams-prefix and UUID syntax without normalizing its spelling. Construction and deserialization preserve that invariant, including persisted map keys. Parsing identifies a possible handle; runtime lookup and authenticated stored authority still decide whether a reply is allowed. The actual MCP, authority, bridge, memory/durable storage, resident delivery, and host projection callers migrate together.

Literal v1 storage fixtures preserve exact prior spelling and one-use behavior through reload. Malformed caller strings fail before authority invocation; malformed stored keys fail before authority is exposed. Existing duplicate, cancellation, pre-send retry, and committed uncertainty behavior remains tested.

Native feature Clippy,109MCP/Teams tests, and all4harness session tests passed. Removing handle validation made the malformed durable-key regression fail; restoring it passed. Logs: /home/syn/code/dione-teams-handles-clippy.log, dione-teams-handles-focused.log, dione-teams-handles-session.log, dione-teams-handles-mutation.log, dione-teams-handles-restored.log. Checks precede the version-only bump to0.66.0.

Independent six-lens source review found two missed test callers, corrected with valid typed handles and preserved socket/shutdown/rebinding assertions. Independent correction review closed them. The comment pass found no issue. Local reviews are not official Forgejo approval; signed-head full gate belongs in the PR.

Tenant/bot/actor/conversation/activity identities, dedup keys, receipts, and inherited runtime error chains remain separate consumed vertical units. This change does not claim complete neutral platform ownership.