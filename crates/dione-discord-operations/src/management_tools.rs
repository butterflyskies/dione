//! Discord-owned message management registration and request boundary.
use dione_discord_messaging_contracts::management::wire as management_wire;
use dione_discord_tool_contracts::management::{ManagementRequest, ManagementTool};

use super::{
    management::{ManagementCtx, create_thread, mutate_message},
    reply_tool::ReplyServices,
};
use dione_config::load_config;
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

struct RegisteredManagementTool {
    operation: ManagementTool,
    services: Arc<ReplyServices>,
}

/// Registers message mutation and thread tools with their resident authority.
pub fn registrations(services: Arc<ReplyServices>) -> Result<Vec<RegisteredTool>, ToolNameError> {
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
