pub mod attention;
pub mod discord;
#[cfg(test)]
mod discord_bells_tests;
pub mod ingress_ledger;
pub mod mcp;
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

/// Explicit application composition adapters.
pub mod composition;

#[cfg(test)]
mod coordination_consumers;

#[cfg(test)]
static CONFIG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn config_test_guard() -> std::sync::MutexGuard<'static, ()> {
    CONFIG_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod teams_test_process;

#[cfg(test)]
mod teams_application_tests;

#[cfg(test)]
mod codex_dispatch_tests;
#[cfg(test)]
mod codex_test_process;
#[cfg(test)]
mod codex_test_wire;
