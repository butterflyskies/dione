//! Discord-owned history, attachment, and search tool boundary.
use super::{
    ids::Snowflake,
    messaging::{
        queries,
        search::{self, SearchPreparationError, SearchRequest},
    },
    query_wire,
    reply_tool::ReplyServices,
    search_wire::{self, SearchInputErrorKind},
    tool_arguments::{ArgumentError, parse_id},
};
use crate::{
    config::load_config,
    core::ModuleInstanceId,
    mcp::{
        protocol::tool,
        tool_registry::{
            McpTool, RegisteredTool, ToolCallError, ToolCallErrorKind, ToolDefinition, ToolName,
            ToolNameError,
        },
    },
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use serenity::model::id::{ChannelId, MessageId};
use std::{num::ParseIntError, sync::Arc};
use thiserror::Error;

#[derive(Clone, Copy)]
enum QueryTool {
    Messages,
    NewSince,
    Pins,
    Download,
    Message,
    Search,
}
impl QueryTool {
    const ALL: [Self; 6] = [
        Self::Messages,
        Self::NewSince,
        Self::Pins,
        Self::Download,
        Self::Message,
        Self::Search,
    ];
    fn name(self) -> &'static str {
        match self {
            Self::Messages => "fetch_messages",
            Self::NewSince => "fetch_new_since",
            Self::Pins => "fetch_pins",
            Self::Download => "download_attachment",
            Self::Message => "get_message",
            Self::Search => "search_messages",
        }
    }
    fn specification(self) -> (&'static str, Value) {
        match self {
            Self::Messages => (
                "Fetch messages from a channel. Returns messages oldest-first. Without cursors, returns the most recent messages. With before/after, paginates through history; use the first returned id as the next before cursor (backward) or the last returned id as the next after cursor (forward). before and after are mutually exclusive. count and has_more (a hint, not a guarantee) are included when a cursor is provided.",
                json!({
                    "type": "object",
                    "required": ["channel_id"],
                    "properties": {
                        "response_version": { "type": "integer", "enum": [1, 2], "default": 1, "description": "Version 2 includes source-qualified context, native metadata, and explicit availability." },
                        "channel_id": { "type": "string" },
                        "before": { "type": "string", "description": "Discord message ID (snowflake); return messages older than this ID. Mutually exclusive with after." },
                        "after": { "type": "string", "description": "Discord message ID (snowflake); return messages newer than this ID. Mutually exclusive with before." },
                        "limit": { "type": "integer", "default": 20, "maximum": 100 }
                    }
                }),
            ),
            Self::NewSince => (
                "Fetch only messages newer than a known message ID (cursor-based). Returns messages oldest-first with a count and a has_more pagination hint; the id of the last returned message is the next cursor.",
                json!({
                    "type": "object",
                    "required": ["channel_id", "after_message_id"],
                    "properties": {
                        "channel_id": { "type": "string", "description": "Discord channel ID" },
                        "after_message_id": { "type": "string", "description": "Discord message ID (snowflake); only messages after this ID are returned" },
                        "limit": { "type": "integer", "default": 20, "maximum": 100 }
                    }
                }),
            ),
            Self::Pins => (
                "Fetch every pinned message visible in a channel, oldest-first. Pins are retrieved explicitly and are not injected into context automatically.",
                json!({
                    "type": "object",
                    "required": ["channel_id"],
                    "properties": {
                        "channel_id": { "type": "string", "description": "Discord channel ID" }
                    }
                }),
            ),
            Self::Download => (
                "Download all attachments from a message to the inbox",
                json!({
                    "type": "object",
                    "required": ["channel_id", "message_id"],
                    "properties": {
                        "channel_id": { "type": "string" },
                        "message_id": { "type": "string" }
                    }
                }),
            ),
            Self::Message => (
                "Retrieve a single message",
                json!({
                    "type": "object",
                    "required": ["channel_id", "message_id"],
                    "properties": {
                        "response_version": { "type": "integer", "enum": [1, 2], "default": 1, "description": "Version 2 includes source-qualified context, native metadata, and explicit availability." },
                        "channel_id": { "type": "string" },
                        "message_id": { "type": "string" }
                    }
                }),
            ),
            Self::Search => (
                "Search messages in a guild. At least one filter required. Results filtered to configured channels.",
                json!({
                    "type": "object",
                    "required": ["guild_id"],
                    "properties": {
                        "guild_id": { "type": "string", "description": "Discord guild (server) ID to search" },
                        "content": { "type": "string", "description": "Text content to search for" },
                        "author_id": {
                            "description": "Filter by message author ID(s). Single string/number or array.",
                            "oneOf": [
                                { "type": "string" },
                                { "type": "array", "items": { "type": "string" } }
                            ]
                        },
                        "mentions": {
                            "description": "Filter by mentioned user ID(s). Single string/number or array.",
                            "oneOf": [
                                { "type": "string" },
                                { "type": "array", "items": { "type": "string" } }
                            ]
                        },
                        "has": {
                            "type": "array",
                            "items": { "type": "string", "enum": ["link", "embed", "file", "video", "image", "sound", "sticker"] },
                            "description": "Filter by attachment type"
                        },
                        "before": { "type": "string", "description": "ISO 8601 date (YYYY-MM-DD); return messages before this date" },
                        "after": { "type": "string", "description": "ISO 8601 date (YYYY-MM-DD); return messages after this date" },
                        "channel_id": {
                            "description": "Filter to specific channel ID(s). Single string/number or array.",
                            "oneOf": [
                                { "type": "string" },
                                { "type": "array", "items": { "type": "string" } }
                            ]
                        },
                        "pinned": { "type": "boolean", "description": "Filter to pinned (true) or unpinned (false) messages" },
                        "sort_by": { "type": "string", "enum": ["timestamp", "relevance"], "description": "Sort field (default: timestamp)" },
                        "sort_order": { "type": "string", "enum": ["asc", "desc"], "description": "Sort direction (default: desc)" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 25, "default": 25, "description": "Results per page (1-25)" },
                        "offset": { "type": "integer", "minimum": 0, "maximum": 9975, "default": 0, "description": "Pagination offset (0-9975)" }
                    }
                }),
            ),
        }
    }
}

#[derive(Debug, Error)]
#[error("invalid {field}: {kind}")]
struct CursorError {
    field: &'static str,
    #[source]
    kind: CursorErrorKind,
}
#[derive(Debug, Error)]
enum CursorErrorKind {
    #[error("null is not a valid snowflake; omit the field instead")]
    Null,
    #[error("empty string is not a valid snowflake")]
    Empty,
    #[error("not a valid u64")]
    Decimal(#[source] ParseIntError),
    #[error("must be a nonzero Discord snowflake")]
    Zero,
    #[error("expected a Discord snowflake (string or integer), got unexpected type")]
    Type,
}
fn parse_strict_optional_id(
    args: &Value,
    field: &'static str,
) -> Result<Option<Snowflake>, CursorError> {
    let number = match args.get(field) {
        None => return Ok(None),
        Some(Value::Null) => {
            return Err(CursorError {
                field,
                kind: CursorErrorKind::Null,
            });
        }
        Some(Value::String(value)) if value.is_empty() => {
            return Err(CursorError {
                field,
                kind: CursorErrorKind::Empty,
            });
        }
        Some(Value::String(value)) => value.parse::<u64>().map_err(|source| CursorError {
            field,
            kind: CursorErrorKind::Decimal(source),
        })?,
        Some(value) => value.as_u64().ok_or(CursorError {
            field,
            kind: CursorErrorKind::Type,
        })?,
    };
    Snowflake::new(number).map(Some).ok_or(CursorError {
        field,
        kind: CursorErrorKind::Zero,
    })
}
fn parse_limit(args: &Value, default: u8) -> u8 {
    args.get("limit")
        .and_then(Value::as_u64)
        .map(|value| value.clamp(1, 100) as u8)
        .unwrap_or(default)
}
#[derive(Debug, Error)]
#[error("{kind}")]
struct QueryInputError {
    #[source]
    kind: QueryInputErrorKind,
}
#[derive(Debug, Error)]
enum QueryInputErrorKind {
    #[error("response_version must be the integer 1 or 2")]
    ResponseVersion,
    #[error("{0}")]
    Argument(#[source] ArgumentError),
    #[error("{0}")]
    Cursor(#[source] CursorError),
    #[error("invalid search params: {}", .0.inner())]
    Search(#[source] serde_path_to_error::Error<serde_json::Error>),
}
impl From<ArgumentError> for QueryInputError {
    fn from(source: ArgumentError) -> Self {
        Self {
            kind: QueryInputErrorKind::Argument(source),
        }
    }
}
impl From<CursorError> for QueryInputError {
    fn from(source: CursorError) -> Self {
        Self {
            kind: QueryInputErrorKind::Cursor(source),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponseVersion {
    Legacy,
    ContextV2,
}
impl ResponseVersion {
    fn parse(args: &Value) -> Result<Self, QueryInputError> {
        match args.get("response_version") {
            None => Ok(Self::Legacy),
            Some(value) => match value.as_u64() {
                Some(1) => Ok(Self::Legacy),
                Some(2) => Ok(Self::ContextV2),
                _ => Err(QueryInputError {
                    kind: QueryInputErrorKind::ResponseVersion,
                }),
            },
        }
    }
}
enum QueryRequest {
    Messages {
        version: ResponseVersion,
        channel: ChannelId,
        before: Option<MessageId>,
        after: Option<MessageId>,
        limit: u8,
    },
    NewSince {
        channel: ChannelId,
        after: MessageId,
        limit: u8,
    },
    Pins(ChannelId),
    Download {
        channel: ChannelId,
        message: MessageId,
    },
    Message {
        version: ResponseVersion,
        channel: ChannelId,
        message: MessageId,
    },
    Search(SearchRequest),
    SearchRejected(SearchPreparationError),
}
impl QueryTool {
    fn parse(self, args: &Value) -> Result<QueryRequest, QueryInputError> {
        Ok(match self {
            Self::Messages => QueryRequest::Messages {
                version: ResponseVersion::parse(args)?,
                channel: parse_id(args, "channel_id")?.channel(),
                before: parse_strict_optional_id(args, "before")?.map(Snowflake::message),
                after: parse_strict_optional_id(args, "after")?.map(Snowflake::message),
                limit: parse_limit(args, 20),
            },
            Self::NewSince => QueryRequest::NewSince {
                channel: parse_id(args, "channel_id")?.channel(),
                after: parse_id(args, "after_message_id")?.message(),
                limit: parse_limit(args, 20),
            },
            Self::Pins => QueryRequest::Pins(parse_id(args, "channel_id")?.channel()),
            Self::Download => QueryRequest::Download {
                channel: parse_id(args, "channel_id")?.channel(),
                message: parse_id(args, "message_id")?.message(),
            },
            Self::Message => QueryRequest::Message {
                version: ResponseVersion::parse(args)?,
                channel: parse_id(args, "channel_id")?.channel(),
                message: parse_id(args, "message_id")?.message(),
            },
            Self::Search => {
                match search_wire::parse_request(parse_id(args, "guild_id")?.guild(), args) {
                    Ok(request) => QueryRequest::Search(request),
                    Err(error) => match error.kind {
                        SearchInputErrorKind::Preparation(error) => {
                            QueryRequest::SearchRejected(error)
                        }
                        SearchInputErrorKind::Deserialize(source) => {
                            return Err(QueryInputError {
                                kind: QueryInputErrorKind::Search(source),
                            });
                        }
                    },
                }
            }
        })
    }
}
struct QueryHandler {
    source: ModuleInstanceId,
    operation: QueryTool,
    services: Arc<ReplyServices>,
    search: Arc<search::SearchTransport>,
}
/// Registers the six Discord query tools and their resident access policy.
pub(crate) fn registrations(
    services: Arc<ReplyServices>,
    source: ModuleInstanceId,
) -> Result<Vec<RegisteredTool>, ToolNameError> {
    registrations_with_search(services, source, search::SearchTransport::default())
}
fn registrations_with_search(
    services: Arc<ReplyServices>,
    source: ModuleInstanceId,
    search: search::SearchTransport,
) -> Result<Vec<RegisteredTool>, ToolNameError> {
    let search = Arc::new(search);
    QueryTool::ALL
        .into_iter()
        .map(|operation| {
            let (description, schema) = operation.specification();
            Ok(RegisteredTool::new(
                ToolDefinition::new(ToolName::new(operation.name())?, description, schema),
                Arc::new(QueryHandler {
                    source: source.clone(),
                    operation,
                    services: services.clone(),
                    search: search.clone(),
                }),
            )
            .with_resident_access())
        })
        .collect()
}
impl McpTool for QueryHandler {
    fn call<'a>(&'a self, arguments: &'a Value) -> BoxFuture<'a, Result<Value, ToolCallError>> {
        Box::pin(async move {
            let request = self
                .operation
                .parse(arguments)
                .map_err(|source| ToolCallError {
                    kind: ToolCallErrorKind::OwnerInput(Box::new(source)),
                })?;
            let ctx = self
                .services
                .messaging_ctx(load_config(self.services.state_dir()));
            Ok(match request {
                QueryRequest::Messages {
                    version,
                    channel,
                    before,
                    after,
                    limit,
                } => {
                    let result = queries::fetch_messages(&ctx, channel, before, after, limit).await;
                    match version {
                        ResponseVersion::Legacy => query_wire::fetched(result),
                        ResponseVersion::ContextV2 => query_wire::fetched_v2(result, &self.source),
                    }
                }
                QueryRequest::NewSince {
                    channel,
                    after,
                    limit,
                } => query_wire::new_since(
                    queries::fetch_new_since(&ctx, channel, after, limit).await,
                ),
                QueryRequest::Pins(channel) => {
                    query_wire::pins(queries::fetch_pins(&ctx, channel).await)
                }
                QueryRequest::Download { channel, message } => query_wire::downloaded(
                    queries::download_attachment(&ctx, channel, message).await,
                ),
                QueryRequest::Message {
                    version,
                    channel,
                    message,
                } => {
                    let result = queries::get_message(&ctx, channel, message).await;
                    match version {
                        ResponseVersion::Legacy => query_wire::retrieved(result),
                        ResponseVersion::ContextV2 => {
                            query_wire::retrieved_v2(result, &self.source)
                        }
                    }
                }
                QueryRequest::Search(request) => {
                    search_wire::result(self.search.search_messages(&ctx, request).await)
                }
                QueryRequest::SearchRejected(error) => search_wire::invalid_request(error),
            })
        })
    }
}
/// Exposes owner definitions for protocol contract fixtures.
pub(crate) fn definitions() -> impl Iterator<Item = Value> {
    QueryTool::ALL.into_iter().map(|operation| {
        let (description, schema) = operation.specification();
        tool(operation.name(), description, schema)
    })
}
#[cfg(test)]
#[path = "query_tools_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "query_context_tests.rs"]
mod context_tests;
