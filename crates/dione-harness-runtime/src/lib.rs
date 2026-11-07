//! Concrete provider session preparation, startup, shutdown, and selection.
#![warn(missing_docs)]

mod selection;
mod session;
pub use selection::HarnessSelection;
pub use session::{
    HarnessSession, HarnessSessionError, HarnessSessionErrorKind, ResidentAdmission,
    RunningHarnessSession,
};

/// Process lifetime policy after the MCP input stream closes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StdioExitPolicy {
    /// Stop the process after its input stream closes.
    ShutdownProcess,
    /// Retain the resident until its cancellation token is signalled.
    DetachUntilCancellation,
}

#[cfg(test)]
use dione_harness::ProtocolOutput;

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod compact_tests;

#[cfg(test)]
mod presentation_single_tests;

#[cfg(test)]
mod typed_delivery_tests;

#[cfg(test)]
mod oh_my_pi_tests;

#[cfg(test)]
mod output_adapter_tests;

#[cfg(test)]
mod session_forwarding_tests;
