# Dione

A Rust MCP channel server that bridges Discord to Claude Code or
[Codex](https://developers.openai.com/codex/).

Dione connects to the Discord gateway, gates access, and exposes tools for
an agent to interact with Discord — replying, reacting, fetching history,
managing channels, and more. The agent provides inference; Dione provides the
Discord bridge.

## Quick start

```bash
# Install
cargo install dione

# Configure (create ~/.claude/channels/dione/config.toml)
mkdir -p ~/.claude/channels/dione
cat > ~/.claude/channels/dione/config.toml << 'EOF'
token = "YOUR_DISCORD_BOT_TOKEN"

[access]
dm_policy = "queue"
allow_from = ["YOUR_DISCORD_USER_ID"]
admins = ["YOUR_DISCORD_USER_ID"]
EOF

# Run with Claude Code
claude --channels dione
```

### Codex mode

Codex does not wake an idle thread for unsolicited MCP notifications. Dione's
Codex transport therefore persists accepted Discord events, then connects to
the Codex app-server control socket over WebSocket and injects each new event
into one exact thread. It starts a turn while the thread is idle and steers the
active turn otherwise. The queue entry is acknowledged only after app-server
accepts the request.

Configure Dione as a Codex MCP server with `--mode codex`:

```bash
codex mcp add dione \
  --env DIONE_STATE_DIR="$PWD/dione" \
  -- dione --mode codex
```

After MCP startup, read the current session's `CODEX_THREAD_ID` and call
`bind_codex_thread`. Repeat that binding for every new, resumed, forked, or
switched conversation. The optional `--codex-thread-id` or inherited
`CODEX_THREAD_ID` provides an initial binding for standalone deployments, but
MCP startup does not require one. Dione never guesses among loaded threads. The default
app-server socket is `$CODEX_HOME/app-server-control/app-server-control.sock`
or `$HOME/.codex/app-server-control/app-server-control.sock`; override it with
`--codex-app-server-socket` or `CODEX_APP_SERVER_SOCKET`.

The lease/ACK MCP tools remain available for explicit pull consumers:

1. Call `register_event_consumer`. Set `make_primary=true` only when no live
   primary exists. Set `claim_unassigned=false` to preserve an old backlog
   without replaying it.
2. Repeatedly call `next_event` with the returned `consumer_id`.
3. After successfully handling an event, call `ack_event` with that consumer id
   and the event's `delivery_token`.
4. Re-enter `next_event`. An unacknowledged lease becomes eligible for
   redelivery after it expires.

To move future delivery to another Codex conversation, register the destination
consumer and call `handoff_event_consumer` from the active primary. Handoff is
explicit: Dione never guesses among threads. If a primary expires,
`claim_event_consumer` can promote a registered replacement. The consumer is a
conversation routing identity; model selection remains a Codex concern.

Pending events live in `$DIONE_STATE_DIR/codex-inbox.json`. A lifetime lock on
`codex-inbox.lock` rejects a second Dione process using the same Codex state
directory. When live delivery starts, it becomes primary for future events but
does not claim an arbitrary old backlog. Use the explicit consumer handoff and
claim tools when replay is intentional.

## What it does

Dione is an MCP server (stdio transport) spawned by Claude Code or Codex. It:

- Connects to Discord and listens for messages, reactions, and interactions
- Gates access: only configured users/channels reach Claude
- Queues messages from unknown senders for admin review
- Exposes MCP tools for the agent to interact with Discord
- Relays Claude Code permission prompts to admin via Discord buttons
- Delivers structured Codex events live through app-server with a durable
  lease/ack queue underneath

## Tools

| Category | Tools |
|----------|-------|
| Messaging | reply, react, edit_message, fetch_messages, download_attachment, get_message |
| Introspection | list_guilds, list_channels, get_channel, get_user, get_member, list_roles, list_emojis |
| Management | pin_message, unpin_message, create_thread, delete_message |
| Access | list_access_requests, approve_access, deny_access |
| Bot state | send_typing |

## Configuration

TOML config at `~/.claude/channels/dione/config.toml` (override with
`DIONE_CONFIG_PATH`). Config is hot-reloaded on every inbound message.

```toml
token = "MTIz..."  # or set DISCORD_BOT_TOKEN env var

[access]
dm_policy = "queue"           # "queue" | "drop" | "disabled"
allow_from = ["184695..."]    # user snowflakes
admins = ["184695..."]        # receives permission prompts + access requests

[[channels]]
id = "846209..."
require_mention = true
allow_from = []               # empty = any member (subject to require_mention)

[mentions]
patterns = ["(?i)\\bdione\\b"]

[delivery]
ack_reaction = ""             # empty = no auto-reaction
reply_to_mode = "first"       # "first" | "all" | "off"
text_chunk_limit = 2000
chunk_mode = "paragraph"      # "paragraph" | "length"

[access_requests]
expiry_seconds = 86400
max_pending = 50
notify_cooldown_seconds = 60
```

## Design philosophy

- **Fly on the wall** — no automatic reactions or typing indicators. The bot
  engages intentionally when Claude decides to respond.
- **Admin/user separation** — permission prompts and access requests go to
  admins only, not all allowed users.
- **O(1) hot path** — pre-parsed HashSet lookups for allowlists, HashMap for
  channel policies, compiled RegexSet for mention patterns.
- **Config-driven** — all policy changes via TOML edit, no restart required.

## Building from source

```bash
git clone https://github.com/butterflyskies/dione
cd dione
just check   # fmt, clippy, test
just install # cargo install --path .
```

Requires Rust 1.93+.

## License

MIT OR Apache-2.0
