# TypeSafe attention — source and pipeline evidence

Observed 2026-09-21. This is a local design receipt, not implementation or deployment evidence.

## Repository identity and base

- Standard Dione: existing `origin` is `ssh://git@forgejo-ssh.svc.echoes:2222/lacuna/dione.git`.
- Feature branch: `callisto/typesafe-attention`.
- Isolated worktree: `/opt/data/work/dione-typesafe-attention`.
- Initial cached base: `8917d4aad2bc0564dbb1a55cd3cd71cdc96a1849`.
- Authenticated upstream SSH ref advertisement returned HEAD and `refs/heads/main` at **`2d52a986fd8c715182e9de1104921ecbd40f246a`**, with HEAD pointing to main.
- `git fetch origin main` then advanced `origin/main` from `8917d4a` to `2d52a98`; `git merge --ff-only origin/main` fast-forwarded the new branch. No feature implementation existed to replay or discard.
- Fresh manifest: Dione **0.45.0**, Rust **1.98**, workspace members `["."]`. Earlier 0.41.0/auspex-core observations are superseded.
- This verifies the base at observation time, not perpetual upstream freshness. Reconcile subsequent upstream changes before implementation.

The first shell probes encountered process-creation errors and one timeout. They did not establish authentication failure. Direct SSH through the configured identity succeeded, followed by Git fetch using `/usr/bin/ssh` without nested command wrappers. No credential values were read into conversation or changed. HTTPS version lookup required sign-in and was not used as an identity proof.

## Current coding route

The inspected seat contract is `/opt/data/dione-seat/state/pipeline/SPEC.md`, locked/amended 2026-09-18. It identifies Janus's seat copy as canonical; that remote copy was not contacted. Relevant local requirements:

- Q7: objective, mechanically checkable acceptance, boundary, and evidence; thin integrated vertical slices with actual dependencies.
- Q14: contract draft, separate different-family validation, then syn approves one contract before code.
- Q15/Q11: implement, different-family verify, different-family review, close approval, human-triggered PR, retro. A second OpenAI model is not a different family.
- Q8: evidence artifacts SHA-256 bound at verify and rechecked at close.
- Q9/Q13: bounded retries, digest-stall detection, terminal blocked-run disposition; no automatic relaunch of unchanged failure.
- Q19: capability-scoped fresh contexts, no messaging from implementation workers, read-only review, standards supplied to reviewers.
- Q4 limits the original proving scope to owned repositories. Syn explicitly selected standard Dione for this feature. Local branch work is authorized; this is not permission to alter shared deployment or publish.

Actual route evidence, not a claim that a new run has executed:

- `state/pipeline/runs/run-2-2026-09-18/audit/RUN.md` and `RETRO.md`: DeepSeek implementation and GLM verification/review, recorded gate chain, syn close approval, and artifact-presence lesson.
- `state/pipeline/runs/run-3-2026-09-18/CONTRACT.md` and `gate-log.jsonl`: draft and GLM validation recorded; subsequent transport block recorded. The old transport block belongs to that run, not this feature.
- `state/pipeline/tools/gate_log.py`: `GateLog.record`, `bind`, and `verify` provide existing JSONL gate/evidence records. Reuse rather than create a second scheduler or ledger.
- `/opt/data/work/turnstile/docs/omp-edit-gate.md`: Turnstile's prepared edit/write check is specifically a scoped Rust quality gate. Its existence does not prove the whole contract/verify/review pipeline is active in this session.
- Current resident roles inspected at `/home/core/.omp/agent/config.yml` are OpenAI Codex general models and a TypeSafe judge, with Nous disabled. No different-family contract validation has been run for this draft. Do not substitute Jev's classification for a capable independent code/contract reviewer or claim a credential is absent from this configuration alone.

For the next phase, select an actually available different-family validator through the established credential/provider route, record the family and findings, reconcile them, then obtain contract approval. This is a requirement on implementation admission, not a reason to withhold the local draft. This preparation does not publish to an issue tracker.

## Fresh-base check commands for later implementation

These are required future evidence, **not checks run or passed by this documentation-only preparation**:

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo clippy --workspace --all-targets --features oneshot-test-seam -- -D warnings`
- `cargo nextest run --workspace --no-fail-fast --features oneshot-test-seam`
- `cargo nextest run --workspace --no-fail-fast --test oneshot_send`
- `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`
- `cargo build --release`

Source: fresh `justfile`, `Cargo.toml`, and `CODING_STANDARDS.md`. The current no-publish instruction overrides the coding standards' ordinary commit/push completion convention for this preparation.

## Existing architectural vocabulary

`docs/design/delivery-contract-two-axis-map.md` distinguishes canonical event, delivery intent, attempt, consumer disposition, and outcome. It is design vocabulary, not proof all those stores exist. Attention decisions must not promote a dispatch attempt into evidence that a recipient read or wanted a message.

`docs/design/100-enrich-reply-context.md` documents the difference between reference IDs and best-effort hydrated reply previews. Missing/deleted parent content must remain unavailable, not generated or assumed present.

## Fresh-base source audit

Read-only worker `DioneAttentionSourceAudit` reconciled its findings against `2d52a986fd8c715182e9de1104921ecbd40f246a`; no tests or implementation were performed. Parent directly inspected the forwarding loop, Codex scheduling, Bell config, and lifecycle ledger.

| Boundary | Source at verified base | Finding and consequence |
|---|---|---|
| Targeting/access | `src/discord/events.rs:217–263,1609–1703,1727–1919`; `src/gate.rs:140–238,367–434` | Existing `MessageTargeting` distinguishes DM/directed/ambient, including configured mentions. Add designated direct rooms; preserve current identity/topology/mute/bot/webhook admission. Attention is not access authority. |
| Shared dispatch | `src/mcp/server.rs:314–438,637–672` | Rate limit → Bell enrichment → coalescing → sink. Insert attention before context serialization; ordinary rate-limit denials stay denied. |
| Bell analogue | `src/config.rs:296–371`; `src/bell_rings.rs:136–142,290–319,530–602` | Reuse provider/deadline/error/test patterns, not semantics: Bell enriches, never performs this admission policy or room-provider authorization. |
| Async behavior | `src/mcp/server.rs:363–387` | Bell Shadow uses detached spawn; Live awaits in the forwarding loop. Neither proves cancellation, epoch fencing, or direct-lane independence. |
| Config epochs | `src/config.rs:1077–1080,1279–1282,1919–2028` | Reuse `LoadedConfig` generation and serialized `ConfigRuntime` publication, adding restart-safe incarnation and effect-time checks. |
| Wire metadata | `src/mcp/notifications.rs:30–105` | Targeting is currently lost at serialization. Carry necessary source/scheduling compatibility metadata without changing off-mode payloads. |
| Codex scheduler | `src/codex/app_server.rs:221–283` | Active threads get `turn/steer`; idle threads get `turn/start`. Non-preemptive ambient delivery requires an actual adapter change, not a label. |
| Lifecycle | `src/discord/events.rs:270–307,978–1239`; `src/ingress_ledger.rs:1–13,30–33,57–100,530–568` | Edits/deletes lack create targeting. Ledger is process-local, with seven-day active/one-day tombstone retention and 16,384 capacity each. It stores hashes, not source text. |
| Durable queue | `src/codex.rs:208–247,399–517,753–760` | Full pending notifications persist until ack without TTL; later delete does not purge an earlier create, and access is not rechecked before injection. Feature-derived pending work needs revalidation. |
| Retrieval | `src/mcp/tools/messaging.rs:1320–1345`; `src/mcp/tools/search.rs:356–542` | Live Discord reads with channel/thread checks, not a source journal. Historical reads do not apply every author ingress filter or post-wait access recheck; feature retrieval needs its own current eligibility checks. |
| Archive | `src/config.rs:138–158`; `src/gaie/archive.rs:34–65,146–212`; `src/gaie/replay.rs:15–81` | GAIE is opt-in one-shot archival, not normal daemon ingress; deletion marks retain content/history. Do not substitute it for source-lifecycle guarantees. |

The old `ARCHITECTURE.md` describes SQLite/Qdrant storage that the current manifest/source does not implement. There is **no existing durable normal-ingress source journal** to reuse. The bounded implementation choice is original Discord source plus a feature-owned metadata/lifecycle index and expiring working cache, not a new permanent raw corpus or DioneZero. Retrieval remains conditional on source existence and current permission.

### Test seams

- `src/discord/events.rs:2711–2826` tests the actual pure admission authority; `:3119–3193,3279–3335` covers post-wait webhook access.
- `src/bell_rings.rs:828–875,951–1007,1094–1159` supplies zero-call, timeout/error and scope-precedence patterns, not full attention delivery proof.
- `tests/delivery_pipeline.rs:122–158,1029–1103` mirrors production and omits provider work. Extending only this simulation would not test the real forwarding path.
- `src/mcp/server.rs:1015–1082` exercises the real Codex sink/buffer, but not the whole chain. Extract the real production admission/forwarding step behind one narrow testable interface, then test controlled provider/clock/sinks plus backend-specific scheduling. Never maintain a second test-only pipeline.

## Live TypeSafe provider audit

Read-only worker `TypeSafeAttentionContractAudit` read the live index, API, models, error references and legal pages. No private material, credentials, inference calls, or account mutation were used. Local archived text was used only for truncation cross-checks; the live rendered API confirmed the error table.

- [API](https://docs.typesafe.ai/api): `POST https://api.typesafe.ai/v1/systemone`, required model; success includes `model`, `answers`, `usage`. No mandatory unique response ID or model fingerprint is documented. Record a local request ID/time, requested model, and exact returned model; optional request-id headers are supplementary.
- [Models](https://docs.typesafe.ai/models): `jev-1.13.0` is the documented current exact version. `jev-latest` and `jev-preview` currently resolve to it but can move. Response `model` reports the exact answering ID. TypeSafe recommends version pinning when tuning thresholds. `GET /v1/models` lists aliases, not an authoritative alias-resolution feed; exact IDs can work without appearing in that list. No immutable-weight checksum or version-lifetime guarantee is published.
- [API errors](https://docs.typesafe.ai/api#errors): 401 invalid/missing key, 422 invalid input, 429 rate limit, 529 overloaded. Exponential backoff is documented for 429/529. Credit exhaustion has no documented exact status/body/retry contract; do not assume HTTP 402 or equate every 429 with an empty balance.
- [MCA §8.2](https://typesafe.ai/legal/mca): absent customer-opted-in refill, exhausted credits may cause Output to be declined. Feature does not inspect/change refill settings, purchase credits, or introduce an allowance. Generic unavailable/error handling must preserve ordinary delivery even when an unrecognized exhaustion response occurs.
- [Models data handling](https://docs.typesafe.ai/models#data-handling), [Privacy](https://typesafe.ai/legal/privacy-policy), [DPA](https://typesafe.ai/legal/data-processing), and [MCA §§4.1–4.3,10.3](https://typesafe.ai/legal/mca): no training on customer inputs is not zero retention. Standard retention is necessity-based, with no numeric raw-data deletion SLA. Telemetry/anti-abuse/legal purposes and backup exceptions exist. US processing and subprocessors are contemplated. Local source deletion cannot promise provider-side erasure.
- [Legal](https://docs.typesafe.ai/legal) offers enterprise ZDR by contacting the provider but does not publish its detailed coverage. No ZDR entitlement or account-specific terms were verified here. Do not claim ZDR for this deployment.

### Downstream learning boundary

[Models — Customizing Jev](https://docs.typesafe.ai/models#customizing-jev) explicitly points to training a downstream classical model on Jev probabilities. [MCA §2.3(b)](https://typesafe.ai/legal/mca) prohibits using Services/Output to perform model distillation, train a model to imitate Services output, or develop a similar/competing product. Preserve both facts.

This contract trains toward **recipient-authored wanted/timeliness labels**, using Jev scores as features; it does not fit Jev output as the target or replace Jev with a distilled substitute. [INFERENCE] That matches the documented downstream-feature pattern rather than imitation. Public material is not an account-specific legal safe harbor; no contrary Order terms were inspected. If implementation changes into imitation/distillation or a competing classifier, hold that change for a specific contract review rather than silently treating it as the accepted feature. No need to invent a blanket prohibition on the docs-endorsed local learning pattern.
