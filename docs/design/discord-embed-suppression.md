# Discord link-preview suppression

## Scope

Implementation owner: the feature maintainer.

The requested behavior is per-message control over automatic link embeds. Add optional
`suppress_embeds: boolean` to Discord `reply`, `send_dm`, and `edit_message`.
Custom rich-embed creation, Teams, global configuration, and deployment are
outside this change.

## Contract

- New sends: omission and `false` preserve existing normal-preview behavior;
  `true` sets Discord's `SUPPRESS_EMBEDS` flag on every outgoing chunk.
- Edits: explicit overrides read raw flags through the existing authenticated SDK
  client and change only `SUPPRESS_EMBEDS`, retaining bits unknown to the SDK.
  Omission sends no flag override or message fetch; `true` suppresses previews and
  `false` explicitly restores them. Content remains required.
- Held/released/rephrased sends and partial retries preserve this transport
  option, without changing consent, recipient, threading, or ping policy.
- A present non-boolean value (including null) is rejected at MCP dispatch.
- Existing public Rust helper signatures retain their default behavior.

## Boundaries and verification

The flag is carried with the existing held reply request and reaches Serenity
only after existing outbound policy gates. It grants no new addressing or
reply authority and does not bypass pre-send checks. Discord suppresses all
embeds for the message, not selected URLs, and this is display control rather
than a promise that Discord never fetches a URL.

Offline HTTP capture tests verify flags on reply chunks, DMs, held releases,
rephrases, and edit omission/true/false. Parser tests cover invalid values.
Repository formatting, lint, nextest, and release-build checks precede
independent exact-revision review and a draft PR. No live Discord test,
production restart, release, or deployment is implied.

Discord API reference: https://docs.discord.com/developers/resources/message
