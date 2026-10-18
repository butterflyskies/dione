//! MCP result projection for Discord management operations.

use crate::discord::{
    management::{CreatedThread, ManagementError, ManagementErrorKind},
    messaging_wire,
};
use serde_json::{Value, json};

fn error(error: ManagementError) -> Value {
    match error.kind {
        ManagementErrorKind::OutboundDenied => json!({"error":"channel not in allowlist"}),
        ManagementErrorKind::Target(cause) => messaging_wire::target_error(cause),
        ManagementErrorKind::Discord(cause) => json!({"error":cause.to_string()}),
    }
}

pub(crate) fn mutation(result: Result<(), ManagementError>) -> Value {
    match result {
        Ok(()) => json!({"ok":true}),
        Err(cause) => error(cause),
    }
}

pub(crate) fn thread(result: Result<CreatedThread, ManagementError>) -> Value {
    match result {
        Ok(thread) => json!({"ok":true,"thread_id":thread.id.to_string(),"name":thread.name}),
        Err(cause) => error(cause),
    }
}
