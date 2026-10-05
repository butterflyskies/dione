# Teams process-lifetime diagnosis — 2026-09-04

## Scope

This receipt covers local process lifetime only. It does not claim Azure Bot,
Microsoft Teams, public tunnel, deployment, or correlated-reply success. No live
Teams canary was run.

## Third-attempt finding

The third live launcher attempt did not start the Dione candidate. The operator
transcript contains only the hidden credential prompt and the successful
`devtunnel user show` line before the launcher returned. Afterward,
`/srv/janus-seat/state/teams-resident-live-proof/` contained neither candidate
nor tunnel logs or PID receipts. The launcher creates all four only after it
stops and verifies the incumbent transport.

Therefore no Dione or Teams runtime shutdown/cancellation path fired in the
third attempt. The launcher exited in its pre-candidate cutover section, between
the Dev Tunnel identity check and candidate-log creation. Its `ERR` cleanup then
restored the incumbent transport. The old report that the program itself stayed
open and exited was incorrect: the visible lifetime belonged to the launcher.

The old launcher did not persist step identity or the failed shell status, so
the surviving evidence cannot distinguish its three silent pre-candidate gates:
stopping the incumbent transport, reading it back as inactive, or confirming
the incumbent PID had disappeared. Another live retry is not justified merely
to recover that missing detail.

## Local listener proof

The current `teams_edge_probe` was run with dummy, non-secret identity values,
a loopback listener, and no HTTP, Azure, or Teams traffic.

- Start: `2026-09-04T03:15:50.994904367Z`
- Bound log: `2026-09-04T03:15:51.004943Z`, `127.0.0.1:43981`
- Idle observation: process and listening socket still present after 3 seconds
- Termination: deliberate `SIGTERM` at `2026-09-04T03:15:54.033567049Z`
- Wait status: `143` (signal-driven termination)
- Panic/stderr output: empty
- Local evidence directory:
  `/srv/janus-seat/state/codex/teams-lifetime-logproof.6eKG1S`

A separate pre-instrumentation run observed the same listener alive at 1, 5,
and 10 seconds, then deliberately terminated it with `SIGTERM`/143. No
spontaneous runtime exit occurred.

## Candidate instrumentation and tests

The candidate now logs:

- the exact listener address after successful bind;
- receipt of the shared cancellation token;
- normal listener completion or listener failure; and
- normal or failed completion of the top-level Teams listener task.

`teams_listener_remains_alive_idle_until_explicit_cancellation` connects to the
bound loopback listener, proves the task remains pending while idle, cancels the
explicit token, and requires clean bounded completion.

Focused validation:

- `cargo fmt --check` — pass
- `cargo test --lib teams_runtime::tests -- --nocapture` — 2 passed
- `cargo test --bin dione runtime_shutdown_tests -- --nocapture` — 1 passed
- `cargo build --bin teams_edge_probe --bin dione` — pass

## Disposition

The Teams listener is deterministic while idle and exits only after an explicit
cancellation or process signal in the local proof. Commit `59af6ca` remains a
separate bound on final Tokio runtime teardown; it is not the cause or fix for
the third launcher failure.

The next live canary remains barred until a launcher records every pre-candidate
transition and preserves its failure status. Repository review can proceed on
the frozen code candidate without access to the Janus host, credentials,
launcher runtime, or Teams-connected environment.

## Fourth-attempt follow-up

The instrumented retry at `2026-09-04T12:26:32Z` crossed the corrected
incumbent-stop gate and created `candidate.log`. The candidate then exited with
status 1 because Clap rejected `--resident-control-socket`; `tunnel.log`
remained empty because the Dev Tunnel host was never started.

The mismatch was compositional rather than a Teams, Tokio, credential, or Dev
Tunnel failure. `feat/teams-rust-transport` contains the Teams resident bridge,
while the deployed Janus resident-control socket was maintained as a separate
reviewed patch and was not present in Forgejo `main`. The launcher incorrectly
assumed both surfaces existed in one binary.

The composed candidate replays that resident-control patch while retaining the
Teams wiring. Its acceptance boundary is exact CLI parity, the closed resident
socket allowlist and negative cases, Teams idle/cancellation behavior, bounded
runtime teardown, and verified restoration of the incumbent transport. No live
canary is authorized until the frozen composition receives repository review.

## Fifth-attempt follow-up

The composed candidate bound the Teams listener, then received immediate EOF
on MCP stdin because the non-interactive launcher backgrounded it without an
open stdin. That EOF cancelled the shared runtime, and Teams and Dione stopped
normally before the tunnel launched.

The lifecycle correction distinguishes stdio-owned and resident operation.
Ordinary stdio mode retains EOF-to-shutdown. When a resident-control socket is
configured, stdin EOF detaches the stdio request transport and waits for shared
cancellation, leaving notification delivery and the resident runtime alive.

Process-level proofs used the rebuilt candidate and no Teams/Azure traffic:

- resident mode launched with `</dev/null>` kept the Teams listener, resident
  control socket, and Discord gateway alive until SIGTERM; it exited 0 and the
  incumbent transport was restored;
- ordinary mode launched with `</dev/null>` logged EOF-to-shutdown, exited 0 in
  172 ms, and the incumbent transport was restored.

The same attempt also showed the launcher omitted `DIONE_STATE_DIR` and fell
back to `/srv/janus-seat/.claude/channels/dione`. That separate launcher defect
is not part of this lifecycle-only repository change and remains a live-canary
blocker.

## First live-ingress follow-up

After the launcher exported the canonical `DIONE_STATE_DIR`, the candidate,
Dev Tunnel, Teams listener, resident-control socket, and Discord gateway all
remained live. One authenticated Microsoft Teams activity reached the bound
Codex thread with an opaque single-use reply handle.

The correlated reply did not run. In resident operation, stdio is deliberately
detached after EOF, so the resident-control socket is the callable MCP surface.
Although `teams_reply` existed in the shared dispatcher and the Teams event
preamble instructed Codex to use it, the resident socket's exact closed social
tool allowlist omitted it and rejected the call before dispatch. No direct
`serviceUrl` request or other authority bypass was attempted.

The bounded correction adds only `teams_reply` to that closed resident social
surface. Existing reply-handle validation, single-use consumption, confined
service-host checks, token acquisition, and outbound construction remain in the
Teams reply authority; the resident socket receives no raw `serviceUrl` or
credential capability.
