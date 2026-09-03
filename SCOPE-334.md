# SCOPE-334 — Phantom canary must not flag proven own-authored targets

**Issue:** [dione#334](https://forgejo.svc.echoes/lacuna/dione/issues/334) — "Phantom canary false-positives on own-authored targets across operations (reply/react/delete) — centralize the exemption in the detector."
**Owner:** Lain. **Priority:** lowest (queued; yields to any runnable higher-priority work). **Status:** scoped, not started.
**Scoped:** S219, 2026-09-02, off the confirmed-phantom post-mortem (see discriminator on the issue, comment 5972).

## Problem

When this seat replies to / reacts to / deletes a message **it itself authored**, the target is (correctly) absent from the *received* ingress ledger — that ledger's domain is received messages, not our own egress. The canary reads that expected absence as `VerifyResult::Unknown` and fires a phantom alert on our own hand. 10+ reproductions logged on the issue; each is the acting seat tripping over its own outbound message.

## The discriminator (two branches — only one is this bug)

"Could not evaluate" (`VerifyResult::Unknown`) splits two ways:

- **known-mine → NOT suspicious** (this bug): the acting seat authored the target. Absence from the received ledger is expected. Answer to "who authored this?" is a positive self-ID; no external oracle needed. **Exempt — do not alert.**
- **unknown → quarantine** (correct existing behavior, must be preserved): externally-attributed, no claimant, the named author's own canonical hand disavows it (tonight's confirmed phantom). **Keep alerting/quarantining.**

**Ari's guard (load-bearing, do not skip):** "known-mine" MUST key on the seat's **authenticated self-emission**, never a payload merely claiming our name — else a spoof self-declares the exemption and walks in wearing our identity (exactly tonight's phantom, which claimed Ari with no receipt). The exemption keys on *proof we sent it*, not on the target's asserted author field.

## Grounded code locations (dione @ main `618aabc`)

- **Fix site:** `src/mcp/tools/messaging.rs:93–102` — the `match ledger.verify(message_id, channel_id) { VerifyResult::Unknown => { … if let Some(alert_ch) = config.phantom_canary_channel { fire } } }` arm. This is where reply/react-target verification turns `Unknown` into an alert. The own-authorship exemption is inserted here (or centralized just below, per the issue title's "centralize the exemption in the detector").
- **Alert fn:** `src/mcp/tools/messaging.rs:44` `phantom_canary_alert(...)`.
- **Ledger + verify:** `src/ingress_ledger.rs:549` `pub fn verify(message_id, claimed_channel) -> VerifyResult`; `VerifyResult::Unknown` variant at line ~112. The ledger already tracks actor lineage (`StoredLineage`, `same_actor_lineage` @ line 53) — candidate substrate for the exemption.
- **Authenticated authorship source:** `src/discord/verified_action_runtime.rs` — `verify_observed_create/update(event, creator_user_id)` supplies the *verified* creator identity. This is the intended source for Ari's guard, NOT the raw claimed-author field.
- **Config:** `src/config.rs:130` `PhantomCanaryConfig`; parsed `phantom_canary_channel` @ ~1015.
- Other `verify()` consumers to audit for the same exemption (per issue title "across operations"): `src/discord/events.rs:1030` (edits), `:3647` (delivery), `:425` (delete transition). The centralized exemption should cover reply/react/delete uniformly.

## Test plan (non-vacuous — the whole point, per S219's g189–g194 lesson)

Tests must **fail if the guard is removed** (mutation-checked), not merely pass on the happy path:
1. **known-mine exemption fires:** seat authored `M`; seat reacts/replies to `M`; assert **no** phantom alert. Mutation: delete the exemption → this test must go red.
2. **spoof does NOT borrow the exemption (Ari's guard):** an inbound claims our user_id as author but carries no authenticated self-emission receipt; assert alert **still fires / quarantines**. Mutation: weaken the check to trust the claimed-author field → this test must go red. (This is the security-critical one — it's the exact shape of tonight's true-positive phantom.)
3. **genuine unknown still quarantines:** externally-attributed, no claimant → alert fires (unchanged).
4. No vacuous-skip: if the authenticated-authorship signal is unavailable, fail **closed** (treat as not-exempt / alert), never silently exempt.

## Rollback

Single-arm change at the fix site + its exemption helper; revert restores current always-alert-on-Unknown behavior. No schema/wire change expected.

## OPEN QUESTIONS (resolve at implementation, from source — do not invent)

1. **What is the authenticated own-authorship signal actually available at `messaging.rs:93`?** Options to verify: (a) an own-egress record the seat writes when it sends (does one exist? the ledger is received-only today), (b) the verified creator identity via `verified_action_runtime` matched against the bot's own `UserId`, (c) the send path already holds the `message_id` it just created. Pick the one that is *authenticated*, not claimed. This determines whether the fix needs a new own-egress set on `IngressLedger` or can reuse existing verified identity.
2. Confirm the "across operations (reply/react/delete)" surfaces all route through this one arm, or whether the exemption must be centralized in the ledger (`verify`) instead of the messaging call site so delete/edit paths inherit it.

## Process

Draft PR (Pace holds draft→ready for vaelii; dione convention: confirm with 🦋/maintainer). `bsky:multimodel-elbow-grease` before ready. Non-vacuous tests are the acceptance bar, not green-count.

## Known residual (multimodel review, S220) — bounded, fail-safe

The exemption reads `SharedState.recent_sent_ids`, an in-memory set capped at
`SENT_IDS_CAP = 200` and empty after a process restart. Two tail cases therefore
still trip the canary on an own message:

1. **>200 sends** between sending a message and acting on it (busy multi-channel bot) — the id is evicted.
2. **Process restart** (e.g. deploy) — the set is empty until the message is re-noted (a reaction-target `get_message` re-populates it).

Both are **fail-safe**: they yield a false phantom *alert + block* on our own hand, never admit a spoof (Ari's guard is unaffected — a spoof still has no authenticated own-send record). They bound rather than fully eliminate #334 for old own-messages. The common/deterministic reproductions (send → act within the recency window) are fixed.

**Follow-up to fully eliminate the residual (not in this PR):** on `Unknown && !own_send`, before alerting, fetch the target and exempt iff `author.id == bot_id` (then `note_sent` it) — the same authenticated pattern `events.rs` already uses for reaction targets, with the HTTP cost paid only on the rare would-alert path. This makes `verify_message_target` async. Left as a scoped follow-up because it grows a lowest-priority fix; the reviewer/maintainer can pull it forward.
