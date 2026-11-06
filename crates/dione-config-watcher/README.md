# Dione config watcher

Owns directory watching, canonical filename/event filtering, bounded coalescing,
ConfigRuntime reload triggering and configuration error forwarding. The config
owner still serializes disk work and publishes the process-wide snapshot.

The callback uses blocking_send into 16 slots. The detached task checks cancellation
only at the outer select; active 50 ms coalescing, reload and awaited error forwarding
finish first. Saturated destinations can delay shutdown. The returned task handle
allows observation but dropping it does not cancel. No prompt/flood-safe shutdown
claim is supplied.

Tests use a real watcher and the public config publication/error APIs, one combined
filesystem case for process-global state, and scoped existing startup trace
observation for readiness. They cover canonical writes, atomic replacement,
unrelated files, LKG/no-LKG error recovery and normal cancellation. They do not
exercise callback or error-queue saturation. Run cargo nextest run -p
dione-config-watcher --locked; ordinary cargo test shares the same one-case global
isolation.
