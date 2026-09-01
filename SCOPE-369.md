# SCOPE-369 — Identity-level ignore (per-user ignore list + stateless reply filtering)

Reviewer note for issue #369. Pace authorized the scope; the ceiling is FIRM.

## What this delivers

A user can ignore a specific person (e.g. an abuser) so that person's content is
filtered everywhere — every channel and DM, a message of any age, restart-proof.

Pace's victim-test: *"if I reply to an ignored person's message from last year, it
should still filter — any age, any channel, restart-proof."*

### Piece 1 — config primitive (mirrors `allow_from`)
- `access.ignore_from: Vec<String>` on `AccessConfig` (+ `Default`). Rides the
  existing ConfigRuntime/#386 reload path exactly like `allow_from` — no new
  config machinery.
- Parsed once per load into `LoadedConfig.ignored_ids: HashSet<u64>` via the
  existing `parse_id_set`, mirroring `allowed_ids`.
- `LoadedConfig::is_ignored(user_id) -> bool` — O(1), mirrors `is_allowed`.
- This is a GLOBAL (identity-level) ignore list, not per-channel.

### Piece 2 — stateless ignore enforcement (v2)

**v2 semantics (Pace-ratified 2026-09-01, supersedes the v1 note below):** only
an ignored **author** drops the whole message. A **reply whose parent author is
ignored** is now **ADMITTED with the quoted preview redacted**
(`reply_to_content_preview = None`) — not dropped. The preview is the sole
content-leak vector a reply carries from the ignored person; stripping it makes
fail-open safe.

- Author-level drop is enforced on ALL SIX ingress paths: DM create, DM edit,
  guild create, guild EDIT (`passive_edit_policy_allows`), and webhook/PK
  create + edit (`admit_verified_{create,edit}_after_wait`, on the resolved
  **principal** — `effective_user_id`, not the transport id). The guild-edit and
  both webhook paths were the three confirmed P1 bypasses; they now run the
  check.
- The reply-parent author is resolved by a **3-tier ladder** in
  `src/discord/events.rs` (`resolve_reply_parent_ignore`): tier1 inlined
  `referenced_message`, tier2 ingress-ledger active snapshot (≤7d, survives
  Discord deletion), tier3 one **bounded** live fetch wrapped in a
  `tokio::time::timeout`, on the reference's OWN `channel_id`. A short-circuit
  returns immediately (no fetch, no ledger lookup) when `ignored_ids` is empty.
- The fail policy is a **parameter** (`classify_reply_parent_ignore(resolution,
  fail_closed)`) with a const-passing wrapper; both directions are unit-tested.
- **Blocklist semantics:** `ignore_from` overrides `allow_from`. An ignored
  author is dropped even if they are also allow-listed.
- **Out of scope (#369):** reactions from an ignored user are NOT filtered —
  documented gap, tracked separately.

The v1 description below is retained for history; where it says a reply to an
ignored parent is *dropped*, read *admitted with preview redacted*.

## Statelessness (the core invariant)

Every ignore decision reads ONLY the current config snapshot — never the
#361/#362 drop-event ledger. Consequences:
- Works across restarts and for a referenced parent of any age.
- A config reload that adds/removes an ID takes effect on the very next message.
- The #361/#362 ledger is untouched and stays correct for its own (non-identity)
  cases. It is a cache, NEVER the authority for identity ignore.

To keep the ledger from silently becoming that authority, identity-ignore drops
in the guild path resolve to a dedicated `DirectGuildAdmission::IdentityIgnored`
variant that is deliberately **NOT** recorded as a #361 reply-inheritance root —
otherwise a stale ledger entry would survive an un-ignore. (Test:
`identity_ignore_drops_without_polluting_the_ledger`.)

## FLAGGED DECISION for Pace/Lain — fail-open vs fail-closed

When a message IS a reply but the parent author cannot be determined — the
gateway did not inline `referenced_message` AND the single bounded API fallback
also failed (e.g. parent deleted / not fetchable) — identity-ignore must choose:

- **fail OPEN** (v2: admit the reply but **redact the preview**) — favors
  delivering legitimate mail; the preview is stripped so an unresolvable parent
  that *might* have been the ignored person cannot leak content.
- **fail CLOSED** (drop the reply) — favors victim protection; also drops
  legitimate replies whose parent was merely deleted / un-fetchable.

This is encoded as a single constant, `IGNORE_REPLY_PARENT_FAIL_CLOSED` in
`src/gate.rs`, with a `TODO(#369)`. **Shipped provisional default = fail-OPEN
(`false`).** Flip the one constant to change direction; behavior is unit-tested
either way (`classify_unresolved_reply_hits_flagged_path`,
`flagged_default_is_fail_open`).

**Recommendation:** genuinely open — Pace/Lain to decide. The primary victim-test
(year-old reply) is already covered by the reliable inlined-parent path, so this
constant only governs a narrow residual case. Security posture (protecting an
abuse victim) leans fail-CLOSED; the cost is dropping legitimate replies to a
deleted parent. I defaulted to fail-OPEN to preserve the existing "admit unless a
filter matches" behavior and avoid dropping legit messages, but did not treat the
question as settled.

## Ceiling honored
- NO new config machinery — `ignore_from` is just a `Vec<String>` on the existing
  path, exactly like `allow_from`.
- NO ledger-as-authority, NO persistence/history layer, NO new stores.
- Scope = the config field + `is_ignored` + the gate checks (author + reply-parent)
  + the minimal threading needed to feed the parent author into the gate.

## Files touched
- `src/config.rs` — `ignore_from`, `ignored_ids`, `is_ignored` (+ tests).
- `src/gate.rs` — `identity_ignored`, the fail-policy constant/enum/classifier,
  ignore checks in `check_dm`/`check_guild` (+ tests).
- `src/discord/events.rs` — thread reply-parent author into the gate calls,
  bounded API fallback, `IdentityIgnored` admission variant (ledger-bypass) (+ test).
