# Operator reference

Reference material for running Dione. The [README](../README.md) covers what
Dione is and how to start it; this page holds the longer details it links to.

## Configuration file

Dione reads TOML from `$DIONE_STATE_DIR/config.toml`. `DIONE_STATE_DIR`
defaults to `~/.claude/channels/dione`. Pass `--config <path>` to read the file
from somewhere else; the state directory still holds Dione's other files.

If the file does not exist at startup, Dione writes a commented default from
[`src/config_template.toml`](../src/config_template.toml). That template is the
annotated list of optional sections: Teams admission, archive, pre-send hooks,
Vaelii, memory recall (`bell_rings`), reply coordination, and the
contradictionary.

Dione watches the file and publishes a new configuration when it changes. The
admin-only `reload_config` tool forces a reload. Some values are read once at
startup and need a restart: Teams `allowed_conversations`, pronoun and nameplate
services, the access-request expiry used for pruning, and the Codex preamble
settings. See the state section of [ARCHITECTURE.md](../ARCHITECTURE.md#state-and-ownership).

The core access and delivery settings:

```toml
# Set DISCORD_BOT_TOKEN in the environment; do not put the token in this file.
# timezone = "America/Los_Angeles"  # IANA name for event timestamps; default UTC

[access]
dm_policy = "queue"             # "queue" | "drop" | "disabled"
allow_from = ["184695..."]      # user IDs allowed to reach the agent
admins = ["184695..."]          # receive permission prompts and access requests
ignore_from = []                # user IDs filtered everywhere; overrides allow_from
admin_only_mutations = false    # true blocks config-changing and attention-changing tools
trusted_webhook_creators = ["466378653216014359"]  # default: PluralKit

[[channels]]
id = "846209..."
require_mention = true
allow_from = []                 # empty = any member (subject to require_mention)
allow_pk_systems = []           # PluralKit system UUIDs or 5-char short IDs
allow_pk_members = []           # PluralKit member UUIDs or 5-char short IDs
# delivery_delay_ms = 0         # per-channel coalescing delay; overrides [delivery]
# coordinate = "claim-once"     # name of a [coordination.<name>] block

[mentions]
patterns = ["(?i)\\bdione\\b"]  # regexes that count as a mention

[delivery]
reply_to_mode = "first"         # "first" | "all" | "off"
text_chunk_limit = 2000
chunk_mode = "paragraph"        # "paragraph" | "length"
delivery_delay_ms = 0           # coalescing delay for channel events; 0 = none
evidence_markers_enabled = false  # opt in to Vaelii sentex locators
preamble_mode = "always"        # "always" | "first" | "never"
# preamble_template = "..."     # custom text, max 1024 bytes

[access_requests]
expiry_seconds = 86400
max_pending = 50
notify_cooldown_seconds = 60
```

DM policy: `queue` holds a message from an unknown sender as an access request
for admins to approve or deny; `drop` discards it; `disabled` drops all DMs,
including from `allow_from`.

Channel identity filter: if any of a channel's `allow_from`,
`allow_pk_systems` or `allow_pk_members` lists is non-empty, only matching
identities pass, and the lists combine with OR. For webhook messages, a missing
or failed observation of the webhook's creator drops the message. A creator
not listed in `trusted_webhook_creators` is treated as an app, admitted only
on channels with no identity filter. A PluralKit message is resolved to its
system and member, and a filtered channel requires an allowed identity.

Attention settings are described in the [attention guide](attention.md).

## Codex event consumers

In `--mode codex`, live delivery through the app-server is the normal path.
The lease and acknowledgement tools remain for explicit pull consumers:

1. Call `register_event_consumer`. Set `make_primary=true` only when no live
   primary exists. Leave `claim_unassigned=false` to keep an old backlog
   without replaying it.
2. Call `next_event` with the returned `consumer_id`.
3. After handling an event, call `ack_event` with that consumer ID and the
   event's `delivery_token`.
4. Call `next_event` again. An unacknowledged lease becomes eligible for
   redelivery after it expires.

To move future delivery to another Codex conversation, register the
destination consumer and call `handoff_event_consumer` from the active
primary. If a primary expires, `claim_event_consumer` can promote a registered
replacement. Dione never guesses among threads. The consumer is a conversation
routing identity; model selection remains a Codex concern.

Pending events live in `$DIONE_STATE_DIR/codex-inbox.json`. A lock on
`codex-inbox.lock`, held for the life of the process, rejects a second Dione
using the same state directory. When live delivery starts it becomes primary
for future events but does not claim an old backlog; use the handoff and claim
tools when replay is intentional. `event_queue_status` inspects the queue
without leasing.

Attention-managed ambient deliveries wait for an idle turn and never steer or
abort an active one. See the [attention guide](attention.md).

## Versioned message context

`get_message` and `fetch_messages` accept an optional `response_version: 2`.
Omitting it, or passing 1, keeps the original response. Other query tools keep
their existing responses.

```json
{"channel_id":"123","message_id":"456","response_version":2}
```

Version 2 returns source-qualified message and sender identities, creation and
edit times in UTC, reply-reference metadata, attachment metadata, and explicit
field availability. `native` preserves Discord-specific details. It does not
resolve represented webhook identities, fetch referenced message bodies, or
download attachments. Local content is untruncated; whether the provider
returned everything is unknown.

History takes the exclusive `before` or `after` cursor and `limit`. The page
reports the requested anchor separately from the observed message bounds.
`has_more_hint` means the page was full, not that another page exists. Access
and provider failures return finite error codes instead of raw provider
diagnostics. Access policy is checked on every call; a read does not grant
later permission to send.

Exact reads keep the optional Vaelii receipt below. Version 2 reports receipt
recording separately from read success. A v2 metadata validation error can
follow an attempted receipt; it does not imply that no receipt was written.

## Explicit-read Vaelii receipts

An explicit `get_message` call can record a compact provenance receipt in
Vaelii. The bridge is off unless `[vaelii].server_url` is set. Ambient
delivery, `fetch_messages` and restart recovery do not write. The receipt
contains the message ID and tool and actor terms, never message content or a
claim that the message is true.

```toml
[vaelii]
server_url = "http://127.0.0.1:4200"
actor_term = "Syne"   # defaults to pre_send.construct_id
timeout_ms = 3000
```

Set `VAELII_API_TOKEN` when the endpoint requires a bearer token. A write
failure is reported in the `get_message` result without discarding the
retrieved message. The six assertions are sent atomically to `CxWell` and use
deterministic `Check<ID>` and `Receipt<ID>` terms, so repeating the same read
does not create a second identity.

The companion vocabulary in
[`resources/vaelii/dione-bridge-overlap.ke`](../resources/vaelii/dione-bridge-overlap.ke)
carries the accepted `arg1`/`arg2`/`arg3` bridge plus experimental KB-invariant
declarations. Dione validates a `naked_term` assertion batch before sending it,
but that check sees only the outbound batch; whole-KB enforcement belongs at
Vaelii's atomic write boundary.

## Vaelii sentex locators

When `delivery.evidence_markers_enabled` is true, `reply` and `send_dm` accept
up to four total `claim_handles` and `citation_handles`. Handles are canonical
positive-decimal `u64` strings; JSON numbers are rejected. Dione appends
role-bearing terminal v3 locators after pre-send hooks, such as
`[🔍=v3:claim:CRY]` for handle 2326, and exposes them as `claim_locators` and
`citation_locators` on receive surfaces. A v3 token is the handle's
minimal-width big-endian bytes in unpadded base64url; decoding rejects empty
tokens, padding, non-base64url characters, a leading zero byte and zero.
Locators are author-offered metadata, not verification or truth.

The former `evidence_keys` input is not accepted. Existing v1 terminal markers
remain readable as citations, and fixed-width v2 markers (`[🔍=v2:claim:…]`,
an eight-byte token) remain readable with their roles, including events already
queued for Codex before an upgrade. Parsed locators are projected exactly as
written, so a v2 marker still reads as `v2:…`. Put literal marker examples inside quoted or fenced content; raw
terminal locators in outbound message text are rejected so they cannot bypass
the structured handle and audit path.
