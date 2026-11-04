# Dione provider contracts

`dione-contracts` owns source-qualified provider identities, descriptive context
records, and checked message-occurrence identity. Numeric local identities and
opaque numeric-looking text remain distinct. A module's provider and configured
slot are separate types.

The `context` module records unavailable values separately from known empty or
absent values. The `observation` module pairs context with a created occurrence
or an edited occurrence whose exact revision agrees with that context.

These values describe observations. They do not authenticate a recipient,
prove current access, grant consent, or authorize a verified action. Adapters
and their consumers retain their existing authorization and coherence checks.

Identity and observation fields remain private. Callers use the existing checked
constructors and accessors; detached borrowed identity projections preserve the
numeric or opaque form without string round trips.

The crate depends only on `chrono` and `thiserror`. Presentation, event delivery,
attention, platform adapters, and Rook's durable wire projections remain with
their respective owners. The package owns the branded identity compile-time
fixtures and public-field privacy doctests.
