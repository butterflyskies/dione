# dione-discord-typing

Owns authorized Discord typing notifications and their MCP result projection.
`TypingCtx` holds the HTTP client, shared state, and caller-selected configuration
snapshot. Each `send_typing` call checks current DM and thread mappings through
`OutboundGate`, releases the read guard, and sends once only when allowed.

Context fields, the transport seam, and SDK failure construction remain private.
The application retains argument parsing, protocol registration, and composition.
Unit tests exercise live authorization changes and original SDK error sources;
an ordinary external consumer checks denial without issuing HTTP. Successful
transport effects use a private fake; no authenticated Discord request is claimed.
