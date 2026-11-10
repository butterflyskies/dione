//! MCP transport, request dispatch, and owner-provided tool registrations.

pub mod dispatch;
#[cfg(test)]
mod protocol_tests;

pub mod server;
pub mod tools;

pub(crate) mod harness_wire;
