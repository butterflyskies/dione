# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
