//! Validated references to single-use Teams reply authority.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// A syntactically valid reference to reply authority; possession alone does not authorize a reply.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ReplyHandle(String);

impl ReplyHandle {
    /// Validates an existing handle without changing its stored spelling.
    pub fn parse(value: impl Into<String>) -> Result<Self, ReplyHandleError> {
        let value = value.into();
        let suffix = value.strip_prefix("teams-").ok_or(ReplyHandleError {
            kind: ReplyHandleErrorKind::MissingPrefix,
        })?;
        Uuid::parse_str(suffix).map_err(|source| ReplyHandleError {
            kind: ReplyHandleErrorKind::InvalidUuid(source),
        })?;
        Ok(Self(value))
    }

    pub(crate) fn generate() -> Self {
        Self(format!("teams-{}", Uuid::new_v4()))
    }

    /// Returns the original handle spelling for a transport boundary.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ReplyHandle {
    type Error = ReplyHandleError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ReplyHandle> for String {
    fn from(handle: ReplyHandle) -> Self {
        handle.0
    }
}

/// A reply handle failed syntactic validation.
#[derive(Debug, Error)]
#[error("invalid Teams reply handle")]
pub struct ReplyHandleError {
    /// The reason validation failed, without retaining the supplied handle.
    #[source]
    pub kind: ReplyHandleErrorKind,
}

/// Supported reply handle validation failures.
#[derive(Debug, Error)]
pub enum ReplyHandleErrorKind {
    /// The required provider prefix is absent.
    #[error("missing teams prefix")]
    MissingPrefix,
    /// The suffix is not a UUID accepted by the persisted format.
    #[error("invalid UUID")]
    InvalidUuid(#[source] uuid::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn validation_preserves_historical_uuid_spellings_and_sources() {
        for spelling in [
            "teams-ABCDEF0123456789ABCDEF0123456789",
            "teams-abcdef01-2345-6789-abcd-ef0123456789",
        ] {
            let handle = ReplyHandle::parse(spelling).unwrap();
            assert_eq!(handle.as_str(), spelling);
            let encoded = serde_json::to_string(&handle).unwrap();
            assert_eq!(encoded, format!("\"{spelling}\""));
            assert_eq!(
                serde_json::from_str::<ReplyHandle>(&encoded).unwrap(),
                handle
            );
        }
        for invalid in ["", "abcdef01-2345-6789-abcd-ef0123456789", "teams-bad"] {
            assert!(ReplyHandle::parse(invalid).is_err());
            assert!(serde_json::from_value::<ReplyHandle>(serde_json::json!(invalid)).is_err());
        }
        let error = ReplyHandle::parse("teams-bad").unwrap_err();
        assert!(error.source().unwrap().source().is_some());
    }
}
