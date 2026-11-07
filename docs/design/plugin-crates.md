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

Each PR migrates callers and tests before removing the old module. Attention
orchestration and whole platform adapters need further dependency work and are outside this
first sequence. Compare native Linux dependency builds, warm application edits,
test preparation, and release builds separately before making speed claims.

## Package and release requirements

The application uses versioned path dependencies on `dione-rendering`,
`dione-durable`, `dione-contracts`, `dione-rook`, `dione-attention`,
`dione-evidence`, `dione-messages`, `dione-rate-limiter`, `dione-vaelii`, and
`dione-gaie`, and `dione-identity-display`.
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
`dione-evidence`, `dione-messages`, `dione-rate-limiter`, `dione-vaelii`, and
`dione-gaie`, and `dione-identity-display`,
configure its approved owners, and register the repository's trusted publisher
for `.github/workflows/publish-crate.yml` and its `release` environment. Local
package verification does not prove those registry settings exist. This
refactor does not publish packages or change registry ownership.

Rook publishing waits for contracts and durable registry reconciliation. Dione
publishing waits for all eleven capabilities. Messages publishing waits for
contracts, attention, and evidence reconciliation. Attention publishing waits for
contracts reconciliation. Each reconciliation verifies the
archive contents after registry resolution.

The new publishing and reconciliation jobs have not been exercised against a
live registry. Local package-state tests mock registry responses; the retry
fixture proves Cargo dependency resolution with a local registry only.
