<!-- design-meta
status: adopted target
last-updated: 2026-10-18
phase: 3
-->

# Adopted architecture target

This document preserves the three-machine target adopted on 2026-09-09 and
recorded in PR414. It is a design contract, not an implementation inventory.
The [current architecture map](../../ARCHITECTURE.md) and source describe what
Dione does today. The [historical R1 through R6 roadmap](refactor-plan.md) maps an older
boundary proposal to PR551's implemented vertical slices; it does not authorize
a new cutover or claim the whole target is complete.

## Three linked machines

Events, turns, and replies are not one-to-one. Each has its own owner, identity,
and evidence of progress.

```text
Discord/provider event
  -> 1. Inbound custody (Dione)
       Received -> Recorded -> Admitted -> Queued -> Offered -> Accepted
                      \-> Held or Rejected       \-> lease expiry -> Offered again

  -> 2. Resident turn (harness/controller)
       Idle -> Starting -> Running <-> Waiting -> Finished
                                  \-> Stopping -> Reconciling

  -> 3. Outbound effect (Dione)
       Proposed -> Authorized -> Pending -> Sending -> Confirmed
                   \-> Denied                  \-> Failed or Unknown
                                                   \-> Reconcile
```

One accepted delivery may contribute to a turn with many events. A turn may
produce no effects or several effects. One logical effect may produce multiple
remote receipts when Dione splits a long message.

The links must be explicit: source event and revision → delivery intent and
attempt → resident turn and generation → outbound effect ID → remote receipt(s).
Subject IDs, such as a Discord message ID, are not unique event IDs: create,
edit, reaction, and delete are distinct events about a subject.

## Target invariants

- Persistence, transport acceptance, harness injection, and consumer handling
  have separate receipts. None implies the next stage happened.
- Inbound delivery is durable at least once, with stable identity across
  retries. Batching changes presentation, not member identities.
- Exactly one live controller owns a resident thread and generation at a time.
- An uncertain outbound send is reconciled before retry. A retry uses the same
  effect ID, including when only one chunk of a logical effect is uncertain.
- Model-written text that resembles Discord input cannot acquire the authority
  of authenticated ingress.
- Restart rejects obsolete controllers, rebuilds pending work, and reconciles
  uncertain effects before resuming them.
- Placement does not define authority. Dione, harness, model, memory, and
  supervisor may be local or remote behind typed capability contracts.

A deterministic supervisor outside the three machines observes binding
generation and enqueue, offer, and acceptance watermarks. It may perform a
fenced rebind only when the expected binding is absent; it must not steal a
conflicting live binding. Persistent ingress stalls produce a typed harness
health event and an alarm outside Dione's own failure path.

## Messaging and MCP boundary

The target puts messaging behavior in an application module independent of
MCP. MCP remains the protocol adapter: it owns request and tool schemas,
argument decoding, invocation, and response and notification encoding.
Messaging owns operation decisions, pre-send hooks, consent, evidence handling,
and Discord delivery behind typed results.

```text
MCP adapter -> Messaging -> Discord transport
                   |-> Pre-send hooks
                   |-> Consent gate
                   \-> Evidence handling
```

Removing MCP should remove an interface, not the application's ability to
perform messaging operations. This dependency direction does not require a
generic service framework or transport-neutral rewrite. PR551 moved the
current operation boundary into [Discord messaging](../../src/discord/messaging.rs),
with [typed tool registration](../../src/mcp/tool_registry.rs) and
[wire projection](../../src/discord/messaging_wire.rs). The
[roadmap](refactor-plan.md) records that supersession. These module boundaries
are current; the bridge-wide durable effect and supervisor guarantees above
remain target contracts, not consequences of file placement.

## Open contracts

Adoption of the target did not settle these design questions:

1. What does `Accepted` acknowledge, who issues it, and which durable
   obligation transfers at that point?
2. Where is resident generation fencing enforced, including rejection of an
   obsolete controller's action?
3. What provider evidence or idempotency mechanism resolves an `Unknown`
   send, especially an uncertain chunk? What happens if it cannot be resolved?
4. How long does outbound authorization remain valid while an effect waits,
   and how do policy or binding changes affect it?

Existing inbox leases, app-server responses, consent journals, and Discord
receipts answer narrower implementation questions. They do not by themselves
close these target contracts. The [current map](../../ARCHITECTURE.md) names
their present owners and limits.

## Current implementation notes retained from main

The following describe existing subsystems, not additional target promises.
Their details remain here while the [current map](../../ARCHITECTURE.md) owns
the overall implementation inventory.

### Config hot-reload and recovery

Config is published through an in-memory `ArcSwap` cache; gate checks read
the current snapshot without touching disk. A file watcher reloads the cache
on `config.toml` changes, and MCP config mutations republish after
persisting. Every producer — startup, watcher, explicit reload, MCP
mutation, recovery — runs through the process-local single-writer
`ConfigRuntime` pipeline (validate → persist → publish). The reader's
`File::try_lock()` is best-effort contention detection only; Dione does not
claim cross-process writer serialization. A parse error
with a last-known-good on disk quarantines the bad file to `config.toml.bad`
and restores + publishes from `config.toml.lkg`; without an LKG the last
valid in-memory config is kept and the file is left in place. Defaults are
used only when the file is missing or unreadable. See
[config-runtime.md](config-runtime.md) for the contract, the recovery
design, and implementation notes.

### GAIE Atom 1b backfill

The one-shot archive path has a discovery phase before its message producer:

1. Build a typed capture root from the configured corpus, guild, and parent.
2. Collect active snapshot A, paginate public/private archives according to
   route-specific cursor rules, and collect active snapshot B.
3. Validate every candidate's guild, parent, and thread type and construct an
   opaque verified capture target. No raw child ID crosses into message fetch.
4. Union and deduplicate targets, then sort parent first and thread snowflakes
   numerically.
5. Load or migrate checkpoint v2 and fetch each stream from its independent
   cursor. New streams begin with a nullable cursor.
6. Sort messages numerically, deduplicate by Discord-global message ID, append
   and fsync each message batch, then advance only the source stream cursor.

Discovery precedes archive mutation in default mode. This keeps an incomplete
enumeration from producing an archive that appears complete. The two active
snapshots bound—but cannot eliminate—the Discord race, so coverage receipts
must say principal-visible and non-atomic.
