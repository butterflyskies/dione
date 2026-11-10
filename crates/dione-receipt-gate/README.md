# dione-receipt-gate

Owns the existing dormant reaction authority receipt store, exact command parser,
and private authenticated-source and policy evidence constructors. It preserves
scope, expiry, global invocation deduplication, bounded capacity and poisoned-state
fail-closed behavior. The ordinary unit tests remain inside the sealed owner.

There is no public runtime interface, gateway registration, or live action protection.
The in-memory fixture is fail-stop across process restart and is not durable storage.
