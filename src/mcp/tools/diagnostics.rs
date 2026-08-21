//! Diagnostics tools: runtime trace-level control and version info.

use crate::tracing_channel::TraceLevelController;
use serde_json::{Value, json};

/// Context for diagnostics tools.
pub struct DiagnosticsCtx<'a> {
    pub trace_controller: &'a TraceLevelController,
}

/// Set the trace filter for the channel-forwarding layer.
///
/// Accepts a tracing-subscriber `EnvFilter` string (e.g. "dione=debug",
/// "dione::discord=trace", "off"). The channel layer forwards matching events
/// as channel notifications with `type="trace"` metadata.
pub async fn set_trace_level(ctx: &DiagnosticsCtx<'_>, filter: &str) -> Value {
    match ctx.trace_controller.set_channel_filter(filter) {
        Ok(()) => json!({
            "ok": true,
            "channel_filter": filter,
        }),
        Err(e) => json!({ "error": e }),
    }
}

/// Set the trace filter for the stderr logging layer.
pub async fn set_stderr_level(ctx: &DiagnosticsCtx<'_>, filter: &str) -> Value {
    match ctx.trace_controller.set_stderr_filter(filter) {
        Ok(()) => json!({
            "ok": true,
            "stderr_filter": filter,
        }),
        Err(e) => json!({ "error": e }),
    }
}

/// Returns the dione version (from Cargo.toml at compile time).
pub async fn get_version() -> Value {
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "name": "dione",
    })
}
