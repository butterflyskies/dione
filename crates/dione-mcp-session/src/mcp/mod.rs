//! MCP transport, request dispatch, and owner-provided tool registrations.

pub(crate) mod dispatch;
#[cfg(test)]
mod protocol_tests;

pub(crate) mod server;
mod tools;

pub(crate) mod harness_wire;
