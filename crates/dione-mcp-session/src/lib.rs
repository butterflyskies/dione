//! Complete application MCP session, request authority and transport lifecycle.
#![warn(missing_docs)]
mod mcp;

pub use mcp::server::{
    DioneServer, ServerConstructionError, ServerConstructionErrorKind, run, run_resident_control,
};

#[cfg(test)]
const TEST_APPLICATION_VERSION: &str = "8.7.6-fixture";
#[cfg(test)]
mod codex_dispatch_tests;
#[cfg(test)]
mod codex_test_process;
#[cfg(test)]
mod codex_test_wire;
#[cfg(test)]
mod config_store_tests;
#[cfg(test)]
#[path = "discord_operations_tests/management.rs"]
mod discord_management_registration_tests;
#[cfg(test)]
#[path = "discord_operations_tests/reply.rs"]
mod discord_reply_registration_tests;
#[cfg(test)]
mod harness_test_support;
#[cfg(test)]
mod introspection_test_support;
#[cfg(test)]
mod protocol_integration;
#[cfg(test)]
mod teams_test_process;
#[cfg(test)]
static CONFIG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(test)]
fn config_test_guard() -> std::sync::MutexGuard<'static, ()> {
    CONFIG_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}
