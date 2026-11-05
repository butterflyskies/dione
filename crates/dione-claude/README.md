# dione-claude

Claude channel notifications, permission envelopes and protocol metadata over the shared MCP writer.

`connect(output, codex_tool_names)` retains the supplied `ProtocolOutput` for notifications and responses. Application composition owns transport construction and supplies the actual immutable Codex catalog. Unknown names fall through; catalog members retain the Codex-only rejection.

The adapter reuses `dione-presentation` and `dione-harness`; it adds no worker or durable state. Projection, adapter fields and error construction remain private. Output poison and completion uncertainty follow the shared writer.
