# TypeSafe attention — implementation contract

**Status: approval candidate reviewed under syn's explicit one-run same-family exception. Not approved, implemented, published, or activated.**

Target: **standard Dione**, never DioneZero. Branch: `callisto/typesafe-attention`. Reviewed base: `2d52a986fd8c715182e9de1104921ecbd40f246a` (Dione 0.45.0). [Source and pipeline evidence](typesafe-attention-source-evidence.md) distinguishes observed facts from proposed changes. [Domain glossary](typesafe-attention-context.md) defines terms.

## Objective

Reduce ambient Discord material injected into a recipient's working context while reliably delivering wanted material. Preserve the existing direct-delivery lane, source attribution, access controls, and recoverability. Use TypeSafe for narrow semantic judgments; let deterministic code and the recipient own admission, learning, and activation.

This is reusable Dione functionality, initially exercised only for Callisto. It must work without OMP at application runtime. It must not turn a recipient's silence into disinterest, learn a personality on their behalf, or enroll other recipients implicitly.

Success is end-to-end behavior and evidence against the acceptance matrix below, not merely a classifier wrapper or an offline accuracy score. Implementation approval does not itself authorize live provider traffic, shared deployment, publication, or enforcement.

## Boundary

In scope:

- Typed recipient/room attention configuration; off/log/on modes and independent failure-notice controls.
- Deterministic direct-lane routing, bounded source-context selection, explicit provider eligibility, TypeSafe HTTP judgments, and admission before harness context delivery.
- Source-bound decision metadata and authorized retrieval of deferred material, without a second permanent raw-message archive.
- Explicit recipient feedback, representative shadow review, replay of stored judgments, recipient-local candidate fitting and evaluation, explicit policy promotion and rollback.
- Source edit/delete/access invalidation, model/rubric revalidation, restart/config-change reconciliation, and loud normal-delivery fallback.
- End-to-end tests and operator/user documentation of these behaviors.

Out of scope:

- DioneZero, a new generic event bus, replacing Dione's transport, generic memory/personality inference, or a separate autonomous scheduler.
- Training Jev, cross-recipient learning by default, automatic speech, inferred permission, automatic top-ups, spending caps/allowance gates, or automatic alternate-provider fallback.
- Implementing a general model-provider framework or adding Python/JavaScript services merely because TypeSafe offers SDKs in those languages.
- Publishing, deployment, live room export, or autonomous activation as part of this preparation.

## Agreed contract

### 1. Mode and authority

- A recipient has default `off`, with per-room `off | log | on` overrides. An operator-wide emergency off dominates all overrides.
- Provider eligibility is a separate explicit room-level permission. Neither a mode nor room-read access grants it. No implicit DM/private-room export. Ineligible traffic retains ordinary delivery and makes zero classification requests.
- `off`: no new TypeSafe requests; no classifier-derived alteration of delivery. Existing retained private records need not be deleted just by disabling classification, but are not processed by a background learning loop in off mode; explicit local review/deletion remains possible.
- `log`: eligible ambient judgments and hypothetical admission are recorded, but actual delivery follows the existing path unchanged. It is neither free nor provider-private; content is sent to TypeSafe.
- `on`: valid admission policy affects eligible ambient traffic only. Selection may leave unwanted ambient material out of working context indefinitely, subject to source availability and retention; this is not a promise to deliver everything later.
- The recipient controls their own feedback, attention brief, and learned-policy promotion. Operator configuration may restrict provider access or disable service, not silently impersonate the recipient's preferences.
- Access, mute, sender, identity, mention, and outbound permissions remain prior constraints. Fallback never bypasses them. Config mutation uses the existing authorized control surface; an untrusted Discord message cannot switch modes.

### 2. Direct lane and semantic state

Direct traffic bypasses classification: explicit mentions, verified replies to the recipient, authorized DMs, and explicitly designated direct-conversation rooms. A sender's presence is not a blanket exemption across all group rooms. These rules do not override existing gates or require a response.

The recipient supplies an inspectable attention brief: interests, curiosity, open conversations, and current work. Do not scrape a whole memory store or infer a permanent personality profile. Missing, stale, or insufficient brief/conversation evidence is an explicit unknown state and preserves normal delivery; it is not a low-relevance label.

Jev receives a bounded segment containing the trigger plus necessary source antecedents, with actor/channel/reply identities and relevant brief fields. Every included message and brief field must be eligible for that provider and recipient. No cross-room context fetch simply because a link or reply points there. Do not download attachments or follow arbitrary URLs as an implicit part of classification; unavailable non-text context is marked missing.

The judgment returns independent reusable dimensions (for example: answers a pending exchange, changes relevant work, invites participation, timely relevance), not one exclusive topic and not a generated summary. Deterministic code binds each answer to the submitted source versions. Question IDs are not semantic instructions; define every dimension explicitly. Keep probability, ordinal score, and model confidence distinct.

### 3. Admission and delivery evidence

The policy yields `prompt attention`, `next natural turn`, or `retrieval-only`, or an explicit `unknown/failure` outcome. Prompt attention means priority at the next supported safe delivery opportunity, not permission to abort running work. Preserve backend-specific delivery semantics; do not call delivery "safe" if the adapter actually interrupts an active turn.

Deliver source excerpts and handles, not model-authored factual assertions. Code re-resolves source IDs/versions, audience, mode/config generation, attention-brief version, and policy compatibility immediately before the actual admission effect. Treat stale results as unusable. Where original evidence is gone, report unavailable rather than inventing a replacement.

An admitted pending item is different from a deferred item, a transport attempt, and a consumer receipt. A classification result must not count as delivery, successful harness dispatch must not count as recipient interest, and explicit feedback must remain attributed to its author.

Already-delivered source content should not be injected again as new traffic solely because a segment overlaps. Preserve ordering within a conversation and the existing create/edit/delete lineage. Do not collapse unrelated message IDs based on semantic similarity.

### 4. Mode transitions, recovery, and faults

Each request/result is bound to a configuration generation. Turning off stops new provider requests, cancels outstanding work where possible, and makes any later responses unable to change delivery or feed a stale policy. It cannot retract data already transmitted to the provider; documentation must say so.

For `on → off` or `on → log`, new traffic resumes ordinary delivery; already-admitted pending deliveries stay pending under current access/lifecycle rules. Previously deferred history remains available for deliberate, bounded authorized replay—not an automatic backlog dump. An item awaiting its first decision at the transition is unresolved new traffic, not silently reclassified as historical deferral; return it once to normal delivery if still valid.

Provider errors, timeout, credit/quota exhaustion, malformed responses, local decision-store failure, and incompatible model/rubric state must not silently drop eligible traffic. Use normal delivery, expose degraded state, and continue the direct lane independently. Unknown semantic evidence is ordinary fallback, not necessarily a service outage. Do not introduce an allowance, spending-approval gate, automatic purchase, or provider switch.

Configuration and record corruption are explicit faults, never silently treated as an intentional off mode or permission grant. If provider eligibility cannot be established, send nothing to TypeSafe. Existing permission gates remain effective while normal delivery handles otherwise eligible traffic.

Failure controls are independent: Discord notices `off | failures | failures-and-recovery` (proposed default: failures-and-recovery), configurable repeat cooldown, deduplication/coalescing for flapping, and a persistent operator-visible degraded/recovery state with counts and cause. Notices go only to the affected recipient's authorized route, never an unrelated person or room. They bypass their own classifier and cannot trigger a recursive notice loop. If that delivery route fails, local/operator diagnostics still expose the fault. Muting chat notices does not suppress diagnostics.

Recovery can resume the previously configured `log` or `on` behavior only when compatible service is restored; an explicit user/operator `off` or muted-notice choice remains off/muted. Never mistake service recovery for policy promotion. A bounded health/retry policy must avoid a request per queued message during a known outage.

### 5. Learning and feedback

Record immutable raw judgments plus source/version handles, question/rubric version, requested and reported model identity, brief/config/policy versions, timestamp, sampling propensity, and actual versus hypothetical delivery. Minimize retained content; never log credentials or raw provider prompts as diagnostics.

Feedback distinguishes `wanted promptly`, `wanted later`, `not needed`, and `unsure`, with annotator identity, source version, and assessment time. Recipient labels are primary. Other people's corrections remain separately attributed and may prompt reconsideration; they do not overwrite recipient experience. No response, no reaction, and no annotation are all unlabeled—not negative.

Accept voluntary corrections and bounded recipient-chosen review batches. Sample admitted and would-be-deferred material across the entire score range, not just uncertain or high-scoring items. Store sampling probabilities so reported estimates can account for unequal selection. Do not imply that statistical weighting fixes missing/nonrandom feedback; report coverage and uncertainty.

Start with replayable thresholds/weights. Fit a small regularized recipient-local model only if sufficient labels support comparison; logistic regression is the proposed first candidate, not a mandatory dependency or assumed improvement. Training and replay are local and do not require new Jev calls over already-scored inputs. Never pool other recipients' content, labels, briefs, or preferences without a separate opt-in.

Use conversation-grouped temporal training/validation/evaluation partitions. Select thresholds and tune on training/validation only; reserve later untouched conversations for promotion assessment. Compare with normal unfiltered delivery and a simple fixed-policy baseline. Report wanted-item recall, timely recall/delay, unwanted delivery, admitted source volume/context reduction, cost/latency, uncertainty, and review coverage separately. A higher engagement/reply rate is not an objective.

Require meaningful context reduction while staying within a predeclared wanted-item miss/delay tolerance. Numerical tolerance and minimum evidence must be fixed after baseline characterization but before opening the final evaluation set. Insufficient evidence stays in log mode. Policy promotion is an explicit, authenticated recipient action tied to an evaluated artifact digest and compatibility signature; retain a compatible rollback. Log mode never silently becomes enforcement.

Changes to Jev identity, questions/rubric, feature meanings, or model-dependent preprocessing invalidate old calibration. Do not apply a returned score to the active learned policy before checking identity. Move the affected learned-policy path to shadow evaluation with ordinary delivery, preserve a compatible prior artifact for rollback if available, and report according to configured controls. If a provider alias can change without an observable identity, do not claim this guard is reliable: use an adequately identifiable/pinned model or keep enforcement unavailable for that path.

### 6. Data lifecycle

Use original source records and existing access/deletion policy; do not create another permanent raw-message archive. Deferred retrieval is possible only while the source exists and current access permits it. The admission ledger, delivery queue, and external evidence log are not interchangeable with a durable source journal.

A bounded feature-owned metadata index may retain judgments, source handles/hashes, feedback, and artifact membership, with explicit expiry and deletion. Source text needed for immediate segmentation is an expiring working cache, not training-corpus duplication. Proposed review defaults: working segments expire after their decision; unpromoted judgments/labels expire after seven days; any alternative retention must be explicit in configuration and documented. Durable learned policies require an auditable bounded membership manifest and revalidation schedule; they must not become an indefinite way to retain purged training information.

Edit invalidates the prior source version. Delete or access loss invalidates pending admission/replay and access to derived evidence; remove affected labels/features and withdraw a learned policy until it is rebuilt without invalidated inputs or a compatible unaffected policy is selected. Do not claim machine unlearning by deleting a row. On restart, recheck current source/access before replay or promotion; source-fetch failure means unknown/unavailable, not proof of deletion or continuing permission.

The base must support these facts, or the implementation must add the narrow missing lifecycle plumbing. An offline disconnect can hide deletion events: validate training membership before fitting/promotion, expire evidence, and define a bounded revalidation cycle for active policy members. Do not promise instantaneous deletion awareness while disconnected, provider-side erasure, or unlimited reconstructibility.

## Source-grounded integration decisions

These decisions incorporate both completed read-only audits, reconciled to the verified 0.45.0 base. Detailed file/line evidence is in the companion source receipt. They are design requirements, not claims of implemented functionality.

1. **Reuse targeting and access, add admission.** Preserve existing `MessageTargeting` and configured mention semantics from the gateway. Add explicit direct-room designation without conflating an open room with a direct room. Use the shared forwarding boundary before serialization/coalescing, after existing ingress and rate-limit eligibility. Keep lifecycle events out of relevance-based suppression: edits/deletes invalidate feature state even when their original source was deferred.
2. **Adapt Bell conventions, not Bell behavior.** Bell already has provider/deadline/fail-open patterns but only annotates, and its Shadow work is detached. Do not rename Bell Shadow/Live to log/on and claim admission is done. Keep Bell's independent configured behavior intact; TypeSafe provider eligibility governs this feature's egress, not a retroactive authorization claim about unrelated providers. Use structured, bounded classifier work with cancellable ownership; waiting ambient work cannot block subsequent direct events.
3. **Fence the actual effect.** Bind to configuration generation plus a process incarnation, source version, recipient/brief and policy signature. Extend the existing config runtime instead of a second config reader. Recheck current eligibility after asynchronous fetch/judgment and at final sink injection for feature-derived pending work. Existing queued creates are not automatically purged by a delete, so add the narrow lifecycle-aware invalidation required for attention-managed items. Do not silently claim all legacy queue history has been retroactively repaired.
4. **Implement non-preemptive scheduling in the adapter.** Current Codex delivery steers active turns; preserve that existing direct/off/failure-fallback behavior, but attention-managed ambient next-turn/prompt items must wait for a safe idle opportunity rather than steer. Carry typed scheduling metadata through the queue. For plain MCP push, retain existing push behavior and never invent a turn-cancel API; advertise scheduling capability honestly and use the highest safe consumer handoff available. If the consumer provides no safe-turn signal, do not claim an idle guarantee from a priority label. A13 must demonstrate that classification introduces no preemption, and the operator docs must state each backend's guarantee.
5. **Original source, not a fictional journal.** Normal Dione has a process-local admission ledger and live Discord retrieval, not the durable raw source journal assumed in the early diagram. Add only a bounded feature-owned metadata/lifecycle index with atomic persistence and expiring working segments. Rehydrate deferred material from Discord under current eligibility. Missing source is unavailable, never silently reconstructed from an unbounded raw queue/archive. This preserves the agreed no-second-permanent-corpus boundary.
6. **Use the actual production seam.** Extract a narrow callable forwarding/admission unit from the real loop for integrated tests. Existing delivery tests mirror production and omit provider logic; extending that simulation alone fails this contract. Exercise actual config publication, attention state, buffer/sink, and backend scheduling, with controlled network/clock dependencies.
7. **Native Rust HTTP integration.** Reuse existing `reqwest`, `serde`, Tokio and typed error conventions for `POST /v1/systemone`; no OMP helper or sidecar runtime dependency. Default the implementation's requested model to pinned `jev-1.13.0`, require a well-formed reported `model`, and bind that returned ID into every result/artifact. API version and model version are separate. Missing/mismatched identity must not reach enforcement. Exact IDs have no published weight fingerprint or lifetime guarantee, so observed version checks and drift monitoring are complementary, not proof of immutable behavior.
8. **Do not invent credit/error semantics.** Handle documented 401/422/429/529 distinctly, honor retry guidance within the bounded delivery deadline, and classify unknown non-success responses as visible provider failure. Public docs do not specify exhaustion's status/body; a fake 402-only path is not acceptance. Sanitize diagnostics rather than echoing private request/response bodies. No allowance, purchase, or alternate-provider machinery.
9. **Learning targets recipient labels, not Jev imitation.** The model docs explicitly support downstream classical models using Jev probabilities; MCA §2.3(b) prohibits distillation, imitation of TypeSafe output, or a similar/competing service. Fit recipient wanted/timeliness labels using scores as features, not scores as training targets. This is a design distinction supported by the cookbook, not an account-specific legal opinion. A later change to imitation/competing service is outside this contract. Standard provider data terms do not promise zero retention or a numeric deletion SLA; local deletion tests cannot claim erasure of TypeSafe's logs, telemetry, backups, or subprocessor copies.

**Scheduling capability boundary:** the selected implementation must give each enforced path an actual safe-delivery mechanism. For Codex, use its observed idle state without steering ambient work. For a push-only consumer with no turn-state signal, add the narrow consumer-ready/next-turn handoff needed by that adapter, or keep that path visibly non-enforcing with ordinary delivery until supported. A priority field alone is not acceptance. This is an explicit integration dependency, not permission to claim universal next-turn delivery or silently omit a supported runtime. The contract-approval review must confirm the adapter coverage; the source receipt identifies the present gap.

## Integrated implementation slices

These are delivery slices, not permission to start code. No slice alone satisfies the whole feature.

| Slice | Integrated outcome | Depends on | Observable evidence |
|---|---|---|---|
| S1 | Typed mode/eligibility/direct-lane control reaches the real dispatch boundary; default/off behavior remains ordinary delivery | Approved contract and base | A01–A04, A08 |
| S2 | Source-bound eligible ambient judgments run in log mode without altering delivery; unknown/failure reporting works | S1 | A05–A07, A10–A12 |
| S3 | On-mode admission, safe scheduling, transitions, lifecycle invalidation and authorized deferred retrieval work through supported backends | S2 | A08–A10, A13–A16 |
| S4 | Recipient feedback, unbiased-enough review sampling, replay, fitting, holdout evaluation and explicit promotion operate on real decision records | S2; promotion requires S3 | A17–A21 |
| S5 | Restart, deletion/access loss, outage/recovery, rollback, end-to-end regression and operator documentation close the complete contract | S3, S4 | A01–A23 and full check receipt |

S3 and S4 can proceed independently after their common record/interface contract in S2 is settled. Each worker gets bounded tools and owned files; integration joins only genuine dependencies. Prefer a deep admission module with a small interface to scattering provider calls throughout transports. Reuse existing HTTP/config/error/delivery conventions; no new generic framework.

## Behavioral acceptance matrix

Every row is a reproducible acceptance scenario, not an assertion that the implementation exists. Use deterministic disposable sources identified by IDs/versions, a controlled clock, a request-recording fake provider, and a receipt-recording transport sink at the real production admission boundary. Compare normal delivery with the same input stream in off mode. Preserve the input/configuration, emitted source IDs/versions, provider call count and authorized captured fields, control/status responses, and raw check output. Fixture-only numbers below do not choose live retention, retry, sampling, or promotion defaults. Never export a real room to run these cases.

| ID | Scenario | Required observation |
|---|---|---|
| A01 | Replay the same authorized ambient source x once with no attention config, explicit off, and emergency-off over room on. | For each case the production sink transcript equals the off baseline (x once); the fake TypeSafe server records zero requests. Enabling the room override cannot defeat emergency off. |
| A02 | Set log and on separately in an ineligible room; then use eligible trigger x whose parent p is provider-ineligible. | Ineligible-room runs send x once by the ordinary route and make zero TypeSafe requests. For the eligible-trigger run, captured provider input contains no text or fields from p; missing context causes ordinary delivery if a judgment cannot be supported. |
| A03 | Submit separate admitted events m (mention), r (reply), d (authorized DM), c (designated direct room), and g (same author in an eligible ordinary group room). Hold the ambient provider response. | m/r/d/c reach the baseline sink without waiting and cause zero classification calls. g produces an ambient request and is not routed as direct merely because its author matches. Unauthorized versions remain denied. |
| A04 | Submit denied sender/channel events, muted traffic, and an admitted message saying switch attention on while configuration is off. | Denied/muted IDs appear in neither delivery nor provider traces. The admitted text follows normal off delivery, causes zero provider calls, and a subsequent effective-configuration read still reports off. |
| A05 | Replay ordered ambient x,y and direct d in off and log; in log hold responses, complete y before x, then repeat with a provider failure. | Delivery transcripts (source IDs, order and create/edit/delete lineage) match the off run; d arrives before the held judgments complete. Only the separately retrieved hypothetical-decision records differ. |
| A06 | Classify trigger x with allowed parent p, missing parent q, deleted parent r, and embedded text instructing a mode change. | The captured request contains exact authorized x/p excerpts and IDs plus missing markers, but no invented q/r text. Delivered excerpts match source bytes/versions. Effective mode is unchanged and no instruction in message text is executed. |
| A07 | In on mode replay x with no brief, a brief explicitly marked stale, and a message requiring unavailable attachment context. | Each run reports unknown rather than low relevance and delivers x once by the off-baseline route. Network capture shows no attachment download or URL follow. Unknown does not create a not-needed label. |
| A08 | Run four transitions separately: on to off, on to log, log to on, and off to on. In the on cases hold undecided x, retain deferred h and admitted pending p; in log deliver x normally while holding its hypothetical judgment; in off deliver x normally with zero provider requests. | On-to-off/log returns still-valid undecided x once to normal delivery, retains p under current access, and never auto-injects h. Late old-generation responses have no effect. Log-to-on does not redeliver its already-delivered x when the held log answer arrives. Off-to-on does not classify or replay old x. In all four cases a fresh event follows the new mode. |
| A09 | Admit x version 1; repeat its gateway event and an overlapping segment, then edit to version 2, delete, and reconnect. Release a held version-1 result after the edit. | The sink contains at most one new-create delivery for x, correct edit/delete lineage, and no new version-1 excerpt after invalidation. Authorized deferred lookup after deletion returns unavailable rather than cached text. |
| A10 | Independently inject timeout, malformed payload, documented HTTP errors, an arbitrary non-success exhaustion response, and local metadata-write failure; then submit admitted ambient x and direct d. | Each fault delivers eligible x/d by the ordinary route, exposes a specific sanitized degraded reason, and makes no alternate-provider or purchase request. A denied source remains absent. Capture deadline/retry counts against the configured finite fixture values. |
| A11 | Use fake time with notice cooldown 60 seconds. Inject the same outage twice within that interval, recover, fail again during cooldown, and repeat for all three notice modes and a failed notice sink. | Off emits no chat notices. Failures emits no recovery notices. Failures-and-recovery may emit the permitted transition notices; repeated same-cause failures within cooldown produce at most one failure notice. Local status/counts remain observable in every case; notices make zero classification calls and a failed notice send creates no recursive send loop. |
| A12 | Begin in configured log/on, induce failure then restore a compatible provider; separately set explicit off during the outage and mute notices before recovery. | The first cases return to their prior configured log/on behavior on the next eligible event without a promotion action. The explicit-off case makes zero new classification calls after recovery; muted notices stay silent while local recovery status changes. |
| A13 | Hold a real adapter in an active turn. Produce prompt, next-turn, and retrieval-only outcomes; then signal the documented idle/consumer-ready transition. Also exercise an adapter without a safe-turn capability. | While active, attention-managed events produce no abort or turn/steer and no ambient injection. After the ready transition prompt/next-turn source IDs dispatch once in the documented order; retrieval-only never dispatches. An unsupported adapter visibly refuses enforcement rather than pretending a priority label guarantees idle delivery. |
| A14 | Defer x, revoke recipient/source access or delete x, then request bounded retrieval/replay as that recipient. | The returned result is unavailable/denied; sink output, response body and retrieved derived-evidence views contain no x excerpt, label or score. An authorized still-existing control source remains retrievable under current gates. |
| A15 | Restart at three controlled points: judgment outstanding, durable admission before dispatch, and candidate update before atomic commit. Also restart after a confirmed consumer receipt and after deliberately losing a receipt. | Old-incarnation results cannot admit. Valid accepted pending work is reconciled; confirmed-received work is not newly dispatched; a partial candidate never becomes active. Corrupt state exposes safe fallback. A lost receipt is explicitly uncertain, not fabricated delivery evidence or an exactly-once claim the backend cannot support. |
| A16 | Activate policy digest P bound to model/rubric/features M/R/F, then change each identity separately and release an old request; attempt rollback to both incompatible and compatible artifacts. | Mismatched responses do not affect admission under P; affected traffic follows ordinary delivery and status names revalidation/shadow. Incompatible rollback is rejected; explicitly selected compatible rollback works without silently promoting the new identity. |
| A17 | Record recipient label wanted-later and a second annotator label not-needed for x; also leave y unreviewed and z with an explicit unsure label. | Authorized feedback output retains both distinct x labels with their authors and source versions. y has no negative target and z remains unsure. The recipient training/evaluation view uses the recipient label, not the disagreeing annotation or silence. |
| A18 | Use a declared synthetic population with low/high score strata, controlled sampling, and different inclusion probabilities; give known wanted labels to the full population. | Review output includes would-be-hidden low-score sources and records each selected source's actual inclusion probability. Reported weighted estimates match a separate hand calculation from the captured sample; coverage/missing labels and uncertainty are shown rather than declaring the sample representative by fiat. |
| A19 | Give two examples from conversation a timestamps on opposite sides of a split date, plus later conversation b reserved for evaluation. Tune twice before opening evaluation; then attempt further tuning on b. | Partition output assigns all of a to one group partition and no conversation appears in both fitting and evaluation. Training/tuning provenance excludes b; reuse of opened evaluation labels for tuning invalidates that evaluation rather than preserving a passing promotion receipt. The report includes normal-delivery and fixed-policy baselines. |
| A20 | Replay thresholds and fit an eligible local candidate twice from the same versioned judgment/recipient-label fixture and declared random seed/environment. Include a candidate that fails a predeclared comparison. | Provider trace remains empty. Replays reproduce the same decisions and fitting reproduces the artifact/predictions within its declared deterministic contract. The failed candidate is not promoted; both baseline comparisons and the actual metric inputs/results are retained. |
| A21 | Configure fixture-only promotion limits before opening a held-out set: wanted recall at least 0.9 and volume reduction at least 0.2, plus declared sample sufficiency. Try insufficient data, failed metrics, another actor's promotion, and explicit recipient promotion of a compatible passing digest. | The first three cases remain unpromoted/log and expose the specific failed prerequisite; only the recipient's explicit action activates the passing digest. Data from an unrelated recipient is absent from the fitting input. These fixture limits do not set live product targets. |
| A22 | Fit candidate P using source x; edit, delete, revoke access or expire x before promotion, during enforcement and while disconnected. Reconnect with source fetch succeeding and separately failing. | Pending promotion using invalid membership is rejected. Once invalidation is known, P is withdrawn and affected source-derived views disappear; only a rebuilt/unaffected compatible artifact can enforce. Failed revalidation leaves the source unknown and P unavailable, not presumed valid. A fake-clock test checks the configured finite revalidation deadline. |
| A23 | Using the documented production CLI/MCP/config surfaces in a clean disposable installation, change modes/notices/brief, review one labeled source, retrieve one authorized deferred source, promote a passing digest and query status; run the full base check commands. | Captured command results and subsequent sink/provider traces demonstrate every operation's effect, including rejected unauthorized controls. Commands exit as documented; production build runs without OMP or a test endpoint override. Search captured diagnostics for planted secret sentinels and require none. Preserve every command's exit code/output, not a screenshot of a claimed pass. |

Tests must assert externally observable outcomes, not source strings, forwarding mock echoes, or incidental wording. Existing tests whose contract changes must be updated; unrelated behavior stays unchanged. Feature tests must fail against a plausible broken implementation. For new behavior absent on the base, show baseline inability/failing new acceptance rather than claiming a compile failure alone proves semantic correctness. Include deletion, late-response, and mode-transition races in a deterministic state-machine test where appropriate.

## Evidence and review requirements

- Bind full in-scope source/docs/test artifacts, base revision, executed commands, outputs, model/family provenance, and acceptance mapping to SHA-256 evidence. Close verifies both artifact presence and matching digests.
- Run the fresh-base `justfile` check sequence recorded in the source evidence, plus the highest applicable transport-boundary scenario. Demonstrate baseline/post-change delta for each changed consumer contract.
- Prove off/ineligible zero egress at the provider boundary; prove direct/log equivalence at the delivery sink; prove on-mode behavior and errors through the same boundary. A pure score function test is insufficient.
- Ordinary deterministic tests use a local fake provider and disposable transport sink. Live shadow activation, if later authorized, measures service behavior separately and never substitutes for race/lifecycle tests.
- Different-family contract validation precedes implementation approval. Different-family verification reruns evidence; review checks standards and test quality. OpenAI-family role aliases alone do not satisfy independence. No self-certified pass.
- **Authorized exception for this run:** syn approved fresh-context OpenAI review with reduced family independence (Discord `1515538364571586628/1551451382572384287`, accepting proposal `1551451319414427669`). Record actual model/family and separate reviewer context at each applicable gate; never describe same-family results as different-family validation. This one-run exception does not remove factual verification, review, contract approval, or close approval and does not amend the default pipeline for future runs.
- Use the existing pipeline gate/evidence records. Contract approval and close approval are syn's actual gates; do not insert intermediate approval prompts for ordinary implementation choices. No automatic PR/push/deployment.

## Remaining activation parameters, not hidden requirements

This draft settles the product contract but does not pretend baseline measurements or private-room grants exist. Before a live shadow trial, name actual provider-eligible rooms and the source/brief audience, confirm provider credential/model identity and data terms, and record retention and service timeout/retry settings. No separate spending allowance is required.

Before promotion, the recipient fixes the wanted-item miss/delay tolerance, context-reduction target, evidence sufficiency, and review plan using measured baseline data before final evaluation. Exact values cannot honestly be validated now. These are activation/evaluation parameters, not permission to omit learning, lifecycle handling, or tests from implementation.

For contract approval, the proposed test boundary is **normalized, access-admitted source traffic through attention admission to the existing delivery sinks**, with provider and clock seams. Retain separate real adapter integration cases where backends differ in interruption/retry semantics. Source inspection may require a narrow new shared admission facade; it does not justify rebuilding all Dione transports.

## Decision trace

| Interview decisions | Contract coverage |
|---|---|
| Q1 reusable/Callisto first; Q2 selective delivery; Q3 recall priority | Objective; sections 3 and 5 |
| Q4 provider eligibility; Q5 mode hierarchy; Q6 direct lane | Sections 1 and 2; A01–A04 |
| Q7 transitions; Q8 loud fallback; Q9 recipient feedback | Sections 4 and 5; A08–A12, A17 |
| Q10 configurable notices; Q11 segments; Q12 non-preemptive timing | Sections 2–4; A06, A11, A13 |
| Q13 voluntary review; Q14 lifecycle; Q15 no pooling | Sections 5 and 6; A14, A17–A22 |
| Q16 holdout promotion; Q17 no allowance/failure path | Sections 4 and 5; A10, A19–A21 |
| Q18 authored brief; Q19 unknown fallback; Q20 revalidation | Sections 2 and 5; A07, A16 |
| Standard Dione/new branch; no implementation/publication in preparation | Header; boundary; evidence requirements |

The private attributed interview record remains `/opt/data/dione-seat/discussion/dione-attention/design-session.md`. This contract summarizes requirements without reproducing private room conversations or credentials. Neither this local file nor its presence on a feature branch is a publication receipt.
