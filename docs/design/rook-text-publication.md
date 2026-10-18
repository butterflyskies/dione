# Rook text publication

Rook has one durable default-reply slot for each recipient and source occurrence. A tool call reserves that slot before a trusted adapter prepares or sends a message. The publication ledger shares the recipient owner's existing snapshot and disk worker.

## Request identity

`TextPublication` contains an event, a recipient-scoped idempotency key, an exact room or reply target, text, and mention controls. Omitted mention controls suppress every notification. Attachments and additional publication purposes are not supported by this version.

The canonical request excludes the retry key. Its versioned serialization and a domain-separated SHA-256 digest are retained together. Equality checks compare the entire canonical request. A new key can alias an identical request in the same event slot. Reusing a key for a different event or request fails. Changing the request in an existing slot also fails.

An event has at most 64 aliases. Aliases consume the configured history budget, and all publication bytes consume the configured snapshot budget. Publication records cannot outlive their retained event because the owner validates that relationship on load and commit.

## Authority and durable progress

`RookService::open_with_publication` installs a trusted adapter. Ordinary `open` remains available for integrations that only need delivery and context. The `PublicationTools` instance binds its recipient at construction. Tool arguments cannot select a different recipient or supply operator authority.

A new reservation requires the current matching claim, an unexpired grant, the current authorization revision, and current access to the retained event. Targets stay in that event's source-qualified room. Exact reply targets are verified again by the adapter. Message content and native supplements do not grant authority.

The owner commits `Reserved` before preparation starts. Preparation runs outside the mailbox and performs no message POST. Its exact bounded bytes are retained with the request. The owner rechecks claim and access, then commits `Dispatching` and synchronizes the directory before releasing the provider attempt.

The provider reports one of four outcomes:

- `Published` supplies an authoritative message ID in the requested source and room.
- `NotSent` reports a final local refusal before a message POST.
- `Refused` reports an authoritative provider refusal.
- `Unknown` reports possible creation without a usable receipt.

Matching terminal retries return the retained outcome, even after the claim expires. No terminal outcome triggers another attempt in this version. An interrupted `Reserved` record remains unattempted and requires an explicit retry with current authority. An interrupted `Dispatching` record becomes `Unknown` before the reopened service becomes available.

Settlement, replay, subscription changes, and binding renewal retain publication identity. A late provider receipt records what occurred even if claim authority has since expired. It does not grant permission for another message.

## Storage failures and completion capacity

Every owner commit accounts for its encoded snapshot plus the completion allowance of all `Reserved` and `Dispatching` slots. Each slot reserves 8 KiB. A validated receipt occupies at most 4 KiB of encoded JSON. The remaining allowance covers the finite state envelope and timestamp growth. Completion does not append an unbounded error or history record. Prepared bytes are charged before dispatch.

Ordinary admissions, aliases, and limit changes cannot spend this allowance. Publication completion consumes it in the same commit that records the outcome. Unknown outcomes use finite codes rather than provider response bodies.

If a receipt commit fails, the owner retains that outcome in memory. Status returns the known result with `durability_pending` and a typed storage or capacity block. Retry does not issue another POST. A hard restart before the receipt becomes durable conservatively recovers `Unknown`.

Schema v1 is decoded through an explicit legacy state type and migrated to schema v2 with an empty publication ledger. The migration must commit before the service opens. Schema v2 requires the ledger field. Unsupported versions and incomplete v2 snapshots fail visibly.

## Secondary bookkeeping

`PublicationDispatch` separates a provider outcome from optional owned follow-up work. The adapter captures its exact context in that future. The owner retains a release gate and starts the follow-up only after a valid receipt or definite refusal commits. A separate supervised task bounds the work to five seconds.

Blocked diary, coordination, or own-send bookkeeping cannot turn a retained receipt into a resendable failure. A crash can interrupt this secondary work without changing the publication slot. No follow-up may send another message.

## Discord boundary

The Discord adapter reuses current access, exact-target verification, pre-send policy, rewrites, and contradictionary checks. It rejects redirected destinations and oversized prepared text. Exact replies ignore the legacy reply-mode setting. Allowed mentions are explicit and suppress user, role, everyone, and reply-author pings by default.

The private message client disables protocol retries, redirects, and proxy discovery. It issues one message POST and does not use the legacy chunk loop. Native response parsing must establish the exact returned channel and message ID. A lost or malformed response remains uncertain.

This module does not activate a webhook, open an authenticated HTTP listener, or send a live message at startup. Transport registration and resident startup remain separate work.

## Verification boundaries

Owner tests exercise the actual mailbox, disk snapshot, and principal-bound tools. They cover aliases, conflicts, canceled callers, policy changes, late receipts, restart recovery, storage failures, capacity pressure, migration, and blocked secondary bookkeeping. Adapter tests use a local HTTP fixture and existing policy fixtures. Native checks and controlled regression mutations must pass before publication.
