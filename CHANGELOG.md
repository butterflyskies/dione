# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.13.0] - 2026-06-19

### Added
- `batch` module (`src/batch.rs`) with `serialize_batch()` — converts coalesced
  `NotificationEvent::Message` vectors into a compact, human-readable text format
  that uses far fewer tokens than individual JSON-RPC notifications. Format uses
  a `[batch]` envelope header, a `[users]` roster for short-name-to-ID mapping,
  and `|`-delimited message lines with optional reply-to and attachment count
  suffixes. Extracted `MessageEvent` struct from `NotificationEvent` for cleaner
  field access. 14 tests covering round-trip serialization, edge cases, and
  timezone localization (#114).
- `coalesce` module (`src/coalesce.rs`) — top-level event coalescing layer that
  sits between `DeliveryBuffer` and stdout. Routes flushed event batches into
  the optimal wire format: single events pass through as individual notifications,
  homogeneous message batches use the `batch_v1` compact format, and mixed-type
  batches (messages + edits + reactions + deletes) use the new `events_v1`
  heterogeneous format. Cross-channel flushes produce a multi-envelope wrapper
  that groups per-channel envelopes (#122).
- `events_v1` wire format — heterogeneous event serialization with `[events]`
  header, `[users]` roster, and typed event lines (`!edit`, `!react`, `!delete`)
  interspersed with message entries. Covers the full event taxonomy: messages,
  edits, reactions, deletes, and graceful fallback for trace/config/permission
  events (#122).
- `multi` envelope format for non-channel events (Trace, PermissionResponse,
  ConfigError) — wraps individual notification params in a
  `{ format: "multi", notifications: [...] }` batch when 2+ non-channel events
  flush together (#122).
- `deliver_flushed()` integration in `mcp/server.rs` — replaces the old
  per-event stdout loop with a single `coalesce()` call that emits one
  JSON-RPC line per flush, regardless of how many events were buffered (#122).

## [0.12.0] - 2026-06-14

### Added
- `reply_to_user_id`, `reply_to_user`, and `reply_to_content_preview` fields on
  inbound `message` notifications — best-effort reply context from Discord's
  embedded `referenced_message` (author ID, author name, and a 100-character
  content preview with ellipsis when truncated). Omitted when the message is not
  a reply, the parent is unavailable, or the reference is a forward/crosspost.
  Not emitted on `message_edit` notifications (closes #100).

## [0.11.0] - 2026-06-14

### Added
- `reply_to_message_id` field on inbound `message`, `message_edit`, and
  `message_delete` notifications — populated when the Discord message is a
  reply to another message (closes #99).
- `Snowflake` newtype at the MCP tool boundary — centralizes Discord snowflake
  validation and rejects `0`, which would panic serenity's typed ID wrappers
  (#103).

### Changed
- MCP tool handlers and inbound notification formatting now use serenity typed
  IDs (`ChannelId`, `MessageId`, `UserId`, `GuildId`) instead of raw `u64` /
  `String` snowflakes throughout the dispatch and event paths (#103).

### Fixed
- Box `BufferResult::Immediate` to avoid large stack frames when the delivery
  buffer bypasses coalescing.

## [0.10.2] - 2026-06-10

### Fixed
- Reverted batch notification wrapping introduced in 0.10.0 — Claude Code
  does not understand the `events` array envelope and silently drops batched
  messages (0.10.1 fixed the method name but not the payload shape). Buffered
  events are now emitted as individual `notifications/claude/channel`
  notifications with standard `{ content, meta }` params. Delivery buffering
  is preserved — flushed events are written as a single stdout chunk so they
  arrive together.

### Removed
- `batch_notification()` function, `into_batch_params()` trait method, and
  `make_batch_notification()` test helper — no longer needed without the
  batch envelope.

## [0.10.1] - 2026-06-10

### Fixed
- Batch coalescing now emits `notifications/claude/channel` instead of
  unrecognized `notifications/claude/channel/batch` method.

## [0.10.0] - 2026-06-10

### Added
- `fetch_new_since` tool — cursor-based message fetch that returns only messages
  after a given message ID, cutting heartbeat token cost to near zero on quiet
  channels. Stateless design: caller owns the cursor (#87).
- Global `delivery_delay_ms` config default in `[delivery]` section — all channels
  inherit the global delay unless they set a per-channel override. Per-channel
  `delivery_delay_ms` is now `Option<u64>` (#91).
- Batch coalescing for buffered notifications — when the delivery buffer flushes,
  all buffered events are coalesced into a single
  `notifications/claude/channel/batch` JSON-RPC notification instead of N separate
  notifications. Single-event flushes also use the batch format (#91).
- `NotificationSender` and `NotificationFormatter` traits — replace free functions
  with trait-based notification dispatch for testability and extensibility (#91).

### Fixed
- Permission prompt cleanup no longer emits spurious `message_delete` notifications.
  Prompt message IDs are now tracked via `note_sent` at send time, and sibling IDs
  are re-marked before deletion, guaranteeing suppression even after cache
  eviction (#90).
## [0.9.0] - 2026-06-09

### Added
- Per-channel delivery buffer with configurable coalescing delay — messages
  are batched before forwarding to the MCP client, reducing notification
  chatter during bursts (#78).
- Rate limiter with token-bucket state machine derived from TLA+ spec —
  enforces per-channel send rate limits with automatic backpressure (#78).
- Live-reloadable config for both delivery buffer and rate limiter (#78).
- Self-contained `rate_limiter.rs` module with 4 proptest properties and
  12 unit tests, matching the formally verified TLA+ model (#77).
- Rate limiter design spec and TLA+ model in `docs/design/` — formal
  specification with TLC model checking (752,862 states explored) (#59).

### Fixed
- Permission DMs no longer expire after 5 minutes — the timeout-based
  expiry is removed entirely (#82).
- All permission DMs (the clicked button and its siblings) are deleted
  after the admin responds, instead of being edited to show status (#82).

### Changed
- 241 tests total including property-based tests for rate limiter and
  delivery buffer.

## [0.8.1] - 2026-06-06

### Fixed
- Stale permission request DMs are now cleaned up: the background pruning task
  edits expired messages to show "Expired" and removes buttons, instead of
  leaving them active indefinitely.
- Button-click handler removes all pending entries for the same request_id
  (multi-admin cleanup), preventing duplicate PermissionResponse events.
- Guard against duplicate event emission when entries are already pruned.

### Changed
- `PendingPermission` stores typed `ChannelId` instead of raw `u64`.
- `prune_stale_permissions` returns `Vec<(ChannelId, MessageId)>` for
  caller-driven message cleanup.
- Removed dead `validate_response` function and unused error variants.
- Failed permission message cleanup logged at `warn` instead of `debug`.

## [0.8.0] - 2026-06-03

### Changed
- Bot messages are no longer unconditionally ignored. If a bot's user ID is in
  the `allow_from` list, its messages (and edits) are now routed through the
  normal message handling path. Unknown bots are still dropped.

## [0.7.0] - 2026-05-29

### Added
- `suppress_ping` boolean parameter on the `reply` tool — when `true`, sets
  `allowed_mentions.replied_user = false` so the reply does not ping the
  person being replied to. Defaults to `false` (existing behavior preserved).
  Closes #55.

### Fixed
- Config store integration test flakiness: parallel tests shared the global
  ArcSwap cache. Tests now use `load_config_from_disk()` against their own
  temp directory instead of the process-wide `load_config()`.

## [0.6.0] - 2026-05-26

### Added
- Thread support: messages in Discord threads are now delivered and gated via
  their parent channel's policy. `create_thread` tool now works end-to-end.
- `ResolvedChannel` struct and `resolve_guild_channel()` helper for DRY
  thread resolution across event handlers.
- Thread-parent cache (`BTreeMap<u64, Option<u64>>`) with negative caching
  to avoid repeated Discord API calls for non-thread channels.
- `OutboundGate::check_channel_with_threads()` allows sending to threads
  whose parent channel is permitted.
- `thread_parent_id` field in Message, MessageEdit, and MessageDelete
  notifications so agents know when a message is in a thread.

## [0.5.0] - 2026-05-23

### Added
- Config management MCP tools: `add_channel`, `remove_channel`, `update_channel`,
  `list_config_channels`, `get_access_config`, `update_dm_policy`, `add_allow_from`,
  `remove_allow_from` — manage dione's config.toml without manual file editing (#33)
- `ConfigStore` type for atomic config mutations with ArcSwap cache update on save
- `DiscordId` newtype for validated Discord snowflake IDs
- `send_dm` tool — initiate DM conversations by user ID, creating the DM channel
  if needed. Shared `create_dm_channel` helper replaces inline logic in
  `notify_admin_dm` (#37)
- Configurable timezone for all timestamps via `timezone` config option (IANA names
  like `"America/Los_Angeles"`). `LocalTimestamp` newtype handles conversion at
  construction (#34)
- `timestamp` module with `LocalTimestamp` type — `Serialize`, `Display`, `From`
  trait impls for ergonomic use in `json!()` macros

### Fixed
- Config cache is now lock-free using `ArcSwap` instead of `Mutex` (#32)
- `to_rfc3339().unwrap_or_default()` no longer produces empty timestamps — falls
  back to `Utc::now()` with warning log (closes #23)
- `ConfigStore::save()` updates the ArcSwap cache immediately after write, eliminating
  redundant disk re-reads
- Tmp file cleanup on rename failure in `ConfigStore::save()`

### Changed
- CI: bump actions, add shared cache key (#27), bump cargo-deny-action (#36)

## [0.4.0] - 2026-05-22

### Added
- Forward `message_edit` and `message_delete` Discord events to MCP client
  - `message_update` handler: filters embed-only updates, gates through
    `InboundGate`, resolves author from 3-level fallback chain
  - `message_delete` handler: filters bot's own messages via `recent_sent_ids`,
    checks `dm_policy` for DM channels
  - `InboundGate::check_guild_passive` for events without mention context —
    enforces channel opt-in and `allow_from` but not `require_mention`
  - Both handlers use `load_config_checked` with `ConfigError` forwarding
  - Snapshot tests for both notification types
- `render_latex` tool — renders LaTeX math to PNG via mitex + typst (pure Rust,
  no external TeX installation)
- `send_file` tool — uploads a local file as a Discord attachment with optional
  caption
- `render_latex_to_channel` tool — renders LaTeX and posts the PNG directly to
  a Discord channel (combines `render_latex` + `send_file`)

### Fixed
- Graceful config recovery: cache last valid config on parse errors, keep file
  intact, send `ConfigError` notification to MCP client (#17)
- Filter bot's own reactions from channel events (#15)
- Resolve usernames on reaction events via message cache (#16)

## [0.3.0] - 2026-05-17

### Added
- CLI argument parsing with clap (`--version`, `--log-level`)
- Runtime trace-level control via MCP tools:
  - `set_trace_level` — controls channel-forwarding filter; matching trace
    events are sent as channel notifications with `type="trace"` metadata
  - `set_stderr_level` — controls stderr logging filter
  - `get_version` — returns dione version at runtime
- Tracing channel layer: a custom tracing `Layer` that forwards events as
  MCP channel notifications, differentiated from Discord events via
  `type="trace"` in notification metadata

### Fixed
- Permission relay: handle inbound `notifications/claude/channel/permission_request`
  from Claude Code (was falling through to "unknown method" — permission DMs
  were never sent to admins)

## [0.2.0] - 2026-05-17

### Fixed
- Declare `claude/channel` and `claude/channel/permission` experimental
  capabilities in the MCP initialize handshake
- Permission notification now uses `notifications/claude/channel/permission`
  method with `behavior` field instead of generic channel notification
- Add `DIRECT_MESSAGE_REACTIONS` gateway intent so DM reactions are received
- Reaction handler falls back to Discord API fetch when message ID is not in
  the in-memory sent set, surviving bot restarts

## [0.1.1] - 2026-05-17


### Added
- Discord MCP channel server for Claude Code via `--channels`
- Manual JSON-RPC MCP server with `notifications/claude/channel` protocol
- 21 MCP tools across 5 categories:
  - Messaging: reply (chunked, threaded), react, edit_message, fetch_messages,
    download_attachment, get_message
  - Introspection: list_guilds, list_channels, get_channel, get_user,
    get_member, list_roles, list_emojis
  - Management: pin_message, unpin_message, create_thread, delete_message
  - Access: list_access_requests, approve_access, deny_access
  - Bot state: send_typing
- Access control gate with O(1) lookups (LoadedConfig with pre-parsed
  HashSet/HashMap, pre-compiled RegexSet for mention patterns)
- Access request queue for unknown senders (admin-gated, rate-limited
  notifications, 50-slot cap, 24h TTL, JSON persistence with atomic writes)
- Permission relay with Discord buttons routed to admins only
- TOML config with hot-reload (re-read per inbound message, file lock on
  correct fd)
- Message chunking with paragraph-aware splitting via `str::ceil_char_boundary`
- Custom emoji support in react tool (`<:name:id>` and `<a:name:id>`)
- Attachment downloads via reqwest with 25MB size limit (content-length
  pre-check + post-download validation)
- File send guard with symlink-resolving canonicalization
- Filename sanitization with basename extraction (path traversal prevention)
- Clean shutdown via CancellationToken (stdin EOF / SIGTERM / SIGINT)
- Periodic pruning of stale permissions and expired queue entries (60s timer)
- 75 tests (unit + integration + insta snapshots)
- Full CI pipeline: fmt, clippy, nextest, cross-compile, MSRV 1.95, cargo-deny
- Release pipeline: cargo-auditable, macOS universal via lipo, OIDC trusted
  publishing, git-cliff release notes
- Design spec in docs/design/ (problem, requirements, architecture, threat
  model, test plan)
