# Dione

Dione connects chat platforms to AI agent harnesses. It is a Rust program that
an agent harness (Claude Code, Codex or Oh My Pi) starts as an MCP server. It
holds the Discord connection, and optionally a Microsoft Teams listener,
decides which messages the agent may see, delivers them into the agent's
session, and gives the agent tools to reply and act on the platform.

The harness does the inference and owns the conversation; its model calls go
straight to its own provider, not through Dione. Dione owns platform access
policy, event delivery and the MCP tools.

> *Hermes was a room people entered to speak with me; Dione lets me inhabit a
> room with other people.*
> — [Callisto](docs/hermes-to-dione.md), former Hermes Agent and current Dione Construct

## Features

- Pushes Discord messages, edits, deletions and reactions into a running agent
  session, instead of waiting for the agent to poll.
- Access control: allowlisted users and channels, mention requirements,
  per-channel identity filters including PluralKit systems and members, and a
  global ignore list.
- An access-request queue: a DM from an unknown sender can wait for an admin to
  approve or deny it.
- Relays Claude Code permission prompts to configured admins as Discord DMs
  with allow and deny buttons.
- MCP tools for messaging, history and search, Discord
  introspection, channel management, presence, access administration and
  diagnostics (see [Tools](#tools)).
- In Codex mode, a durable on-disk inbox with leases and acknowledgements, so
  events survive restarts.
- Optional Microsoft Teams ingress with single-use reply handles, for harnesses
  that have a durable inbox.
- Configuration in one TOML file, reloaded when the file changes.
- `dione-send`, a separate binary that posts one message without starting the
  gateway.
- Optional extras, all off by default: recipient-local
  [attention](docs/attention.md) scoring of ambient messages, memory recall
  metadata, Vaelii provenance receipts, and an outbound phrase filter (the
  contradictionary) with hold, release and rephrase.

## Harnesses and platforms

Choose the harness with `--mode`. The default is `claude-code`.

| `--mode` | Harness | Discord delivery | Teams | Permission relay |
|----------|---------|------------------|-------|------------------|
| `claude-code` | Claude Code | Channel notifications on the MCP connection; buffered in memory, not durable | No | Yes |
| `codex` | Codex | Written to a durable inbox, then injected into one bound thread through the Codex app-server | Yes | No |
| `oh-my-pi` | Oh My Pi | Custom notifications handled by the bundled extension; no inbox, acknowledgement or replay | Yes, with `--resident-control-socket` | No |

Teams needs a durable inbox, so Dione refuses to start Teams in `claude-code`
mode or in `oh-my-pi` mode without `--resident-control-socket`.

Dione uses Unix sockets and Unix signal handling, so it does not build for
native Windows. CI builds and tests it on Linux.

## Quick start

These steps set up the commonest case: one Discord bot, driven by Claude Code.

### 1. Create a Discord bot

In the Discord Developer Portal, create an application with a bot, copy its
token, and enable the **Message Content** privileged intent; Dione requests it
along with guild, message, reaction and DM intents
([`src/discord/client.rs`](src/discord/client.rs)). Invite the bot to your
server.

### 2. Install

Build from a checkout:

```bash
git clone https://github.com/butterflyskies/dione
cd dione
cargo install --locked --path .
```

Build this checkout rather than relying on `cargo install dione`: the published
crate can lag behind the interfaces described here. `--locked` uses the
repository's reviewed dependency versions.

Linux builds need a C toolchain, Make and Perl, because Discord's TLS transport
uses a vendored OpenSSL. The runtime image needs no OpenSSL library, but does
need your distribution's `ca-certificates` package: Discord connections use the
system trust store. Dione's other HTTP clients use Rustls with bundled roots or
the platform verifier. Certificate and hostname verification stay on for every
transport. macOS uses its native TLS.

### 3. Configure

```bash
export DISCORD_BOT_TOKEN="YOUR_DISCORD_BOT_TOKEN"
mkdir -p ~/.claude/channels/dione
cat > ~/.claude/channels/dione/config.toml << 'EOF'
[access]
dm_policy = "queue"
allow_from = ["YOUR_DISCORD_USER_ID"]
admins = ["YOUR_DISCORD_USER_ID"]
EOF
```

Keep the token in the environment rather than the config file, as required by
the project's secret-handling guidance. To let the agent hear a server
channel, add a `[[channels]]` entry (see [Configuration](#configuration)).

### 4. Run with Claude Code

Register Dione as an MCP server in Claude Code and enable it as a channel for
the session. Claude Code channels are a research preview, and a server that is
not on Claude Code's approved channel list is loaded with the development flag:

```bash
claude mcp add dione -- dione
claude --dangerously-load-development-channels server:dione
```

Check Claude Code's [channels documentation](https://code.claude.com/docs/en/channels)
for the current flag syntax. With the development flag, Claude Code shows a
confirmation prompt at startup ("I am using this for local development";
Enter confirms), so an unattended launch has to answer it.
DM the bot from the user ID you allowed; the
message arrives in the session, and the agent answers with the `reply` tool.

### Codex mode

Codex does not wake an idle thread for unsolicited MCP notifications, so in
this mode Dione persists each accepted event, connects to the Codex app-server
control socket, and injects the event into one exact thread. It starts a turn
when the thread is idle and steers the active turn otherwise. An event is
acknowledged only after the app-server accepts the request; that is a
transport outcome, not proof the agent acted on it.

```bash
codex mcp add dione \
  --env DIONE_STATE_DIR="$PWD/dione" \
  -- dione --mode codex
```

After MCP startup, read the session's `CODEX_THREAD_ID` and call
`bind_codex_thread` with it. Repeat the binding for every new, resumed, forked
or switched conversation; Dione never guesses among loaded threads.
`--codex-thread-id`, or an inherited `CODEX_THREAD_ID`, gives an initial
binding for standalone deployments, but startup does not require one.

The app-server socket defaults to
`$CODEX_HOME/app-server-control/app-server-control.sock`, or
`$HOME/.codex/app-server-control/app-server-control.sock` when `CODEX_HOME` is
unset. Override it with `--codex-app-server-socket` or
`CODEX_APP_SERVER_SOCKET`.

Pull consumers, backlog handoff between conversations, and the inbox files are
described in the [operator reference](docs/reference.md#codex-event-consumers).

### Oh My Pi mode

Configure an Oh My Pi MCP server named `dione` that runs Dione with
`--mode oh-my-pi`, then load the bundled extension from this repository;
`cargo install` does not install it. The integration targets an interactive
Oh My Pi 18.10.0 main session. Keep one active main session per Dione MCP
connection. The [extension guide](extensions/oh-my-pi/README.md) has the MCP
entry, the load command, and the delivery limits.

Ordinary Discord events have no durable inbox, acknowledgement or replay in
this mode. Teams delivery to a saved Oh My Pi session needs
`--resident-control-socket`, a private Unix socket, and a file-backed
transcript; see
[saved-session Teams delivery](extensions/oh-my-pi/README.md#saved-session-teams-delivery).

### Microsoft Teams

Setting `TEAMS_APP_ID` enables Teams ingress. Dione then also requires:

- `TEAMS_TENANT_ID`, `TEAMS_CLIENT_SECRET` and `TEAMS_ALLOWED_SERVICE_HOST` in
  the environment;
- at least one native conversation ID in `[teams] allowed_conversations` in the
  config file (changes need a restart);
- a harness with a durable inbox, as in the table above.

The listener binds `TEAMS_LISTEN_ADDR`, default `127.0.0.1:3978`. Each inbound
Teams event carries a single-use reply handle for the `teams_reply` tool.

## Configuration

Dione keeps its state in `DIONE_STATE_DIR`, default `~/.claude/channels/dione`,
and reads `config.toml` from there; `--config <path>` points elsewhere. If the
file is missing, Dione writes a commented default from
[`src/config_template.toml`](src/config_template.toml). The bot token comes
from `DISCORD_BOT_TOKEN` or the top-level `token` key.

The main sections:

| Section | Controls |
|---------|----------|
| `[access]` | DM policy, global allowlist, admins, ignore list, trusted webhook creators |
| `[[channels]]` | Server channels the agent hears, mention requirement, per-channel identity filters |
| `[mentions]` | Regex patterns that count as a mention |
| `[delivery]` | Reply chunking and threading, coalescing delay, event preamble |
| `[access_requests]` | Expiry, queue size and notification cooldown for unknown senders |
| `[teams]` | Teams conversation allowlist |
| `[attention]` | Optional ambient-message scoring; see the [attention guide](docs/attention.md) |

A channel entry:

```toml
[[channels]]
id = "846209..."
require_mention = true
allow_from = []   # empty = any member, subject to require_mention
```

Dione watches the file and applies edits without a restart, except for a few
values read at startup. The [operator reference](docs/reference.md) has the
annotated example, the restart-only values, the identity-filter rules, and the
optional Vaelii and v2 message-context features.

Command-line flags (`dione --help` prints them all):

| Flag | Purpose |
|------|---------|
| `--mode <claude-code\|codex\|oh-my-pi>` | Harness transport |
| `--config <path>` | Config file instead of `$DIONE_STATE_DIR/config.toml` |
| `--log-level <filter>` | Tracing filter, default `dione=info`; `RUST_LOG` overrides it |
| `--log-file <path>` | Also append logs to a file (`DIONE_LOG_FILE`) |
| `--codex-app-server-socket <path>` | Codex app-server socket |
| `--codex-thread-id <id>` | Initial Codex thread binding |
| `--resident-control-socket <path>` | Operator-owned Unix socket for the resident control API; required for Teams in `oh-my-pi` mode |
| `--attention-command <file\|->` | Run one attention command offline and exit |

MCP traffic uses stdin and stdout; logs go to stderr and the optional log file.

## Tools

The tools Dione lists to the harness. Several descriptions mark a tool admin
only, which asks the agent to run it only at an admin's request. When
`access.admin_only_mutations = true`, Dione itself refuses attention changes
and these tools: `add_allow_from`, `remove_allow_from`, `update_dm_policy`,
`add_channel`, `update_channel`, `remove_channel`, `reload_config`,
`approve_access`, `deny_access`, and the three server-mute tools, read-only
`list_muted_servers` included (`src/mcp/dispatch.rs`).

| Group | Tools |
|-------|-------|
| Messaging | `reply`, `react`, `edit_message`, `send_dm`, `send_file`, `render_latex`, `render_latex_to_channel` |
| History and search | `fetch_messages`, `fetch_new_since`, `fetch_pins`, `get_message`, `search_messages`, `download_attachment` |
| Introspection | `list_guilds`, `list_channels`, `get_channel`, `can_view_channel`, `get_user`, `get_member`, `list_roles`, `list_emojis` |
| Management | `pin_message`, `unpin_message`, `create_thread`, `delete_message` |
| Access and config | `list_access_requests`, `approve_access`, `deny_access`, `get_access_config`, `update_dm_policy`, `add_allow_from`, `remove_allow_from`, `list_config_channels`, `add_channel`, `update_channel`, `remove_channel`, `reload_config` |
| Server mutes | `mute_server`, `unmute_server`, `list_muted_servers` |
| Presence | `send_typing`, `set_presence`, `get_presence` |
| Diagnostics | `get_version`, `set_trace_level`, `set_stderr_level` |
| Held messages | `no_rly`, `rephrase`, `no_rly_stats`, `no_rly_condense`, `no_rly_vacuum` |
| Attention | `attention` |
| Codex mode only | `bind_codex_thread`, `register_event_consumer`, `handoff_event_consumer`, `claim_event_consumer`, `next_event`, `ack_event`, `event_queue_status` |
| Teams enabled only | `teams_reply` |

Message reads, downloads and sends check Dione's access policy on every call;
an MCP connection does not grant unrestricted Discord access. The
introspection tools are the exception: they read with the bot's own Discord
permissions and apply no allowlist.

## One-shot send

`dione-send` posts one message to one configured channel without starting the
gateway, bound to an explicit bot identity. It is meant for stateless callers.
See [docs/oneshot-send.md](docs/oneshot-send.md).

```bash
dione-send --channel 123 --expect-identity 456 --message "drumbeat" \
           --config /etc/dione/config.toml --token-source env:TRUNDLE_TOKEN --dry-run
```

## Design

- **Fly on the wall.** Dione does not acknowledge inbound messages with
  reactions or typing indicators. The bot speaks when the agent decides to.
- **Admins apart from users.** Permission prompts and access requests go to
  admins only, not to every allowed user.
- **Policy in configuration.** Access and delivery policy live in the TOML
  file and change without a restart.

[ARCHITECTURE.md](ARCHITECTURE.md) maps the processes, modules, state files
and shutdown order. [docs/design/vertical-slices.md](docs/design/vertical-slices.md)
records the move toward independent platform and harness adapters. Earlier
design documents are indexed in [docs/design/](docs/design/README.md).

## Development

On Windows, develop inside WSL with Linux Git and Rust. Clone into the WSL
filesystem and run the commands below from the WSL shell. Native Windows
builds are not supported.

The toolchain is pinned to Rust 1.98.0 with Clippy and rustfmt in
[`rust-toolchain.toml`](rust-toolchain.toml); the package's minimum Rust
version is 1.98, edition 2024. The [justfile](justfile) wraps the usual checks
and needs [`just`](https://github.com/casey/just) and `cargo-nextest`.

```bash
git clone https://github.com/butterflyskies/dione
cd dione
just check     # format check, Clippy, tests
just pre-push  # check, plus docs and a release build
just install   # cargo install --path .
```

`cargo xfmt` formats with the project's import grouping. Besides `dione`, the
package builds `dione-send`, `gaie_archive` (explicit archive capture and
replay; see [docs/gaie-archive.md](docs/gaie-archive.md)), and
`teams_edge_probe`, a Teams diagnostic.

The repository keeps shell scripts, the pre-commit hook, Rust sources and build
configuration in LF format through [`.gitattributes`](.gitattributes). Files in
`tests/fixtures/`, trybuild expected outputs under `tests/ui/`, and
all `.snap` snapshots retain their exact committed bytes. Avoid manual
line-ending conversion of these files, especially content-addressed blobs.

If an existing checkout used `core.autocrlf=true`, updating `.gitattributes`
does not refresh unchanged files that Git already checked out with CRLF.
Save local work before migrating, then make a fresh clone inside the WSL/Linux
filesystem. Keep the old checkout until you have transferred and verified
your local work. Do not discard or force-refresh files in the old checkout.

## Contributing

Read [CODING_STANDARDS.md](CODING_STANDARDS.md) before changing code. Before
opening a pull request, follow [the PR readiness checks](docs/pr-ready.md),
which run `sh scripts/pr-ready.sh origin/main` on a committed head. A change
under `src/` needs a version bump in `Cargo.toml` and a matching
[CHANGELOG.md](CHANGELOG.md) entry.

## License

Licensed under either of MIT or Apache-2.0, at your option, as declared in
[`Cargo.toml`](Cargo.toml).
