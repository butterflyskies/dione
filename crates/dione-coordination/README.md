# Dione coordination

The client speaks newline-delimited JSON over fresh TCP connections. It owns
hello, claim, done and release exchanges, bounded server lines, acknowledgment
deadlines, lease waiter tasks, the shared event bus and gateway identity fallback.
Application config, current authorization, generation checks, publication policy
and bookkeeping remain with their existing owners.

Ordinary claim retains a waiting connection and forwards validated done/promoted
pushes. Checked acquisition closes that waiter and returns a private ClaimAttempt.
Consume into_parts and handle WriteUncertain before applying fail-open policy.
NotSent means no claim write began; Written means write_all and flush completed.
WriteUncertain means bytes may have reached the server. Unavailable.claim_sent
is true only for Written and cannot replace the progress check. Checked release
requires an affirmative acknowledgment and returns an exhaustive typed error.

The shared bus survives newly constructed clients after config reload. Gateway
identity is resolved at call time when no explicit identity was supplied. This
preserves existing globals and background-task lifetime; it adds no shutdown,
delivery durability or stronger authorization guarantee.

## Inherited limitations

String identity arguments, raw millisecond config fields, mutable public config,
derived serde and presentation strings in Unavailable retain their existing type
policy debt. Direct config construction can bypass the nonzero connect-timeout
deserializer. CoordinationError retains the existing exhaustive enum categories,
string JSON/protocol diagnostics and error text; this is not a redesign to the
preferred context-struct/kind pattern. Server text is logged in bounded form and
fixed protocol wording is retained for callers. No transport abstraction, parallel
identity system or application dependency is introduced.

Subscribed claims validate the aggregate lease deadline before starting a waiter.
An unrepresentable deadline becomes a typed protocol-unavailable outcome with
claim write progress retained. Checked claims that do not subscribe retain their
Wait outcome because they create no waiter deadline.
