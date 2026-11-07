use crate::HarnessError;
use futures_util::future::BoxFuture;
use serde_json::Value;

/// Protocol capabilities and tool hooks supplied by the same delivery adapter.
pub trait HarnessProtocol: Send + Sync {
    /// Notify the adapter that its stdio transport initialized.
    fn stdio_initialized(&self) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async { Ok(()) })
    }
    /// Notify the adapter that its stdio transport disconnected.
    fn stdio_disconnected(&self) -> BoxFuture<'_, Result<(), HarnessError>> {
        Box::pin(async { Ok(()) })
    }
    /// Handle application-private control after the caller authorizes its origin.
    /// The default returns None, meaning this adapter did not handle the request.
    fn private_control<'a>(
        &'a self,
        _name: &'a str,
        _args: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, HarnessError>>> {
        Box::pin(async { None })
    }
    /// Return the adapter's advertised protocol capabilities.
    fn capabilities(&self) -> Value;
    /// Return ordinary adapter tool declarations for application registration.
    fn tools(&self) -> Vec<Value>;
    /// Report whether this adapter supports resident-control routing.
    fn resident_control(&self) -> bool;
    /// Report whether this adapter supports permission relay.
    fn permission_relay(&self) -> bool;
    /// Handle an application-dispatched ordinary tool, or return None if unhandled.
    fn call<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, HarnessError>>>;
}
