# Rook durable recipient owner

The `dione::rook` library API owns retained events for one configured recipient. It admits an event and its matching subscription links in one private snapshot, grants one active handling claim across those links, and preserves the result across restarts. Production webhook registration and process startup are not enabled by this change.

## Trusted embedding boundary

An embedding application supplies `Clock`, `Authorizer` and `WakeTransport`, opens `RookService`, and holds its `RookHandle`. It authenticates the caller before constructing `AuthenticatedRecipient` or `AuthorizedOperator`. Those constructors attest to authentication performed by the application. They do not authenticate a name, token or request themselves. Do not expose them as tools whose arguments come from message content.

The application passes verified source observations to `admit`. `Authorizer::check` must independently enforce current provider and recipient access, including sender provenance and loop exclusions. A retained preview, a deserialized sender field, or possession of a claim is not proof of current access. An application that supplies an always-allow authorizer is explicitly choosing that policy. It must not use that implementation for an untrusted public integration.

`configure` and `replay` require operator authority. `claim`, `settle` and `status` require the exact configured recipient. `admit` and the optional deterministic `tick` entrypoint belong to trusted resident code. No message content changes subscriptions or permissions.

`RetainedObservation` preserves the common context contract. Its versioned `NativeSupplement` retains bounded adapter bytes without interpreting those bytes as authority. The Discord conversion captures full modeled attachment and reference metadata, raw and represented sender coordinates, and thread parent metadata. It excludes recursive parent message bodies. The owner compares complete retained evidence when an occurrence identity repeats; conflicting evidence does not overwrite history.

Common raw sender and known workspace identities must belong to the event's exact provider and module slot. A represented principal or explicit message reference may deliberately use another provider. A resolved sender must belong to its resolved message's source. Public codec constructors and deserialization report typed `ObservationError` categories when these invariants fail.

## Handling and recovery

A source occurrence is a source-qualified message plus `Created` or its authoritative edit timestamp. Each recipient has one handling record for that occurrence, even when several subscriptions match it. Claim request identifiers bind to the same event after a successful grant or a busy response. Retrying a granted request recovers the same token while that claim remains valid. Expired or invalidated grants cannot allocate a new claim under the old request identifier.

Callback success records acceptance of that wake only. The first accepted wake starts the claim deadline; later overlapping receipts do not extend it. An active claim suppresses additional automatic wakes. Explicit `Handled` or `NoReply` settlement cancels pending wakes without disabling subscriptions. A run that disappears, stays silent, or fails a tool call remains outstanding until its claim or accepted-wake deadline expires.

Defaults are a five-minute accepted-wake deadline, a ten-minute claim, two automatic recovery rounds, and six HTTP attempts per binding per recovery cycle. All limits are validated at service open and snapshot load. Recovery chooses a verified, enabled binding by priority and stable subscription identity. A single derived binding-progress classification governs dispatch, exhaustion and status: usable bindings can send when due, in-flight or accepted attempts await their outcome, refreshable bindings retain budget but need a current lease, and exhausted bindings cannot send. When no binding can progress but an alternate is refreshable, due work remains retained and reports `RefreshBinding`, even if an exhausted binding still has a verified lease. Refresh does not refund attempts. Access denial, unavailable verification, exhausted callback attempts and exhausted recovery remain distinct status results.

The resident owner checks timers every 100 milliseconds. Each pass performs at most one asynchronous access check and rotates through retained records. Checks and callbacks have ten-second timeouts; callback concurrency is capped at four. Deadlines are absolute eligibility times, not promises that every callback will execute at that exact instant. Persisted high-water time prevents a backward clock from extending a grant or allocating another attempt budget.

Policy changes advance a typed authorization revision and invalidate prior claims. Binding renewal alone changes no handling identity or authorization revision. Removing a matching subscription leaves the shared event and claim history. Replay creates a new bounded cycle with the previous counters retained. A settled event is inspection-only during replay.

An interrupted callback spends its already-reserved attempt. If that consumed the final attempt, recovery marks the binding exhausted without a refund or another send. A selected recovery binding stops excluding alternates when it exhausts its budget or its lease expires. The next currently verified alternate uses its own retained budget. An active callback or accepted wake is not exhausted merely because its reservation used the final attempt.

History and snapshot-byte limits also apply to mandatory recovery. If a retained event cannot advance within those limits, the owner leaves its durable obligation unchanged and reports `capacity_blocked` with `IncreaseCapacity` for that event. Automatic processing skips the blocked event until an authorized `set_limits` operation allows another attempt; other events and status remain available. Reopen derives the block again from the retained obligation without terminating the service. If startup normalization itself cannot fit, delivery waits for that operator action while inspection remains available. Reaching the maximum supported limits requires reviewed administrative retention rather than silent record eviction.

## Durable writes and restart

Supported Unix hosts require a dedicated recipient directory owned by the current user with mode 0700. The lock, temporary file and snapshot are regular files owned by that user with mode 0600 and one hard link. Directory walking rejects symlinks and unsafe writable ancestors. Child operations use an open directory descriptor, so replacing a pathname does not redirect a subsequent write. Other platforms return an explicit unsupported-storage result.

The owner holds the process lock until its disk worker exits. Writes run on a blocking worker outside the async runtime. Each write serializes a bounded snapshot, synchronizes the temporary file, renames it within the pinned directory, then synchronizes that directory. A failure before rename preserves the previous state. A failure after rename retains the visible candidate and reports unconfirmed crash durability. No usable claim grant or callback dispatch is released before synchronization succeeds.

Dropping a request future does not cancel an accepted owner command or its disk write. Retrying the request recovers retained identity. Shutdown closes intake, drains accepted commands, joins disk work, and cancels remaining callback tasks. Their persisted dispatch records remain uncertain for reopen. Worker failure reports an unavailable owner independently of enabled subscription configuration.

Reopening converts interrupted callback dispatch into a visible restart failure, retains its spent attempt, and permits a bounded repeated wake. Shared claims protect handling identity. This is callback retry behavior only. The later outbound-publication atom must retain an uncertain message POST as `Unknown` and must not resend that POST merely because it lacks a receipt.

## Remaining integration

Recipient-authenticated exact/history context is implemented by the separate context service and principal-bound tool dispatcher; see [recipient context](rook-recipient-context.md). The durable publication slot, attachment publication, webhook challenge/signing compatibility and supervised production startup remain separate atoms. The publication slot will extend this same recipient snapshot so claim validation and publication reservation share one commit point. This owner does not establish a live subscription or send a production callback by itself.
