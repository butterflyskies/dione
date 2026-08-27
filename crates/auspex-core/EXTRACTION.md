# Auspex-Core Extraction Contract

This crate is the provenance verification kernel for the Auspex system.
It currently lives inside the dione repository as a workspace member for
development convenience. The plan is to extract it to its own repository
(`butterflyskies/auspex`) once the tracer phase stabilizes.

## Boundary rules

- **No transport implementation types.** Serenity types, HTTP clients, and MCP
  tools do not belong here. `MessageRef` provider-tags native message IDs so
  different transports cannot silently collide; `ChannelRef` and
  `PrincipalRef` remain opaque u64 newtypes until separately migrated.
- **No side effects.** No logging, no network calls, no file I/O, no alerts.
  The crate returns decisions/reasons; the adapter (dione) owns effects.
- **No async.** The kernel is synchronous. Mutex for thread safety, Instant
  for time. The adapter wraps in async if needed.
- **Injected time.** The epoch is recorded at construction. Future extensions
  should accept time evidence rather than calling `Instant::now()` internally
  where possible.

## What lives here

- `IngressLedger` — bounded in-memory record of gateway-admitted messages
- `VerifyResult` — typed verification outcomes (Admitted, Unknown, Expired,
  ChannelMismatch, Unavailable)
- Domain references — provider-tagged `MessageRef`; opaque `ChannelRef`,
  `PrincipalRef`, and `ContentHash` newtypes
- (future) `ActivationRoot` enum and causal tree types
- (future) `verify_activation` — the full provenance walk

## What stays in dione

- Discord ↔ auspex-core type conversion (message ID → `MessageRef::discord`)
- Phantom canary alerts (transport effect)
- Tracing/logging of verification results
- Configuration (alert channel, TTL overrides)
- The `reply_with_hook_overrides` egress wiring

## Extraction steps (when ready)

1. `git subtree split -P crates/auspex-core -b auspex-core-split`
2. Push to `butterflyskies/auspex` as initial commit
3. Replace path dependency with git dependency in dione's Cargo.toml
4. CI: add auspex-core to the auspex repo's workflow
