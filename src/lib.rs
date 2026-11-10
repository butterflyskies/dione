#[cfg(test)]
mod discord_bells_tests;

#[expect(
    dead_code,
    reason = "standalone receipt-gate fixture is runtime-wired in a follow-up PR"
)]
pub(crate) mod receipt_gate;

/// Agent harness integration.
#[cfg(test)]
mod harness_test_support;

/// Process composition and offline operator commands.
pub mod startup;

#[cfg(test)]
mod coordination_consumers;

#[cfg(test)]
mod teams_test_process;

#[cfg(test)]
mod teams_application_tests;

#[cfg(test)]
mod codex_test_process;
#[cfg(test)]
mod codex_test_wire;

#[cfg(test)]
mod mcp_session_tests;
