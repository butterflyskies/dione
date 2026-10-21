//! Shared Discord tool argument parsing at the MCP boundary.
use crate::{
    discord::ids::Snowflake,
    evidence::{SentexError, SentexHandles, parse_tool_sentex_handles},
    pre_send::{HookName, PipelineError},
};
use serde_json::{Value, json};
use std::num::ParseIntError;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("{kind}")]
pub(crate) struct ArgumentError {
    #[source]
    pub(crate) kind: ArgumentErrorKind,
}
#[derive(Debug, Error)]
pub(crate) enum ArgumentErrorKind {
    #[error("missing {0}")]
    Missing(&'static str),
    #[error("invalid {field}: not a valid u64")]
    InvalidId {
        field: &'static str,
        #[source]
        source: ParseIntError,
    },
    #[error("invalid {0}: expected a numeric or string ID")]
    IdType(&'static str),
    #[error("invalid {0}: must be a nonzero Discord snowflake")]
    ZeroId(&'static str),
    #[error("{0} must be a boolean")]
    Boolean(&'static str),
    #[error("no_rly_hooks must be an array of hook-name strings")]
    HookArray,
    #[error("no_rly_hooks must contain only strings")]
    HookItem,
    #[error("invalid user_ids: expected an array of IDs")]
    UserIdsArray,
    #[error("invalid user_ids: each ID must be a numeric or string u64")]
    UserIdItem,
    #[error("invalid user_ids: must be nonzero Discord snowflakes")]
    UserIdZero,
    #[error(transparent)]
    VisibilityRequest(#[from] crate::discord::visibility::VisibilityRequestError),
    #[error("{0}")]
    Hook(#[from] PipelineError),
    #[error("claim_handles and citation_handles require evidence_markers_enabled")]
    EvidenceDisabled,
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

pub(crate) fn parse_enabled_sentex_handles(
    args: &Value,
    evidence_markers_enabled: bool,
) -> Result<SentexHandles, ArgumentError> {
    let handles = parse_tool_sentex_handles(args).map_err(ArgumentErrorKind::Evidence)?;
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
pub(crate) fn parse_id(args: &Value, key: &'static str) -> Result<Snowflake, ArgumentError> {
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
pub(crate) fn parse_optional_id(
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
pub(crate) fn parse_optional_bool(
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
pub(crate) fn parse_hook_overrides(args: &Value) -> Result<Vec<HookName>, ArgumentError> {
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

pub(crate) fn sentex_handles_schema(role_description: &str) -> Value {
    json!({
        "type": "array",
        "maxItems": crate::evidence::MAX_SENTEX_REFS,
        "items": {
            "type": "string",
            "pattern": "^[1-9][0-9]*$",
            "maxLength": 20
        },
        "description": format!(
            "{role_description}. Canonical positive decimal u64 strings naming opaque Vaelii sentex handles; JSON numbers are rejected to avoid precision loss. Dione transports the role and handle in terminal [🔍=v3:<role>:<token>] locators after pre-send hooks, where the token is the minimal-width big-endian handle bytes in unpadded base64url. At most {} claim and citation handles combined and {} aggregate marker bytes. A locator is author-offered metadata, not verification or truth.",
            crate::evidence::MAX_SENTEX_REFS,
            crate::evidence::MAX_SENTEX_MARKER_BYTES,
        )
    })
}
