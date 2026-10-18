# Discord reply tool ownership

Discord owns the complete reply schema, typed argument parsing, execution, and
wire projection. The central dispatcher no longer defines or executes reply.
Registration uses the existing module registry and retains collision checks.

An owner-provided schema projection preserves evidence-dependent discovery.
Each call reloads configuration. One shared reply-service owner supplies both
the registered reply tool and the remaining legacy tools, preventing divergent
state during consent release. Version 0.72.0 reflects constructor and service
access API changes; production callers and fixtures migrate together.

Historical candidate 07ad15e passed native Linux formatting, feature Clippy with
warnings denied, and 527 focused Nextest tests. Those actual MCP requests
cover evidence discovery and configuration reload, allowed and denied reply,
concrete error causes, and held reply followed by legacy release and duplicate
denial. The local source review's ownership and documentation findings were
corrected. Local review is not official Forgejo approval.

Flipping the evidence discovery flag made the actual request test fail its
schema assertion; byte restoration made it pass. The final review also found a
transparent wrapper that hid the concrete hook-validation error. The regression
failed against preserved old production behavior, then passed with the retained
PipelineError source. Corrected Clippy and all 527 focused tests passed again.

The publication composition retains the partial-publication receipt implementation
at the moved Discord wire path. It also retains the concrete sentex parser cause
and the overflow rejection and maximum-value acceptance regression. Construction
preserves accepted attention authority alongside the reply event sender.

The composed source still requires independent review, focused native checks, and
the full signed-head gate. Historical checks do not verify this composition.
This unit does not claim neutral outbound capabilities, deployment, or real-host
acceptance.
