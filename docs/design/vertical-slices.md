# Vertical platform and harness slices

This plan moves Dione toward independent chat-platform and agent-harness adapters. The first tranche changes ownership while preserving the Teams wire format, queue format, and delivery behavior for valid state.

## Fixed remaining scope after PR #505

The user fixed the remaining implementation scope to R1-R7. Splitting an item
across PRs does not add scope. Explain any new required work and obtain the
user's agreement before adding it. Optional improvements belong in the
post-project backlog; a repository-wide standards audit follows this project.

Checkboxes track implementation completion. PR verification, CI, official review,
and construct validation remain separate requirements.

- [x] R1: Rendering — separate image production, retained files, Discord delivery,
  and MCP projection while preserving captions, hooks, authorization and output.
- [x] R2: Access requests — typed listing, approval and denial with existing
  configuration persistence and uncertain-durability behavior.
- [x] R3: Journal and diagnostics boundary audit — move only remaining business
  logic; close already-correct adapters by inspection.
- [x] R4: Harness-neutral routing — replace process-wide harness choice and
  remove Codex queue knowledge from shared wiring.
- [x] R5: Existing harness adapters — Claude Code and Codex delivery, session
  binding, startup, shutdown and reconnect behavior behind the shared boundary.
- [x] R6: Composition cleanup — startup assembly, MCP protocol registration and
  dispatch, and removal of obsolete compatibility aliases and migration paths.
- [x] R7: Oh My Pi support — notify the user first, inspect its source/docs
  outside Dione, then implement the adapter.

After R1-R7: construct validation with Oh My Pi, required fixes and stack
integration with CI/official review, then final merge-back review before main.
Additional chat platforms and unrelated cleanup are outside this finish line.

## First tranche

1. Move provider-qualified event identity out of the Codex queue module. Keep its validation and serialized form identical.
2. Give Teams admission one delivery operation with distinct committed, duplicate, and visibly committed but durability-uncertain receipts. A delivery error means no event was inserted.
3. Put Teams authentication, connector access, reply authority, and resident ingress under one Teams module. Keep the Codex delivery adapter outside that module.
4. Keep the exact startup order: restore reply authority, bind and start the Codex worker, then start the Teams listener.

The first tranche is done when all existing Teams and queue tests pass, the queue and notification fixtures retain their current shape, and `TeamsResidentBridge` no longer stores or calls `CodexEventQueue`. HTTP acceptance remains 202 only after a committed or duplicate event. A visible commit with uncertain durability remains a 503 response while retaining its reply authority.

PR review found that persisted provider IDs bypassed the live constructor's validation. The core slice now validates each ID on deserialization with the same shared validator. Malformed stored IDs fail inbox loading without rewriting the file. Valid historical state and unknown fields retain their existing behavior.

## Teams reply tool slice

`teams/tools.rs` owns the `teams_reply` definition, argument parsing, reply authority call, and receipt fields. MCP registers the definition and routes calls through the Teams facade. The resident allowlist and common MCP response wrapper remain in MCP.

The design comparison selected two module functions over a stateless tool descriptor. Both designs preserved behavior, but the descriptor added a type with no state. The request remains two borrowed strings, and authority availability is checked before either argument. Missing fields, wrong types, runtime errors, empty strings, and extra fields retain their existing behavior.

This slice is on `refactor/vertical-slices-teams-egress`, based on `refactor/vertical-slices-core`. One writer makes the code changes. Design exploration runs in parallel, followed by boundary tests and review before the final source freeze and release build.

## Discord ownership slice

The next unit moves `messaging.rs`, `bot_state.rs`, and `ids.rs` into `src/discord/` from MCP. Discord owns the complete messaging operations, presence commands, and external ID validation. This removes the production dependency from Discord gateway code to MCP. MCP still owns dispatch, argument parsing, schema registration, and response envelopes. Presence schema enum helpers remain with the Discord types.

The data shapes stay `MessagingCtx`, `BotStateCtx`, `DiscordCommand`, `Snowflake`, and Serenity's distinct ID types. Internal callers use the canonical Discord paths. Public aliases preserve `mcp::tools::messaging`, `mcp::tools::bot_state`, and `mcp::ids` as the same definitions. The `contradictionary_flow` integration test retains the published messaging path as compatibility coverage. The Discord event test retains its `IntoNotification` import to check the wire format across the boundary.

The design comparison chose the three-module move over messaging alone because presence commands and ID validation were the other production gateway dependencies on MCP. A wider move of every Discord tool would also relocate search, rendering, and management without removing another gateway dependency. Moving all tools would incorrectly assign process diagnostics and consent-journal operations to Discord. A typed service rewrite would change result contracts and lifecycle handling at the same time as ownership, so it is a later unit.

This move preserves existing type debt. `discord/messaging.rs` and `discord/bot_state.rs` still return JSON `Value` results and string errors. Their context fields remain public. Presence wiring and activity type/name remain independently optional, and existing presence booleans remain unchanged. The slice does not establish full compliance with the stricter type policy or a fully harness-independent Discord API. A later typed result migration must represent Held, Delivered, and partial delivery explicitly while preserving coordination completion and evidence semantics.

One writer makes the relocation and caller changes. Review can proceed independently after source freeze. Focused verification precedes the complete PR gate against the final commit. The expected costly step is the release build, so source changes and review findings must settle before that build starts.

Verification must compare the two moved snapshot payloads byte for byte and inspect the diff with rename detection. The isolated global-mute regression captures child output and requires the summary `test result: ok. 1 passed; 0 failed;`. Exit success alone can conceal a selector that runs zero tests. A deliberate stale-selector run must fail before the restored selector passes. Focused checks cover messaging snapshots, presence, IDs, protocol schemas, dispatch parsing, and the integration test through the compatibility alias. Full exact-commit checks and independent six-lens review remain required before opening the PR.

## Typed Discord messaging slice

Discord returns operation-specific `Result` contracts for replies, DMs, release, and rephrase. `ReplyOutcome` distinguishes complete delivery, consent hold, and partial delivery. `DmOutcome` records the direct channel only for complete direct delivery. `Report` carries delivery-stage rewrites. MCP's `messaging_wire` module projects these values into the existing tool response fields. Attention accepts only complete delivery.

The design comparison selected operation-specific results over a universal command or result enum. Shared preparation failures migrate in the same change, including their callers in rendering and management. Discord reads retain their existing successful JSON shapes and I/O ownership, but their top-level failures use `MessagingError`. Moving those reads back to MCP would reverse the preceding ownership change.

Existing operation entry points remain useful conveniences over typed IDs and unrestricted text. They now take `ReplyPing`, `LinkPreviews`, and `HoldHandle` where those policies or identities apply. Separate request wrappers would repeat those arguments without enforcing another invariant. Rephrase checks whitespace and raw locators before rewriting or reserving a handle. A resolved channel value is required at the delivery boundary.

Consent owns one delivery contract. `DeliverError::Partial` requires `PostedMessages`, a nonempty sequence of typed IDs, and a private retry remainder. `DeliverError::Total` cannot carry posted IDs. Public construction preserves these distinctions, and no receipt or failure derives serialization. `RejectedHandle::SendFailed` exposes this attempt's progress while successful retries return accumulated IDs. The consent gate alone updates retry text, diary records, reservations, expiry, and fence continuation.

Settlement preserves the distinction between total and partial rephrase failures. After a total failure, the queue retains the replacement as written and the earlier entry's pending records. After a partial failure, it retains the sent-text remainder and its records. Initial partial delivery completes the coordination claim using the first published ID and writes only Auto diary records. Total initial failure releases the claim. Consent retries do not acquire coordination claims.

MCP explicitly writes message ID arrays as numbers and individual message or channel fields as strings. Held and partial outcomes still project an error. Initial partial IDs stay outside the wire response. Retry failures reveal the live handle and expiry without retry text, diary records, or fence state. Rewrites appear at the same preparation or delivery stage as before.

Version 0.52.0 acknowledges the public Rust API break. Callers of the public messaging aliases must handle typed results, migrate suppression flags to policy enums, and pass `HoldHandle` to consent operations. Implementations of `DeliverReply` return `MessageId` values and typed total or partial errors. Queue progress uses `MessageId` too. The MCP wire contract remains unchanged.

Unchanged type debt remains in legacy query success JSON, presence operations, public context fields, optional presence wiring, and existing edit preview flags. The coordinator already erases transport causes into `ClaimOutcome::Unavailable.reason`; the named messaging variant preserves that reason without claiming recovery of its source. The download helper's logged, best-effort failures and nested Vaelii receipt payload retain their existing contracts. This slice does not establish repository-wide type compliance.

Focused verification covers the existing consent, fences, expiry, coordination, mute, evidence, and integration regressions. New assertions cover typed delivery outcomes, empty reply input reaching the transport without new validation, literal MCP projections, and partial, total, partial, then successful retry accumulation. A deliberate removal of accumulated retry IDs must make the new retry test fail before the restored implementation passes. The full PR gate follows source freeze and independent review.

## Typed Discord presence and typing slice

Presence requests, desired state, queue submission, and reconnect replay now share `discord/presence.rs`. `PresenceActivity` pairs a kind with its required unrestricted text. `PresenceCapability` distinguishes direct application, queued acceptance, and unavailability. The gateway and MCP receive handles from one assembly function. Production uses the direct capability without creating an unused queue; queue-only callers retain the existing receiver lifecycle and Accepted response.

The design comparison selected a dedicated presence owner over keeping authority in gateway events or adding a service that forwards every operation. Request and activity fields are private with constructors and read-only accessors. MCP interprets permissive optional arguments and projects snapshots and typed results. Events alone converts domain requests to Serenity activities and statuses. `typing.rs` owns live channel authorization and typed transport failures through a private transport seam.

Mutations retain the effects-then-state lock order. Sink callbacks run outside the state lock, and replacement sinks replay the original request without changing its decision timestamp. A direct acknowledgement means requested state is stored, not that Discord received it. A queued acknowledgement establishes only acceptance. Sink installation remains distinct from a live connection probe.

Version 0.53.0 removes `discord::bot_state`, its MCP compatibility alias, the single-variant `DiscordCommand`, and the paired optional wiring builders. Rust callers migrate to `PresenceRequest`, `PresenceCapability`, and `TypingCtx`; snapshots expose read-only domain values. SDK-only offline and unknown stored states are no longer constructible. MCP's admitted status and activity tokens, coercion defaults, validation ordering, error placement, fields, and notes remain unchanged.

Behavior tests pin literal MCP results before and after the move, SDK conversion including custom text, immediate requested-state reads, queue ordering and closure, replay timestamps, and later queued delivery to the replacement sink. Typing tests change live DM and thread authorization between calls and preserve the original failure cause. The parent verification pass runs a replay mutation and the full exact-commit gate after review.

At the end of the presence slice, remaining type debt included legacy query success JSON, existing edit preview flags, and public contexts outside that slice. The query and search slices below replace those query success contracts. The optional queue consumer retains its existing detached task lifecycle; a future queue-only integration needs an explicit shutdown owner. Production uses no queue consumer. Harness separation and broader composition cleanup remain later work.

## Typed Discord queries slice

`discord::messaging::queries` owns the five message read and download operations, their models, and their tests. The owner keeps live authorization before cursor validation and network access. `MessageView` carries typed identities, the existing `Timestamp`, attachment byte units, independent reply references and authors, and parsed evidence offerings. It exposes no SDK message or success JSON.

`FetchedMessages`, `NewMessages`, and `PinnedMessages` have private fields and expose messages in ascending ID order. Counts derive from their message slices. Optional cursor metadata distinguishes recent reads from paginated reads, and continuation hints retain the direct Rust API's zero-limit behavior. `RetrievedMessage` separates successful retrieval from an optional typed receipt success or failure. Downloads return `Vec<Utf8PathBuf>` containing only successful saves.

MCP's `query_wire` module owns response envelopes, string identities, omitted collection media types, explicit single-message null media types, and receipt error text. Search uses the same observation and projection while retaining authorization before SDK decoding, malformed-record skipping, transport order, and its raw-count continuation expression. The old success JSON builders and `messaging_wire::legacy` are removed with their callers.

The design selected complete query ownership over keeping I/O in the large messaging module. It adds no forwarding service or second context. Existing local HTTP fixtures remain in use; this slice adds no mock transport. Five literal characterization tests passed against the old implementation before migration. The search envelope's raw-count expression remains unchanged; the pure filter test exercises filtering and order, not the HTTP envelope.

Version 0.54.0 acknowledges the public Rust import and result-type changes. Attachment URLs still preserve Discord's supplied text without new validation. The best-effort download helper retains boxed errors and string-generated size failures. Public context fields and edit preview flags remain outside this slice. These retained constraints do not establish repository-wide type compliance.

## Typed Discord search slice

`discord::messaging::search` owns search preparation, HTTP execution, current authorization, tolerant Discord response decoding, and typed observations. A consuming builder produces a private `SearchRequest` only after checking for a filter and converting the before bound followed by the after bound. The completed request retains the guild, canonical query pairs, limit, and offset. Direct message bounds use `MessageId`; calendar bounds preserve the existing epoch-zero and wrapping date behavior.

`SearchHit` pairs a typed channel identity with the shared `MessageView`. `SearchPage` keeps ordered hits, the requested offset, and a continuation hint computed from the raw outer group count. Visible count derives from the hits. A missing or non-array `messages` field produces an empty page. The owner still checks the first group member's string channel ID and current DM/thread permissions before SDK message decoding. Denied and malformed records are omitted, while allowed duplicates and transport order survive.

MCP's `search_wire` module owns a private argument DTO, scalar/date and ID-array visitors, and JSON projection. Domain leaf types retain their validating deserialization. Guild parsing still precedes argument parsing; syntax failures remain dispatch errors, while preparation and HTTP failures remain tool error payloads. Execution errors retain request, read, and parse causes, HTTP status and body, or an optional raw UTF-8 retry header. Only the MCP adapter supplies the missing-header text `unknown` and legacy protocol error strings.

The design selected a prepared request and one nested owner rather than a shared public serde options struct or a forwarding service. The crate-private entry point builds the production URL, fresh client, and authorization header. A private execution function accepts the concrete request builder, allowing real loopback HTTP characterization without exposing an injectable endpoint. This extraction was a production change before characterization, not evidence from an untouched implementation. Ten new characterization tests passed before the typed migration, including raw pagination and response-coordinated permission updates. The migrated tests retain their literal protocol expectations.

Version 0.55.0 removes the formerly public `mcp::tools::search` API. Its replacement is crate-private, and external Rust consumer compatibility is not claimed. The broad messaging context, fresh HTTP client per call, buffered bodies, existing timeout and cancellation behavior, raw error bodies, and lack of retries remain unchanged. Other Discord tools and harness composition remain later work. This slice does not establish repository-wide type compliance.

## Typed Discord management slice

Discord owns complete pin, unpin, delete, and thread creation operations in
`discord::management`. `MessageCommand` pairs typed channel and message IDs with
an exhaustive `MessageAction`. The shared mutation entry point returns unit
success; thread creation returns `CreatedThread` with the returned ID and name.
The narrow context retains only HTTP, state, configuration, and ingress ledger.

Both entry points check current outbound permission before target verification.
The owner reads authenticated own-send evidence and fixes management's target
policy internally. Standalone thread creation still checks outbound permission.
Successful thread creation registers the returned ID against the requested
parent before returning. MCP owns JSON projection and preserves the existing
argument order and permissive optional-message grammar.

Version 0.56.0 removes the formerly public `mcp::tools::management` module and
its context and four operation functions. The replacement is crate-private;
external Rust compatibility is not claimed. Configuration snapshots, separate
state reads, ledger expiry and unavailable-ledger policy, canary alerts, SDK
HTTP behavior, and cancellation semantics remain unchanged. No retry or
after-HTTP authorization check was added. The construct test with Oh My Pi
remains required before considering a merge to `main`.

## Typed Discord introspection slice

Seven complete metadata reads now belong to `discord::introspection`. Each
operation returns its own observation type with typed Discord IDs. Channel
observations distinguish guild, private, and unknown categories; list-channel
subtypes retain the SDK's authoritative `ChannelType`. Role hierarchy positions
use a private-field `RolePosition`, preserving SDK normalization without adding
range restrictions. Failure context pairs each operation with its required IDs
and retains the SDK cause.

The owner resolves private-channel names and localizes member join times with
the configuration snapshot selected at call entry. MCP projects IDs,
discriminators, emoji mentions, channel vocabularies, and original SDK error
text. The existing null fields, response order, and absence of local
introspection authorization checks remain unchanged. Discord's own HTTP
permissions still apply.

The design selected seven concrete free functions and a narrow immutable
context. Channel variants prevent an arbitrary name in the unknown category;
no request dispatcher, forwarding service, or duplicate SDK subtype enum is
needed. Literal HTTP fixtures were characterized before moving the operations.
Version 0.57.0 removes the public `mcp::tools::introspection` API; external Rust
consumer compatibility is not claimed. Harness adapters and construct testing
remain later work.

## Later slices

### Journal and diagnostics audit

R3 closes by inspection. Journal filtering, aggregation, chain retention,
condensation, and vacuum rules already belong to `no_rly::journal`.
`JournalHandle` serializes maintenance with appends through its writer.
`TraceLevelController` owns trace-filter validation and subscriber reloads.

The MCP adapters parse arguments, apply configured defaults to omitted retention
arguments, convert relative days to absolute cutoffs, and project results.
Statistics add a separately observed pending count. Version reporting projects
compile-time metadata. These operations need no additional owner or forwarding
layer. See the [R3 audit](../../.audit/journal-diagnostics-verification.md) for
the caller census, preserved behavior, and verification limits.

### Remaining ownership work

- Convert the remaining Discord tool contracts into typed operations and protocol adapters without moving identity or permission rules into the shared core.
- Split Claude Code and Codex delivery into harness adapters. Remove the process-wide harness choice after both adapters can use the shared route.
- Reduce `main.rs`, MCP dispatch, and MCP schema registration to assembly and protocol translation.

### Harness-neutral routing

`HarnessConnection` owns two internal facets supplied by one adapter instance.
Its private fields prevent mismatched delivery and protocol capabilities.
`DioneServer` receives the complete connection during construction and derives
attention enforcement from its policy. Callers cannot replace the connection
after attention initialization.

Claude delivery and MCP responses share one output lock. Codex delivery owns
the durable queue and mandatory binding publisher. Its seven MCP extensions
own their schemas, parsing, and projection; shared dispatch applies the existing
result envelope once. Attention delivery guards and receipts now belong to the
attention module. Shared routing no longer selects a harness or operates a
Codex queue directly.

Version 0.60.0 removes `TransportMode` and the server's optional Codex builders
without aliases. The CLI retains its existing mode names and default. Concrete
worker lifecycle remains R5, and remaining startup/protocol assembly remains R6.

## Construct testing before main

After the structural refactor reaches testing, add Oh My Pi as a supported
harness and test Dione on a construct using it. This is a required validation
step before considering a merge to `main`. Resolve issues found during that
test and review the results before merging. The implementation estimate for
the structural refactor excludes Oh My Pi support and testing fixes.

Before beginning Oh My Pi support, notify the user and review Oh My Pi's source
and documentation in a separate location. Do not add those sources or documents
to the Dione repository. Use that investigation to decide the integration shape.

Slack and other additional platforms remain outside this refactor. Build their
adapters when a concrete use case supplies the requirements.

## Rendering ownership

LaTeX compilation and retained PNG creation belong to `src/rendering.rs`. Only successful encoding constructs a PNG value, and only successful tempfile persistence constructs a retained-file receipt. Their private fields prevent callers from fabricating either artifact. Native paths and concrete compiler, encoder, I/O, and persistence causes remain typed until MCP projects them.

`src/discord/rendering.rs` owns the complete authorized render-and-send operation. Captionless overrides and outbound permission are checked before compilation. The existing attachment helper handles caption hooks, redirects, transport, and successful sent-ID registration. Channel errors retain the requested channel and the full messaging error.

`src/mcp/render_wire.rs` preserves the existing JSON fields, error capitalization, and structured hook feedback. Compiler and literal PNG fixture tests now live with the neutral owner; wire and loopback transport tests live with the projector. Version 0.58.0 removes the former public `mcp::tools::render` API without compatibility aliases. Compilation and file I/O remain synchronous, an inherited exception to the async I/O standard retained for this behavior-preserving change.

## Access-request ownership

`discord::access` owns complete listing, approval, and denial operations on a
private context. Stored IDs have a separate observation type that preserves all
historical values, including zero. Commands use validated `UserId` values.
Private approval receipts reuse `ConfigDurability`; MCP derives warning presence
and the request-retention flag from that outcome.

Approval still snapshots the pending request before config mutation and releases
the queue lock during the await. A rename can apply while a failed directory fsync leaves crash persistence uncertain. Unknown config durability keeps the retry
request. Known config durability precedes attempted queue removal. Queue writes
remain synchronous and best effort, and the operation does not add transactional
or cancellation guarantees. The upstream config error remains boxed inside a
typed access error until MCP projects its original message.

`mcp::access_wire` owns JSON projection. Dispatch preserves the existing blanket
mutation-disable switch before argument parsing and leaves listing ungated.
Version 0.59.0 removes the public `mcp::tools::access` API without compatibility
aliases. The literal historical queue fixture and behavior tests cover ordering,
localization, retries, config failure, queue write failure, and MCP error placement.

## Existing harness lifecycle

`HarnessSession` prepares the concrete connection and consumes itself into
`RunningHarnessSession` only after the shared attention authority exists.
The Codex session owns the queue, matching binding publisher and receiver,
configuration resolution, and existing reconnect worker. Claude sessions need
no background delivery worker. Both use the same shared routing boundary.

The running owner keeps the inbox lock until delivery shutdown completes.
Process cancellation does not cancel delivery while accepted ingress and
replies drain. Version 0.61.0 adds these session owners; queue formats, tool
contracts, and the reconnect engine are unchanged. Startup assembly and the
offline attention CLI policy selection remain R6 work.

## Application composition

`startup::run` owns process assembly and offline operator commands. `main` installs
error reporting and calls that entry. `HarnessSelection` supplies adapter-owned
attention policy without creating delivery resources. One private prepared Teams
value supplies the listener, reply authority, and drain from the same bridge.

Version 0.62.0 removes obsolete Teams and MCP import aliases. Local callers use
the canonical module owners. Startup and shutdown ordering remains visible in one
function. Moved panic sites become typed errors, and malformed secret values are
excluded from returned diagnostics. The public startup entry initializes globals
and must be called only once. The R6 checkbox records implemented scope; the PR
records the final committed-head gate and review result.

## Oh My Pi support

The user was notified before the upstream source and documentation inspection.
Those references and the installed OMP package remain outside this repository.
Version 0.63.0 adds an explicit adapter and an original extension tested against
OMP 18.10.0. Ordinary MCP tools retain their existing authority. The extension
accepts only the configured dione server and the versioned custom event method,
validates the payload, and submits full metadata to the main agent.

In version 0.63.0, the adapter owned no background worker or durable inbox, and
Teams ingress was Codex-only. Generic Discord notifications still have no
acknowledgment or replay guarantee. The subsequent optional Teams integration
adds a durable inbox and exact saved-session receipt acknowledgment through
private Unix controls. See the
[operator guide](../../extensions/oh-my-pi/README.md) and
[verification record](../../.audit/oh-my-pi-verification.md).
The R7 checkbox records implemented scope. The PR records its final gate result;
construct validation, required fixes, and stack review remain before main.

## Dependency maintenance after the refactor

The user requested eventual resolution of both retained package issues. These
follow-ups are outside the fixed R1-R7 implementation list.

- [ ] Remove `unsafe-libyaml` through both the nameplate YAML parser and Typst's
  YAML dependency path, then remove its advisory exception.
- [ ] Replace the yanked `yoke-derive` 0.8.3 through a compatible dependency update.

See the [dependency maintenance record](../../.audit/oh-my-pi-verification.md#dependency-maintenance-after-the-fixed-refactor)
for the known paths and required verification.
