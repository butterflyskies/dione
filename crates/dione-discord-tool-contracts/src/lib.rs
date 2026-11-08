//! Descriptive catalogs and typed requests for twenty root-adapted Discord tools.
//! The embedding retains current policy, registration exposure and execution authority.
#![warn(missing_docs)]
/// Pin, unpin, delete and thread request descriptions.
pub mod management;
/// Metadata, visibility and audience request descriptions.
pub mod metadata;
/// History, attachment and search request descriptions.
pub mod query;
/// Reply presentation and borrowed argument decoding.
pub mod reply;

fn tool(name: &str, description: &str, input_schema: serde_json::Value) -> serde_json::Value {
    serde_json::json!(dione_mcp_tools::ToolDefinition::new(
        dione_mcp_tools::ToolName::new(name).expect("catalog literals use valid MCP tool names"),
        description,
        input_schema,
    ))
}
