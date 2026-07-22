# PronounDB identity metadata (#214)

## Boundary

Dione may enrich the model-visible author identity of a Discord message with an
English PronounDB v2 record:

```text
Display name — she/her
```

Pronouns are attributed identity metadata, not instructions or authorization.
Discord user IDs remain the access-control identity.

Discord's bot API does not expose profile pronouns. PronounDB is therefore an
external, user-managed source: its privacy policy says users opt in and link
their external accounts, after which third parties may look up those external
IDs.

## Privacy and trust

- Lookup is disabled by default and configured per globally allowed user with
  `access.include_pronouns`.
- An opted-in ID must also occur in `access.allow_from`; invalid configuration
  is rejected before replacing the last valid runtime config.
- Opted-out and non-allowed IDs are never queried.
- Dione requests only the `en` set through PronounDB's documented v2 Discord
  lookup endpoint. Unknown or malformed sets are not rendered.
- Logs contain only bounded outcomes (`enriched`, `absent`, `timeout`,
  `provider_error`, or `malformed`), never user IDs or returned pronouns.
- Successful presence and absence results are cached in-process for one hour.
  The cache is capped at 256 opted-in users; provider failures are never cached.
- Responses larger than 64 KiB are rejected before parsing, both from declared
  length and while streaming. Pronoun sets must be unique documented v2 values
  and cannot exceed the eight English sets.

## Availability

PronounDB is outside Dione's trust and availability boundary. Each lookup has a
configurable hard deadline (default 300 ms, maximum 2 seconds). Missing records,
timeouts, malformed responses, and provider failures all fail open to the
existing plain display name so Discord delivery continues.

At most eight upstream requests run concurrently. Concurrent misses for one
user are single-flighted and recheck the cache before using a global request
permit.

The adapter sends a descriptive, versioned Dione `User-Agent`. Tests use the
provider contract and never call Discord or PronounDB.
