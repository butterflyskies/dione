//! Validated identities for durable attention decisions.

use serde::{Deserialize, Deserializer, Serialize, de};
use std::{borrow::Borrow, fmt};
use thiserror::Error;

/// Identity of one source-bound decision; possession does not establish authority.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RecordId(String);

impl RecordId {
    /// Accepts nonblank identifiers of at most 512 UTF-8 bytes without rewriting them.
    pub fn new(value: impl Into<String>) -> Result<Self, RecordIdError> {
        let value = value.into();
        let kind = if value.trim().is_empty() {
            Some(RecordIdErrorKind::Blank)
        } else if value.len() > 512 {
            Some(RecordIdErrorKind::TooLong)
        } else {
            None
        };
        if let Some(kind) = kind {
            return Err(RecordIdError { kind });
        }
        Ok(Self(value))
    }

    /// Borrows the unchanged identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RecordId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

impl Borrow<str> for RecordId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for RecordId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<RecordId> for String {
    fn from(value: RecordId) -> Self {
        value.0
    }
}

/// A rejected decision identifier, without retaining potentially sensitive input.
#[derive(Debug, Error)]
#[error("invalid attention record identity: {kind}")]
pub struct RecordIdError {
    /// The violated constraint.
    #[source]
    pub kind: RecordIdErrorKind,
}

/// Decision identity validation failures.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RecordIdErrorKind {
    /// No non-whitespace content was supplied.
    #[error("identity must not be blank")]
    Blank,
    /// The encoded identity exceeds the durable size bound.
    #[error("identity exceeds 512 UTF-8 bytes")]
    TooLong,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn construction_and_deserialization_share_exact_byte_limits() {
        for value in [
            "".to_owned(),
            " \n\t".to_owned(),
            "x".repeat(513),
            "é".repeat(257),
        ] {
            assert!(RecordId::new(&value).is_err());
            assert!(serde_json::from_value::<RecordId>(serde_json::json!(value)).is_err());
        }
        for value in [" record-a ".to_owned(), "x".repeat(512), "é".repeat(256)] {
            let id = RecordId::new(&value).unwrap();
            assert_eq!(id.as_str(), value);
            assert_eq!(
                serde_json::from_value::<RecordId>(serde_json::json!(value)).unwrap(),
                id
            );
        }
        assert!(serde_json::from_str::<BTreeMap<RecordId, bool>>(r#"{"":true}"#).is_err());
        assert_eq!(
            serde_json::from_str::<BTreeMap<RecordId, bool>>(r#"{"r1":true}"#)
                .unwrap()
                .get("r1"),
            Some(&true)
        );
    }
}
