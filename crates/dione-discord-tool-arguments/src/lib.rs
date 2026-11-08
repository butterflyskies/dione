//! Shared Discord tool argument parsing at the MCP boundary.
#![warn(missing_docs)]
use dione_pre_send::{HookName, PipelineError};
use serde_json::{Value, json};
use std::num::ParseIntError;
use thiserror::Error;
use {
    dione_discord_identities::Snowflake,
    dione_evidence::{SentexError, SentexHandles, parse_tool_sentex_handles},
};

/// A typed argument refusal preserving its displayed message and original source.
#[derive(Debug, Error)]
#[error("{kind}")]
pub struct ArgumentError {
    #[source]
    kind: ArgumentErrorKind,
}
/// Exact argument failure categories shared by Discord tool consumers.
#[derive(Debug, Error)]
pub enum ArgumentErrorKind {
    /// A required field is absent.
    #[error("missing {0}")]
    Missing(&'static str),
    /// A decimal string does not fit an unsigned64-bit identifier.
    #[error("invalid {field}: not a valid u64")]
    InvalidId {
        /// The rejected argument field.
        field: &'static str,
        #[source]
        /// The original numeric parse failure.
        source: ParseIntError,
    },
    /// The required identifier has an unsupported JSON shape.
    #[error("invalid {0}: expected a numeric or string ID")]
    IdType(&'static str),
    /// Discord identifiers must be nonzero.
    #[error("invalid {0}: must be a nonzero Discord snowflake")]
    ZeroId(&'static str),
    /// A present optional boolean has an unsupported shape.
    #[error("{0} must be a boolean")]
    Boolean(&'static str),
    /// Hook overrides must be an array.
    #[error("no_rly_hooks must be an array of hook-name strings")]
    HookArray,
    /// Every hook override must be a string.
    #[error("no_rly_hooks must contain only strings")]
    HookItem,
    /// Visibility user IDs must be an array.
    #[error("invalid user_ids: expected an array of IDs")]
    UserIdsArray,
    /// A visibility user ID has an unsupported shape.
    #[error("invalid user_ids: each ID must be a numeric or string u64")]
    UserIdItem,
    /// A visibility user ID is zero.
    #[error("invalid user_ids: must be nonzero Discord snowflakes")]
    UserIdZero,
    /// The visibility request violates the existing bounded contract.
    #[error(transparent)]
    VisibilityRequest(#[from] dione_discord_introspection::visibility::VisibilityRequestError),
    /// A hook name violates the existing pipeline contract.
    #[error("{0}")]
    Hook(#[from] PipelineError),
    /// Evidence handle fields are present while markers are disabled.
    #[error("claim_handles and citation_handles require evidence_markers_enabled")]
    EvidenceDisabled,
    /// The evidence owner rejected the handle arguments.
    #[error("{0}")]
    Evidence(#[source] SentexError),
}
impl From<ArgumentErrorKind> for ArgumentError {
    fn from(kind: ArgumentErrorKind) -> Self {
        Self { kind }
    }
}
impl From<ArgumentError> for String {
    fn from(error: ArgumentError) -> Self {
        error.to_string()
    }
}

/// Parse evidence role handles with the selected grammar and independent enabled switch.
///
/// # Errors
/// Returns the original typed argument failure without changing its source or display.
pub fn parse_enabled_sentex_handles(
    args: &Value,
    evidence_markers_enabled: bool,
    version: dione_evidence::EvidenceMarkerVersion,
) -> Result<SentexHandles, ArgumentError> {
    let handles = parse_tool_sentex_handles(args, version).map_err(ArgumentErrorKind::Evidence)?;
    if evidence_markers_enabled {
        return Ok(handles);
    }
    if args.get("claim_handles").is_some() || args.get("citation_handles").is_some() {
        return Err(ArgumentErrorKind::EvidenceDisabled.into());
    }
    Ok(SentexHandles::empty())
}

fn snowflake(id: u64, field: &'static str) -> Result<Snowflake, ArgumentError> {
    Snowflake::new(id).ok_or_else(|| ArgumentErrorKind::ZeroId(field).into())
}
fn decimal_id(value: &str, field: &'static str) -> Result<Snowflake, ArgumentError> {
    snowflake(
        value
            .parse::<u64>()
            .map_err(|source| ArgumentErrorKind::InvalidId { field, source })?,
        field,
    )
}
/// Parse a required nonzero Discord identifier from a number or decimal string.
///
/// # Errors
/// Returns the original typed argument failure without changing its source or display.
pub fn parse_id(args: &Value, key: &'static str) -> Result<Snowflake, ArgumentError> {
    if let Some(id) = args.get(key).and_then(Value::as_u64) {
        return snowflake(id, key);
    }
    if let Some(id) = args.get(key).and_then(Value::as_str) {
        return decimal_id(id, key);
    }
    Err(if args.get(key).is_some() {
        ArgumentErrorKind::IdType(key)
    } else {
        ArgumentErrorKind::Missing(key)
    }
    .into())
}
/// Parse an optional identifier, preserving historical absent and unsupported-shape handling.
///
/// # Errors
/// Returns the original typed argument failure without changing its source or display.
pub fn parse_optional_id(
    args: &Value,
    key: &'static str,
) -> Result<Option<Snowflake>, ArgumentError> {
    if let Some(id) = args.get(key).and_then(Value::as_u64) {
        return snowflake(id, key).map(Some);
    }
    if let Some(id) = args.get(key).and_then(Value::as_str) {
        if id.is_empty() {
            return Ok(None);
        }
        return decimal_id(id, key).map(Some);
    }
    Ok(None)
}
/// Parse an optional boolean, rejecting every other present JSON shape.
///
/// # Errors
/// Returns the original typed argument failure without changing its source or display.
pub fn parse_optional_bool(
    args: &Value,
    name: &'static str,
) -> Result<Option<bool>, ArgumentError> {
    args.get(name)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| ArgumentErrorKind::Boolean(name).into())
        })
        .transpose()
}
/// Parse canonical hook names in caller order.
///
/// # Errors
/// Returns the original typed argument failure without changing its source or display.
pub fn parse_hook_overrides(args: &Value) -> Result<Vec<HookName>, ArgumentError> {
    let Some(value) = args.get("no_rly_hooks") else {
        return Ok(Vec::new());
    };
    let values = value.as_array().ok_or(ArgumentErrorKind::HookArray)?;
    values
        .iter()
        .map(|value| {
            let name = value.as_str().ok_or(ArgumentErrorKind::HookItem)?;
            HookName::parse(name).map_err(|error| ArgumentErrorKind::Hook(error).into())
        })
        .collect()
}

/// Describe the existing evidence role-handle array contract for the selected marker grammar.
pub fn sentex_handles_schema(
    role_description: &str,
    version: dione_evidence::EvidenceMarkerVersion,
) -> Value {
    json!({
        "type": "array",
        "maxItems": dione_evidence::MAX_SENTEX_REFS,
        "items": {
            "type": "string",
            "pattern": "^[1-9][0-9]*$",
            "maxLength": 20
        },
        "description": format!(
            "{role_description}. Canonical positive decimal u64 strings naming opaque Vaelii sentex handles; JSON numbers are rejected to avoid precision loss. Dione transports the role and handle in terminal {}. At most {} claim and citation handles combined and {} aggregate marker bytes. A locator is author-offered metadata, not verification or truth.",
            version.grammar_description(),
            dione_evidence::MAX_SENTEX_REFS,
            dione_evidence::MAX_SENTEX_MARKER_BYTES,
        )
    })
}

impl ArgumentError {
    /// Borrows the exact typed refusal without discarding its source chain.
    pub fn kind(&self) -> &ArgumentErrorKind {
        &self.kind
    }
}
