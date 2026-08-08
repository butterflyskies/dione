<!-- design-meta
status: approved
last-updated: 2026-05-17
phase: 1
-->

# Problem Space

## What are we solving?

The official Claude Code Discord plugin (TypeScript/Bun) works but has limitations:

1. **Limited Discord introspection** — Claude can reply, react, and fetch history,
   but can't discover what guilds/channels the bot is in, look up users, or reason
   about the Discord topology.
2. **Blunt permission routing** — permission requests spam all allowlisted DM users,
   not just designated admins.
3. **Runtime overhead** — Bun process + discord.js for what's essentially a bridge.
4. **Not extensible in your stack** — the plugin is TypeScript you don't control.

Dione replaces this with a Rust-native MCP channel server that provides a richer
tool surface, proper admin/user separation, and config-driven behavior.

## Why now

Claude Code's `--channels` mechanism + memory-mcp make it cost-effective to run an
AI Discord presence without a separate Anthropic API key. Dione is the bridge layer
that makes this work — it owns Discord connectivity and access control while Claude
Code provides inference and memory-mcp provides persistence.

The existing Discord bot token (already configured in the Developer Portal with
Message Content Intent and guild permissions) will be reused.

## Inputs and outputs

| Direction | Data | Protocol |
|-----------|------|----------|
| Discord → Dione | Gateway events (messages, interactions) | WSS (serenity) |
| Dione → Claude Code | Channel notifications | MCP stdio (`notifications/claude/channel`) |
| Claude Code → Dione | Tool calls | MCP stdio (`tools/call`) |
| Claude Code → Dione | Permission requests | MCP notification |
| Dione → Discord (admins) | Permission relay (buttons) | Discord API |
| Discord (admins) → Dione | Permission responses (button clicks) | Gateway interaction |
| Config file | Access policy, delivery settings | TOML (hot-reload per message) |

Key transformation: Discord messages are gated (access control), metadata is
sanitized, and the message is forwarded as an MCP channel notification. Claude
Code's tool calls are validated against the outbound gate before executing Discord
API actions.

## Boundaries

### In scope

- Discord gateway connection and lifecycle (serenity)
- MCP server (stdio transport, `claude/channel` + `claude/channel/permission`)
- Access control (pairing, allowlist, guild opt-in, mention detection)
- Tool execution: messaging, reactions, editing, history, attachments,
  guild/channel/user introspection
- Message chunking and delivery configuration
- Permission relay routed to admins only
- A transport-native `/roll` application command that completes without LLM
  inference or construct notification delivery
- TOML config with hot-reload (re-read per inbound message)

`/roll` keeps up to 4,096 full local receipts for retry and crash recovery.
Deleting the Discord response does not immediately delete its local receipt.
At capacity, Dione replaces the oldest fully published receipt with an exact
interaction-ID tombstone retained for 24 hours, comfortably beyond Discord's
interaction-token lifetime. This prevents a replay from sampling again without
retaining its expression, faces, actor, or channel. Tombstones are exact rather
than a scalar snowflake watermark, so delayed out-of-order interactions are not
mistaken for previously handled ones.

### Out of scope (future phases)

- General-purpose slash commands / application commands beyond `/roll`
- Voice channel interaction
- Scheduled messages / reminders (handled by Claude Code scheduling)

### Out of scope (handled elsewhere)

- LLM inference — Claude Code
- Memory / personality — memory-mcp + Claude Code system prompt
- Model routing / metering — Claude Code

### Adjacent systems

| System | Relationship |
|--------|-------------|
| Claude Code | Host process; MCP client; provides inference |
| memory-mcp | Separate MCP server in Claude Code's stack; provides persistence |
| Discord API / Gateway | External service; message source and sink |
| Filesystem (`~/.claude/channels/dione/`) | Config, pairing state, attachment inbox |

### Constraints

- Must work with Claude Code's `--channels plugin:...` mechanism
- Discord Message Content Intent is privileged (already enabled)
- Discord rate limits on API calls (especially message send)
- MCP stdio transport — single connection, not multiplexed
- Discord hard limit: 2000 chars per message
- Config changes take effect without restart (hot-reload)

## Success criteria

1. Drop-in replacement for the TypeScript plugin — same `--channels` invocation,
   same MCP channel notification protocol.
2. Richer tool surface — Claude can discover and reason about Discord topology
   (guilds, channels, users, roles).
3. Permission relay respects admin/user separation — only admins receive
   permission requests, not all DM-approved users.
4. Stable long-running process — no zombie states on stdin EOF, clean shutdown
   on SIGTERM/SIGINT.
5. Config-driven — all policy changes via TOML, no restart required.
6. Single static binary, low resource footprint.
## GAIE owned-thread backfill

Archiving only an allowlisted Discord parent channel omits messages whose
canonical message endpoint belongs to an owned thread. The product boundary is
one capture root, not one HTTP channel: the configured parent plus every active
or archived thread visible to the authenticated principal and verified to have
that exact parent. The system must close this gap without widening the
allowlist, claiming globally complete Discord history, or accepting arbitrary
child channel IDs at the message-fetch boundary.

Discord thread discovery is not an atomic snapshot. Atom 1b therefore promises
principal-visible, non-atomic coverage using active snapshot A, archived-page
enumeration, and active snapshot B. The failure to enumerate or validate that
set is a failure of the default backfill, not permission to silently archive the
parent alone. Parent-only capture remains an explicit `allow_partial`
break-glass mode.
