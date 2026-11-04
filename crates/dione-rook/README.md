# dione-rook

Rook owns durable recipient handling, callback scheduling, context reads, and
publication receipts. The embedding application supplies authenticated
recipients, current authorization, provider adapters, and callback transport.

`RookService` owns the recipient actor and private snapshot storage. Its handle
accepts commands without transferring commit lifetime to the caller. Context
reads recheck access, and uncertain publication outcomes never authorize another
message POST. Storage schemas and filesystem admission remain private.

Provider identities and observations come from `dione-contracts`. Rook retains
its checked durable projection and uses `dione-durable` for atomic snapshot
replacement. Numeric-looking opaque identities remain opaque.

The application implements `Authorizer`, `WakeTransport`, `ContextProvider`,
and `PublicationProvider`. Captured observations describe data; they do not
authenticate a recipient or prove current access. Principal-bound tool wrappers
keep authentication outside tool arguments.

The default-off `test-support` feature lets adapter fixtures separate dispatch
outcomes from secondary bookkeeping. It bypasses normal owner scheduling for
tests and is additive Cargo functionality, not a security boundary. Production
dependencies do not enable it.

This package is part of the Dione workspace and uses the same Rust 1.98 minimum
version and MIT OR Apache-2.0 license.
