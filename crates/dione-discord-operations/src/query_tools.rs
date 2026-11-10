//! Discord-owned history, attachment, and search tool boundary.
use super::reply_tool::ReplyServices;
use dione_discord_queries::{queries, search, search_wire, wire as query_wire};
use dione_discord_tool_contracts::query::{QueryRequest, QueryTool, ResponseVersion};

use dione_config::load_config;
use dione_contracts::ModuleInstanceId;
use dione_mcp_tools::McpTool;
use dione_mcp_tools::RegisteredTool;
use dione_mcp_tools::ToolCallError;
use dione_mcp_tools::ToolCallErrorKind;
use dione_mcp_tools::ToolDefinition;
use dione_mcp_tools::ToolName;
use dione_mcp_tools::ToolNameError;
use futures_util::future::BoxFuture;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use std::sync::Arc;

struct QueryHandler {
    source: ModuleInstanceId,
    operation: QueryTool,
    services: Arc<ReplyServices>,
    search: Arc<search::SearchTransport>,
}
/// Registers the six Discord query tools and their resident access policy.
pub fn registrations(
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
                .query_ctx(load_config(self.services.state_dir()));
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
#[cfg(test)]
#[path = "query_tools_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "query_context_tests.rs"]
mod context_tests;
