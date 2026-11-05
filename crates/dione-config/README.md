# Dione configuration

This crate owns validated settings, resource assembly, comment-preserving TOML
editing, candidate/sidecar merging, recovery, and one process-wide snapshot.
Runtime handles share the path override, generation allocator, watch sender,
cache, and writer. A second handle does not create a second publication owner.

ConfigRuntime serializes startup, reload, and mutation. Rename is the applied
boundary. A parent-directory fsync failure after rename publishes the validated
visible bytes and reports Unknown durability. Cancellation leaves the spawned
owner holding the writer through its blocking effects. Recovery retains captured
validated bytes and permissions for last-known-good promotion.

Consumers can read generation, subscribe through a receiver, and acquire the
same writer guard for attention admission. Publication, cache replacement,
counter selection, persistence helpers, and fault injection remain private.
The application retains concrete file watching, startup lifetimes, and native
permission decisions. Read-only loading consumes the shared generation allocator
without publishing or writing defaults.

The configuration template ships in src/config_template.toml. Existing raw
settings, string errors, and author-ID untagged serde are inherited behavior.
The four settings enums are exhaustive. No runtime variants or wire spelling
change in this extraction.
