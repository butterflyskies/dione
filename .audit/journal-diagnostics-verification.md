# R3 journal and diagnostics boundary audit

## Decision

Close R3 by inspection, as allowed by the fixed R1-R7 plan. This PR changes
documentation only. Its base is R2 at
`f8f19ed5c7a30db7226fd9955b321f4ca4329d09`, PR #508.
No source, test, dependency, version, or wire contract changes are required.

Laziness Protocol favors the existing direct calls. Another service would
forward the same typed journal operations without taking ownership of an
additional decision. This audit establishes ownership, not repository-wide
coding-standard compliance.

## Ownership and callers

| Operation | MCP responsibility | Existing owner |
| --- | --- | --- |
| `no_rly_stats` | Parse optional arguments, convert outcome and relative days, serialize statistics, append pending count. | `Journal::stats` filters records, aggregates counts and latency, and validates chains. |
| `no_rly_condense` | Apply the configured default when the argument is absent, calculate a cutoff, serialize the report. | `Journal::condense` retains chain ancestors and folds eligible records into summaries. |
| `no_rly_vacuum` | Apply the configured default when the argument is absent, calculate a cutoff, serialize the report. | `Journal::vacuum` retains raw and unknown records and drops eligible summaries and malformed lines. |
| `set_trace_level` | Read a string and project success or failure. | `TraceLevelController::set_channel_filter` validates and reloads the channel filter. |
| `set_stderr_level` | Read a string and project success or failure. | `TraceLevelController::set_stderr_filter` validates and reloads the stderr filter. |
| `get_version` | Project compile-time package metadata. | No domain operation is needed. |

Evidence at the base commit:

- `src/mcp/dispatch.rs:264` and `:782` contain journal and filter dispatch.
  Version dispatch is at `:702`.
- `src/mcp/tools/no_rly.rs:26`, `:67`, and `:84` contain the journal adapters.
  `days_ago` at `:103` adapts relative protocol arguments to absolute cutoffs.
- `src/no_rly/journal.rs:326`, `:410`, and `:505` own journal algorithms.
  `JournalHandle` at `:603` owns the writer command loop. Maintenance methods
  at `:654` and `:665` await writer replies.
- `src/mcp/tools/diagnostics.rs:16`, `:28`, and `:38` project diagnostics.
  `src/tracing_channel.rs:36` constructs the validating reload closures.
- `src/mcp/server.rs:176` and `:210` assemble adapter contexts.
  `src/no_rly/consent.rs:246` and `:251` expose the journal and pending count.
- `src/mcp/protocol.rs:85`, `:375`, `:383`, and `:390` declare the tools.
  Configured retention defaults originate in `src/contradictionary.rs:171`.

Production adapter callers are MCP dispatch. The controller setters are called
by the diagnostics adapter. Journal maintenance executes through the writer
commands. Direct domain calls in tests verify the algorithms independently.

## Preserved behavior and limits

- Statistics reject wrongly typed filter arguments. Maintenance treats a
  wrongly typed `older_than_days` as absent and uses the configured default.
  These existing protocol semantics are unchanged.
- Statistics do not flush queued writes before reading. Pending count is
  observed afterward, so the combined response is not an atomic snapshot.
- Condense keeps raw ancestors required by retained chains. Vacuum keeps raw
  bounce records and unknown record shapes, but removes malformed lines.
- Synchronous file work, detached writer lifecycle, string controller errors,
  public adapter contexts, and async wrappers remain inherited implementation
  details. No claim of improved durability, cancellation, or authorization is made.
- Diagnostics integration tests use a no-op controller. They validate parsing
  and response fields, not live subscriber reload.
- No new regression test or mutation is needed for a documentation-only diff.
  Existing tests do not constitute exhaustive coverage of every boundary case.

## Throughput checkpoint

1. Blocking first steps: inspect all six operations, their callers, and domain
   owners before deciding whether source changes are needed.
2. Independent workstreams: a read-only grounding agent checks the ownership
   conclusion. Review follows the completed documentation candidate.
3. Shared mutable state: the parent owns documentation and the native check
   checkout. Reviewers do not modify source or run Cargo there.
4. Smallest safe decomposition: one documentation change records the audit and
   closes R3. Source design exploration and implementation are not applicable
   because no function or data shape changes.

## Verification

The existing `tests/mcp_protocol.rs` cases cover version fields, valid channel
and stderr filters, invalid and missing channel filters, empty journal tools,
invalid outcomes, wrongly typed stats filters, and tool registration.
The journal unit tests cover record formats, filtering, aggregation, chain
retention, malformed and unknown records, stale temporary-file recovery, and
concurrent append and condense.

Run `sh scripts/pr-ready.sh f8f19ed5c7a30db7226fd9955b321f4ca4329d09`
from a clean native Linux checkout at the final candidate. Record its exact
commit and outcome in the PR after completion. The local log is
`/home/syn/code/dione-journal-diagnostics-pr-ready.log`.
Check `git diff --exit-code f8f19ed -- src tests Cargo.toml Cargo.lock` to
confirm that this audit introduces no executable changes.

Independent pre-PR review and the final local gate are separate from CI and
official approval. Construct testing with Oh My Pi remains required before
considering a merge to main. R4-R7 remain the fixed implementation scope.
