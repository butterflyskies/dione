# Dione access requests

The crate stores pending access requests in queue.json and tracks in-memory
administrator-notification cooldown. Enqueue preserves the first request for
each stored key, enforces caller-supplied capacity, and truncates previews to
100 UTF-8 bytes at a character boundary. Listing is chronological, with map-key
order breaking timestamp ties. Notification state resets when the queue loads.

The disk format is a pretty JSON object with numeric keys encoded as strings.
Historical zero and maximum IDs, and mismatched map keys and payload user IDs,
remain readable and are preserved on write. AccessRequest is an observational
record. It does not prove Discord identity or current authorization. Removing
an approved request does not grant access.

Mutations update memory first and attempt persistence. Failures are logged;
a removed entry can remain on disk. Persistence uses a temporary file and
rename without a new synchronization or durability guarantee. Malformed or
unreadable state logs a warning and starts empty. Filesystem operations are
synchronous.

The application owns policy, authenticated IDs, ConfigRuntime-backed approval,
async locking, administrator delivery, capacity, expiry, and pruning lifetime.

## Inherited limitations

The unchanged AccessRequest public raw u64 field and derived deserialization
do not enforce the project's distinct-identity type policy. AccessQueue still
uses raw numeric map keys and operation arguments. This extraction preserves
those existing contracts and historical observations. It does not introduce
a new semantic identity API or claim type-policy compliance for them.

The existing synchronous filesystem calls remain inside load, persist and
mutation operations. Existing async application callers therefore retain their
preexisting blocking-I/O limitation. Best-effort persistence and empty fallback
are also unchanged. A future semantic or durability redesign must migrate
callers and historical data together.
