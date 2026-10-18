//! Teams-owned MCP parsing and receipt projection for single-use reply authority.

use super::{ReplyHandle, runtime::TeamsReplyAuthority};
use crate::mcp::tool_registry::{
    McpTool, RegisteredTool, ToolCallError, ToolCallErrorKind, ToolDefinition, ToolName,
    ToolNameError,
};
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use thiserror::Error;

/// Builds MCP tools backed by the supplied event-bound Teams reply authority.
/// An absent authority contributes no tools.
pub fn mcp_tools(
    authority: Option<Arc<dyn TeamsReplyAuthority>>,
) -> Result<Vec<RegisteredTool>, ToolNameError> {
    let Some(authority) = authority else {
        return Ok(Vec::new());
    };
    let definition = ToolDefinition::new(
        ToolName::new("teams_reply")?,
        "Reply once to an authenticated Microsoft Teams Activity using the opaque single-use authority handle delivered with that event.",
        json!({
            "type": "object",
            "required": ["reply_handle", "content"],
            "properties": {
                "reply_handle": { "type": "string", "description": "Opaque single-use reply authority from a Teams inbound event" },
                "content": { "type": "string", "description": "Reply text" }
            }
        }),
    );
    Ok(vec![
        RegisteredTool::new(definition, Arc::new(TeamsReplyTool { authority }))
            .with_resident_access(),
    ])
}

struct TeamsReplyTool {
    authority: Arc<dyn TeamsReplyAuthority>,
}

#[derive(Deserialize)]
struct ReplyArguments<'a> {
    reply_handle: &'a str,
    content: &'a str,
}

#[derive(Debug, Error)]
#[error("expected an object")]
struct ExpectedReplyArgumentsObject;

impl McpTool for TeamsReplyTool {
    fn call<'a>(&'a self, arguments: &'a Value) -> BoxFuture<'a, Result<Value, ToolCallError>> {
        Box::pin(async move {
            if !arguments.is_object() {
                return Err(ToolCallError {
                    kind: ToolCallErrorKind::Arguments(Box::new(ExpectedReplyArgumentsObject)),
                });
            }
            let request: ReplyArguments<'_> =
                serde_path_to_error::deserialize(arguments).map_err(|error| ToolCallError {
                    kind: ToolCallErrorKind::Arguments(Box::new(error)),
                })?;
            let handle =
                ReplyHandle::parse(request.reply_handle).map_err(|error| ToolCallError {
                    kind: ToolCallErrorKind::Arguments(Box::new(error)),
                })?;
            let receipt = self
                .authority
                .reply(&handle, request.content)
                .await
                .map_err(|error| ToolCallError {
                    kind: ToolCallErrorKind::Execution(Box::new(error)),
                })?;
            Ok(json!({
                "incoming_activity_id": receipt.incoming_activity_id,
                "outgoing_activity_id": receipt.outgoing_activity_id,
                "conversation_id": receipt.conversation_id,
            }))
        })
    }
}
