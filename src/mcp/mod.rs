//! MCP transport, request dispatch, and owner-provided tool registrations.

pub mod dispatch;
pub(crate) mod presence_wire;
pub mod protocol;
pub(crate) mod render_wire;
pub mod server;
pub mod tools;

pub(crate) mod harness_wire;
