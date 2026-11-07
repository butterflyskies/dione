//! Harness delivery and protocol boundaries.

mod claude;
mod codex;
mod codex_session;
mod oh_my_pi;
pub(crate) mod omp_session;
mod session;
pub use codex_session::CodexSessionOptions;
pub(crate) use dione_harness::ProtocolOutput;
pub use dione_harness::{AttentionPolicy, HarnessConnection};
pub(crate) use dione_harness::{HarnessDelivery, HarnessError};
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

#[cfg(test)]
pub(crate) use claude::claude_with_output;
pub use claude::connect_claude;
pub use codex::connect_codex;
pub use oh_my_pi::connect_oh_my_pi;
pub(crate) use oh_my_pi::oh_my_pi_with_control;
#[cfg(test)]
pub(crate) use oh_my_pi::{oh_my_pi_with_control_and_output, oh_my_pi_with_output};

#[cfg(test)]
mod output_adapter_tests;
