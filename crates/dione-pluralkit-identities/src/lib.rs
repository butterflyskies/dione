//! Configured PluralKit UUID syntax and the official deployment identity.

#![warn(missing_docs)]

use thiserror::Error;

/// Discord application ID of the official PluralKit deployment.
///
/// Identifying a deployment does not establish trust or grant permission.
pub const PLURALKIT_APPLICATION_ID: u64 = 466_378_653_216_014_359;

/// A configured UUID failed validation without retaining the supplied value.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("{kind}")]
pub struct PkUuidError {
    /// The violated UUID syntax constraint.
    #[source]
    pub kind: PkUuidErrorKind,
}

/// Rejected configured UUID syntax categories.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PkUuidErrorKind {
    /// The input contains no bytes.
    #[error("pk uuid must be non-empty")]
    Empty,
    /// The input is not the full dashed ASCII-hex shape.
    #[error(
        "pk identifier must be a full uuid (8-4-4-4-12 hex); short ids are not accepted; use the full uuid from the pk api"
    )]
    InvalidFormat,
}

/// A validated PluralKit UUID.
///
/// Validates that the string is a well-formed UUID (8-4-4-4-12 hex format).
/// Prevents raw arbitrary strings from being used as identity claims.
///
/// ```compile_fail
/// use dione_pluralkit_identities::PkUuid;
/// let id = PkUuid("unvalidated".to_owned());
/// ```
///
/// ```compile_fail
/// use dione_pluralkit_identities::PkUuid;
/// let id = PkUuid::parse("00000000-0000-0000-0000-000000000000").unwrap();
/// let raw = id.0;
/// ```
///
/// ```compile_fail
/// use dione_pluralkit_identities::PkUuid;
/// let mut id = PkUuid::parse("00000000-0000-0000-0000-000000000000").unwrap();
/// id.0 = "unvalidated".to_owned();
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PkUuid(String);

impl PkUuid {
    /// Parse and validate a PluralKit UUID string.
    ///
    /// Accepts only the standard UUID format: 8-4-4-4-12 hex with dashes.
    /// PK short IDs (5-char alphanumeric) are NOT accepted because the PK API
    /// returns full UUIDs — a configured short ID would never match the
    /// resolver's output, silently failing to authorize.
    pub fn parse(s: impl Into<String>) -> Result<Self, PkUuidError> {
        let s = s.into();
        if s.is_empty() {
            return Err(PkUuidError {
                kind: PkUuidErrorKind::Empty,
            });
        }
        let is_uuid = s.len() == 36
            && s.chars().enumerate().all(|(i, c)| {
                if i == 8 || i == 13 || i == 18 || i == 23 {
                    c == '-'
                } else {
                    c.is_ascii_hexdigit()
                }
            });
        if !is_uuid {
            return Err(PkUuidError {
                kind: PkUuidErrorKind::InvalidFormat,
            });
        }
        // Canonicalize to lowercase for consistent matching.
        Ok(Self(s.to_ascii_lowercase()))
    }

    /// Borrows the canonical lowercase UUID.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PkUuid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_pk_uuid_empty_rejected() {
        assert!(PkUuid::parse("").is_err());
    }

    #[test]
    fn test_pk_uuid_invalid_format_rejected() {
        assert!(PkUuid::parse("not-a-uuid").is_err());
        assert!(PkUuid::parse("sys-1").is_err());
    }

    #[test]
    fn test_pk_uuid_short_id_rejected() {
        // Short IDs cannot match resolver output (which returns full UUIDs),
        // so they must be rejected to prevent silent authorization failures.
        assert!(PkUuid::parse("abcde").is_err());
    }

    #[test]
    fn test_pk_uuid_valid_uuid_accepted() {
        assert!(PkUuid::parse("12345678-1234-1234-1234-123456789abc").is_ok());
    }

    #[test]
    fn test_pk_uuid_uppercase_canonicalized() {
        let uuid = PkUuid::parse("12345678-ABCD-1234-1234-123456789ABC").unwrap();
        assert_eq!(
            uuid.as_str(),
            "12345678-abcd-1234-1234-123456789abc",
            "uppercase UUID must be canonicalized to lowercase"
        );
    }
}
