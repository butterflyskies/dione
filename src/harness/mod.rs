//! Harness delivery and protocol boundaries.

mod claude;
mod codex;
mod codex_session;
mod connection;
mod oh_my_pi;
pub(crate) mod omp_session;
mod output;
mod session;
pub use codex_session::CodexSessionOptions;
pub use connection::{AttentionPolicy, HarnessConnection};
pub(crate) use connection::{HarnessDelivery, HarnessError};
pub(crate) use output::ProtocolOutput;
pub(crate) use session::ResidentAdmission;
pub use session::{
    HarnessSession, HarnessSessionError, HarnessSessionErrorKind, RunningHarnessSession,
};

mod selection;
pub(crate) use codex::TOOL_NAMES;
pub(crate) use selection::HarnessSelection;

pub(crate) mod presentation;

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod typed_delivery_tests;
