# Capability crate splits

Dione's platform and harness refactor established capability ownership inside
one package. Workspace crates make selected dependency boundaries explicit.
Composition stays in Dione; this split does not add dynamic plugin loading.

## Rendering boundary

`dione-rendering` owns LaTeX conversion, Typst compilation, bundled fonts, PNG
encoding, and retained files. Callers pass a LaTeX string to `render_png` or
`render_file` and receive a private-field `PngImage` or `PersistedImage` receipt.
Only successful encoding or persistence constructs these values. Discord keeps
authorization and delivery; MCP keeps JSON projection and exact error text.

The first public API retains the existing error struct and exhaustive kind
enums. Typst, PNG, and tempfile errors remain concrete sources. This accepts
backend types in the API to preserve diagnostic chains and avoid a second
classification model. A persistence error retains its temporary path until
drop, including the existing cleanup behavior. Opaque source wrappers would
complicate cross-crate failure fixtures without removing those dependencies.
No compatibility module remains in the application.

The renderer owns its unit tests and the eight literal PNG fingerprints.
Application tests continue checking protocol text, cleanup, authorization,
caption hooks, and HTTP delivery. This extraction establishes an independently
compiled capability; no build-time improvement has been measured.

## Durable queue boundary

`dione-durable` owns the generic queue engine, store, checked identities,
admission operations, replay suppression, leasing, and resident binding.
Codex and Oh My Pi keep their disk schemas, payload validation, and runtime
adapters. Both implement `QueueCodec` over public detached snapshot records;
decoded state still passes the engine's existing validation before installation.
The live store, engine, and resident-handle construction remain private.

The shared atomic snapshot replacement primitive moves with durable storage.
Rook consumes that primitive directly and retains its own directory admission
policy. No compatibility module remains in Dione.

The default-off `test-support` feature preserves adapter-level clock, persistence
fault, completion-pause, and snapshot fixtures through Dione's dev-dependency.
The feature is additive Cargo functionality, not a security boundary. Production
dependencies do not opt in. Member tests and application adapter tests retain
the existing cancellation, crash-durability, codec, and private-file contracts.

## Provider contracts boundary

`dione-contracts` owns provider and module identities, context records, and
checked source-observation identity. Numeric identities and opaque numeric-looking
text remain distinct. Owned fields stay private, and an edited occurrence still
requires exact agreement with its context edit timestamp. Borrowed identity
projections expose the existing numeric or opaque form without string conversion.

The crate has no application, attention, platform, or Rook dependency. Event and
delivery values belong to `dione-messages`; literal formatting belongs to
`dione-identity-display`. Application adapters and
current authorization remain in Dione. Rook keeps its wire schemas and validation. Common
observations are descriptive data, not proof of current access or action authority.

All callers use the new crate directly. Its identity/context unit tests, branded
identity UI fixtures, and private-field doctests live with the contract. No old
identity or context re-export remains in Dione's core module.

## Rook boundary

`dione-rook` owns the recipient actor, callback scheduling, private snapshots,
context service, publication state, and principal-bound tool wrappers. Claim
and publication transitions share one snapshot commit boundary. Splitting the
model or tools again would expose private records without an independent consumer.

Discord keeps its current access policy, native observation projection, and HTTP
transport. Its adapters use the existing Rook provider traits directly. Captures
and numeric projections expose descriptive data without granting access. Opaque
numeric-looking identifiers remain opaque.

Member tests retain private fault injection and crash-recovery checks. A
default-off `test-support` feature lets two Discord adapter fixtures inspect a
dispatch outcome before running secondary bookkeeping. It does not expose the
store or owner commands. Production dependencies do not enable the feature.

No compatibility module remains in Dione. The source and behavior checks retain
the existing wire formats, access revocation checks, uncertain-dispatch policy,
and callback completion barriers.

## Attention engine boundary

`dione-attention` owns configuration values, source-bound decisions, delivery
descriptors, the TypeSafe client, metadata storage, and learning. Private store
state keeps representative sampling, evaluation provenance, and promotion
together. Application callers use existing record lookup and review operations
instead of reading storage internals.

Dione keeps admission, source resolution, runtime scheduling, and controls.
Those modules own current source access, export consent, config generations,
and transport. The application's string-operation adapter error stays with
them; concrete initialization, store, and receipt error causes move with the
engine. No compatibility re-export remains.

The engine retains Serenity ID types and existing HTTP/TLS settings. A separate
provider crate is possible, but has no independent consumer today. This split
does not introduce a provider trait or change source identity representation.

The default-off `test-support` feature preserves the loopback-only provider
constructor used by runtime and MCP fixtures. Its URL validation remains
unchanged. Production dependency features leave that constructor disabled.

## Evidence and text grammar boundary

`dione-evidence` owns the complete sentex facade, locator encoding, typed
validation failures, and shared Markdown scanner. Checked handle construction
and decoder tests stay with their private representation. Keeping the facade
beside the parser preserves count-before-decode error precedence without a new
constructor or fixture API.

Chunking, status lint, and contradictionary consume this grammar independently
of delivery. A separate evidence crate keeps those consumers outside the
attention-bound message dependency. Shared events and delivery values can move
next on top of the evidence and attention crates.

Supplied-author projection remains a pure association with a caller-provided
Discord ID. The library does not authenticate authors or verify evidence truth.
Canonical markers, role separation, byte/count bounds, and code/quote exclusion
retain their existing implementations and tests. Compiler privacy examples
protect the handle and handle-collection fields.

## Shared message boundary

`dione-messages` owns observed event values, typed deliveries, and timestamps.
It depends on the existing provider contracts,
attention delivery types, and evidence grammar. No configuration, application
state, source lookup, or transport implementation moves with these values.

Private delivery fields preserve the existing constructors. Ordinary batches
still require a first event. Managed descriptors retain supplied observations
and recorded decisions; current authority remains an application check.

Timestamp construction reuses the existing `From<DateTime<FixedOffset>>`
implementation. Five parsing/compact-format tests live with the member. Nine
localization, fallback, DST, and configured presentation tests remain with
Dione's configuration module, with their bodies unchanged. Only the four
origin/targeting expectations made obsolete by public accessors are removed.

All callers use the new package directly. The old core and timestamp modules
and compatibility paths are removed. Compiler examples protect timestamp and
delivery fields from direct external construction.

## Rate-limiter boundary

`dione-rate-limiter` owns the complete existing engine and its unit/property
tests. It resolves configuration and advances token buckets using caller-supplied
time. Its buckets remain independent for each participant/channel pair. Existing
public bucket fields remain public; no construction operation changes.

Dione retains TOML validation, admitted identity keying, live reload scheduling,
edit/delete bypass, and denial handling. The MCP loop still supplies no sender
classes and drops denied `Buffer` results. This extraction does not implement
those existing unfinished integrations.

## Vaelii boundary

`dione-vaelii` owns the existing optional receipt client, its configuration and
public receipt/error values, private sentex helpers, and seven tests. It records
six provenance-only sentexes for supplied identifiers. The caller must establish
the read and its authority. Discord keeps access checks, lookup, exact-target
validation, message projection, receipt failure handling, and both wire protocols.

The move preserves endpoint/symbol validation, rustls, no redirects, the timeout
clamp, and optional per-request environment-token lookup. Existing tests cover
exact request bodies and status-only refusal; no additional redirect/token
runtime coverage is claimed.

## GAIE boundary

`dione-gaie` owns the complete archive, retained origin evidence, backfill client,
event model, and latest-state replay. The private origin/checkpoint/discovery
relationships remain within one crate. Existing API visibility and opaque
representations are preserved. The existing `CorpusIdError` gains a facade
re-export so callers can name the concrete parse failure.

The unchanged per-attachment and cumulative download defaults move from config
to the archive facade. Dione imports them directly and keeps settings, credential
loading, channel allowlists, archive CLI composition, and oracle fixtures. All
four oracle integration tests remain with the application.

Archive validation, locking, torn-tail recovery, quarantine, exact bytes,
checkpoint ordering, and HTTP/download policy retain their implementations and
tests. Retained-origin consistency does not establish external authenticity.
The member retains the existing `cfg(kani)` lint admission and one test-scoped
`await_holding_lock` allowance; the normal gate does not execute Kani proofs.

## Identity display and enrichment boundary

`dione-identity-display` owns the existing literal formatter and complete
nameplate/PronounDB clients. The clients call that formatter locally, removing
their former messages/attention dependency. Their HTTP and cache policies remain
distinct. No wrapper, provider trait, shared cache, or feature flag is added.

Dione retains settings, construction, bot/human routing, and exclusion decisions.
Nameplates relies on caller exclusion checks; PronounService also checks them
internally. Preserve that asymmetry and lookup-before-suffix handling. The
existing 100 ms real PronounDB test remains unchanged and is not hermetic.

Messages 0.2.0 removes its old `identity_display` module. All callers import the
new package directly. Messages has no dependency or publication wait on identity
display. The new package includes its HTTP dependencies even for formatting-only
checks; no application compile-time improvement is claimed.

## Harness core boundary

`dione-harness` owns the connection, delivery/protocol facets, and bounded
JSON-line writer. Both facets derive from one adapter allocation. Application
factories pass the adapter's writer; private fields prevent later independent
facet replacement. They do not establish an arbitrary adapter's implementation.

Dione retains cross-provider session selection, private request origins,
reserved control routing, and application tool registration.
The session facade directly re-exports its external connection and policy types.
Claude, Codex and OMP construct their concrete adapters through their owning
crates. Output/control assembly remains application-owned.

The private-control trait hook requires application-authorized routing. No
request-origin or reserved-name authority moves into the crate. Writer fields,
error kinds, pending-frame guard, and the ten-second deadline remain private.
Serialization precedes the shared mutex/write/flush deadline. Cancellation and
unknown-completion behavior retain their implementations and tests.

Ten existing core tests move with their bodies. The one real adapter uncertainty
test stays in Dione without writer-fixture duplication. A shared-state test
checks both facets across clones; compiler examples protect the private fields
and public constructor. Member dev Tokio adds only the existing test-util feature.

## Presentation boundary

`dione-presentation` owns the shared projector and its typed descriptive output.
Provider envelopes and legacy scope validation remain application-owned. The four
output types, two channel fields, and `project` become public; locator fields,
grouping, and the projector's metadata helpers remain private. Output is not an authority
receipt or proof of delivery.

The 21 single and nine compact tests remain in Dione to cover native conversion
and provider encoding. The 18 private-group tests move into the member with typed
message fixtures and the same assertions. They deliberately retain singleton
batch formatting; public projection still selects single formatting for an
ordinary singleton. No public helper or test-support feature is added for them.

## Contradictionary boundary

`dione-contradictionary` owns the matcher, automatic rewrite rules, and sidecar
entry schema. Its seven public types and loader retain their visibility and
behavior. Private matching helpers and tests move together. The shared evidence
Markdown scanner remains the grammar owner.

Raw settings and their test remain in the application configuration adapter.
The diary owner retains record serialization, timestamps, truncation, append
behavior, and seventeen evaluation/persistence tests. Evaluation becomes an
application function because its former matcher receiver was unused. Remove the
corresponding redundant send-side argument while preserving the presence branch
and record deferral. Fifty-one matcher tests move to the member; all sixty-nine
historical test cases retain their expected results.

The two existing Aho-Corasick constructor `expect` calls, fully qualified fmt
paths, and unrestricted descriptive fields remain recorded inherited limitations.
This extraction does not redesign constructor errors or confer authority on hits.

## Bell retrieval boundary

`dione-bell-rings` owns the complete memory retrieval backend. One query operation
hides MCP setup, session reuse, concurrent connections and recalls, wire decoding,
admission, ranking and aggregation. Its provider interface and parser remain private.
The existing parallel single-provider test algorithm is removed. Native eligibility
and shadow/live publication remain application policy; delivery retains its
exhaustive conversion to the separate retained-message status enum.

Recall options validate finite distance 0..=2, count 1..=100 and timeout 1..=2000 ms.
This is an intentional Rust API/behavior change for previously invalid programmatic
configuration. The application replaces three raw fields with one validated value.
Existing TOML names/defaults and per-field validation timing remain. Provider leaf
parse errors become typed errors. No old application re-export aliases remain.

Connections retain their fixed five-second bound, followed by per-provider recall
timeouts, followed by parsing. This is not a total deadline. Cache keys remain URL
and scope, excluding alias; eviction does not cancel in-flight clients. Session
locking, reconnect behavior, cancellation and telemetry distinctions are preserved.
No generation fence, shutdown/drain guarantee, privacy improvement or speedup is
claimed. Protocol fixtures exercise the real application adapter; private mocks
exercise the same production aggregation, not a second algorithm.

## Delivery scheduler boundary

dione-delivery-scheduler owns the existing generic BufferedEvent and
DeliveryBuffer implementation and its eight engine tests. It depends on
dione-contracts for complete conversation identities and uses the Tokio clock.
Discord retains native event classification and evidence eligibility. MCP
retains routing, retry decisions, and shutdown ownership. All callers import
the new member directly, and the old application module is removed.

The fixed immediate budget stays private. Discord adapter tests assert four
immediate events before buffering, rather than importing the engine constant.
FIFO, rejected payloads, first deadlines, key ordering, control bypass, and
failed-tail requeue retain their implementations and tests.

## Access-request storage boundary

dione-access-requests owns AccessQueue, AccessRequest, queue.json loading and
replacement, preview truncation, expiry, and notification cooldown. Its eight
unit tests and six persistence consumer tests move with it. Application
AccessCtx retains configuration-backed authorization and unknown-durability
handling. Locks, pruning scheduling, native admission, notifications and MCP
projection remain with application owners. The old queue module is removed.

Stored records remain observations. Zero and maximum IDs and mismatched
historical map/payload keys remain readable. Preview length is 100 UTF-8 bytes,
not characters. Memory-first mutation, logged persistence failure, malformed
state fallback, and synchronous I/O retain their existing behavior.

The unchanged raw identity fields, operation arguments and derived
deserialization are inherited type-policy violations, scoped to this storage
API. This extraction does not extend their semantics or establish compliance.
The crate README records that debt and the existing async blocking-I/O limit.

## Mute-store boundary

dione-mutes owns receipt replay, projection, mutation, clock, notifier and expiry
mechanics with all 27 existing unit tests. Historical optional fields, raw IDs,
line-numbered corrupt replay errors, append-before-projection and expiry retry
behavior remain unchanged. The store still accepts zero TTL; MCP rejects it.

The existing singleton and its initialization functions move only to
composition::mutes. All concrete types are imported directly from the member.
Startup retains load/install before the offline-command return, worker spawn
after that return and explicit abort on normal shutdown. Current authorization
and post-wait mute rechecks stay in application adapters.

Raw numeric identities/TTL, mutable public fields, derived deserialization,
operation/optional-field combinations, String mutation errors and boxed errors
are inherited scoped policy violations. Append errors do not prove no bytes
reached disk. No torn-tail recovery, cross-instance lock, directory synchronization
or shutdown redesign is added. The member README records these limits.

## Coordination boundary

dione-coordination owns the claim-once protocol, coordinator configuration shape,
checked claim progress, acknowledged release, shared push bus and gateway identity.
The three progress states remain exhaustive. ClaimAttempt has private fields and
construction; into_parts consumes the result without allowing fabricated outcome
and progress combinations. Rook checks WriteUncertain before fail-open policy.
Ordinary claims retain a waiter connection; checked acquisition closes it.

Config assembly, gateway installation, startup subscription, Discord inheritance,
current authorization/generation checks, publication receipts and after-commit
bookkeeping remain in Dione. Twenty-two protocol tests move with the owner; two
real config/harness consumer tests remain in a private application test module.
No reverse application dependency or compatibility re-export is added.

Existing raw string identities, millisecond config values, mutable config,
derived deserialization and error/presentation strings remain scoped inherited
type/error-policy debt. Direct config construction still bypasses the nonzero
connect-timeout deserializer. Worker lifetime and shared globals are unchanged.
No new shutdown, authorization or delivery-durability guarantee is claimed.

## Pre-send boundary

dione-pre-send keeps the runner, validated hook/construct identities, private
context and outcome representations, sinks, private status lint and one
production roster together. The lint reuses the evidence Markdown scanner.
Composition retains the existing installation/getter state. Application adapters
retain config enablement, author attribution, native authority, redirect checks,
Rook preparation, contradictionary consent and one-shot's empty Observe roster.
Named hook bypass does not bypass the separate consent mechanism.

Seven banned non_exhaustive attributes are removed at the new crate boundary;
fields remain private and application matches remain exhaustive. Runtime behavior,
error wording and construction invariants are unchanged. Existing string errors,
manual Display/Error, channel classification/destination combinations and some
unused setter states remain scoped inherited policy debt. Production-roster
checks prove the builder executes lint and emits real audit events; they do not
observe whether startup calls that builder.

## Teams identity boundary

dione-teams-identities owns TenantId, ApplicationId, BotId, ActorId,
ConversationId, ActivityId and their shared typed errors. The six private
representations, common validator, conversions and validating serde move unchanged.
Values preserve case, spaces and spelling; limits remain nonempty, at most 512
UTF-8 bytes and no control characters. Whitespace-only values remain accepted.
Syntax does not establish authenticated identity or grant reply authority.

Configuration imports ConversationId directly, removing its application Teams
backedge. Native auth, persisted reply handles, startup, probe and OMP consumers
use the owning crate. The obsolete module and compatibility-only facade reexports
are removed; no generic ID abstraction or duplicate validator is introduced.
Actual persisted config/auth/store tests remain separate from member serde proof.

## Discord input identity boundary

dione-discord-identities owns Snowflake with its private NonZeroU64,zero-rejecting
constructor and existing consuming ChannelId/MessageId/UserId/GuildId conversions.
The numeric getter remains unit-test-only. No serde,generic parser,new semantic
identity family or compatibility alias is introduced. Nonzero input grants no
existence or permission.

Config author parsing,strict optional cursors,search visitors,tool arguments and
persisted queue codecs retain their string/numeric/null/empty/error distinctions.
The application Discord ids module is removed and direct owning imports eliminate
the config backedge. config_store::DiscordId stays with its editor-input owner.
Serenity workspace features are retained; explicit existing native-tls vendored
feature supports standalone builds. No verified model-only minimum is claimed.

## PluralKit configured identity boundary

dione-pluralkit-identities owns private String PkUuid, its existing full dashed
ASCII-hex syntax and lowercase normalization, and the sole official application
ID constant. Nil and arbitrary hex version/variant values remain accepted. The
new public error struct carries an exhaustive Empty or InvalidFormat kind without
rejected input. Diagnostics intentionally become lowercase fragments. No serde,
FromStr, mutable constructor, HTTP client, or trust proof is added.

Config retains raw_had_identity_entries and fail-closed selector behavior.
The HTTP resolver retains uuid::Uuid parsing and verified response binding.
Verified-action constructors, fields, linear state and configured trust remain
application-owned. The constant identifies a deployment and grants no authority.

## Configuration authority boundary

dione-config keeps model validation and resource assembly, candidate merge,
editor, persistence, recovery, and global publication together. Its three
implementation modules are private. Consumers use explicit root exports and the
existing fallible LoadedConfig constructor. There is one process-wide path,
cache, generation allocator, receiver source, and canonical writer. Read-only
loading still consumes that allocator without publishing or writing defaults.

The attention admission guard acquires that same writer. No public cache reset,
publisher, counter selection, fault injector, or dependency test convenience is
introduced. The four settings enums are exhaustive without changing variants or
wire spelling. Existing author-ID untagged serde and raw/string-error settings
remain scoped debt.

Actual injected post-rename fsync proof stays in member tests. Application
completion consumes the existing typed receipt and proves real queue/wire/retry
handling of an explicit Unknown receipt after a real normal mutation. This split
loses the former combined syscall-to-MCP injected path. The application retains
its concrete file watcher and process lifetimes.

## Outbound consent boundary

dione-outbound-consent keeps consent decisions, held reservation and retry state,
journal maintenance, deferred diary and fence-aware chunking together. Native
HTTP, authorization, coordination and process lifetime stay application-owned.
Requests have ordinary constructors, while fence fields, retry remainder and
nonempty publication representation remain private. The terminal native adapter
consumes typed cause, progress and diary without exposing retry remainder; the
consent gate retains that remainder for subsequent attempts.

The queue remains process-local and single-principal, with lookup handles rather
than authentication tokens. Journal appends remain best effort. The inherited
plain chunker tiny-limit multibyte risk is unchanged; this extraction makes no
arbitrary-limit safety claim.

## MCP tool registration boundary

dione-mcp-tools owns immutable registration validation, lexical discovery,
explicit resident opt-in and borrowed Send handler futures. All invariant-bearing
representations remain private. Server assembly supplies a fresh concrete
BTreeSet catalog at initial construction and replacement, joining all fixed
harness names, current selected-harness tool definitions and private controls.
The member has no platform or harness dependency. Native producers retain
services and reply authority; transport retains origin, readiness and admission.

Existing malformed discovery behavior is retained. Missing tools becomes null
through mutable JSON indexing; existing non-array values remain unchanged.

## Shared provider delivery boundary

dione-provider-delivery owns the existing concrete provider-qualified replay key,
Teams resident event and sink contracts, classified admission receipts and
reply-handle syntax. Private implementation modules publish explicit root types.
The event is descriptive and retains an independent replay key; OMP still checks
it against native coordinates. Codex legacy/version0 and OMP version1 codecs
remain separate, with unchanged fixture bytes and replay ordering.

Handle parsing preserves historical UUID spelling; typed UUID conversion formats
canonical syntax. Random generation stays private in Teams runtime. Native
producers retain authenticated envelopes, sealed store and current authorization,
including prepare/consume/send order and no send on uncertain consume. Concrete
composition adapters remain application-owned.

## Whole Teams owner

`dione-teams` owns the existing authentication, JSON boundary, native edge,
reply store, resident bridge, listener and Teams tool as one unit. Direct
application consumers use its public service operations; no root forwarding
module or reverse application dependency remains. Authenticated envelopes,
metadata clients, tokens, correlated/prepared requests, sealed authority and
store/fault machinery remain private.

Native-only tests stay beside the owner. Actual Codex and OMP queue/readiness,
restart, cancellation and tool cases use the owner's private unit-test process.
Only descriptive events, classified receipts and reply operations cross that
test boundary. Actual root JoinError assertions happen before serialization;
native typed-source forwarding is separately exercised locally. IPC does not
preserve Rust cause identity. Existing consume-fault/native HTTP-count assertions
remain together in the member. Verification supplies the exact current fixture
compiler artifact before each Nextest run, with no nested Cargo or public fixture
API.

## Ordered follow-up splits

1. Rendering, including caller migration and workspace packaging.
2. Durable queues, after assigning persistence-helper ownership and the public
   admission operations.
3. The narrow provider identity and context contracts needed by Rook. Moving
   all of `core` would create a dependency cycle with attention.
4. Rook, after its shared contracts and persistence dependencies have owners.
5. The attention engine, with application admission and runtime retained.
6. Evidence and Markdown grammar, independent of attention-bound delivery.
7. Shared message events, presentation, and delivery values.
8. The independent rate-limiter engine.
9. The optional Vaelii provenance receipt client.
10. The complete GAIE archive and backfill owner.
11. Literal identity display and the two enrichment clients.
12. Coupled harness facets and bounded protocol output.
13. Shared presentation and compact batch formatting.
14. Contradictionary matching, rewrites, and sidecar entry schema.
15. Bell memory retrieval backend, with native eligibility and publication retained.
16. Generic scoped delivery scheduler, with native evidence classification retained.
17. Pending access-request storage, with configuration-backed authorization retained.
18. Mute store and expiry mechanics, with process wiring and authorization retained.
19. Checked coordination protocol, with publication policy retained.
20. Cohesive pre-send hooks and private status lint, with native consent retained.
21. Teams identity family, with native authentication and reply authority retained.
22. Discord nonzero input identity and SDK conversions, with adapter parsing retained.
23. PluralKit configured UUID and deployment identity, with network trust retained.
24. Unified configuration validation, persistence, recovery, and global publication.
25. Cohesive outbound consent, held retries, diary, journal and fence-aware chunks.
26. Immutable MCP tool registration and owner handler contracts.
27. Shared provider replay identity, Teams delivery and reply-handle contracts.
28. Whole native Teams runtime and private authority.
29. Cohesive OMP actor, lifecycle, codec and protocol adapter.

Each PR migrates callers and tests before removing the old module. Attention
orchestration and whole platform adapters need further dependency work and are outside this
first sequence. Compare native Linux dependency builds, warm application edits,
test preparation, and release builds separately before making speed claims.

## Package and release requirements

The application uses versioned path dependencies on `dione-rendering`,
`dione-durable`, `dione-contracts`, `dione-rook`, `dione-attention`,
`dione-evidence`, `dione-messages`, `dione-rate-limiter`, `dione-vaelii`,
`dione-gaie`, `dione-identity-display`, `dione-harness`,
`dione-presentation`, `dione-contradictionary`, `dione-bell-rings`, `dione-delivery-scheduler`, `dione-access-requests`, `dione-mutes`, `dione-coordination`, `dione-pre-send`, `dione-teams-identities`, `dione-discord-identities`, `dione-pluralkit-identities`, `dione-config`, `dione-outbound-consent`, `dione-mcp-tools`, `dione-provider-delivery`, `dione-teams`, `dione-omp`, `dione-codex`, and `dione-claude`.
`cargo package --workspace --locked` verifies the interdependent packages
before their new versions exist on crates.io. The gate checks all archives with the existing
public-package privacy checker. Do not replace verification with an unchecked
archive or a private package dependency.

Each member declares its own explicit package version. Any change under an
existing member directory, including its manifest, README, or fixtures, requires
a greater member version. Rust source changes also require a Dione version bump
and changelog entry. New members may start at their initial version. Release
hygiene enforces these rules before tagging.

The release workflow verifies capability namespace owner allowlists and package
states. It publishes and reconciles the capabilities before publishing Dione.
Dione retains exact archive checks. Capability comparisons ignore only the
root `.cargo_vcs_info.json` receipt so an unchanged renderer version can survive
later application commits; every other archived byte remains protected.
The already-published path remains mandatory.
Before the first release, an authorized operator must reserve `dione-rendering`,
`dione-durable`, `dione-contracts`, `dione-rook`, `dione-attention`,
`dione-evidence`, `dione-messages`, `dione-rate-limiter`, `dione-vaelii`,
`dione-gaie`, `dione-identity-display`, `dione-harness`,
`dione-presentation`, `dione-contradictionary`, `dione-bell-rings`, `dione-delivery-scheduler`, `dione-access-requests`, `dione-mutes`, `dione-coordination`, `dione-pre-send`, `dione-teams-identities`, `dione-discord-identities`, `dione-pluralkit-identities`, `dione-config`, `dione-outbound-consent`, `dione-mcp-tools`, `dione-provider-delivery`, `dione-teams`, `dione-omp`, `dione-codex`, and `dione-claude`,
configure its approved owners, and register the repository's trusted publisher
for `.github/workflows/publish-crate.yml` and its `release` environment. Local
package verification does not prove those registry settings exist. This
refactor does not publish packages or change registry ownership.

Rook publishing waits for contracts and durable registry reconciliation. Dione
publishing waits for all twenty-nine capabilities. Contradictionary publishing waits
for evidence reconciliation. Presentation publishing waits for
contracts, attention, evidence, messages, and identity-display reconciliation. Harness publishing waits for
contracts and messages reconciliation. Messages publishing waits for
contracts, attention, and evidence reconciliation. Attention publishing waits for
contracts reconciliation. Each reconciliation verifies the
archive contents after registry resolution.

The new publishing and reconciliation jobs have not been exercised against a
live registry. Local package-state tests mock registry responses; the retry
fixture proves Cargo dependency resolution with a local registry only.

Scheduler publishing waits for contracts reconciliation. Dione publishing also waits for scheduler reconciliation.

Access-request storage has no capability publication prerequisite. Dione publishing waits for its reconciliation.

Mute storage has no capability publication prerequisite. Dione publishing waits for mute reconciliation.

Coordination is a leaf publication. Dione publishing waits for its tokenless reconciliation.

Pre-send publishing and reconciliation wait for evidence reconciliation. Root publication and reconciliation wait for pre-send.

Teams identities is a leaf publication. Root publication and reconciliation wait for its reconciliation.

Discord identities is a leaf publication. Root publication/reconciliation wait for its reconciliation.

PluralKit identities is a leaf publication. Root publication and reconciliation wait for its reconciliation.

Config publication and tokenless reconciliation wait for all thirteen direct
capability reconciliations: attention, Bell rings, contradictionary, coordination,
Discord identities, GAIE, identity display, messages, PluralKit identities,
pre-send, rate limiter, Teams identities, and Vaelii. Root publication and
reconciliation also wait for config reconciliation.

Outbound consent publishing and tokenless reconciliation wait for configuration,
evidence, Contradictionary and messages reconciliation. Dione waits for outbound
consent reconciliation as well.

MCP tools is a capability leaf. Root publication and tokenless reconciliation
wait for its reconciliation before publishing the application.

Provider delivery publication and tokenless reconciliation wait for messages
and Teams identities; root waits for provider delivery reconciliation.

Teams publication and tokenless reconciliation wait for messages, Teams identities, provider delivery and MCP tools. Root waits for Teams reconciliation.

The `dione-omp` member owns the durable OMP session actor, strict version-1 codec and protocol adapter. Binding identities, recovery state and command controls remain private. Application composition supplies the actual Codex tool catalog and shared protocol output; transport-origin admission and process shutdown policy remain in the application. Publication and tokenless reconciliation wait for durable, harness, messages, provider-delivery, Teams identities, contracts and presentation before root publication.


## Codex runtime owner

The `dione-codex` member owns the complete queue, historical codec, protocol adapter, delivery worker, session lifecycle, and concrete Teams admission mapping. Resident handles, binding controls, fault injection, and stored payload internals remain private. Application composition selects the owner, supplies delivery configuration and attention guards, and maps its concrete lifecycle errors exhaustively. The actual immutable Codex tool catalog also supplies Claude reserved-name handling and OMP composition.

Private process fixtures preserve actual root dispatch and native admission effects across the test boundary. The owning process inspects genuine completion causes before descriptive transfer. Root observes explicit remote provenance; transferred attention descriptors preserve every decoded field through existing public constructors and do not confer authority. Presentation fixtures observe actual public adapter/queue output. Publication waits for attention, configuration, contracts, Discord identities, durable, harness, messages, presentation, and provider-delivery prerequisites before Codex and root publication.

## Claude adapter owner

`dione-claude` owns channel and permission envelopes, protocol metadata, reserved-tool rejection and shared-output error classification. Application composition supplies one `ProtocolOutput` and the actual immutable Codex catalog. The owner reuses harness output and presentation; it has no Codex runtime dependency. Root retains selection, stdout construction, lifecycle and native authorization. Publication waits for contracts, harness, messages and presentation before Claude and root reconciliation.
