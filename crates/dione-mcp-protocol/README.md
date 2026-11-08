# dione-mcp-protocol

Pure MCP initialization and built-in tool discovery responses for Dione.
The application supplies its own version and current capability/tool snapshots.
The owner preserves protocol negotiation, all thirty-three built-in descriptions,
ordering and current evidence-marker presentation. Harness definitions follow
built-ins in caller-supplied order.

The library does not authenticate requests, initialize a harness, install tools,
execute handlers or retain current policy. The application remains responsible
for all of those operations and for appending installed module definitions.
A metadata snapshot grants no execution authority.

This package uses Rust 1.98 or newer and the MIT OR Apache-2.0 license.
