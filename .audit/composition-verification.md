# R6 composition verification

## Scope and design

R6 finishes executable composition and removes obsolete import aliases. The
selected design keeps one library startup entry with the process ordering visible.
Main installs the error reporter and calls that entry. A complete private Teams
value contains one bridge and listener address; reply authority derives from it.
Harness selection is construction-only and reads adapter-owned policy constants.
Offline attention can inspect policy without creating delivery resources.

Three design candidates each considered cohesive composition, platform wrappers,
and a whole-application state machine. They converged on cohesive composition.
Parent scores A/B/C were 22/24/22, saved before the independent judge returned
21/23/21. Both selected B. The final design rejects erased application errors in
the library, applying typed StartupError and exhaustive causes to the touched
boundary. Existing boxed mute and string attention causes remain confined to
specific variants. It does not redesign their owners.

Version 0.62.0 removes teams_delivery, teams_edge, teams_runtime, mcp::ids,
and mcp::tools::messaging public aliases, plus private teams_reply_store.
All local callers move to canonical owners. Persisted-data compatibility remains.
External Rust consumers are not claimed compatible with removed imports.

## Preserved behavior and bounded diagnostic changes

Startup retains configuration, pipeline and mute initialization before offline
commands. Teams capability rejection precedes required environment validation.
One bridge restores before harness startup and supplies listener, reply and drain.
Harness startup still follows attention construction. Shutdown keeps accepted
request/reply drains before harness cancellation and join. The drain statements
were moved without adding a task registry or deadlines.

Moved startup panic sites become typed failures, including invalid log filters.
Mute loader causes remain available rather than being formatted away. A malformed
Teams secret now becomes a value-free Missing or NonUnicode cause, preventing
its raw bytes from appearing in error reports. These diagnostic changes are
intentional; successful CLI, MCP and stored-state behavior remains the target.
The startup entry is process-global and is not promised reentrant.

MCP registration and dispatch already perform protocol translation. Keep adapter
extension composition, tools order, evidence/Teams gating, parsing, and shared
result envelopes. Existing config and mute request grammar remains, including
the direct-store/MCP zero-TTL difference. This is not a repository-wide type audit.

## Characterization before integration

The six new subprocess CLI tests passed against R5 source at 9b5c41c before R6
integration. They cover both offline policies, successful configure persistence,
held inbox ownership, no observed socket connection, command validation, missing
source-validation token, held attention-store refusal, and recovery after release.
The test-only native snapshot was stashed before R5's final gate.
Evidence is /home/syn/code/dione-r6-cli-baseline.log.

An earlier standalone capture used the R5 focused debug binary, version 0.61.0,
prior to its transparent-error correction. It is not an exact-commit binary claim.
It captured help, metadata outputs and failure contexts outside the repository at
/home/syn/code/dione-r6-baseline. The Rust baseline above uses the signed source.

## Reviews and verification

Initial local source reviews covered all six lenses. They found a P2 secret-value
error exposure, inherited from the old environment path, and seven narrative
comment blocks in moved code. The writer replaced the secret cause with a closed
value-free enum and added a canary test, then deleted the specified comments.
A fresh fix-aware review closed those findings by source inspection. A later full
six-lens review covered the formatted source, concrete Serenity error boxing,
and architecture update without new P1-P3 findings.

Feature Clippy passed with warnings denied, and 28 focused harness, startup,
and CLI tests passed. The SocketAddr test parse needed an explicit type, and
the large concrete Serenity cause is boxed. Logs are
/home/syn/code/dione-r6-clippy.log and /home/syn/code/dione-r6-focused.log.

Release CLI comparison and the clean exact-commit gate remain
pending. No full gate, live readiness, construct validation, official approval,
merge, or deployment is claimed here.

## Secret diagnostic regression proof

The valid redacted implementation passed the canary test. Replacing only the
TeamsSecret source field with raw VarError reproduced the inherited disclosure
and failed at the Debug canary assertion. The original error source file was
restored byte-for-byte with SHA256
fec533b34d149764f87133863a04f3c30c03db4a0ae1566178e508fa2baff71e.
Logs are /home/syn/code/dione-r6-secret-baseline.log and
/home/syn/code/dione-r6-secret-mutant.log. The first mutant attempt failed at a
missing-value source-type assertion; its separate log is retained and is not
counted as disclosure proof. The malformed case now runs first.

The lexical boundary checker passed for six shared files plus main and startup.
The retained R4 comparator confirms three tests and 17 literal JSON expressions.
MCP protocol.rs and dispatch.rs have no R6 diff. This does not certify an
exhaustive wire-schema matrix. External logs r6-boundary.log and r6-fixtures.log
are retained in the handoff directory. Native cargo xfmt also regrouped unrelated
imports; only the 25 declared source paths were copied back for integration.

The later R5 release binary probe reproduced invalid-filter panic exit101;
/home/syn/code/dione-r6-invalid-filter-baseline.log records the binary hash and
old diagnostic. This was captured after R6 implementation. The seventh CLI test
asserts the intentional new exit1 reported error. Debug CLI help exactly matches
R5; release help will be compared after the committed-head gate.

Clippy and28 focused tests preceded the final canary case reordering. Only the
corrected one-test baseline and mutation cover that final order so far. The
full gate must cover the final committed candidate.
