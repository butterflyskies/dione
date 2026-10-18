# Oh My Pi integration verification

## Scope and source grounding

R7 adds an explicit Oh My Pi adapter and an original opt-in extension. The user
was notified before research. Upstream source and docs were inspected in
C:/Users/Jaycee/code/references/oh-my-pi-dione-r7-20261012 at commit
b83200eb866821f02625332025b453f06ce4e12a. Published OMP 18.10.0 was installed in a
separate proof directory, with package scripts disabled. No upstream source,
documentation, dependencies, or active user configuration is added to Dione.
The checked-in TypeScript extension and test are original integration code.

Three design packages compared an extension bridge, a Dione-owned RPC process,
a configurable shared adapter, and duplicated output. They converged on a
concrete adapter sharing the existing MCP writer. Parent scores A/B/C 25/23/24
were saved before the separately scoped same-model judge returned 24/22/23. Both selected A.
The final design grafted strict envelope validation, multi-runner proof, and a
main-agent guard mutation. No competing implementations were built.

## Ownership and behavior

The CLI selects oh-my-pi through the existing construction owner. Named prepared
and running session variants own no worker or durable inbox. Teams ingress and
Codex queue tools remain unsupported, and Claude permissions are not advertised
or relayed. Existing Claude/Codex rendering, queue, and writer behavior remains.

The adapter privately validates the legacy rendered notification and emits
notifications/dione/event with version 1, content, and complete JSON metadata.
The original extension accepts only server dione, that method/version, and a
main agent. It submits displayed custom messages with model-visible metadata,
triggerTurn:true and deliverAs:followUp. The existing Dione tools retain access
authority. A textual external-data label does not prevent prompt injection.

One active interactive main session per Dione MCP connection is the supported
operator shape. Main-kind filtering excludes subagents, including depth-zero
children, but does not select among multiple top-level listeners. Rust's writer
logs I/O failures and returns unit; extension sendMessage also returns void.
Successful local completion proves an attempted write/submission, not acceptance,
replay, model action, or ordering. Upstream finite buffers, session changes,
and failures can lose events. ACP and collaboration guest turns are not covered.

## TypeScript boundary and proof

The pinned pi.zod facade lacks recursive JSON combinators. Its record accepts
arrays, refine does not narrow output, and its error formatter throws on BigInt.
A bounded recursive JSON predicate and checked transform retain recursively typed
metadata without casts. A precheck protects malformed non-JSON runtime inputs.
The accepted envelope is limited to 128 object/array containers along any path.
The guard assumes transport-decoded data, not hostile local JavaScript accessors.

The original proof uses the actual published loadExtensions and ExtensionRunner,
local empty auth/model storage, and recording runtime actions. It covers startup
buffering before initialize, main/subagent filtering, exact server/method checks,
malformed input, nested evidence, hostile strings, diagnostic metadata, and two
separate factories. Idle/streaming flags verify requested options only. There is
no inference, Discord call, or real MCP client transport in that proof.

The implementation owner's run passed 7 tests with 47 assertions. Removing the
main-kind guard made the depth-zero child assertion fail; restored source passed.
Strict TypeScript 5.9.3 checking against published OMP declarations passed with
skipLibCheck. This does not certify dependency declarations. The external type
configuration uses machine-local Bun type paths; the README's runtime proof is
portable, while the recorded typecheck recipe is local to this environment.
The parent reran all seven tests and strict typechecking against the integrated Dione extension path. The runtime suite passed with 47 assertions, and typechecking exited zero. The runtime log is parent-test-result.txt in the external proof directory. The prior quiet typecheck produced no retained output file. The parent reran it during pickup to save an explicit exit status in parent-typecheck-result.txt.

## Rust proof and review

Authored Rust fixtures cover real rendered messages, reactions, config and trace
events, evidence metadata, coalescing, malformed/permission rejection, concurrent
complete output lines, MCP initialize/list/ordinary tool calls, explicit mode,
unchanged default, and inert session ownership. The native focused run passed all 27 selected tests, including the subprocess CLI suite. Feature Clippy passed with warnings denied. Logs are /home/syn/code/dione-r7-clippy.log and /home/syn/code/dione-r7-focused.log. The PR records the full gate at the signed commit.

Two separately scoped same-model Rust source reviews covered all six lenses and found no actionable
P1-P3 issue. A separate extension review covered all six as applicable and kept
two comments documenting verified upstream limitations. All reports are local
agent reviews, not official Forgejo approval. No construct or deployment is
claimed. Construct validation remains required before considering main.

## Review records

The Rust source reviews are r7-rust-semantics.md and r7-rust-design.md in the
external handoff directory. They cover the staged-R6 owner's R7-only delta,
including both new adapter files. The separate r7-extension-review.md verifies
the pinned schema-facade limitations, actual runner fixture, and mutation log.
No source reviewer ran Cargo or asserted runtime success. Comment review found
no deletions or unresolved constraints. The parent additionally extended the
existing subprocess status test with OMP's literal Direct-policy expectation.
The signed-head gate includes that final test and the integrated formatting.

## Integrated candidate checkpoint

The fresh final review in r7-final-review.md covered the integrated formatting,
parent-added CLI test, extension hashes, and operator setup instructions. It
found no actionable P1-P3 issue across all six lenses. Its note that the Rust
results paragraph was stale overlapped the parent's checkpoint update. The
27-test result is now recorded above. The TypeScript rerun exited zero and its
explicit status is retained. The boundary checker also passed for the six
shared modules and the main/startup ownership rules.

The final signed-head gate result and commit ID will be attached to the PR.
This file records the precommit checkpoint rather than claiming a future run.

## Cross-model audit and dispositions

Claude Sonnet 5.5 audited the trail and scoped transcript activity at positions
4895-5157. It also sampled earlier messages. It did not read the whole thread
or recover every truncated item. The report is r7-trail-review.md in the external
handoff directory. This audit is separate from the same-model source reviews.
It found no blocking issue and raised the following evidence and residual-risk flags.

- F1. The initial typecheck emitted no text. The pickup rerun produced a retained
  parent exit-status line, not compiler diagnostic output. The actual invocation
  was Bun running TypeScript 5.9.3 bin/tsc with -p tsconfig.dione.json from the
  external proof directory. The transcript records the command and exit zero.
  The configuration retains the published OMP declarations, strict checking,
  skipLibCheck, and the disclosed machine-local Bun type path. The artifact was
  re-recorded during the audit, and the earlier claim is preserved in history.
- F2. The parent repeated the main-kind guard mutation in an external copy of the
  final extension. final-mutation-result.txt records original and mutant hashes,
  the command, and failure. final-restored-result.txt records the unchanged
  original extension's complete suite. No repository code was mutated.
- F3. The final source review covers formatting, the Cargo description, startup
  documentation, CLI tuple, README, architecture, plan, and integrated extension.
  Subsequent edits only clarify audit dispositions and operator limitations.
- F4. Design candidates and source reviewers used the same model. Their separate
  scopes do not establish cross-model agreement. The grounding already favored
  the extension design, so convergence alone is not proof of design quality.
- F5. Rust and TypeScript use separate literal fixtures. A real Dione frame must
  reach OMP and produce one dione-event during construct validation. Local tests
  do not prove the real MCP-manager transport path.
- F6. Dione tool access checks do not constrain OMP's shell and file tools.
  Review the construct's OMP approval policy before admitting external events.
- F7. The PR carries the final signed-head result. This document describes the
  precommit checkpoint, and the implementation checkbox is not deployment approval.
- F8. Prose spacing was corrected. Append-only TSV history retains its original
  compact spelling. The closure row records seven tests and 47 assertions plainly.
- F9. The proof was run on Windows with Bun 1.4.2. Its empty auth workspaces are
  intentionally retained outside Dione, as the test guide states. Linux OMP runtime
  validation remains part of the construct phase. No cleanup code was added.

Multiple top-level listeners can submit duplicate model input. Direct mode does
not retract events already submitted when a later edit or deletion arrives.
These limits do not change the existing shared delivery contract.

## Dependency advisory discovered during the signed gate

The first signed gate at ee89a264b959cdfe190e621f7b9d9ea2ba21b9c7 passed
1,673 feature tests, four default-feature tests, 34 Python tests, formatting,
both Clippy configurations, MSRV, and package checks. It then failed cargo-deny
on RUSTSEC-2024-0449. The log is /home/syn/code/dione-r7-pr-ready.log. The release
step in that gate was not reached. A separate release build passed before this
gate. Its log was overwritten during the later exact-head release run. No earlier
log file is retained. Only the exit-zero terminal result survives in the transcript.
The later log is /home/syn/code/dione-r7-release-ee89a26.log.

RustSec advisory-db commit 25a288f assigned the advisory on 2026-10-12 at
11:18:57 UTC, after R6's completed gate. The advisory describes unsafe-libyaml
0.2.11 as unmaintained and lists no patched version. It is informational
maintenance status, not proof that the parser is safe. The official text is
https://raw.githubusercontent.com/RustSec/advisory-db/main/crates/unsafe-libyaml/RUSTSEC-2024-0449.md.

R7 changes no dependency versions. The parser is reachable through Dione's
serde_yaml_ng nameplate loader. Typst also retains it through serde_yaml in
the dependency graph; this review did not trace a Typst execution path. Nameplates come from
a configured remote URL. The existing size check and output sanitization do not
prove parser memory safety. Replacing only serde_yaml_ng would leave the Typst
path. Alternative parsers exist, but unsafe-libyaml has no patched release.

A separate same-model source review in r7-advisory-review.md accepted an exception naming
only this advisory under the existing unmaintained-dependency policy. deny.toml
records both dependency paths and this follow-up. Other advisories retain their
existing enforcement. This is retained risk, not remediation or approval to deploy.
The PR records the rerun against the final signed head.

### Dependency maintenance after the fixed refactor

- [ ] Replace the nameplate YAML parser with a maintained implementation and
  verify current remote fixtures plus malformed-input behavior.
- [ ] Upgrade or replace the Typst dependency path that retains serde_yaml and
  unsafe-libyaml. Verify rendering and bibliography behavior.
- [ ] Remove the exact advisory exception once cargo tree confirms both paths
  are gone, then run dependency policy and the full local gate.

This maintenance is separate from R1-R7 and must remain visible in the stack
review before main. The unchanged yoke-derive 0.8.3 yanked warning is separate.

The user explicitly requested that both packages remain tracked for eventual
resolution. The unsafe-libyaml tasks above cover both parser paths.

- [ ] Replace yoke-derive 0.8.3 with a compatible non-yanked release through its
  dependency chain. Inspect the yank reason and parent constraints first, then
  run dependency policy and behavior checks for affected consumers. Do not treat
  a tolerated warning as completed maintenance.

### Final trail review

Claude Sonnet 5.5 reviewed the closure artifacts and scoped transcript items
5158-5278 in r7-trail-closure-review.md. It accepted the narrow exception as
reasonable under existing policy, with no blocking finding. This is an agent
policy decision for PR readiness under the user's autonomous-work grant. It is
not a human risk acceptance or authorization to merge or deploy. The policy
change has its own commit, and official stack review must decide its disposition.

The newly assigned advisory also affects the lower R4-R6 dependency graphs,
which retain the same parser without this exception. Their earlier local gates
passed before the advisory database update. R6 was published after the update
using that historical gate result. Fresh dependency checks at those unchanged
heads are expected to fail. The PR descriptions disclose this consequence.
The policy correction must be applied where needed during stack preparation
before considering merge. No lower branch was rewritten during this run.

The typecheck used bun-types 1.3.14 while the runtime proof used Bun 1.4.2.
Actual OMP runner behavior was exercised, but matching Linux construct runtime
validation remains open. Native verification stashes are retained as recovery
snapshots. They were not deleted under this implementation task.
