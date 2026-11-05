//! Immutable MCP registrations and owner-provided tool contracts.

//! Validated representations and registry storage cannot be constructed directly.
//!
//! ```compile_fail
//! use dione_mcp_tools::ToolName;
//! let name = ToolName("unchecked".into());
//! ```
//!
//! ```compile_fail
//! use dione_mcp_tools::{ToolDefinition, ToolName};
//! let definition = ToolDefinition { name: ToolName::new("valid").unwrap(), description: "text".into(), input_schema: serde_json::json!({}) };
//! ```
//!
//! ```compile_fail
//! use dione_mcp_tools::RegisteredTool;
//! fn forge(registration: RegisteredTool) { let _ = registration.handler; }
//! ```
//!
//! ```compile_fail
//! use dione_mcp_tools::ToolRegistry;
//! let registry = ToolRegistry::default();
//! let storage = registry.tools;
//! ```
//!
//! ```compile_fail
//! use dione_mcp_tools::ToolName;
//! let name: ToolName = serde_json::from_str("\"unchecked\"").unwrap();
//! ```

#![warn(missing_docs)]

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
    /// Validates a nonempty ASCII tool name of at most 128 bytes.
    pub fn new(name: &str) -> Result<Self, ToolNameError> {
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

/// A validated name and its owner-provided MCP discovery description.
#[derive(Clone, Serialize)]
pub struct ToolDefinition {
    name: ToolName,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}
impl ToolDefinition {
    /// Builds discovery metadata using an already validated name.
    pub fn new(name: ToolName, description: impl Into<String>, input_schema: Value) -> Self {
        Self {
            name,
            description: description.into(),
            input_schema,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolExposure {
    SeatOnly,
    SeatAndResident,
}

/// Current presentation settings supplied for each discovery request.
#[derive(Clone, Copy)]
pub struct ToolDiscoveryContext {
    /// Whether the current caller enables evidence-marker presentation.
    pub evidence_markers_enabled: bool,
}

/// Owner-provided discovery presentation and borrowed asynchronous execution.
pub trait McpTool: Send + Sync {
    /// Presents this definition using the current request context.
    fn present_definition(
        &self,
        definition: &ToolDefinition,
        _context: ToolDiscoveryContext,
    ) -> Value {
        serde_json::json!(definition)
    }
    /// Calls the owner while borrowing the handler and argument value.
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
    /// Registers an owner handler with seat-only access by default.
    pub fn new(definition: ToolDefinition, handler: Arc<dyn McpTool>) -> Self {
        Self {
            definition,
            exposure: ToolExposure::SeatOnly,
            handler,
        }
    }
    /// Explicitly permits resident routing to this registration.
    pub fn with_resident_access(mut self) -> Self {
        self.exposure = ToolExposure::SeatAndResident;
        self
    }
    /// Calls the registered owner while retaining its typed failure.
    pub fn call<'a>(&'a self, arguments: &'a Value) -> BoxFuture<'a, Result<Value, ToolCallError>> {
        self.handler.call(arguments)
    }
}

/// A typed owner failure retained until the protocol presentation boundary.
#[derive(Debug, Error)]
#[error("{kind}")]
pub struct ToolCallError {
    #[source]
    /// The failure category and its original cause.
    pub kind: ToolCallErrorKind,
}
/// The category of an owner-provided tool failure.
#[derive(Debug, Error)]
pub enum ToolCallErrorKind {
    /// The owner rejected domain input.
    #[error("{0}")]
    OwnerInput(#[source] Box<dyn Error + Send + Sync>),
    /// The request arguments could not be parsed.
    #[error("tool call failed: invalid arguments: {0}")]
    Arguments(#[source] Box<dyn Error + Send + Sync>),
    /// Execution failed after argument validation.
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

/// Immutable validated registrations shared by clones.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Arc<BTreeMap<ToolName, RegisteredTool>>,
}
impl ToolRegistry {
    /// Rejects reserved names before duplicates, preserving input error order.
    pub fn new(
        reserved: &BTreeSet<String>,
        registrations: Vec<RegisteredTool>,
    ) -> Result<Self, RegistrationError> {
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
    /// Finds an installed registration by its exact name.
    pub fn find(&self, name: &str) -> Option<&RegisteredTool> {
        self.tools.get(name)
    }
    /// Reports whether an installed registration opted into resident access.
    pub fn resident_allowed(&self, name: &str) -> bool {
        self.find(name)
            .is_some_and(|tool| tool.exposure == ToolExposure::SeatAndResident)
    }
    /// Appends lexical definitions to an existing tools array.
    pub fn append_definitions(&self, response: &mut Value, context: ToolDiscoveryContext) {
        if let Some(tools) = response["tools"].as_array_mut() {
            tools.extend(
                self.tools
                    .values()
                    .map(|tool| tool.handler.present_definition(&tool.definition, context)),
            );
        }
    }
}
