//! Harness delivery and protocol boundaries.

mod claude;
mod codex;
mod codex_session;
mod session;
pub use codex_session::CodexSessionOptions;
pub use dione_harness::{AttentionPolicy, HarnessConnection};
pub(crate) use dione_harness::{HarnessDelivery, HarnessError, ProtocolOutput};
pub(crate) use session::ResidentAdmission;
pub use session::{
    HarnessSession, HarnessSessionError, HarnessSessionErrorKind, RunningHarnessSession,
};

mod selection;
pub(crate) use codex::TOOL_NAMES;
pub(crate) use selection::HarnessSelection;

#[cfg(test)]
mod compact_tests;
#[cfg(test)]
mod presentation_single_tests;

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod typed_delivery_tests;

#[cfg(test)]
pub(crate) use claude::claude_with_output;
pub use claude::connect_claude;
pub use codex::connect_codex;
#[cfg(test)]
mod oh_my_pi_tests;

#[cfg(test)]
mod output_adapter_tests;
