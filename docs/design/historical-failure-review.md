# Historical failure review for PR quality gates

The baseline was `origin/main` at `5d5153b` on 2026-10-07. This is a focused sample of merged fixes, not an
inventory of every prior PR. The question for each fix is whether a current
check observes the user-visible failure that motivated it.

| Past failure | Current check | Remaining boundary |
| --- | --- | --- |
| A consumer-free v0.48.0 Codex inbox could leave old entries behind during an update. | `tests/codex_inbox_upgrade.rs` loads a file written by v0.48.0, drains both entries, reopens the queue, and times out a hung child. `historical_inbox_does_not_block_new_live_delivery` starts the live worker with that file present, expects a newer live event to reach a fake app server, and checks that the older entries survive. | This fixture has no registered consumer. It does not reproduce the v0.46.5 pre-kind live-primary reservation that broke startup in [#472](https://forgejo.svc.echoes/lacuna/dione/pulls/472). [#473](https://forgejo.svc.echoes/lacuna/dione/issues/473) and [#475](https://forgejo.svc.echoes/lacuna/dione/pulls/475) track that migration. The tests also do not start the real Discord gateway or Codex process. |
| Inbox rename followed by directory-sync failure could lose ordering or terminate live work ([#467](https://forgejo.svc.echoes/lacuna/dione/pulls/467), `b724e14`). | `uncertain_lease_does_not_hide_first_event_behind_second`, `live_worker_waits_for_pending_sync_at_registration`, and the live renewal, defer, and invalidation tests inject persistence failures and check recovery. | A local injected failure cannot prove durability under a real power loss. |
| Downloaded attachments with the same or long filename could overwrite each other ([#463](https://forgejo.svc.echoes/lacuna/dione/pulls/463), `401767d`). | Repeated and concurrent download tests check each returned file's bytes; long-name tests exercise 254/255-byte component limits. | These use a local HTTP server and filesystem, not Discord's CDN. |
| Delivery of a long Codex thread requested unbounded history and hit a frame limit ([#384](https://forgejo.svc.echoes/lacuna/dione/pulls/384), `91a105d`). | `delivery_does_not_reach_available_history_larger_than_64_mib` puts an oversized response behind the old request path and expects delivery to use bounded `thread/read`. | The fake server verifies the request contract; it does not measure a deployed Codex version. |
| A stale active turn caused repeated rejected steering and blocked later events ([#415](https://forgejo.svc.echoes/lacuna/dione/pulls/415), `83d9f97`). | `stale_active_turn_retries_once_with_server_reported_turn`, `second_stale_turn_mismatch_fails_closed_after_one_retry`, and the discriminator test assert the outgoing requests and bounded retry. | Codex can change its JSON-RPC error wording; a changed response must fail closed and be reviewed. |
| A short consumer TTL reset to the default after refresh ([#342](https://forgejo.svc.echoes/lacuna/dione/pulls/342), `ce946da`). | Lease/ack refresh, legacy state, and restart tests assert exact expiration times. | Real-clock scheduling remains outside these deterministic checks. |
| A trusted-main release tag job lacked Rust and failed before policy evaluation ([#421](https://forgejo.svc.echoes/lacuna/dione/pulls/421), `b03e945`). | `workflow_exposes_tag_writes_only_after_required_checks` asserts a pinned Rust setup step before checkout and tagger execution. The local branch's `Required checks` job gates the main-path tagger. | The tagger only runs on pushes to `main`; its runner environment is not executed in PR CI. A manual release drill should verify this path after changing the workflow or runner image. |

The new worker test uses the historical inbox bytes and a local fake app server.
It observes the `turn/start` request for a new event, the acknowledgement,
and both older entries after reopening. It passed in a native Linux checkout.
In that disposable checkout, making the worker exit when it saw queued old
data made this test fail at its registration assertion. The original worker
source was restored and the test passed again. The remaining
gateway and real-runner boundaries belong in a small release drill rather
than a claim that the unit suite covers production startup end to end.
With the worker restored, the default workspace Nextest run passed 1,560 tests
with one skipped. All-target Clippy with warnings denied and the workspace
format check also passed in the native Linux checkout.

This table supports [issue #479](https://forgejo.svc.echoes/lacuna/dione/issues/479)
and [PR #480](https://forgejo.svc.echoes/lacuna/dione/pulls/480). The issue
tracks uncovered boundaries. Forgejo's branch rule remains the merge gate.
