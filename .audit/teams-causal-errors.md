# Teams causal errors

This unit preserves concrete causes across authentication, reply authority,
durable admission, and runtime boundaries. Struct errors and exhaustive kind
enums replace flattened strings. One dynamic-source seam lets the Codex adapter
retain its actual cause without introducing a Teams dependency on Codex types.

Authority mint, consume, discard, and cleanup distinguish a visible mutation
with uncertain durability from rejection before the current mutation. Admission
and cleanup failures retain both causes. Public error text excludes underlying
URLs, paths, identifiers, and panic payloads.

All affected production callers and test fixtures migrate together. Version
0.73.0 reflects the public Rust error API changes. Reply preparation, authority
consumption, cancellation-resistant tasks, drain order, and wire status behavior
remain covered by the existing composed tests.

The original causal-errors candidate was signed as
`41ce9ca3dde371043b6b5ee892d6be46501a3ae6` over identities `557304a`.
Its retained source evidence records feature Clippy and an initial 231 focused
Nextest tests, followed by 218 after the exhaustive-match correction. A
synthetic-cause mutation failed the original-source downcast assertion, and
restoring the source passed that same test. These are historical checks.

This secure publication candidate transfers only the causal-errors atom onto
the prepared secure reply and published identities composition. The existing
identities commit remains 1f5abebd211a8dd8c8e13b29f5cd5bf447cf5091; its PR is
not replaced. Both Discord resident registration and Teams public error
redaction tests remain. Version 0.73.0 advances the shared 0.72.0 prerequisite.

Fresh source review and native verification remain pending. No deployment
or live host acceptance was performed.
