//! Coupled harness facets and bounded JSON-line output.
//!
//! Application factories supply one adapter and its output. The application
//! retains provider runtimes, request-origin authorization, and tool registration.
#![warn(missing_docs)]

mod connection;
mod output;
mod protocol;

pub use connection::{AttentionPolicy, HarnessConnection, HarnessDelivery, HarnessError};
pub use output::{ProtocolOutput, ProtocolOutputError};
pub use protocol::HarnessProtocol;
