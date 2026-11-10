//! Recipient-local Attention execution and complete notification delivery lifecycle.
#![warn(missing_docs)]
mod control;
mod forwarding;
mod runtime;
mod store_access;
pub use control::execute;
pub use forwarding::run_notifications;
pub use runtime::{AttentionRuntime, AttentionServices};

#[cfg(test)]
static CONFIG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(test)]
fn config_test_guard() -> std::sync::MutexGuard<'static, ()> {
    CONFIG_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}
#[cfg(test)]
mod bell_test_support;
#[cfg(test)]
mod codex_test_process;
#[cfg(test)]
mod codex_test_wire;
#[cfg(test)]
mod gateway_test_support;
#[cfg(test)]
mod harness_test_support;
