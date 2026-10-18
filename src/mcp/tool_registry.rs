//! Immutable MCP adapter registrations, validated before requests are served.

use crate::{
    harness::{HarnessConnection, TOOL_NAMES},
    mcp::{harness_wire::PRIVATE_CONTROL_TOOLS, protocol::tools_list},
};
use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::Value;
use std::{
    borrow::Borrow,
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    sync::Arc,
};
use thiserror::Error;

/// A validated MCP tool name used to identify registration conflicts.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ToolName(Box<str>);

impl ToolName {
    pub(crate) fn new(name: &str) -> Result<Self, ToolNameError> {
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(ToolNameError {
                name: name.to_owned(),
            });
        }
        Ok(Self(name.into()))
    }
}
impl Borrow<str> for ToolName {
    fn borrow(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for ToolName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A module supplied a name outside the supported MCP name syntax.
#[derive(Debug, Error)]
#[error("invalid MCP tool name `{name}`")]
pub struct ToolNameError {
    name: String,
}

#[derive(Clone, Serialize)]
pub(crate) struct ToolDefinition {
    name: ToolName,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}
impl ToolDefinition {
    pub(crate) fn new(name: ToolName, description: impl Into<String>, input_schema: Value) -> Self {
        Self {
            name,
            description: description.into(),
            input_schema,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolExposure {
    SeatOnly,
    SeatAndResident,
}

#[derive(Clone, Copy)]
pub(crate) struct ToolDiscoveryContext {
    pub(crate) evidence_markers_enabled: bool,
}

pub(crate) trait McpTool: Send + Sync {
    fn present_definition(
        &self,
        definition: &ToolDefinition,
        _context: ToolDiscoveryContext,
    ) -> Value {
        serde_json::json!(definition)
    }
    fn call<'a>(&'a self, arguments: &'a Value) -> BoxFuture<'a, Result<Value, ToolCallError>>;
}

/// An owner-provided MCP definition, exposure policy, and execution handler.
///
/// Construct registrations through the platform module that owns their authority.
#[derive(Clone)]
pub struct RegisteredTool {
    definition: ToolDefinition,
    exposure: ToolExposure,
    handler: Arc<dyn McpTool>,
}
impl RegisteredTool {
    pub(crate) fn new(definition: ToolDefinition, handler: Arc<dyn McpTool>) -> Self {
        Self {
            definition,
            exposure: ToolExposure::SeatOnly,
            handler,
        }
    }
    pub(crate) fn with_resident_access(mut self) -> Self {
        self.exposure = ToolExposure::SeatAndResident;
        self
    }
    pub(crate) fn call<'a>(
        &'a self,
        arguments: &'a Value,
    ) -> BoxFuture<'a, Result<Value, ToolCallError>> {
        self.handler.call(arguments)
    }
}

#[derive(Debug, Error)]
#[error("{kind}")]
pub(crate) struct ToolCallError {
    #[source]
    pub(crate) kind: ToolCallErrorKind,
}
#[derive(Debug, Error)]
pub(crate) enum ToolCallErrorKind {
    #[error("{0}")]
    OwnerInput(#[source] Box<dyn Error + Send + Sync>),
    #[error("tool call failed: invalid arguments: {0}")]
    Arguments(#[source] Box<dyn Error + Send + Sync>),
    #[error("tool call failed: execution failed: {0}")]
    Execution(#[source] Box<dyn Error + Send + Sync>),
}

/// A module tool conflicts with another callable MCP tool.
#[derive(Debug, Error)]
#[error("cannot register tool `{name}`: {kind}")]
pub struct RegistrationError {
    /// The conflicting validated tool name.
    pub name: ToolName,
    /// The category of registration conflict.
    #[source]
    pub kind: RegistrationErrorKind,
}
/// The reason a module registration was rejected.
#[derive(Debug, Error)]
pub enum RegistrationErrorKind {
    /// Another module has already registered the same name.
    #[error("duplicate registration")]
    Duplicate,
    /// A built-in or harness tool owns the name.
    #[error("name is reserved by a built-in or harness tool")]
    Reserved,
}

#[derive(Clone, Default)]
pub(crate) struct ToolRegistry {
    tools: Arc<BTreeMap<ToolName, RegisteredTool>>,
}
impl ToolRegistry {
    pub(crate) fn new(
        harness: &HarnessConnection,
        registrations: Vec<RegisteredTool>,
    ) -> Result<Self, RegistrationError> {
        let definitions = tools_list(harness, true);
        let mut reserved: BTreeSet<&str> = TOOL_NAMES.iter().copied().collect();
        if let Some(tools) = definitions["tools"].as_array() {
            reserved.extend(tools.iter().filter_map(|tool| tool["name"].as_str()));
        }
        reserved.extend(PRIVATE_CONTROL_TOOLS.iter().copied());
        let mut tools = BTreeMap::new();
        for registration in registrations {
            let name = registration.definition.name.clone();
            let kind = if reserved.contains(name.0.as_ref()) {
                Some(RegistrationErrorKind::Reserved)
            } else if tools.contains_key(&name) {
                Some(RegistrationErrorKind::Duplicate)
            } else {
                None
            };
            if let Some(kind) = kind {
                return Err(RegistrationError { name, kind });
            }
            tools.insert(name, registration);
        }
        Ok(Self {
            tools: Arc::new(tools),
        })
    }
    pub(crate) fn find(&self, name: &str) -> Option<&RegisteredTool> {
        self.tools.get(name)
    }
    pub(crate) fn resident_allowed(&self, name: &str) -> bool {
        self.find(name)
            .is_some_and(|tool| tool.exposure == ToolExposure::SeatAndResident)
    }
    pub(crate) fn append_definitions(&self, response: &mut Value, context: ToolDiscoveryContext) {
        if let Some(tools) = response["tools"].as_array_mut() {
            tools.extend(
                self.tools
                    .values()
                    .map(|tool| tool.handler.present_definition(&tool.definition, context)),
            );
        }
    }
}
