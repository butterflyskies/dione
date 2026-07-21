<!-- design-meta
status: draft
last-updated: 2026-05-17
phase: 2
-->

# Requirements

## Use Cases

### Actors

| Actor | Description |
|-------|-------------|
| Discord User | Someone in `allow_from` or an opted-in guild channel |
| Admin | Someone in `admins` list; receives permission requests and access requests |
| Unknown Sender | Not yet approved; message goes into access request queue |
| Claude | The AI agent (via Claude Code) calling MCP tools |
| Attacker | Malicious actor attempting injection, exfiltration, or abuse |

### Use Case Table

| ID | Actor | Use Case | Type | Priority |
|----|-------|----------|------|----------|
| UC-01 | Discord User | Send a DM that reaches Claude | Normal | Must |
| UC-02 | Discord User | Send a message in an opted-in guild channel via @mention | Normal | Must |
| UC-03 | Discord User | Send attachments (images, files) for Claude to inspect | Normal | Must |
| UC-04 | Unknown Sender | DM the bot; message queued for admin review | Normal | Must |
| UC-05 | Admin | Review and approve/deny access requests from the queue | Normal | Must |
| UC-06 | Admin | Receive and respond to permission requests via Discord buttons | Normal | Must |
| UC-07 | Admin | Configure access policy without restarting the bot | Normal | Must |
| UC-08 | Claude | Reply to a Discord message (with chunking, threading, attachments) | Normal | Must |
| UC-09 | Claude | React to a Discord message with emoji | Normal | Should |
| UC-10 | Claude | Edit a previously sent message (progress updates) | Normal | Should |
| UC-11 | Claude | Fetch message history from a channel | Normal | Must |
| UC-12 | Claude | Download attachments from a message | Normal | Must |
| UC-13 | Claude | List guilds the bot is in | Normal | Must |
| UC-14 | Claude | List channels in a guild (with types and metadata) | Normal | Must |
| UC-15 | Claude | Look up a channel by name or ID | Normal | Should |
| UC-16 | Claude | Get info about a user (username, display name, avatar) | Normal | Should |
| UC-17 | Claude | Get guild member info (roles, permissions, join date) | Normal | Should |
| UC-18 | Claude | List roles in a guild | Normal | Should |
| UC-19 | Claude | Pin/unpin a message | Normal | Could |
| UC-20 | Claude | Create a thread from a message | Normal | Could |
| UC-21 | Claude | Delete the bot's own message | Normal | Could |
| UC-22 | Claude | Set bot presence/status | Normal | Could |
| UC-23 | Claude | Send typing indicator manually | Normal | Could |
| UC-24 | Claude | Get a single message by ID | Normal | Should |
| UC-25 | Claude | Get channel topic and metadata | Normal | Should |
| UC-26 | Discord User | React to a bot message; Claude is notified | Normal | Must |
| UC-27 | Claude | Send a voice message (audio attachment with voice flag) | Normal | Should |
| UC-28 | Discord User | Send a voice message; Claude receives audio for processing | Normal | Should |
| UC-29 | Claude | Join a voice channel on demand | Normal | Future |
| UC-30 | Claude | Leave a voice channel | Normal | Future |
| UC-31 | Admin | Receive rate-limited notifications about pending access requests | Normal | Must |
| UC-32 | Claude | Review the access request queue | Normal | Should |
| AC-01 | Attacker | Impersonate admin via forged Discord message to approve access | Abuse | Must-mitigate |
| AC-02 | Attacker | Prompt-inject via message content to make Claude edit config | Abuse | Must-mitigate |
| AC-03 | Attacker | Exfiltrate host files via reply tool's file attachment | Abuse | Must-mitigate |
| AC-04 | Attacker | DoS via rapid messaging to exhaust Claude Code capacity | Abuse | Should-mitigate |
| AC-05 | Attacker | Forge attachment metadata to confuse Claude | Abuse | Should-mitigate |
| AC-06 | Attacker | Escalate from user to admin via permission reply interception | Abuse | Must-mitigate |
| SC-01 | System | Validate outbound sends against the access gate | Security | Must |
| SC-02 | System | Sanitize attachment filenames in notifications | Security | Must |
| SC-03 | System | Refuse to attach files from state directory | Security | Must |
| SC-04 | System | Route permission requests only to admins | Security | Must |
| SC-05 | System | Expire access requests after timeout | Security | Must |
| SC-06 | System | Cap access request queue size | Security | Must |
| SC-07 | System | Log security-relevant events (pairing, permission, gate denials) | Security | Must |

## Requirements

### Functional Requirements

| ID | Requirement | Source |
|----|-------------|--------|
| R-01 | System shall connect to the Discord gateway and maintain a persistent WebSocket connection with automatic reconnection | UC-01, UC-02 |
| R-02 | System shall implement MCP stdio transport with `claude/channel` and `claude/channel/permission` capabilities | UC-01, UC-08 |
| R-03 | System shall gate inbound DMs: deliver if sender is in `allow_from`; if not, queue the message as an access request | UC-01, UC-04 |
| R-04 | System shall gate inbound guild messages: deliver only from opted-in channels, respecting `require_mention` and per-channel `allow_from` | UC-02 |
| R-05 | System shall detect mentions via: @mention, reply-to-bot, configurable regex patterns | UC-02 |
| R-06 | System shall emit `notifications/claude/channel` with message content and structured metadata (chat_id, message_id, user, user_id, ts, attachment info) | UC-01, UC-02 |
| R-07 | System shall list attachment metadata (name, type, size) in notification without auto-downloading | UC-03 |
| R-08 | Access request queue shall have a configurable cap (generous default, e.g. 50); requests beyond the cap are dropped silently | UC-04, SC-06 |
| R-08a | Admin shall be notified of new access requests via DM, rate-limited (e.g. max 1 notification per minute, batched) | UC-31 |
| R-08b | System shall expose `list_access_requests` tool for Claude/admin to review pending requests | UC-05, UC-32 |
| R-08c | System shall expose `approve_access` and `deny_access` tools for admin to act on requests | UC-05 |
| R-08d | Access requests shall expire after a configurable timeout (default 24h) | SC-05 |
| R-09 | System shall send typing indicator on direct mentions/DMs | UC-01, UC-02 |
| R-10 | System shall react with configurable ack emoji on message receipt | UC-01, UC-02 |
| R-11 | Reply tool shall chunk messages at configurable limit (max 2000), with paragraph-aware or length-based splitting | UC-08 |
| R-12 | Reply tool shall support threading (reply_to), file attachments (max 10 files, 25MB each), and configurable reply-to mode (first/all/off) | UC-08 |
| R-13 | Outbound gate shall mirror inbound gate: only send to channels the bot would accept inbound from | UC-08, SC-01 |
| R-14 | Permission requests shall be sent only to users in the `admins` list, not `allow_from` | UC-06, SC-04 |
| R-15 | Permission responses shall be accepted only from users in `admins` via button interactions or text reply pattern | UC-06, SC-04 |
| R-16 | Config shall be re-read from disk on every inbound message (hot-reload) | UC-07 |
| R-17 | System shall expose `list_guilds` tool returning guild names, IDs, member counts, icon URLs | UC-13 |
| R-18 | System shall expose `list_channels` tool returning channels in a guild with type, name, ID, topic, position, parent category | UC-14 |
| R-19 | System shall expose `get_channel` tool for lookup by ID with full metadata (topic, type, permissions, slowmode, nsfw) | UC-15, UC-25 |
| R-20 | System shall expose `get_user` tool returning username, display name, avatar URL, bot flag | UC-16 |
| R-21 | System shall expose `get_member` tool returning guild-specific info: roles, nick, join date, permissions | UC-17 |
| R-22 | System shall expose `list_roles` tool returning role names, IDs, colors, permissions, position | UC-18 |
| R-23 | System shall expose `pin_message` and `unpin_message` tools | UC-19 |
| R-24 | System shall expose `create_thread` tool (from message or standalone) | UC-20 |
| R-25 | System shall expose `delete_message` tool (bot's own messages only) | UC-21 |
| R-26 | System shall expose `set_presence` tool (status text, activity type) | UC-22 |
| R-27 | System shall expose `send_typing` tool for manual typing indicator | UC-23 |
| R-28 | System shall expose `get_message` tool for single message lookup by ID | UC-24 |
| R-29 | System shall shut down cleanly on stdin EOF, SIGTERM, or SIGINT, destroying the Discord client | All |
| R-32 | System shall emit a channel notification when a user reacts to one of the bot's messages (emoji, user, message_id) | UC-26 |
| R-33 | System shall support sending voice messages (audio file with `IS_VOICE_MESSAGE` flag) via the reply tool | UC-27 |
| R-34 | System shall detect inbound voice messages (attachment with voice flag) and include voice metadata in the notification | UC-28 |
| R-35 | System shall require `GuildMessageReactions` and `GuildVoiceStates` gateway intents | UC-26, UC-29 |
| R-30 | File send guard shall refuse to attach any file within the state directory (except inbox/) | SC-03, AC-03 |
| R-31 | Attachment filenames shall be sanitized (strip `[\[\]\r\n;]`) in notifications | SC-02, AC-05 |
| R-36 | Access queue shall store at most one entry per user_id (dedup) | Threat review |
| R-37 | Access queue message preview shall be truncated to 100 characters | Threat review |
| R-38 | Outbound gate shall re-read config on every tool call (not cache from inbound) | Threat review |
| R-39 | Pending permission map shall be pruned on a 5-minute sweep | Threat review |
| R-40 | Queue persistence shall use atomic write (tmp file + rename) | Threat review |
| R-41 | File send guard shall canonicalize paths before state-directory check | Threat review |

### Non-Functional Requirements

| ID | Requirement | Source |
|----|-------------|--------|
| NF-01 | Binary shall be statically linkable and produce a single executable | Success criteria |
| NF-02 | System shall not panic on Discord API errors; all errors logged and handled gracefully | Reliability |
| NF-03 | Memory usage shall remain bounded (cap recent-sent-IDs set, prune expired pairings) | Reliability |
| NF-04 | Config parsing errors shall not crash the process; fall back to defaults and log | Reliability, UC-07 |

### Project & CI Requirements

| ID | Requirement | Source |
|----|-------------|--------|
| P-01 | Rust edition 2024, MSRV 1.93 (edition 2024 stable since 1.85) | Convention |
| P-02 | License: MIT OR Apache-2.0 (dual) | Convention |
| P-03 | CI pipeline: `cargo fmt --check` → `cargo clippy -- -D warnings` → `cargo nextest run` → cross-compile check → MSRV check → cargo-deny | Convention |
| P-04 | PR titles enforce conventional commits (feat, fix, chore, docs, refactor, revert, test, ci, perf, build) | Convention |
| P-05 | All GitHub Actions pinned to full commit SHA with version comment | Convention |
| P-06 | Release pipeline: tag-release (workflow_run on CI) → release (cargo-auditable, macOS universal via lipo, SHA256 checksums, OIDC trusted publishing, git-cliff release notes) | Convention |
| P-07 | Dependabot: weekly Friday, grouped Rust deps (minor+patch), grouped actions (all), conventional commit prefixes | Convention |
| P-08 | deny.toml: standard license allowlist, advisory ignore with rationale, deny unknown registries/git | Convention |
| P-09 | Release profile: `lto = true`, `strip = true`, `codegen-units = 1` | Convention |
| P-10 | justfile with `check`, `fmt`, `test`, `install` targets | Convention |
| P-11 | `.githooks/pre-commit` running `cargo fmt --check` | Convention |
| P-12 | CHANGELOG.md in Keep a Changelog format | Convention |
| P-13 | Cross-compile targets: x86_64-unknown-linux-gnu, universal-apple-darwin | Convention |
| P-14 | No Docker/container build in CI | User decision |
| P-15 | Use native async traits (no `async-trait` crate) and language/library features stable in Rust 1.93; adopting newer features requires an explicit MSRV change | Convention |
| P-16 | No `async-trait` dependency; native `impl Trait` in return position for async | Convention |

## ASVS & ISO 27001 Review

### ASVS Categories Reviewed

| Category | Applicable | Notes |
|----------|-----------|-------|
| V1: Architecture | Yes | Trust boundaries: Discord (untrusted) → Dione → Claude Code (trusted). Addressed in architecture phase. |
| V2: Authentication | Yes | User identity via Discord snowflake. Admin identity via `admins` config list. No passwords — Discord handles authn. |
| V4: Access control | Yes | Core feature. Gate system (R-03, R-04), outbound gate (R-13), admin separation (R-14, R-15). |
| V5: Validation | Yes | Untrusted input from Discord: filenames (R-31), message content, channel/user IDs. Tool arguments validated before Discord API calls. |
| V7: Error handling/logging | Yes | Security events logged (SC-07). No sensitive data (tokens, file contents) in logs. |
| V8: Data protection | Yes | Bot token in env/config file with restricted permissions (0o600). State directory protected from exfiltration (R-30). |
| V11: Business logic | Yes | Access request queue logic (R-08), permission relay routing (R-14/R-15), outbound gate mirroring (R-13). |
| V12: Files/resources | Yes | Attachment download size cap (25MB), file send guard (R-30), inbox directory isolation. |
| V14: Configuration | Yes | Hot-reload (R-16), corrupt config handling (NF-04), permission semantics of config file. |

### Categories Deemed Not Applicable

| Category | Rationale |
|----------|-----------|
| V3: Session management | No sessions. Each message is independently gated by sender identity. |
| V6: Cryptography | No stored secrets beyond bot token (env file). No encryption at rest needed. |
| V9: Communication | Discord gateway is TLS. MCP is stdio (same-host). No custom transport security needed. |
| V10: Supply chain | Covered by cargo-deny (P-08) and Dependabot (P-07). No additional ASVS-specific work. |
| V13: API/web services | No HTTP API exposed. Dione is a stdio server and Discord gateway client. |

### ISO 27001 Controls Reviewed

| Control | Applicable | Notes |
|---------|-----------|-------|
| A.8.15: Logging | Yes | Gate denials, pairing events, permission decisions logged at appropriate levels (SC-07). |
| A.8.12: Data leakage prevention | Yes | File send guard prevents exfiltrating state directory contents (R-30). Outbound gate prevents sending to unauthorized channels (R-13). |
| A.8.11: Data masking | Marginal | Bot token must not appear in logs. User message content may be logged at debug level only. |

## Security Requirements Traceability Matrix (SRTM)

| Req ID | Requirement | Source UC | Security Ref | Test Case |
|--------|-------------|-----------|--------------|-----------|
| R-03 | Gate inbound DMs: deliver or queue | UC-01, UC-04 | V4, V11 | TC-01: DM from allowlisted user delivers |
| | | | | TC-02: DM from unknown sender queued as access request |
| | | | | TC-03: DM when policy=disabled drops silently |
| R-04 | Gate inbound guild messages | UC-02 | V4 | TC-04: Message in opted-in channel with mention delivers |
| | | | | TC-05: Message in non-opted channel drops |
| | | | | TC-06: Message without mention in require_mention channel drops |
| R-08 | Access request queue constraints | UC-04 | V11 | TC-07: Request expires after configured timeout |
| | | | | TC-08: Queue cap enforced; excess dropped |
| | | | | TC-09: Admin notification rate-limited |
| R-13 | Outbound gate mirrors inbound | UC-08 | V4, V11 | TC-10: Reply to allowlisted DM channel succeeds |
| | | | | TC-11: Reply to non-opted channel rejects |
| R-14 | Permission requests to admins only | UC-06 | V4 | TC-12: Permission request sent to admin |
| | | | | TC-13: Permission request NOT sent to non-admin allowlisted user |
| R-15 | Permission responses from admins only | UC-06 | V4, V2 | TC-14: Button click from admin accepted |
| | | | | TC-15: Button click from non-admin rejected |
| R-30 | File send guard | AC-03 | V12, A.8.12 | TC-16: Attach file from inbox/ succeeds |
| | | | | TC-17: Attach file from state dir (non-inbox) rejects |
| | | | | TC-18: Attach file outside state dir succeeds |
| R-31 | Sanitize attachment filenames | AC-05 | V5 | TC-19: Filename with brackets/newlines stripped |
| SC-07 | Security event logging | SC-07 | V7, A.8.15 | TC-20: Gate denial logged |
| | | | | TC-21: Access request queued logged |
| | | | | TC-22: Permission decision logged |
| NF-04 | Config error handling | UC-07 | V14 | TC-23: Corrupt config → fallback to defaults + log |
| | | | | TC-24: Missing config file → defaults |

## Tool Inventory

Complete list of MCP tools Dione exposes to Claude Code:

### Messaging (ported from TypeScript plugin)

| Tool | Parameters | Returns |
|------|-----------|---------|
| `reply` | `chat_id`, `text`, `reply_to?`, `files?` | Sent message ID(s) |
| `react` | `chat_id`, `message_id`, `emoji` | Confirmation |
| `edit_message` | `chat_id`, `message_id`, `text` | Edited message ID |
| `fetch_messages` | `channel`, `limit?` (default 20, max 100) | Formatted message list (oldest-first) |
| `download_attachment` | `chat_id`, `message_id` | Downloaded file paths + metadata |
| `get_message` | `chat_id`, `message_id` | Single message with full metadata |

### Discord Introspection (new)

| Tool | Parameters | Returns |
|------|-----------|---------|
| `list_guilds` | (none) | Guild names, IDs, member counts, icon |
| `list_channels` | `guild_id`, `type?` (filter) | Channel list with type, name, ID, topic, category |
| `get_channel` | `channel_id` | Full channel metadata |
| `get_user` | `user_id` | Username, display name, avatar URL, bot flag |
| `get_member` | `guild_id`, `user_id` | Nick, roles, join date, permissions |
| `list_roles` | `guild_id` | Role names, IDs, colors, permissions, position |

### Channel Management (new)

| Tool | Parameters | Returns |
|------|-----------|---------|
| `pin_message` | `chat_id`, `message_id` | Confirmation |
| `unpin_message` | `chat_id`, `message_id` | Confirmation |
| `create_thread` | `chat_id`, `message_id?`, `name`, `auto_archive?` | Thread channel ID |
| `delete_message` | `chat_id`, `message_id` | Confirmation (bot's own only) |

### Access Management (new)

| Tool | Parameters | Returns |
|------|-----------|---------|
| `list_access_requests` | (none) | Pending requests with sender info, message preview, timestamp |
| `approve_access` | `user_id` | Confirmation; adds to `allow_from` |
| `deny_access` | `user_id` | Confirmation; removes from queue |

### Bot State (new)

| Tool | Parameters | Returns |
|------|-----------|---------|
| `set_presence` | `status?`, `activity_type?`, `activity_name?` | Confirmation |
| `send_typing` | `chat_id` | Confirmation |

### Voice (future phase)

| Tool | Parameters | Returns |
|------|-----------|---------|
| `join_voice` | `channel_id` | Confirmation |
| `leave_voice` | `guild_id` | Confirmation |

## Config Schema

TOML config at `~/.claude/channels/dione/config.toml` (overridable via `DIONE_CONFIG_PATH` env var).

```toml
# Discord bot token. Can also be set via DISCORD_BOT_TOKEN env var (env wins).
token = "MTIz..."

# --- Access Control --------------------------------------------------------

[access]
# How to handle DMs from senders not in allow_from.
# "queue"    — message queued for admin review, sender notified
# "drop"     — silently ignored
# "disabled" — all DMs dropped, including from allow_from
dm_policy = "queue"

# User snowflakes allowed to DM.
allow_from = ["184695080709324800"]

# Admins receive permission requests. Subset or overlap of allow_from.
admins = ["184695080709324800"]

# --- Guild Channels --------------------------------------------------------

# Each opted-in channel gets its own [[channels]] entry.
[[channels]]
id = "846209781206941736"
require_mention = true
allow_from = []  # empty = any member (subject to require_mention)

[[channels]]
id = "912345678901234567"
require_mention = false
allow_from = ["184695080709324800", "221773638772129792"]

# --- Mention Detection -----------------------------------------------------

[mentions]
# Case-insensitive regexes that count as a mention (in addition to @mention
# and reply-to-bot).
patterns = ["^hey dione\\b", "\\bdione\\b"]

# --- Delivery --------------------------------------------------------------

[delivery]
# Emoji to react with on receipt. Empty string disables.
ack_reaction = "👀"

# Threading on chunked replies: "first" | "all" | "off"
reply_to_mode = "first"

# Max chars per outbound message before splitting. Discord caps at 2000.
text_chunk_limit = 2000

# Split strategy: "length" (hard cut) | "paragraph" (prefer boundaries)
chunk_mode = "paragraph"

# --- Access Requests -------------------------------------------------------

[access_requests]
# How long an access request stays in the queue before expiring.
expiry_seconds = 86400  # 24 hours

# Maximum number of pending requests in the queue. Generous cap.
max_pending = 50

# Rate limit for admin notifications about new requests.
# At most one notification per this many seconds.
notify_cooldown_seconds = 60

# --- Voice (future) --------------------------------------------------------

[voice]
# Whether voice channel features are enabled. Requires GuildVoiceStates intent.
enabled = false
```

### Config Precedence

1. Environment variables override config file values:
   - `DISCORD_BOT_TOKEN` → `token`
   - `DIONE_CONFIG_PATH` → config file location
   - `DIONE_STATE_DIR` → state directory (default `~/.claude/channels/dione/`)
2. Config file is TOML, re-read on every inbound message
3. Missing file → all defaults (queue policy, empty lists)
4. Parse error → rename to `.corrupt-{timestamp}`, log error, use defaults
## GAIE archive Atom 1b requirements

- **GAIE-1B-R1:** One configured capture root shall include its parent and all
  principal-visible active and archived threads with the exact expected guild,
  parent ID, and admitted thread type.
- **GAIE-1B-R2:** Discovery shall use guild-active, parent-public-archived, and
  private-archived Discord routes; a 403 on private-all shall fall back to
  joined-private.
- **GAIE-1B-R3:** Public/private archives shall use ISO-8601 `before` cursors,
  joined-private shall use a snowflake cursor, and `has_more` shall control
  pagination.
- **GAIE-1B-R4:** Discovery shall union active snapshot A, all archive pages,
  and active snapshot B, then order the parent first and threads by numeric
  snowflake.
- **GAIE-1B-R5:** The default mode shall complete discovery and validation
  before archive mutation or fail closed. `allow_partial` shall remain an
  explicit parent-only break-glass mode.
- **GAIE-1B-R6:** Child message fetches shall accept only verified capture
  targets. Wrong-parent, wrong-guild, and wrong-type candidates shall be
  rejected before any child message request.
- **GAIE-1B-R7:** Message identity shall remain Discord-global by `message_id`.
  Parent/thread starter aliases shall deduplicate while preserving the embedded
  thread relation. Reaction ordering shall remain stable.
- **GAIE-1B-R8:** Checkpoint v2 shall identify corpus, guild, and parent and use
  a deterministic stream map with nullable `after_message_id` values.
- **GAIE-1B-R9:** The exact v1 checkpoint shall migrate to a v2 parent-only
  stream. Unknown, mixed, foreign, and corrupt forms shall fail closed.
- **GAIE-1B-R10:** Each stream cursor shall advance only after its batch commit
  is fsynced. A later run shall discover new threads without replaying completed
  streams, and an identical rerun shall append zero events without semantic
  checkpoint churn.
