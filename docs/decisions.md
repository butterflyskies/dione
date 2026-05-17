# Dione — Decision Journal

Running log of implementation decisions, open questions resolved, and sub-agent
dispatches. Append-only during development.

---

## 2026-05-17 — Session Start

### Plan Approved (implicit)
- Two-pass implementation: core modules first, then integration layer
- User directive: fly solo, minimize interruptions, keep this journal

### Key Decision: Custom MCP Notification Transport
rmcp 1.7's `ServerNotification` enum is closed — no way to emit
`notifications/claude/channel` through the official API. Solution: build a
channel-based transport adapter that owns stdout. rmcp writes tool responses
to a channel, our code writes custom notifications to the same channel, and a
unified writer task serializes both to stdout with mutual exclusion.

### Key Decision: toml_edit for Config Write-Back
`approve_access` needs to add a user_id to `allow_from` in config.toml without
destroying comments or formatting. Using `toml_edit` crate instead of
serialize-then-write.

### Key Decision: serenity async_trait
serenity 0.12 still requires `#[async_trait]` on EventHandler. We use
serenity's re-exported macro — NOT adding async-trait as a direct dependency.
This satisfies "no async-trait crate" since it's a transitive dep of serenity.

### Implementation Order
1. Pass 1: Cargo.toml, CI, config, state, gate, chunker, queue, unit tests
2. Pass 2: discord client/events, permissions, MCP server, tools, main, integration tests

---

## 2026-05-17 — Pass 1 Complete

### What was implemented
All 9 modules compiled and all 40 unit tests pass. Verified with `cargo nextest run`
and `cargo build --release`.

**Cargo.toml**: Added the full dependency set. Noted that serenity 0.12's `gateway`
feature requires the `client` feature to be enabled explicitly — without it, the
`shard_manager` module fails to compile due to missing `crate::client` imports. Added
`"client"` to serenity's feature list.

**src/config.rs**: Full TOML config parsing with hot-reload design. Three structs
(`Config`, `MentionConfig`, `VoiceConfig`) were made derivable-Default by clippy;
switched from manual `impl Default` to `#[derive(Default)]`. The
`test_env_var_overrides_token` test calls `std::env::set_var` which is unsafe in
edition 2024 — wrapped in `unsafe {}` blocks with a safety comment.

**src/state.rs**: `prune_sent_ids` retains the _largest_ (newest) snowflake IDs by
removing the smallest until the set is at cap — correct for monotonically increasing
Discord snowflakes. `prune_stale_permissions` uses `BTreeMap::retain` (stable 1.91+
via `.retain`), not `extract_if`, because `retain` is more idiomatic for
unconditional pruning.

**src/gate.rs**: Three decision types: `InboundGate::check_dm`, `check_guild`, and
`OutboundGate::check_channel`, `check_file_send`. The file send guard canonicalizes
both the target path and the state dir before prefix-checking, resolving symlinks per
R-41. `MentionDetector::is_mentioned` logs invalid regex patterns at warn level and
continues without panicking. `recent_sent_ids` is accepted as a parameter (for
future caller use) but not yet used internally — suppressed with `let _ = ...`.

**src/discord/chunker.rs**: Uses `str::floor_char_boundary` (stable 1.91) to find
the last safe split point at or before `limit`. Paragraph mode searches for `\n\n`,
then `\n`, then space via `rfind`. Leading whitespace is trimmed from the start of
each continuation chunk to avoid orphaned newlines. Three nested `if let ... { if
... }` patterns collapsed to `if let ... && ...` (let-chains, edition 2024) per
clippy suggestion.

**src/queue.rs**: `enqueue` signature takes `max_pending` as a parameter (not read
from config) so the caller controls the cap — allows hot-reload semantics. Persist
uses write-to-`.tmp`-then-rename for atomicity. Two `if entry.is_some() { if let
Err(e) = ... }` patterns collapsed to `if entry.is_some() && let Err(e) = ...`
(edition 2024 let-chains) per clippy.

**src/state.rs, mcp/*, discord/client.rs, discord/events.rs, permissions.rs**:
Stub modules present for compilation; content deferred to Pass 2.

### Deviations from spec
- `BTreeMap::extract_if` is stable on 1.91+ (confirmed), but `retain` was used in
  `state.rs::prune_stale_permissions` since there is no need to collect the removed
  values — simpler and equally correct.
- serenity needed `"client"` feature added to the feature list (spec omitted it).
- `env::set_var`/`remove_var` require `unsafe` in edition 2024; test wrapped in
  `unsafe {}` with justification comment.
- The `recent_sent_ids` parameter in `MentionDetector::is_mentioned` is not yet
  used by the internal logic (the referenced-message-ID path is handled via
  `referenced_author_id` instead). It is kept in the signature for the Pass 2 caller.

---

## 2026-05-17 — Pass 2 Complete

### What was implemented

All integration layer modules compiled. All 40 unit tests still pass.
`cargo build --release` succeeds. `cargo clippy -- -W clippy::all -D warnings` passes.

### Key decisions and deviations

**MCP transport — manual JSON-RPC over stdio (confirmed approach)**:
Implemented a hand-rolled JSON-RPC 2.0 server rather than using rmcp's
transport. This gives us a single `Arc<Mutex<Stdout>>` that both the request
handler and the Discord-event notification task write to with mutual exclusion.
rmcp is still in Cargo.toml for its type/schema definitions but its transport
layer is not used. The `notification_tx: mpsc::Sender<Value>` field on
`DioneServer` is present for future use (server-push from tool handlers); the
primary path for notifications is the `event_rx` channel bridged from Discord.

**`PermissionError::Http` boxed to avoid `result_large_err` clippy lint**:
`serenity::Error` is 136+ bytes. Rather than boxing the entire error, we
switched `Http(#[from] serenity::Error)` to `Http(String)` — all call sites
already used `match` with manual error logging rather than `?` propagation, so
no code had to change.

**Attachment download via `curl` spawn**:
No `reqwest` in our Cargo.toml. Rather than adding it, `download_attachment`
uses `tokio::task::spawn_blocking` + `std::process::Command::new("curl")` to
fetch attachment bytes. This avoids a new dependency at the cost of requiring
`curl` in the runtime environment (acceptable for a server bot).

**`AttachmentMeta.size` as `u64`**:
serenity's `Attachment.size` is `u32`; our `AttachmentMeta.size` is `u64` to
be forward-safe. The conversion uses `u64::from(a.size)` at the boundary.

**`Timestamp::to_rfc3339` returns `Option<String>`**:
Used `.unwrap_or_default()` (returns empty string) rather than panicking.

**`MessageReference` from `(ChannelId, MessageId)` tuple**:
`CreateMessage::reference_message` requires `impl Into<MessageReference>`.
`MessageId` alone does not implement that trait; the tuple
`(ChannelId, MessageId)` does. Fixed in `tools/messaging.rs::reply`.

**`set_presence` requires shard manager**:
Presence updates need the Gateway connection, not just `Http`. Added a
`DiscordCommand` enum and a `discord_cmd_tx: Option<mpsc::Sender<DiscordCommand>>`
channel that the MCP server can use to request gateway operations. The channel
is currently wired as `None` in main.rs — the Discord task would need a `recv`
loop to handle it. This is intentional: the tool returns a clear error message
if the channel is absent, and full presence support can be added when the
Discord task is extended to run a command loop alongside `client.start()`.

**`cargo xfmt` not installed**:
`cargo xfmt` (custom import grouper) is not installed in this environment.
Used standard `cargo fmt` instead. Import grouping is close to the project
style but not exact.

**Guild-message Queue path**:
`InboundGate::check_guild` never returns `GateDecision::Queue` (it returns
`Deliver` or `Drop`). The `Queue` arm in the guild message handler in
`events.rs` logs a debug trace but takes no action — this is correct and
intentional per the gate design.

**`PrivateChannel::name()` method**:
`Channel::Private(c)` exposes `.name()` as an owned `String`. Used directly
in `introspection.rs::channel_name`.

### Files created in Pass 2
- `src/discord/events.rs` — Handler, NotificationEvent, AttachmentMeta
- `src/discord/client.rs` — `build_client`
- `src/permissions.rs` — `send_permission_request`, `validate_response`
- `src/mcp/server.rs` — `DioneServer`, `run`, full JSON-RPC dispatch
- `src/mcp/tools/messaging.rs` — reply, react, edit_message, fetch_messages, download_attachment, get_message
- `src/mcp/tools/introspection.rs` — list_guilds, list_channels, get_channel, get_user, get_member, list_roles
- `src/mcp/tools/management.rs` — pin_message, unpin_message, create_thread, delete_message
- `src/mcp/tools/access.rs` — list_access_requests, approve_access, deny_access
- `src/mcp/tools/bot_state.rs` — set_presence, send_typing
- `src/main.rs` — full bootstrap

---

## 2026-05-17 — Code-Review Fix Pass

Applied 18 findings from the post-scaffold code review. All 40 tests still pass.
`cargo clippy -- -D warnings` clean. `cargo build --release` succeeds.

### P1 Critical

**Fix 1 — Path traversal in `sanitize_filename` (`gate.rs`)**
The original implementation only stripped `[\[\]\r\n;]` but left `/` and `..` intact,
allowing path traversal when writing attachments to the inbox directory.
Fixed by: (a) keeping the char filter, then (b) extracting only the `file_name()`
component via `Utf8Path::new(&filtered).file_name()` — this strips any directory
prefix including `..` components. Empty results fall back to `"attachment"`.
Added test cases for `../../etc/passwd`, `/absolute/path/`, and empty input.

**Fix 2 — Missing outbound gate on `send_typing` (`bot_state.rs`)**
`send_typing` could be used to probe arbitrary channel IDs. Added `state: State` and
`state_dir: Utf8PathBuf` fields to `BotStateCtx`; `send_typing` now calls
`OutboundGate::check_channel` before issuing the typing request.

**Fix 3 — Missing outbound gate on management tools (`management.rs`)**
`pin_message`, `unpin_message`, `create_thread`, and `delete_message` had no
outbound gate. Added `state: State` and `state_dir: Utf8PathBuf` to `ManagementCtx`.
Extracted an `async fn check_outbound(ctx, channel_id) -> Result<(), Value>` helper
and called it at the top of each tool function.

### P2 Important

**Fix 4 — `validate_response` always returned `true` for `granted` (`permissions.rs`)**
Changed signature from `Result<(String, bool), PermissionError>` to
`Result<String, PermissionError>`. The `granted` boolean is now determined solely by
the caller (`events.rs`) from the button `custom_id` — `validate_response` only
validates admin identity and resolves the `request_id`.

**Fix 5 — Race in `approve_access` (`access.rs`, `queue.rs`)**
The original code removed the request from the queue first, then wrote config. If the
config write failed, the request was silently lost. Reordered: now peeks (new
`AccessQueue::peek` method) to verify existence, writes config via `add_to_allow_from`
(returns error on failure without touching the queue), then removes from the queue
only on config success.

**Fix 6 — `notif_task.abort()` (`server.rs`)**
Hard-aborting the notification task could drop queued events. Replaced with a
graceful shutdown: `drop(server)` closes the `DioneServer` (including its
`notification_tx`), which signals the channel is closed. The task drains remaining
events and exits when the channel is empty. A 500ms `tokio::time::timeout` guards
against a stall.

**Fix 7 — Regex recompiled every message (`gate.rs`)**
`MentionDetector::is_mentioned` was creating a new `Regex` per pattern per message.
Replaced with `regex::RegexSet::new(valid_patterns)` which compiles all patterns in
one shot and uses a DFA optimized for "does any pattern match?" queries. Invalid
patterns are still warned-on and skipped before building the set.

**Fix 8 — No periodic pruning timer (`main.rs`)**
Spawned a background task with `tokio::time::interval(Duration::from_secs(60))` that
calls `prune_stale_permissions()` on the state and `prune_expired(expiry)` on the
queue. Expiry duration comes from `config.access_requests.expiry_seconds`. The task
is CancellationToken-aware and exits cleanly on shutdown.

**Fix 9 — Unused `rmcp` in Cargo.toml**
Removed `rmcp = { version = "1", ... }`. The MCP server is implemented by hand over
stdio. The removal exposed a missing tokio feature: `rmcp` was transitively enabling
`tokio/io-std` (for `tokio::io::stdin`/`stdout`). Added `"io-std"` explicitly to our
tokio feature list.

**Fix 10 — Tool errors missing `isError: true` (`server.rs`)**
In `call_tool`, after wrapping the tool result, we now check `result.get("error").is_some()`.
If present, `"isError": true` is added to the MCP tool-response envelope, per the MCP spec.

**Fix 11 — No download size limit (`messaging.rs`)**
Added `"--max-filesize", "26214400"` to the curl args (25 MB = 25 × 1024 × 1024).
curl aborts with an error if the server signals a larger content-length.

**Fix 12 — `interaction_create` ordering (`events.rs`)**
Previously the `PermissionResponse` event was sent on `self.tx` before the Discord
`create_response` acknowledgment. If the acknowledgment failed, the MCP layer would
act on a permission that was never confirmed to the user. Now the acknowledgment
happens first; the event is only forwarded if `create_response` succeeds.

### P3 Suggestions

**Fix 13 — `is_allowed` made public; `is_admin` helper added (`gate.rs`)**
`is_allowed` was a private function. Made it `pub` so callers in `permissions.rs` and
`events.rs` can use it directly. Added `pub fn is_admin(config, user_id) -> bool` as a
thin wrapper checking `config.access.admins`.

**Fix 14 — DRY in `check_dm` (`gate.rs`)**
Restructured the nested `match` into an early-return for `Disabled`, then an
early-return for `is_allowed`, then a final `match` on the remaining policies.
Eliminates duplicated `is_allowed` calls.

**Fix 15 — Gate-check boilerplate in messaging tools (`messaging.rs`)**
Extracted `async fn check_outbound(ctx: &MessagingCtx, channel_id: u64) -> Result<(), Value>`
and replaced six identical gate-check blocks with `if let Err(e) = check_outbound(ctx, channel_id).await { return e; }`.

**Fix 16 — `set_presence` removed from tools list (`server.rs`)**
`set_presence` is non-functional (the gateway command channel is `None` in production).
Removed it from `tools/list` response; the implementation stub remains in `bot_state.rs`
for future use. A comment in the tools list explains why it is omitted.

**Fix 17 — Queue `list()` returns chronological order (`queue.rs`)**
The `BTreeMap` was ordered by user_id (u64), which is arbitrary from the admin's
perspective. Changed `list()` to collect values and sort by `r.timestamp` so entries
are returned oldest-first — more useful for triage. Also added `peek(user_id)` to
support the `approve_access` reordering (Fix 5).

**Fix 18 — Chunker trim order (`chunker.rs`)**
Changed `rest.trim_start_matches('\n').trim_start_matches('\r')` to
`rest.trim_start_matches(['\n', '\r'])` — equivalent semantics (trim any leading
`\n` or `\r` characters), but uses clippy's preferred array-of-char form and requires
only a single pass.

---

## 2026-05-17 — LoadedConfig Migration

Completed the migration from raw `Config` to `LoadedConfig` throughout the codebase.

### What changed

`LoadedConfig` (introduced in `config.rs`) wraps `Config` and pre-parses string ID lists
into `HashSet<u64>` for O(1) membership tests, and pre-compiles mention regexes into a
`RegexSet` at load time. All gate functions (`InboundGate::check_dm`, `check_guild`,
`OutboundGate::check_channel`) were already updated to accept `&LoadedConfig`.

This pass fixed the remaining mismatches:

- **`gate.rs` tests**: `base_config()` still returned raw `Config`; added a `loaded()`
  helper that wraps it in `LoadedConfig::from_raw()`. All gate test call-sites updated.
- **`gate.rs` — removed `pub fn compile_mention_patterns`**: The function was duplicated
  between `gate.rs` (public) and `config.rs` (private, called at load time). The gate.rs
  copy is removed; callers now use `config.mention_patterns` (pre-computed field on
  `LoadedConfig`) instead of recompiling on every message.
- **`events.rs` guild-message path**: Replaced `crate::gate::compile_mention_patterns(&config)`
  with `config.mention_patterns.as_ref()` — avoids recompiling the regex set per message.
- **`events.rs` interaction handler**: Replaced the inline
  `config.access.admins.iter().any(|a| a == &user_id_str)` admin check with
  `config.is_admin(sender_id)` — the O(1) `HashSet`-backed method.
- **`permissions.rs`**: Updated `send_permission_request` and `validate_response` to take
  `&LoadedConfig` instead of `&Config`; `validate_response` now uses `config.is_admin(user_id)`.

### Tooling added

- `justfile` with `check`, `fmt`, `fmt-check`, `lint`, `test`, `install`, and `pre-push` targets.
- `.githooks/pre-commit` runs `cargo fmt --check` before every commit.

---

## 2026-05-17 — Test Coverage Pass

### Test strategy

Added 37 new tests (40 → 77 total) across four areas:

**MCP protocol (`tests/mcp_protocol.rs`, 19 tests)**:
Tested via `test_helpers` module gated on `feature = "test-helpers"` (added to
`Cargo.toml`). This exposes `handle_request`, `tools_list`, `initialize_response`,
and `event_to_notification` to integration tests without leaking them to production
binaries. Tests cover: initialize handshake shape, tools/list enumeration, unknown
method error, client notifications producing no response, gate-rejection path for
`send_typing`, missing tool name, and snapshot tests for all three notification
event types (message, reaction, permission_response) using `insta`.

**Queue persistence (`tests/queue_persistence.rs`, 6 tests)**:
End-to-end write → reload tests that simulate a process restart. Verifies the
atomic rename protocol leaves no `.tmp` file, that JSON round-trips correctly, and
that approve/deny operations are reflected on disk.

**Gate edge cases (`src/gate.rs`, 4 new unit tests)**:
`test_outbound_allows_dm_channel_in_map` (TC-10), `test_outbound_rejects_channel_
not_in_map_nor_config` (TC-11), `test_outbound_allows_opted_in_guild_channel`
(TC-11a), and `test_file_send_symlink_into_state_dir_rejected` (TC-41, `#[cfg(unix)]`).

**LoadedConfig methods (`src/config.rs`, 8 new unit tests)**:
`is_allowed` true/false, `is_admin` true/false, `channel_policy` known/unknown
channel, invalid non-numeric IDs silently skipped, empty allow_from + admins
functional (TC-62).

### Key decisions

- **`test-helpers` feature flag** (not `#[cfg(test)]`): `#[cfg(test)]` modules in
  a library crate are not visible to integration tests in `tests/`. A Cargo feature
  is the idiomatic solution — it adds zero overhead to production builds and is
  automatically enabled when running `cargo nextest run --features test-helpers`.
  The CI command should pass `--features test-helpers` to pick up MCP protocol tests.

- **No mock serenity HTTP**: Tools that require real Discord HTTP (reply, react, etc.)
  are not tested here — that requires a live bot token. The `send_typing` gate-rejection
  test exercises the outbound gate logic without reaching the HTTP layer.

- **Insta snapshots committed**: Three JSON snapshot files under `tests/snapshots/`
  pin the exact notification wire format. Changes to `event_to_notification` will
  fail CI until snapshots are reviewed and updated, giving intentional format changes
  a clear review step.
