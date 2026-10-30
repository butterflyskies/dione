# Historical messaging boundary roadmap

PR414's R1 through R6 proposal described the pre-PR551 implementation at main
`6339319`. It proposed moving operation decisions out of MCP-shaped handlers.
PR551, integrated as `3d0f53b2`, superseded that implementation sequence with
vertical slices and typed owners. This page retains the original dependency
order and proof obligations as history; it does not authorize another refactor
or assert that every [adopted target](architecture.md) guarantee is complete.
The [current architecture map](../../ARCHITECTURE.md) describes runtime owners.

## Current counterparts after PR551

| Source | Current responsibility |
| --- | --- |
| [Tool dispatch](../../src/mcp/dispatch.rs), [server](../../src/mcp/server.rs), and [tool registry](../../src/mcp/tool_registry.rs) | Invoke typed registrations and preserve remaining protocol schemas, decoding and wire results. |
| [Discord messaging](../../src/discord/messaging.rs), [reply registration](../../src/discord/reply_tool.rs), and [messaging wire](../../src/discord/messaging_wire.rs) | Own operation decisions and typed outcomes, with protocol projection at the adapter. |
| [Management](../../src/discord/management.rs) and [management registration](../../src/discord/management_tools.rs) | Own pin, unpin, delete and thread operations without importing policy from the removed MCP messaging module. |
| [Queries](../../src/discord/messaging/queries.rs), [search](../../src/discord/messaging/search.rs), and [rendering](../../src/discord/rendering.rs) | Own their Discord behavior; [query wire](../../src/discord/query_wire.rs) and [search wire](../../src/discord/search_wire.rs) preserve protocol projection. |
| [Pre-send](../../src/pre_send.rs), [consent](../../src/no_rly/consent.rs), and [evidence](../../src/evidence.rs) | Retain distinct rules and state. A consent journal alone is not a durable pending-effect store. |

The former `src/mcp/tools/messaging.rs`, `management.rs`, `render.rs` and
`search.rs` references are historical paths, not current source links or work
that should be recreated. See [the vertical-slice design](vertical-slices.md)
for PR551's implementation direction and compatibility decisions.

## R1 through R6 historical dependency order

The original rows were proposed complete operation boundaries, not permission
to move files piecemeal. The present counterparts below identify where to
inspect preserved behavior; they are not new test results or a fresh claim that
all historical proof obligations have been exercised on every platform.

| Cut | Historical proposed outcome | Depends on | Current counterpart and retained proof obligation |
| --- | --- | --- | --- |
| R1 | Put mutation-target verification and its alert decision behind a typed application result for reply, react, pin, unpin, referenced thread creation and delete. | None | Discord messaging/management own the operations. Retain blocked-target no-mutation, appropriate unknown-target alert and authenticated own-send distinctions at real adapters. |
| R2 | Move preparation, hooks, overrides, redirects and evidence checks behind operation results while MCP encodes responses. | R1 | Messaging and pre-send own preparation. Retain halt, redirect, evidence and reload refusals across reply, DM, edit, file/render captions and voice text. |
| R3 | Move reply/DM delivery and consent release/rephrase orchestration behind typed outcomes. | R2 | Discord messaging and consent own these paths. Preserve concurrency, expiry, known partial remainders and lost-response uncertainty rather than blindly resending. |
| R4 | Move edit, react, file and render mutation execution behind the application boundary while retaining rendering ownership. | R2, R3 | Discord messaging/rendering own these operations. Preserve accepted/refused/transport-error results, captions and intentional self-react behavior. |
| R5 | Move reads, message/pin fetch, search and download semantics behind typed results with shared edge projection. | R1 | Discord queries/search and their wire adapters own these paths. Preserve order, pagination, permissions, evidence markers, attachment bounds and transport error behavior. |
| R6 | Remove cross-module dependence on MCP messaging policy from management, admin DM creation, rendering, search and assembly; remove obsolete helpers. | R3, R4, R5 | Discord-owned modules and startup/server assembly replace the old dependency direction. Inspect real callers and public protocol results rather than inferring closure from file placement. |

The policy distinctions remain important after supersession: the outbound
channel gate precedes target verification and mutation; management's
ingress-or-own-send rule differs from reply/react's bounded canonical lookup.
A ledger miss alone is not always a blocked mutation. Edit preparation is not
permission to add a new target-verifier requirement. Any such behavior change
needs its own explicit contract instead of being smuggled into this roadmap.

Relevant characterization tests now live in
[Discord messaging](../../src/discord/messaging.rs), with own-send evidence in
[state.rs](../../src/state.rs). Historical test names and a source move do not
prove a future implementation is wired through every caller. Future changes
still require a discriminating probe, affected checks and independent review
on their own exact candidate.

Public tool and Rust API compatibility follows current documented contracts,
including PR551's explicitly versioned breaks; the old proposal does not restore
removed compatibility aliases. The target's bridge-wide durable custody,
resident-generation fencing, uncertain-effect reconciliation and independent
supervisor are separate guarantees. Specific inbox leases, Teams session
owners and Rook publication receipts implement narrower contracts; typed
module boundaries do not by themselves close the whole adopted target.
