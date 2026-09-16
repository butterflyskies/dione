# One-shot outbound send (`dione-send`)

`dione-send` posts one message to one configured channel and exits. It exists
for stateless callers — Trundle's `trundle-wake` egress seam (#426) — that
must not launch the full gateway against ambient state to deliver a single
message, and must never succeed under the wrong bot identity.

```
dione-send --channel <ID> --expect-identity <BOT_USER_ID> --message <TEXT|-> \
           [--config <PATH>] [--token-source config|env:<VAR>] [--nonce <KEY>] \
           [--dry-run] [--allow-multi-chunk]
```

- `--config` defaults to `$DIONE_STATE_DIR/config.toml`; pass it explicitly
  under systemd `ProtectHome=read-only`.
- `--message -` reads the text from stdin.
- `--token-source` defaults to `config`. The environment is consulted only
  when named (`env:TRUNDLE_TOKEN`); ambient `DISCORD_BOT_TOKEN` never takes
  precedence, unlike the daemon's `resolve_token`.
- `--nonce` is a caller-stable idempotency key (at most 25 characters), sent
  as `nonce` with `enforce_nonce: true`. It is defense-in-depth for a
  *prompt manual* replay only: Discord deduplicates by nonce for an
  undocumented "few minutes", so it never makes an ambiguous failure
  retryable (see below). For a multi-chunk send each chunk gets
  `<key>-<index>`, which must still fit in 25 characters.
- `--expect-identity` is required. The send is bound to the **numeric** bot
  user id returned by `GET /users/@me`, never to the username.

## What it does not do

The one-shot path never initializes the gateway client, inbound consumers,
the MCP server, the config watcher, the mute store, the ingress ledger, or
the no_rly consent gate, and it writes nothing under the state directory or
`$HOME`. The config is loaded read-only: a missing file is an error rather
than a template write, and there is no last-known-good promotion or
quarantine. A contradictionary `Bounce` therefore *refuses* instead of
holding — no ticket is issued, because nothing would ever release it.

## Preflight, in order

Every step fails closed and happens before any write; `--dry-run` stops after
the last one.

1. Config parse (`config_invalid`).
2. Token from the named source only (`no_token`).
3. `GET /users/@me`: the bot user id must equal `--expect-identity`
   (`identity_mismatch`; a 401 is `no_token`). This is also the liveness check.
4. Outbound gate against `[[channels]]` only (`not_permitted_target`). DM
   channels and threads of allowed parents need gateway caches that a
   one-shot never has; the detail says so.
5. Chunk count under `delivery.{text_chunk_limit,chunk_mode}`, using the same
   fence-preserving chunker as `reply`; more than one chunk refuses
   (`would_chunk`) unless `--allow-multi-chunk`.
6. Pre-send pipeline (when `pre_send.enabled`) and the contradictionary
   judge, in-process (`contradictionary_bounce`).

## Output

stdout carries exactly one JSON object; every log line goes to stderr.
`RUST_LOG` is honored only as a plain level (`error|warn|info|debug|trace`)
and governs only Dione's own targets (`dione`, `dione_send`); serenity,
reqwest, hyper, h2, rustls, and tokio are clamped to `warn` regardless,
because their trace-level instrumentation renders request bodies (the
message text) into the log.

There is no way to point a production build at anything but Discord: the
API-base override used by the test suite's mock (`--discord-api-base`,
`oneshot::run_with_api_base`) exists only in builds with the
`oneshot-test-seam` cargo feature, which is off by default and never part
of a release build; a production binary refuses the flag as `usage`. All Discord snowflakes are JSON **strings**,
and `retryable` and `delivery_ambiguous` are required booleans on every shape.

```json
{"ok":true,"status":"sent","retryable":false,"delivery_ambiguous":false,
 "channel_id":"…","message_ids":["…"],
 "identity":{"bot_user_id":"…","username":"…","token_source":"config"}}
{"ok":true,"status":"preflight_ok","retryable":false,"delivery_ambiguous":false,
 "channel_id":"…","chunks":1,
 "identity":{"bot_user_id":"…","username":"…","token_source":"env:TRUNDLE_TOKEN"}}
{"ok":false,"status":"refused","reason":"<slug>","detail":"…","retryable":false,
 "delivery_ambiguous":false}
```

Reason slugs: `usage | config_invalid | no_token | identity_mismatch |
not_permitted_target | would_chunk | contradictionary_bounce | send_failed`.

`usage` covers bad arguments (including an invalid `--token-source` or an
over-long `--nonce`) and a failed stdin read for `--message -`; it exits 1
and still prints the JSON object (the human text also goes to stderr). The
one exception to "exactly one JSON object" is `--help` / `--version`, which
print clap's text on stdout and exit 0.

### Retry semantics

The one-shot **never retries internally** — Serenity's ratelimiter is
disabled so a 429 returns at once instead of sleeping for `retry-after` —
and the caller owns retry, guided by two booleans:

- `retryable`: a retry cannot double-post and may succeed.
- `delivery_ambiguous`: the `POST` may have been accepted by Discord even
  though no success response arrived. Always `false` on success shapes and
  deterministic refusals. When `true`, `retryable` is `false` and `detail`
  ends with "delivery ambiguous; reconcile before resending": the caller
  should reconcile (e.g. read the channel) rather than drop or resend.

On `POST`, only failures that prove no message was created are retryable:
HTTP 429, and a connection that was never established. HTTP 5xx, timeouts,
and lost responses are ambiguous **regardless of `--nonce`** — Discord's
`enforce_nonce` deduplicates only within an undocumented few-minute window,
and a caller that persists retryable work across later runs would
double-post past it. Identity lookup is a `GET`, so its transport and 5xx
failures stay retryable: nothing was created.

| failure | identity `GET /users/@me` | `POST` (with or without nonce) | `delivery_ambiguous` |
|---------|---------------------------|--------------------------------|----------------------|
| HTTP 429 | retryable | retryable | false |
| connect refused / DNS (request never written) | retryable | retryable | false |
| HTTP 5xx | retryable | **not retryable** | **true** |
| timeout / lost response | retryable | **not retryable** | **true** |
| other transport error after the request may have been sent | retryable | **not retryable** | **true** |
| HTTP 4xx (400/401/403/404) | not retryable | not retryable | false |
| non-HTTP client error | not retryable | not retryable | false |

A failure after some chunks were already delivered is never retryable (the
delivered ids are listed in `detail`) and carries `delivery_ambiguous` per
the failing chunk's class. A Discord 4xx during the send is still
`send_failed` and carries the HTTP status in `detail`.

`delivery.text_chunk_limit` above Discord's 2000-character limit is refused
in preflight as `config_invalid` (fail closed) rather than letting
`--dry-run` clear a message the send would reject.

`detail` is sanitized: the token never appears in it. Config diagnostics are
deliberately value-free — a `config_invalid` detail carries the config path,
the error kind, and a line/column for parse errors, but never a source
excerpt or the parser's message (a malformed `token = "..."` line would
otherwise be echoed to stdout). stderr carries the same redacted diagnostic
as stdout, and the one-shot never logs a config value on either stream:
config-load warnings name the field (and a length or index), not what the
operator wrote.

## Exit codes

| code | meaning |
|------|---------|
| 0 | sent, or `--dry-run` cleared preflight |
| 1 | usage, config, or IO error (`usage` / `config_invalid`) |
| 2 | preflight refusal |
| 3 | Discord REST failure (`send_failed`; consult `retryable`) |

Trundle's adapter maps `sent` to `settled/drumbeat_delivered`.
