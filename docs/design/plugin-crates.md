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

The application uses a versioned path dependency on `dione-rendering`.
`cargo package --workspace --locked` verifies both interdependent packages
before either new version exists on crates.io. The gate checks both archives with the existing
public-package privacy checker. Do not replace verification with an unchecked
archive or a private package dependency.

The release workflow verifies both namespace owner allowlists and package
states. It publishes and reconciles the renderer before publishing Dione.
Dione retains exact archive checks. Renderer comparisons ignore only the
root `.cargo_vcs_info.json` receipt so an unchanged renderer version can survive
later application commits; every other archived byte remains protected.
The already-published path remains mandatory.
Before the first release, an authorized operator must reserve `dione-rendering`,
configure its approved owners, and register the repository's trusted publisher
for `.github/workflows/publish-crate.yml` and its `release` environment. Local
package verification does not prove those registry settings exist. This
refactor does not publish packages or change registry ownership.
