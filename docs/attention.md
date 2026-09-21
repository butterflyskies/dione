# Recipient-local attention

Attention is optional and **off by default**. It scores eligible ambient Discord text with the native TypeSafe HTTP API, records source-bound judgments, and lets a recipient explicitly evaluate and promote a local admission policy. It does not change harness model selection or infer preferences from silence.

Implementation does not authorize room disclosure or activation. Reading a room, selecting `log`/`on`, and granting provider eligibility are separate decisions. No recipient or room is enrolled implicitly.

## Delivery and privacy

| Mode | Provider requests | Delivery |
| --- | --- | --- |
| `off` | None for new work | Existing ordinary path |
| `log` | Eligible ambient text only | Ordinary path; hypothetical decisions are recorded |
| `on` | Eligible ambient text only | A compatible, explicitly promoted policy may select prompt, next-turn, or retrieval-only delivery; otherwise ordinary delivery |

The global `emergency_off` overrides room settings. An exact room grant is required for export; parent-room grants do not implicitly export threads. Authorized DMs, explicit mentions, verified replies to the recipient, and configured direct rooms bypass classification. A sender is not exempt in every group room.

The request contains the recipient-authored, unexpired, provider-eligible brief and a bounded trigger/reply chain. Ancestors are nearest first and stay in the same room. Links are not followed and attachments are not downloaded. Missing evidence is unknown, not a negative preference label. Deferred retrieval re-fetches original source versions under current access; it never substitutes newer text for an old decision.

Only the built-in Codex app-server delivery path supports attention enforcement. A managed message waits while a turn is active: it neither steers nor aborts that turn. Prompt and next-turn deliveries retain source identity and conversation order at an idle opportunity. Other transport modes keep ordinary delivery and expose `safe_delivery_supported: false`. Explicit pull consumers must honor managed delivery metadata and current source guards; ordinary lease acknowledgement is not an attention consumer receipt.

Turning off cannot retract data already sent to TypeSafe. On-to-off/log returns unresolved new traffic once to ordinary delivery, preserves already-admitted work under current source/access rules, and never dumps deferred history into the context.

## Operator surfaces

Use the authenticated local MCP tool **`attention`** while Dione is running. Its arguments are the command JSON below. Discord text is never parsed as a control command. `admin_only_mutations = true` disables mutation tools; changing that operator policy is not a recipient bypass.

For an offline seat, the same command can run without the Discord gateway:

```sh
printf '%s\n' '{"operation":"status"}' |
  DIONE_STATE_DIR=/path/to/offline-seat dione --mode codex --attention-command -
# A JSON file is also accepted:
DIONE_STATE_DIR=/path/to/offline-seat dione --mode codex --attention-command command.json
```

Success prints JSON and exits zero; invalid input, unavailable sources, rejected authority, or store-ownership conflicts exit nonzero. `status` and `configure` need no Discord token. Source-dependent commands use Dione's normal Discord token configuration and validate sources through Discord; replay/fitting are local with respect to **TypeSafe**, not a bypass of current source validation. Standalone commands require exclusive metadata ownership. Do not run them against a live daemon's store; use MCP instead.

`status` reports settings, config generation, adapter capability, health, record count, selected usable policy digest, and any store error. A selected artifact awaiting revalidation is not advertised as an active usable policy.

### Configure from current status

`configure` replaces attention settings; copy the `settings` object from `status`, change the intended fields, and submit:

```json
{"operation":"configure","settings":{"recipient":"default","mode":"off","emergency_off":true,"notices":"off"}}
```

This minimal example intentionally resets omitted fields to their defaults. For normal changes, send the complete current settings object. The recipient identity cannot be changed through its attention tool. Wire values for notices are `off`, `failures`, and `failures_and_recovery`.

Equivalent inert TOML:

```toml
[attention]
recipient = "default"
mode = "off"
emergency_off = true
model = "jev-1.13.0"
api_key_env = "TYPESAFE_API_KEY"
notices = "off"
notice_cooldown_ms = 60000
request_timeout_ms = 3000
outage_retry_ms = 30000
max_segment_bytes = 16384
max_antecedents = 8
max_in_flight = 8
retention_ms = 604800000
revalidate_ms = 60000
max_records = 10000

[attention.rooms."100"]
mode = "log"
provider_eligible = false
direct = false
```

Room `100` is an illustrative ID, not an authorization. A real disclosure grant must precede setting `provider_eligible = true`. A usable `brief` has `text`, future Unix-epoch `expires_at_ms`, and its own explicit `provider_eligible` boolean. Set `notice_channel` to the recipient's existing authorized channel if notices are wanted. Provide the named TypeSafe environment variable through the deployment's secret-management route; never put a key value in attention JSON/TOML, feedback, or diagnostics.

The native endpoint is fixed at `https://api.typesafe.ai/v1/systemone`; production has no endpoint override or OMP runtime dependency. Use an identifiable pinned `jev-x.y.z` model. Unsupported aliases or mismatched returned identities cannot enforce a learned policy and cause ordinary-delivery fallback. Changing model, rubric/features, or brief invalidates compatibility with old calibration; it is not automatic promotion.

## Review, label, and retrieve

Start with an authorized `log` interval and a recipient-authored brief. Review before labeling when collecting a sampled cohort:

```json
{"operation":"review","requested":20,"seed":19}
```

The response contains `items`, selection probabilities, source-bound current excerpts, and requested/selected/returned coverage. Sampling includes low-score and would-be-deferred material, not just uncertain or already-admitted examples. Sources that cannot be revalidated are omitted and counted unavailable. Already decisively labeled records are excluded from a new review batch.

For a returned `selection.record_id`:

```json
{"operation":"feedback","record_id":"RECORD_ID","label":"wanted_promptly"}
{"operation":"feedback","record_id":"RECORD_ID","label":"wanted_later"}
{"operation":"feedback","record_id":"RECORD_ID","label":"not_needed"}
{"operation":"feedback","record_id":"RECORD_ID","label":"unsure"}
{"operation":"retrieve","record_id":"RECORD_ID"}
```

Submit the one label that reflects the recipient's assessment, not all four. Identity comes from the authenticated seat, never an `annotator` argument. Feedback binds source versions and assessment time. Silence and `unsure` are not negative targets. Voluntary labels are allowed, but weighting does not repair missing/nonrandom feedback; inspect coverage and uncertainty.

Retrieval is bounded to the record's current authorized source versions. Deleted, edited, expired, or access-revoked evidence becomes unavailable. A retrieval failure does not authorize fetching a different room or disclosing a cached old excerpt.

## Local fitting and explicit promotion

These commands reuse captured scores and make no new TypeSafe scoring requests. They still revalidate source access and existence.

1. Choose conversation-grouped chronological boundaries:

   ```json
   {"operation":"partitions","spec":{"training_end_ms":1789900000000,"validation_end_ms":1789950000000}}
   ```

   The numbers are illustrative epoch milliseconds. Use real observation boundaries. Whole conversations stay together; later held-out conversations must remain untouched by tuning.

2. Compare a fixed policy locally:

   ```json
   {"operation":"replay","record_ids":["TRAINING_RECORD_ID"],"policy":{"wanted":0.5,"prompt":0.5,"participation":0.5,"change":0.5}}
   ```

3. Fit on sufficient recipient-labeled training records. This is a **synthetic example**, not recommended production evidence minima or tolerances:

   ```json
   {"operation":"fit","record_ids":["TRAINING_RECORD_IDS"],"options":{"minimum_labels":12,"minimum_wanted":8,"minimum_not_needed":4,"minimum_promptly":4,"minimum_later":4,"regularization":0.01,"iterations":800,"learning_rate":0.2,"seed":19,"environment":"rust-f64-v1","wanted_threshold":0.5,"timely_threshold":0.5,"candidate_ttl_ms":3600000}}
   ```

   Replace the example ID array with the actual training cohort. Retain the returned artifact `digest`. Fitting does not select or activate it. Seed, environment, hyperparameters, compatibility, membership, and expiry are recorded for reproducibility.

4. After baseline characterization, predeclare the recipient's acceptable miss/delay, reduction, uncertainty, and evidence requirements **before opening the final evaluation cohort**. Example structure, again synthetic:

   Obtain the held-out IDs from a server-issued review batch before labeling that
   cohort, and use the whole batch unchanged. Training records should already be
   labeled so they are outside the unlabeled review pool. The batch must cover
   every occupied score stratum, and its source versions must remain current.
   Opening the evaluation seals that batch's actual inclusion probabilities in
   private metadata. Hand-picked IDs, shortened batches, and old record-level
   probabilities can still support exploratory evaluation, but cannot authorize
   promotion; missing probabilities are never treated as one.

   ```json
   {"operation":"open_evaluation","plan":{"id":"evaluation-1","record_ids":["HELD_OUT_RECORD_IDS"],"fixed_baseline":{"wanted":0.5,"prompt":0.5,"participation":0.5,"change":0.5},"limits":{"minimum_labeled":12,"minimum_wanted":8,"minimum_promptly":4,"minimum_wanted_recall":1.0,"minimum_timely_recall":1.0,"minimum_volume_reduction":0.2,"maximum_unwanted_delivery_rate":0.0,"maximum_standard_error":1.0},"opened_at_ms":0}}
   ```

   The server records the real opening time; zero is not a backdating mechanism. Opened conversation groups cannot overlap or become later tuning material. Evaluation receipts are immutable.

5. Evaluate and inspect the separate candidate, normal-delivery, and fixed-baseline metrics:

   ```json
   {"operation":"evaluate","evaluation_id":"evaluation-1","digest":"CANDIDATE_DIGEST"}
   ```

   Inspect wanted/timely recall, delay, unwanted delivery, source volume reduction, coverage, effective sample size, uncertainty, latency, and token usage. Dollar cost is not invented when the provider supplies only token counts. A passing synthetic fixture is not evidence of real recipient quality.

6. Only the recipient's explicit decision promotes a passing compatible artifact:

   ```json
   {"operation":"promote","evaluation_id":"evaluation-1","digest":"CANDIDATE_DIGEST"}
   {"operation":"status"}
   ```

   Promotion does not change `log` to `on`; mode remains a separate explicit control. Insufficient evidence must not be treated as a promotion. For a previously promoted, still-compatible, currently revalidated artifact:

   ```json
   {"operation":"rollback","digest":"PRIOR_COMPATIBLE_DIGEST"}
   ```

   Invalid/expired membership, incompatible signatures, missing evidence, or a changed control generation reject the write. Rollback is not permission to resurrect deleted training evidence.

## Faults, retention, and restart

Provider authorization, quota/credit, malformed responses, model mismatch, timeout, or local metadata faults preserve eligible ordinary delivery and expose degraded health. Only HTTP 429/529 receive bounded retries (at most three attempts, within the finite request deadline). Known outages suppress per-message retry storms. No alternate provider, purchase, or automatic top-up is used.

Notices bypass classification. Cooldown/coalescing prevents floods; failed notice delivery remains visible in local status without recursively generating notices. Muting notices preserves diagnostics. A notice send serializes with configuration publication for its bounded send (at most five seconds), preventing a removed route or mute from becoming a stale asynchronous send. Ordinary direct delivery does not wait for that notice. Recovery preserves explicit off/mute choices and never promotes a policy.

Notice delivery uses the shared outbound reply pipeline, including configured pre-send checks, consent holds, mention suppression, and sent-message tracking. A held, rejected, or failed notice is recorded as a local notice delivery failure; it does not trigger another attention notice.

The seat operator must bind a notice audience separately from recipient-controlled
attention settings, in the top-level configuration:

```toml
[attention_notice_route]
recipient = "callisto"
channel = "123456789012345678"
```

The recipient's attention settings may select that channel with notice_channel,
but cannot change this operator-owned binding through the attention tool. Both
recipient identity and channel must match when configuring enabled notices and
again under the final send guard. Missing or mismatched bindings fail closed with
a local notice error, even if Dione can otherwise post to that channel. Explicit
notice muting remains available without a valid route. Inbound attention.rooms
grants are not notice-audience authority. The operator remains responsible for
declaring the correct audience in the independent binding.

Feature metadata lives in `$DIONE_STATE_DIR/attention/records.json`; health lives in `attention-status.json`. The metadata store has a lifetime owner lock and atomic writes. It retains source IDs/hashes, bounded judgments, labels, sampling, evaluation, and artifact membership—not a second raw-message corpus. The existing delivery queue is separate. Default unpromoted retention is seven days; explicit limits bound records, manifests, and review batches. Known source invalidation withdraws dependent labels/artifacts; unknown revalidation withholds enforcement rather than assuming validity.

On Unix, new metadata directories are created with mode 0700; metadata, temporary,
and lock files use 0600. Existing store directories with group/other permissions
are refused rather than silently changing a possibly shared directory. Correct
the intended directory's permissions before reopening it; do not delete the
metadata to bypass the fault. Symlinked store paths and artifacts are rejected.
A parent-directory sync error after an atomic rename is still reported, but the
in-memory state follows the committed replacement so later writes cannot restore
stale data.

Temporary final-delivery guard unavailability retains admitted live-incarnation
work for the existing one-second retry tick, in per-channel FIFO order. Deferred
work remains duplicate-fenced; invalidated sources are withdrawn, not retried.

On restart, selected policy membership must be revalidated. Confirmed dispatched records are not re-injected. Valid previously admitted work without a confirmed receipt is marked receipt-uncertain and reconciled using the same logical attention message identity. Unresolved first decisions recover through ordinary delivery; retrieval-only history is not dumped. Lost consumer receipts are not an exactly-once guarantee. Inspect health/store diagnostics rather than deleting metadata to make a fault disappear.
