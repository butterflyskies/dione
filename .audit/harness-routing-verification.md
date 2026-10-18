# R4 harness-neutral routing verification

## Contract and design

R4 replaces the shared server's harness mode, optional Codex queue, and optional
binding publisher with one complete connection. A single adapter supplies its
delivery and MCP protocol facets. Shared routing consumes those facets without
choosing a named harness or operating its queue directly.

The selected design is candidate C. The parent scored A/B/C as 22/22/23 out of
25; an independently scoped same-model judge scored them 23/21/24. The final
design incorporates A's source-preserving errors and B's complete constructors
and structural check. The alternatives and scoring are retained locally under
`C:/Users/Jaycee/code/dione/.git/codex-handoffs/dione-r4-r7/`.

The connection is publicly usable by the binary, with private fields and
internal facets. Both facets come from the same adapter instance. The existing
rendered notification JSON remains an internal transport boundary. R4 does not
introduce a new notification schema or a generic plugin registry.

## Baseline

The base is `4be9583f31aea7e12348c30f2285048385dc0bbd`, PR #509.
Its full local gate passed before this slice.

Three `harness_contract_` tests were added before changing production code.
They passed against the old implementation in the native Linux checkout:

```sh
cargo nextest run --workspace --locked --features oneshot-test-seam -E 'test(harness_contract_)'
```

Run `8a4b7d89-9e08-4e8e-8762-a1ddfac49287` passed 3 tests and skipped
1644. Log: `/home/syn/code/dione-r4-characterization.log`.
The signed test-only commit is
`f861c8bbc734aa7814ec7d5cc8713ce62177c98c`.
The focused run used a copy of the test file over unchanged base production
source, followed by native formatting. It was not an exact-commit gate.

The tests assert literal initialize capabilities, all seven queue-tool
unavailability errors before missing-argument parsing, and a successful queue
status, registration, lease, acknowledgement, and empty timeout sequence.
Changing expiry timestamps must exist and parse as RFC3339, but are excluded
from literal comparisons. These tests add no live-service, restart, handoff,
or permission-relay coverage.

## Behavior to preserve

- Claude uses the same stdout lock for notifications and protocol responses.
  Write and flush errors remain logged delivery success. Codex enqueue errors
  propagate to the existing failure path.
- Evidence-bearing flushes preserve individual FIFO envelopes. A failed
  delivery returns the failed event and unattempted tail. Ordinary coalesced
  failures retain their existing different behavior.
- Edit and delete processing invalidates admission state, durable attention
  state, and queued attention before forwarding ready results. Queued
  invalidation still matches by message ID.
- Server attention enforcement derives from the connection's safe-turn capability.
  Existing source checks, receipt persistence, and ordinary-before-managed
  ordering remain unchanged.
- Tool names, schemas, error precedence, permission capabilities, and the
  single MCP result envelope remain unchanged. Known but unavailable tools
  must not fall through to an unknown-tool error.
- The same queue owns the inbox lock. Binding persistence, worker startup,
  listener startup, and ingress-first shutdown retain their order.

The offline attention command still maps the CLI selection to an attention
policy without opening a queue or creating a connection. This second mapping
is retained for R5/R6 composition review; R4 does not claim it was eliminated.

## Throughput checkpoint

1. Ground routing, concrete lifecycle, and historical constraints before the
   independent design comparison.
2. One isolated writer owns the coupled migration. The parent integrates and
   owns the native checkout and Cargo runs. Reviewers are read-only.
3. No native source synchronization occurs during a build. Each design runner
   writes its own artifact. The writer's checkout is separate from integration.
4. Verify the behavior characterization first, migrate callers as one unit,
   then review and run the final exact-head gate before opening R4's PR.

## Final verification requirements

### Focused execution

Feature Clippy passed after fixing migration imports and redundant borrows.
The affected queue, attention, evidence, adapter, and protocol selection passed
156 tests with 1493 skipped. Log: `/home/syn/code/dione-r4-focused.log`.
The fixture comparator confirmed all 17 JSON expressions across three baseline
tests. The boundary checker passed its negative fixtures and six production files.
It is a lexical check for the `codex` module token and removed routing names
after masking literals, comments, and test items. It does not resolve imports
or detect every possible aliased or re-exported concrete type. Source review
supplies the broader dependency assessment.

The actual notification-loop test passed separately, then failed its literal
event-order assertion when the adapter invalidation call was replaced by a
successful no-op. Both edit and delete notifications still arrived, so the
failure was not a timeout. The source was restored byte-for-byte with SHA256
`184559521fd44a5d67e8dc062beb8acd89e3350774b25324dc795dbd40d5f90f`.
Logs: `/home/syn/code/dione-r4-loop-baseline.log` and
`/home/syn/code/dione-r4-loop-mutant.log`. The mutation driver is retained under
the local handoff directory as `mutate-invalidation.py`.

### Review corrections

The Safety, Security, and Privacy pass found no additional behavior regression.
The Design, Idiomacy, Tests, and comment pass found three actionable issues.
The first synthetic invalidation test bypassed the notification loop, the
source checker missed grouped imports, and a constructor comment described
removed builders. The implementation now tests edit/delete events through the
actual loop using injected input, rejects the module identifier in grouped
imports, and documents the complete connection. A new reviewer received those
findings and checked their fixes by inspection. Runtime proof is recorded above.
The [review record](harness-routing-reviews.md) preserves the coverage split
between the two initial reviewers and the follow-up review.

The server connection is private so external callers cannot replace it after
attention policy initialization. Temporary queue fixtures retain their owning
directories through each test. One redundant receive-loop comment was removed.
The broad dispatch diff includes indentation caused by delegating harness tools
before the existing common-tool match; unrelated tool behavior was not changed.

Preserve characterization expectations, exercise a synthetic third adapter
through common routing and MCP, and run the source-boundary checker. Run the
existing queue, attention, evidence FIFO, protocol, and lifecycle tests. The
full final gate is `sh scripts/pr-ready.sh 4be9583f31aea7e12348c30f2285048385dc0bbd`
from a clean native Linux checkout. Record its exact head and outcome in the PR.
The native migration snapshot was saved with `git stash push -u` before fetching
the signed bundle and switching to the exact commit. The gate checks that the
resulting checkout has no tracked or untracked changes. The CLI check compares
`/home/syn/code/dione-r4-cli-baseline.txt` with
`/home/syn/code/dione-r4-cli-final.txt`; the PR records its final result.

The existing synchronous queue I/O and upstream erased errors are not claims
of repository-wide type compliance. No construct or live Discord test is
claimed. R5 owns concrete harness lifecycle, R6 owns remaining composition,
and R7 adds Oh My Pi after external-source inspection. CI and official approval
remain separate from local verification.
