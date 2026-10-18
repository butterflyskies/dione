//! Discord-owned message management registration and request boundary.

use super::{
    ids::Snowflake,
    management::{ManagementCtx, MessageAction, MessageCommand, create_thread, mutate_message},
    management_wire,
    reply_tool::ReplyServices,
    tool_arguments::{ArgumentError, ArgumentErrorKind, parse_id, parse_optional_id},
};
use crate::{
    config::load_config,
    mcp::tool_registry::{
        McpTool, RegisteredTool, ToolCallError, ToolCallErrorKind, ToolDefinition, ToolName,
        ToolNameError,
    },
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use serenity::model::id::{ChannelId, MessageId};
use std::sync::Arc;

#[derive(Clone, Copy)]
enum ManagementTool {
    Pin,
    Unpin,
    CreateThread,
    Delete,
}

impl ManagementTool {
    const ALL: [Self; 4] = [Self::Pin, Self::Unpin, Self::CreateThread, Self::Delete];
    fn name(self) -> &'static str {
        match self {
            Self::Pin => "pin_message",
            Self::Unpin => "unpin_message",
            Self::CreateThread => "create_thread",
            Self::Delete => "delete_message",
        }
    }
    fn description(self) -> &'static str {
        match self {
            Self::Pin => "Pin a message in a channel",
            Self::Unpin => "Unpin a message",
            Self::CreateThread => "Create a thread",
            Self::Delete => "Delete a message",
        }
    }
    fn schema(self) -> Value {
        match self {
            Self::CreateThread => {
                json!({"type":"object", "required":["channel_id","name"], "properties":{"channel_id":{"type":"string"},"message_id":{"type":"string","description":"If set, creates a thread from this message"},"name":{"type":"string"}}})
            }
            Self::Pin | Self::Unpin | Self::Delete => {
                json!({"type":"object", "required":["channel_id","message_id"], "properties":{"channel_id":{"type":"string"},"message_id":{"type":"string"}}})
            }
        }
    }
    fn parse(self, args: &Value) -> Result<ManagementRequest<'_>, ArgumentError> {
        let channel = parse_id(args, "channel_id")?.channel();
        let action = match self {
            Self::Pin => MessageAction::Pin,
            Self::Unpin => MessageAction::Unpin,
            Self::Delete => MessageAction::Delete,
            Self::CreateThread => {
                let message = parse_optional_id(args, "message_id")?.map(Snowflake::message);
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or(ArgumentErrorKind::Missing("name"))?;
                return Ok(ManagementRequest::Thread {
                    channel,
                    message,
                    name,
                });
            }
        };
        Ok(ManagementRequest::Mutation(MessageCommand {
            channel,
            message: parse_id(args, "message_id")?.message(),
            action,
        }))
    }
}

enum ManagementRequest<'a> {
    Mutation(MessageCommand),
    Thread {
        channel: ChannelId,
        message: Option<MessageId>,
        name: &'a str,
    },
}

struct RegisteredManagementTool {
    operation: ManagementTool,
    services: Arc<ReplyServices>,
}

/// Registers message mutation and thread tools with their resident authority.
pub(crate) fn registrations(
    services: Arc<ReplyServices>,
) -> Result<Vec<RegisteredTool>, ToolNameError> {
    ManagementTool::ALL
        .into_iter()
        .map(|operation| {
            Ok(RegisteredTool::new(
                ToolDefinition::new(
                    ToolName::new(operation.name())?,
                    operation.description(),
                    operation.schema(),
                ),
                Arc::new(RegisteredManagementTool {
                    operation,
                    services: services.clone(),
                }),
            )
            .with_resident_access())
        })
        .collect()
}

impl McpTool for RegisteredManagementTool {
    fn call<'a>(&'a self, arguments: &'a Value) -> BoxFuture<'a, Result<Value, ToolCallError>> {
        Box::pin(async move {
            let request = self
                .operation
                .parse(arguments)
                .map_err(|source| ToolCallError {
                    kind: ToolCallErrorKind::OwnerInput(Box::new(source)),
                })?;
            let ctx = ManagementCtx {
                http: self.services.http().clone(),
                state: self.services.state().clone(),
                config: load_config(self.services.state_dir()),
                ingress_ledger: self.services.ingress_ledger().clone(),
            };
            Ok(match request {
                ManagementRequest::Mutation(command) => {
                    management_wire::mutation(mutate_message(&ctx, command).await)
                }
                ManagementRequest::Thread {
                    channel,
                    message,
                    name,
                } => management_wire::thread(create_thread(&ctx, channel, message, name).await),
            })
        })
    }
}

/// Returns owner definitions to existing protocol contract fixtures.
pub(crate) fn definitions() -> impl Iterator<Item = Value> {
    ManagementTool::ALL.into_iter().map(|operation| {
        crate::mcp::protocol::tool(
            operation.name(),
            operation.description(),
            operation.schema(),
        )
    })
}
