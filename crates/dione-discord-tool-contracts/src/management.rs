//! Management operation metadata and request decoding; execution stays in the adapter.
use dione_discord_identities::Snowflake;
use dione_discord_tool_arguments::{ArgumentError, ArgumentErrorKind, parse_id, parse_optional_id};
use serde_json::{Value, json};
use serenity::model::id::{ChannelId, MessageId};
#[derive(Clone, Copy, Debug)]
/// Descriptive MessageAction metadata; construction grants no execution authority.
pub enum MessageAction {
    /// Describes the Pin operation or request shape.
    Pin,
    /// Describes the Unpin operation or request shape.
    Unpin,
    /// Describes the Delete operation or request shape.
    Delete,
}

#[derive(Clone, Copy, Debug)]
/// Descriptive MessageCommand metadata; construction grants no execution authority.
pub struct MessageCommand {
    /// The decoded channel.
    pub channel: ChannelId,
    /// The decoded message.
    pub message: MessageId,
    /// The decoded action.
    pub action: MessageAction,
}

#[derive(Clone, Copy)]
/// Descriptive ManagementTool metadata; construction grants no execution authority.
pub enum ManagementTool {
    /// Describes the Pin operation or request shape.
    Pin,
    /// Describes the Unpin operation or request shape.
    Unpin,
    /// Describes the CreateThread operation or request shape.
    CreateThread,
    /// Describes the Delete operation or request shape.
    Delete,
}

impl ManagementTool {
    /// Every operation in the existing discovery order.
    pub const ALL: [Self; 4] = [Self::Pin, Self::Unpin, Self::CreateThread, Self::Delete];
    /// Returns the existing MCP tool name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Pin => "pin_message",
            Self::Unpin => "unpin_message",
            Self::CreateThread => "create_thread",
            Self::Delete => "delete_message",
        }
    }
    /// Returns the existing discovery description.
    pub fn description(self) -> &'static str {
        match self {
            Self::Pin => "Pin a message in a channel",
            Self::Unpin => "Unpin a message",
            Self::CreateThread => "Create a thread",
            Self::Delete => "Delete a message",
        }
    }
    /// Returns the existing argument schema.
    pub fn schema(self) -> Value {
        match self {
            Self::CreateThread => {
                json!({"type":"object", "required":["channel_id","name"], "properties":{"channel_id":{"type":"string"},"message_id":{"type":"string","description":"If set, creates a thread from this message"},"name":{"type":"string"}}})
            }
            Self::Pin | Self::Unpin | Self::Delete => {
                json!({"type":"object", "required":["channel_id","message_id"], "properties":{"channel_id":{"type":"string"},"message_id":{"type":"string"}}})
            }
        }
    }
    /// Decodes metadata without checking access or executing an operation.
    pub fn parse(self, args: &Value) -> Result<ManagementRequest<'_>, ArgumentError> {
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

/// Descriptive ManagementRequest metadata; construction grants no execution authority.
pub enum ManagementRequest<'a> {
    /// Describes the Mutation operation or request shape.
    Mutation(MessageCommand),
    /// Requests a public thread, optionally from an existing message.
    Thread {
        /// The decoded channel.
        channel: ChannelId,
        /// The decoded message.
        message: Option<MessageId>,
        /// The decoded name.
        name: &'a str,
    },
}

/// Returns owner definitions to existing protocol contract fixtures.
/// Projects this catalog in its existing discovery order.
pub fn definitions() -> impl Iterator<Item = Value> {
    ManagementTool::ALL.into_iter().map(|operation| {
        crate::tool(
            operation.name(),
            operation.description(),
            operation.schema(),
        )
    })
}
