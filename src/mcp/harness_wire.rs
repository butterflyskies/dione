//! Harness-owned MCP capabilities and tool extensions.
use crate::harness::HarnessError;
use futures_util::future::BoxFuture;
use serde_json::Value;
pub(crate) const PRIVATE_CONTROL_TOOLS: &[&str] = &[
    "teams_bind_session",
    "teams_check_session",
    "teams_recover_session",
    "teams_unbind_session",
    "teams_ack_event",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RequestOrigin {
    Stdio,
    ResidentControl,
}

pub(crate) trait HarnessProtocol: Send + Sync {
    fn stdio_initialized(&self) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async { Ok(()) })
    }
    fn stdio_disconnected(&self) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async { Ok(()) })
    }
    fn private_control<'a>(
        &'a self,
        _name: &'a str,
        _args: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, HarnessError>>> {
        Box::pin(async { None })
    }
    fn capabilities(&self) -> Value;
    fn tools(&self) -> Vec<Value>;
    fn resident_control(&self) -> bool;
    fn permission_relay(&self) -> bool;
    fn call<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, HarnessError>>>;
}
