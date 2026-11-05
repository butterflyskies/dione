//! Harness delivery and protocol boundaries.

mod session;
pub use dione_codex::CodexSessionOptions;
pub(crate) use dione_harness::ProtocolOutput;
pub use dione_harness::{AttentionPolicy, HarnessConnection};
pub(crate) use session::ResidentAdmission;
pub use session::{
    HarnessSession, HarnessSessionError, HarnessSessionErrorKind, RunningHarnessSession,
};

mod selection;
pub(crate) use dione_codex::TOOL_NAMES;
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
mod oh_my_pi_tests;

#[cfg(test)]
mod output_adapter_tests;
