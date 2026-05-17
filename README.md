# Dione

A Rust MCP channel server that bridges Discord to [Claude Code](https://claude.ai/code).

Dione connects to the Discord gateway, gates access, and exposes tools for
Claude to interact with Discord — replying, reacting, fetching history,
managing channels, and more. Claude Code provides inference; Dione provides
the Discord bridge.

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

## What it does

Dione is an MCP server (stdio transport) that Claude Code spawns via
`--channels`. It:

- Connects to Discord and listens for messages, reactions, and interactions
- Gates access: only configured users/channels reach Claude
- Queues messages from unknown senders for admin review
- Exposes 21 tools for Claude to interact with Discord
- Relays Claude Code permission prompts to admin via Discord buttons

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

Requires Rust 1.95+.

## License

MIT OR Apache-2.0
