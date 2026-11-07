# Dione rate limiter

The engine resolves scoped configuration and maintains independent token buckets
for each supplied participant and channel pair. Callers supply an `Instant` to
advance state, check messages, and prune idle buckets. It has no platform or
asynchronous runtime dependency.

Individual configuration wins over the first matching class in the caller's
ordered class list, then the default applies. A channel override replaces that
entire result. Changing a resolved configuration replaces its bucket on the next
check. Constructing a bucket with zero tokens panics.

The application owns admission, identifier selection, scheduling, and the action
taken after denial. `OverflowPolicy` describes requested handling; this package
does not send notifications, discard messages, or buffer them. Existing bucket
state and remaining-token fields remain public.

This crate is part of the Dione workspace and uses the same MIT OR Apache-2.0
license. Its existing unit and property tests move with the implementation.
