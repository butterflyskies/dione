# Dione harness runtime

Owns concrete Claude, Codex, and OMP session preparation, startup, shutdown,
and resource-free selection above the existing provider and harness crates.
The connection and lifecycle remain private; typed admission handles support
explicit application composition without granting Discord authority.

Only resident Codex detaches on MCP EOF. MCP enforces the selected policy;
providers retain their existing queues, catalogs, writers, cancellation, and
resource lifetimes. Private tests exercise real adapters and durable resources.
