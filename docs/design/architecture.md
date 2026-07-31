<!-- design-meta
status: draft
last-updated: 2026-05-17
phase: 3
-->

# Architecture

## Overview

Dione is a single Rust binary that acts as an MCP channel server, bridging
Discord to Claude Code. It is spawned by Claude Code via `--channels` and
communicates over MCP stdio transport. Discord connectivity is handled by
serenity (gateway + REST). The two long-lived tasks — MCP server and Discord
client — run concurrently on one tokio runtime.

## Key Decisions

### 1. Runtime topology

Two tasks on one tokio runtime:

- **MCP server task** — owns stdin/stdout, handles tool calls, dispatches
  notifications. This is the "main" task since Claude Code spawns and
  communicates with it.
- **Discord client task** — holds the gateway WebSocket, receives events,
  runs the gate check, and forwards approved events to the MCP task.

### 2. Inter-task communication

- **Inbound (Discord → MCP):** `tokio::sync::mpsc` channel from the event
  handler to the MCP server. Events are gated before entering the channel.
- **Outbound (MCP → Discord):** Tool handlers hold `Arc<serenity::Http>` and
  make REST API calls directly. No routing through the gateway task.
- **Shared state:** `Arc<RwLock<SharedState>>` containing recent-sent-IDs
  set, DM-channel-user map, and access request queue reference.

### 3. Config hot-reload

Config is re-read from disk (`config.toml`) at the start of every inbound
gate check. The file is small (< 1KB typically), so this is cheap.
`File::try_lock()` (Rust 1.89) ensures safe concurrent access if another
process is writing. Parse errors rename the file to `.corrupt-{timestamp}`
and fall back to defaults.

### 4. Access request queue

Lives in memory, persisted to `queue.json` on mutation. Loaded on startup.
Bounded: 50 entries max, 24h TTL per entry. Approval is a tool call from
Claude (`approve_access`), which adds the user to `allow_from` in the config
TOML and sends a confirmation DM.

### 5. Permission relay

Permission requests from Claude Code arrive as MCP notifications. Dione
formats them as Discord messages with Allow/Deny buttons and sends them to
admin DMs only (not all allowlisted users). Admin button clicks route back
through the gateway as `interactionCreate` events, are validated against
the admins list, and forwarded as MCP permission response notifications.

### 6. Shutdown coordination

`tokio_util::sync::CancellationToken` triggers on:
- stdin EOF (Claude Code closed the connection)
- SIGTERM / SIGINT

Shutdown sequence: cancel token fires → Discord client destroys gateway →
MCP server drains pending tool responses → process exits within 2s timeout.

### 7. Gateway intents

```rust
GatewayIntents::DIRECT_MESSAGES
| GatewayIntents::GUILDS
| GatewayIntents::GUILD_MESSAGES
| GatewayIntents::MESSAGE_CONTENT  // privileged — must be enabled in dev portal
| GatewayIntents::GUILD_MESSAGE_REACTIONS
| GatewayIntents::GUILD_VOICE_STATES  // for future voice channel support
```

### 8. Dependency choices

| Crate | Purpose | Notes |
|-------|---------|-------|
| `serenity` | Discord gateway + REST | Latest, raw event handlers (not poise) |
| `rmcp` | MCP server | Official Rust SDK, stdio transport |
| `tokio` | Async runtime | Full features |
| `tokio-util` | CancellationToken | Shutdown coordination |
| `serde` + `toml` | Config deserialization | |
| `serde_json` | Queue persistence, MCP payloads | |
| `tracing` + `tracing-subscriber` | Structured logging | env-filter |
| `thiserror` | Domain error types | |
| `color-eyre` | Rich panic reports in main | |
| `regex` | Mention pattern matching | |
| `chrono` | Timestamps | Minimal features (clock, serde) |
| `camino` | UTF-8 path types | |

Not used: `async-trait` (native async traits are available at the MSRV), `anyhow` (color-eyre
at boundaries, thiserror for domain).

### 9. Modern Rust features leveraged (1.89–1.93)

| Feature | Use |
|---------|-----|
| `str::ceil_char_boundary` (1.91) | Message chunking at valid UTF-8 split points |
| `BTreeMap::extract_if` (1.91) | Pruning expired access requests |
| `VecDeque::pop_front_if` (1.93) | Queue management |
| `File::try_lock()` (1.89) | Safe concurrent config reads |
| `Result::flatten()` (1.89) | API call chains |
| `fmt::from_fn` (1.93) | Custom formatters for Discord rendering |
| Native async traits (1.75+) | No `async-trait` crate needed |

Features stabilized after Rust 1.93 require an explicit MSRV change before use.

## Module Structure

```
src/
  main.rs              — tokio bootstrap, spawn tasks, shutdown coordination
  config.rs            — Config struct, TOML deserialization, env overlay, hot-reload
  gate.rs              — InboundGate, OutboundGate, MentionDetector
  state.rs             — SharedState, RecentSentIds, DmChannelMap
  queue.rs             — AccessQueue (in-memory + JSON persistence)
  permissions.rs       — PermissionRelay (format requests, route to admins, handle responses)

  mcp/
    mod.rs             — re-exports
    server.rs          — MCP server setup, tool registration, notification dispatch
    tools/
      mod.rs           — tool registry
      messaging.rs     — reply, react, edit_message, fetch_messages, download_attachment, get_message
      introspection.rs — list_guilds, list_channels, get_channel, get_user, get_member, list_roles
      management.rs    — pin_message, unpin_message, create_thread, delete_message
      access.rs        — list_access_requests, approve_access, deny_access
      bot_state.rs     — set_presence, send_typing

  discord/
    mod.rs             — re-exports
    client.rs          — serenity Client setup, intent registration
    events.rs          — EventHandler impl: message_create, reaction_add, interaction_create
    chunker.rs         — chunk() using str::ceil_char_boundary
```

## Diagrams

### System Context

Dione sits between two fundamentally different trust domains: Discord
(untrusted public users) and the local agent harness (trusted local process).
Claude Code consumes server-initiated MCP notifications directly. Codex mode
persists structured events to `codex-inbox.json`; the live worker leases up to
128 pending events from the highest-priority compatible lane as one durable
batch, connects to the app-server Unix socket using WebSocket, and resumes one
explicit thread. It derives active-turn state once from the resume receipt,
then maintains it from `turn/start` responses and lifecycle notifications
instead of rereading the full thread history before each delivery. The worker
uses `turn/start` or `turn/steer` and removes the whole batch atomically only
after app-server accepts delivery. Batch membership and the app-server client
message ID survive lease expiry and restart, so retries preserve order and
idempotent identity without absorbing later arrivals. Explicit MCP pull
consumers remain available, leases expire for redelivery, and a lifetime
filesystem lock enforces one Dione owner per Codex state directory.

```mermaid
C4Context
    title System Context — Dione Discord MCP Channel Server

    Person(user, "Discord User", "Sends DMs or guild messages to the bot")
    Person(admin, "Admin User", "Manages access queue, responds to permission requests via Discord buttons")
    System(claude, "Claude Code", "MCP host process; spawns Dione via --channels; sends tool calls, receives notifications")

    System_Boundary(dione_boundary, "Dione (Rust binary)") {
        System(dione, "Dione", "MCP channel server — gates Discord access, relays messages as MCP notifications, executes Discord actions on behalf of Claude")
    }

    System_Ext(discord_gw, "Discord Gateway", "WebSocket (WSS) — delivers message, reaction, and interaction events")
    System_Ext(discord_rest, "Discord REST API", "HTTPS — send messages, react, fetch history, manage channels")
    System_Ext(discord_cdn, "Discord CDN", "HTTPS — file attachment downloads")
    System_Ext(fs, "Filesystem", "~/.claude/channels/dione/ — config.toml, queue.json, inbox/ attachments")

    Rel(user, discord_gw, "DMs / guild messages / reactions")
    Rel(admin, discord_gw, "Button clicks (approve/deny), DMs")
    Rel(claude, dione, "Tool calls (reply, react, fetch, manage)", "MCP stdio")
    Rel(dione, claude, "Notifications (message, reaction, permission)", "MCP stdio")
    Rel(discord_gw, dione, "Events (messageCreate, reactionAdd, interactionCreate)", "WSS")
    Rel(dione, discord_rest, "Send message, react, fetch history", "HTTPS REST")
    Rel(dione, discord_cdn, "Download attachments", "HTTPS")
    Rel(dione, fs, "Read config, persist queue, write inbox/")
    Rel(fs, dione, "config.toml (hot-reload per message)")
```

### Component Diagram

The two long-lived tasks communicate exclusively through well-typed channels
and shared state. The mpsc channel is strictly one-directional (Discord → MCP),
while outbound actions use `Arc<Http>` directly from tool handlers.

```mermaid
graph TB
    subgraph claude_code["Claude Code (host process, MCP client)"]
        CC[Claude Code]
    end

    subgraph dione["Dione Binary (tokio runtime)"]
        subgraph mcp_task["MCP Server Task"]
            MCP_SRV["mcp/server.rs\nTool registration\nNotification dispatch"]
            subgraph tools["mcp/tools/"]
                T_MSG["messaging\n(reply, react, fetch)"]
                T_INT["introspection\n(list guilds/channels/users)"]
                T_MGT["management\n(pin, thread, delete)"]
                T_ACC["access\n(approve/deny queue)"]
                T_BOT["bot_state\n(presence, typing)"]
            end
        end

        subgraph discord_task["Discord Client Task"]
            D_CLIENT["discord/client.rs\nSerenity client\nIntent registration"]
            D_EVT["discord/events.rs\nmessageCreate\nreactionAdd\ninteractionCreate"]
            D_CHUNK["discord/chunker.rs\nMessage splitting"]
        end

        subgraph shared["Shared Infrastructure"]
            GATE["gate.rs\nDM gate\nGuild gate\nMention detection\nOutbound gate"]
            QUEUE["queue.rs\nIn-memory + JSON\nAccess request queue\n(cap 50, 24h TTL)"]
            PERM["permissions.rs\nPermission relay\nAdmin DM buttons"]
            STATE["state.rs\nArc<RwLock<SharedState>>\nSent IDs, DM channel map"]
            CONFIG["config.rs\nTOML hot-reload\nEnv overlay"]
            MAIN["main.rs\nBootstrap\nCancellationToken\nShutdown"]
        end

        subgraph comms["Inter-task Communication"]
            MPSC["tokio::sync::mpsc\nDiscord → MCP"]
            HTTP["Arc<serenity::Http>\nMCP tools → Discord REST"]
        end
    end

    subgraph datastores["Data Stores (filesystem)"]
        FS_CFG["config.toml"]
        FS_QUEUE["queue.json"]
        FS_INBOX["inbox/"]
    end

    subgraph external["External Systems"]
        DGW["Discord Gateway (WSS)"]
        DREST["Discord REST API (HTTPS)"]
    end

    CC <-->|"MCP stdio"| MCP_SRV
    MCP_SRV --- tools
    T_MSG & T_INT & T_MGT & T_ACC & T_BOT --> HTTP
    HTTP -->|"REST calls"| DREST

    DGW -->|"Events"| D_CLIENT
    D_CLIENT --> D_EVT
    D_EVT --> GATE
    GATE -->|"Approved"| MPSC
    MPSC --> MCP_SRV

    D_EVT --> PERM
    PERM --> HTTP

    T_MSG --> D_CHUNK
    D_CHUNK --> HTTP

    T_ACC --> QUEUE
    QUEUE <-->|"persist"| FS_QUEUE
    CONFIG <-->|"hot-reload"| FS_CFG
    T_MSG -->|"attachments"| FS_INBOX

    tools --> STATE
    D_EVT --> STATE
    GATE --> CONFIG
    GATE --> STATE

    MAIN -->|"spawn"| mcp_task
    MAIN -->|"spawn"| discord_task
```

### Data Flow with Trust Boundaries

Each boundary crossing is a potential attack surface. Boundary 1 (Discord →
Dione) is where untrusted user input must be sanitized and gated. Boundary 3
(Claude Code → Dione) is where tool call arguments must be validated against
the outbound gate before any Discord action is taken.

```mermaid
flowchart TD
    subgraph TB0["Boundary 0: External — Untrusted Internet"]
        DU["Discord User\n(DM / guild / reaction)"]
        AU["Admin User\n(button click / DM)"]
    end

    subgraph TB1["Boundary 1: Discord Platform — Semi-trusted"]
        DGW["Discord Gateway (WSS)"]
        DREST_IN["Discord CDN (attachment URLs)"]
    end

    subgraph TB2["Boundary 2: Dione — Trusted Enforcement Point"]
        subgraph inbound["Inbound Pipeline"]
            EVT["Event Handler"]
            GATE_IN["Inbound Gate\n· DM allowlist\n· Guild opt-in\n· Mention detection"]
            QUEUEING["Access Queue\n· Unknown → queue\n· Cap 50 / 24h TTL"]
            PERM_RELAY["Permission Relay\n· Admin identity check"]
        end
        subgraph outbound_pipe["Outbound Pipeline"]
            GATE_OUT["Outbound Gate\n· Destination check\n· File send guard"]
            CHUNKER["Chunker\n· ceil_char_boundary"]
            HTTP_CLIENT["HTTP dispatch"]
        end
        NOTIF["MCP Notification"]
    end

    subgraph TB3["Boundary 3: Claude Code — Trusted Host"]
        CLAUDE["Claude Code (MCP client)"]
    end

    subgraph TB4["Boundary 4: Discord Outbound"]
        DREST_OUT["Discord REST API"]
    end

    subgraph TB5["Boundary 5: Filesystem — Local"]
        CFG["config.toml"]
        QJSON["queue.json"]
        INBOX["inbox/"]
    end

    DU -->|"message / reaction"| DGW
    AU -->|"button click"| DGW

    DGW -->|"events ⚠ TB1→TB2"| EVT
    DREST_IN -->|"attachment download ⚠ TB1→TB2"| INBOX

    EVT --> GATE_IN
    GATE_IN -->|"allowed"| NOTIF
    GATE_IN -->|"unknown"| QUEUEING
    GATE_IN -->|"admin interaction"| PERM_RELAY
    QUEUEING <-->|"persist ⚠ TB2↔TB5"| QJSON
    PERM_RELAY -->|"response ⚠ TB2→TB3"| NOTIF
    CFG -->|"hot-reload ⚠ TB5→TB2"| GATE_IN

    NOTIF -->|"MCP notification ⚠ TB2→TB3"| CLAUDE

    CLAUDE -->|"tool call ⚠ TB3→TB2"| GATE_OUT

    GATE_OUT --> CHUNKER
    CHUNKER --> HTTP_CLIENT
    GATE_OUT -->|"manage access"| QUEUEING
    GATE_OUT -->|"download ⚠ TB2→TB5"| INBOX

    HTTP_CLIENT -->|"send ⚠ TB2→TB4"| DREST_OUT
    PERM_RELAY -->|"admin DM ⚠ TB2→TB4"| DREST_OUT
```

### Sequence: Inbound Message Flow

The gate check runs synchronously in the event handler before any data crosses
into the MCP layer. The outbound gate on the reply path is a second independent
enforcement point.

```mermaid
sequenceDiagram
    actor User as Discord User
    participant DGW as Discord Gateway
    participant EVT as events.rs
    participant GATE as gate.rs
    participant MPSC as mpsc channel
    participant MCP as mcp/server.rs
    participant CLAUDE as Claude Code
    participant GOUT as Outbound Gate
    participant CHUNK as chunker.rs
    participant REST as Discord REST API

    User->>DGW: sends message
    DGW->>EVT: messageCreate event
    EVT->>GATE: check_inbound(author_id, channel_id)
    alt sender allowed
        GATE-->>EVT: Deliver
        EVT->>MPSC: NotificationEvent::Message { ... }
        MPSC->>MCP: recv()
        MCP->>CLAUDE: notifications/claude/channel
        CLAUDE->>MCP: tools/call: reply { chat_id, text }
        MCP->>GOUT: validate(chat_id)
        GOUT-->>MCP: valid
        MCP->>CHUNK: chunk(text, limit)
        loop each chunk
            MCP->>REST: POST /channels/{id}/messages
            REST-->>MCP: message_id
        end
        MCP-->>CLAUDE: tool result: { sent_ids }
    else unknown sender
        GATE-->>EVT: Queue
        EVT->>EVT: enqueue access request
    else denied
        GATE-->>EVT: Drop
    end
```

### Sequence: Access Request Flow

Unknown senders are queued rather than dropped. Admin notification is
rate-limited. Approval happens via MCP tool call, keeping the workflow
inside Claude Code.

```mermaid
sequenceDiagram
    actor Unknown as Unknown User
    participant DGW as Discord Gateway
    participant EVT as events.rs
    participant GATE as gate.rs
    participant QUEUE as queue.rs
    participant MCP as mcp/server.rs
    participant CLAUDE as Claude Code
    participant REST as Discord REST API

    Unknown->>DGW: DM to bot
    DGW->>EVT: messageCreate
    EVT->>GATE: check_inbound(unknown_id)
    GATE-->>EVT: Queue

    EVT->>QUEUE: enqueue(user_id, message_preview, timestamp)
    QUEUE->>QUEUE: persist to queue.json

    alt within rate limit
        QUEUE->>MCP: admin notification (pending_count)
        MCP->>CLAUDE: notifications/claude/channel (admin alert)
        CLAUDE->>MCP: tools/call: list_access_requests
        MCP-->>CLAUDE: [{ user_id, username, preview, age }]
        CLAUDE->>MCP: tools/call: approve_access { user_id }
        MCP->>QUEUE: approve(user_id)
        QUEUE->>QUEUE: add to allow_from, persist
        MCP-->>CLAUDE: ok
        MCP->>REST: DM to user: "Access granted"
    end
```

### Sequence: Permission Relay

Permission relay originates from Claude Code sending a notification *to* Dione
(reversed direction). Dione formats it as Discord buttons for admins. The
admin's button click returns through the gateway.

```mermaid
sequenceDiagram
    participant CLAUDE as Claude Code
    participant MCP as mcp/server.rs
    participant PERM as permissions.rs
    participant STATE as SharedState
    participant REST as Discord REST API
    actor Admin as Admin User
    participant DGW as Discord Gateway
    participant EVT as events.rs

    CLAUDE->>MCP: permission_request { request_id, tool_name, description }
    MCP->>PERM: relay_permission_request(payload)
    PERM->>STATE: get admin DM channels
    STATE-->>PERM: [admin_channel_ids]

    loop each admin
        PERM->>REST: send message with [Allow] [Deny] buttons
        REST-->>PERM: message_id
        PERM->>STATE: record pending { message_id → request_id }
    end

    Admin->>DGW: clicks Allow button
    DGW->>EVT: interactionCreate
    EVT->>STATE: lookup pending permission
    EVT->>EVT: verify user ∈ admins
    alt verified
        EVT->>REST: acknowledge interaction
        EVT->>MCP: PermissionResponse { request_id, granted: true }
        MCP->>CLAUDE: notifications/claude/channel/permission
    else not admin
        EVT->>REST: ephemeral "Not authorized"
    end
```

## Data Stores

| Store | Location | Format | Purpose |
|-------|----------|--------|---------|
| `config.toml` | `$DIONE_STATE_DIR/config.toml` | TOML | Access policy, delivery config, admin list |
| `queue.json` | `$DIONE_STATE_DIR/queue.json` | JSON | Pending access requests (survives restart) |
| `inbox/` | `$DIONE_STATE_DIR/inbox/` | Binary files | Downloaded attachments |

Default `$DIONE_STATE_DIR`: `~/.claude/channels/dione/`

## Error Handling Strategy

| Layer | Approach |
|-------|----------|
| `main.rs` | `color_eyre::install()`, catch-all for setup errors |
| Discord events | Log + continue. Never panic on API errors. |
| MCP tool calls | Return `isError: true` with message. Never panic. |
| Config parse | Rename corrupt file, fall back to defaults, log at error level |
| Queue persistence | Best-effort write; in-memory state is authoritative |
| Shutdown | 2s timeout; force-exit if tasks don't terminate |
## GAIE Atom 1b backfill architecture

The one-shot archive path gains a discovery phase before its existing message
producer:

1. Build a typed capture root from the configured corpus, guild, and parent.
2. Collect active snapshot A, paginate public/private archives according to
   route-specific cursor rules, and collect active snapshot B.
3. Validate every candidate's guild, parent, and thread type and construct an
   opaque verified capture target. No raw child ID crosses into message fetch.
4. Union and deduplicate targets, then sort parent first and thread snowflakes
   numerically.
5. Load or migrate checkpoint v2 and fetch each stream from its independent
   cursor. New streams begin with a nullable cursor.
6. Sort messages numerically, deduplicate by Discord-global message ID, append
   and fsync each message batch, then advance only the source stream cursor.

Discovery precedes archive mutation in default mode. This keeps an incomplete
enumeration from producing an archive that appears complete. The two active
snapshots bound—but cannot eliminate—the Discord race, so coverage receipts
must say principal-visible and non-atomic.
