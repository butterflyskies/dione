//! Discord-owned metadata tool definitions, parsing, and execution.
use dione_discord_tool_contracts::metadata::MetadataTool;

use super::reply_tool::ReplyServices;
use dione_config::load_config;
use dione_discord_introspection::{
    audience,
    introspection::{self, IntrospectionCtx, IntrospectionOperation},
    visibility_read, wire as introspection_wire,
};
use dione_mcp_tools::McpTool;
use dione_mcp_tools::RegisteredTool;
use dione_mcp_tools::ToolCallError;
use dione_mcp_tools::ToolCallErrorKind;
use dione_mcp_tools::ToolDefinition;
use dione_mcp_tools::ToolName;
use dione_mcp_tools::ToolNameError;
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::sync::Arc;

struct IntrospectionTool {
    operation: MetadataTool,
    services: Arc<ReplyServices>,
}

/// Builds the complete Discord metadata registrations with their resident policy.
pub(crate) fn registrations(
    services: Arc<ReplyServices>,
) -> Result<Vec<RegisteredTool>, ToolNameError> {
    MetadataTool::ALL
        .into_iter()
        .map(|operation| {
            Ok(RegisteredTool::new(
                ToolDefinition::new(
                    ToolName::new(operation.name())?,
                    operation.description(),
                    operation.schema(),
                ),
                Arc::new(IntrospectionTool {
                    operation,
                    services: services.clone(),
                }),
            )
            .with_resident_access())
        })
        .collect()
}

impl McpTool for IntrospectionTool {
    fn call<'a>(&'a self, arguments: &'a Value) -> BoxFuture<'a, Result<Value, ToolCallError>> {
        Box::pin(async move {
            let operation = self
                .operation
                .parse(arguments)
                .map_err(|source| ToolCallError {
                    kind: ToolCallErrorKind::OwnerInput(Box::new(source)),
                })?;
            let ctx = IntrospectionCtx::new(
                self.services.http().clone(),
                load_config(self.services.state_dir()),
            );
            Ok(match operation {
                IntrospectionOperation::Guilds => {
                    introspection_wire::guilds(introspection::list_guilds(&ctx).await)
                }
                IntrospectionOperation::Channels(guild) => {
                    introspection_wire::channels(introspection::list_channels(&ctx, guild).await)
                }
                IntrospectionOperation::Channel(channel) => {
                    introspection_wire::channel(introspection::get_channel(&ctx, channel).await)
                }
                IntrospectionOperation::User(user) => {
                    introspection_wire::user(introspection::get_user(&ctx, user).await)
                }
                IntrospectionOperation::Member { guild, user } => {
                    introspection_wire::member(introspection::get_member(&ctx, guild, user).await)
                }
                IntrospectionOperation::Roles(guild) => {
                    introspection_wire::roles(introspection::list_roles(&ctx, guild).await)
                }
                IntrospectionOperation::Emojis(guild) => {
                    introspection_wire::emojis(introspection::list_emojis(&ctx, guild).await)
                }
                IntrospectionOperation::Visibility(request) => introspection_wire::visibility(
                    visibility_read::can_view_channel(&ctx, &request).await,
                ),
                IntrospectionOperation::Audience(channel) => introspection_wire::audience(
                    audience::channel_effective_audience(&ctx, channel).await,
                ),
            })
        })
    }
}
