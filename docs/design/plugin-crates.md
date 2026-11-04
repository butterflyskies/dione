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

The crate has no application, attention, platform, or Rook dependency. Display,
events, and delivery remain in Dione because they have presentation or application
ownership. Rook keeps its wire schemas, validation, and authorization. Common
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

## Ordered follow-up splits

1. Rendering, including caller migration and workspace packaging.
2. Durable queues, after assigning persistence-helper ownership and the public
   admission operations.
3. The narrow provider identity and context contracts needed by Rook. Moving
   all of `core` would create a dependency cycle with attention.
4. Rook, after its shared contracts and persistence dependencies have owners.

Each PR migrates callers and tests before removing the old module. Attention
and whole platform adapters need further dependency work and are outside this
first sequence. Compare native Linux dependency builds, warm application edits,
test preparation, and release builds separately before making speed claims.

## Package and release requirements

The application uses versioned path dependencies on `dione-rendering`,
`dione-durable`, `dione-contracts`, and `dione-rook`.
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
Before the first release, an authorized operator must reserve `dione-rendering`, `dione-durable`, `dione-contracts`, and `dione-rook`,
configure its approved owners, and register the repository's trusted publisher
for `.github/workflows/publish-crate.yml` and its `release` environment. Local
package verification does not prove those registry settings exist. This
refactor does not publish packages or change registry ownership.

Rook publishing waits for contracts and durable registry reconciliation. Dione
publishing waits for all four capabilities. Each reconciliation verifies the
archive contents after registry resolution.

The new publishing and reconciliation jobs have not been exercised against a
live registry. Local package-state tests mock registry responses; the retry
fixture proves Cargo dependency resolution with a local registry only.
