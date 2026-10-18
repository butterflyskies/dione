//! MCP transport, request dispatch, and owner-provided tool registrations.

pub(crate) mod access_wire;
pub mod dispatch;
pub(crate) mod presence_wire;
pub mod protocol;
pub(crate) mod render_wire;
pub mod server;
pub mod tools;

pub(crate) mod harness_wire;

pub(crate) mod tool_registry;

pub use tool_registry::{
    RegisteredTool, RegistrationError, RegistrationErrorKind, ToolName, ToolNameError,
};
