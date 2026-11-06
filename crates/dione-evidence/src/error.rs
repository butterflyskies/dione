//! Typed failures while validating externally supplied sentex handles.

use super::locators::{MAX_SENTEX_MARKER_BYTES, MAX_SENTEX_REFS};
use std::{fmt, num::ParseIntError};
use thiserror::Error;

/// The tool argument containing a role-specific sentex handle collection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SentexField {
    /// Handles offered as claims through `claim_handles`.
    Claims,
    /// Handles supplied as citations through `citation_handles`.
    Citations,
}
impl SentexField {
    /// Returns the exact JSON argument name for this role partition.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claims => "claim_handles",
            Self::Citations => "citation_handles",
        }
    }
}
impl fmt::Display for SentexField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
/// A typed rejection of externally supplied sentex arguments.
#[derive(Debug, Error)]
#[error("{kind}")]
pub struct SentexError {
    #[source]
    /// The violated argument, count, handle, or marker-size constraint.
    pub kind: SentexErrorKind,
}
/// Validation failures that preserve the supplied argument role and concrete cause.
#[derive(Debug, Error)]
pub enum SentexErrorKind {
    /// The request used the removed, role-free `evidence_keys` argument.
    #[error("evidence_keys was removed; use claim_handles and/or citation_handles")]
    RemovedLegacyField,
    /// The combined claim and citation count could not be represented.
    #[error("sentex handle count overflowed")]
    CountOverflow,
    /// The combined claim and citation count exceeds the supported bound.
    #[error(
        "claim_handles and citation_handles support at most {MAX_SENTEX_REFS} total references"
    )]
    TooManyReferences,
    /// A role-specific argument was present with a non-array JSON value.
    #[error("{0} must be an array")]
    ExpectedArray(SentexField),
    /// A handle in the named argument failed canonical numeric parsing.
    #[error("{field} must contain canonical positive decimal u64 strings")]
    InvalidHandle {
        /// The role-specific argument containing the rejected handle.
        field: SentexField,
        /// The original canonicality or numeric-range failure.
        #[source]
        cause: HandleError,
    },
    /// The encoded visible suffix would exceed the marker-byte limit.
    #[error("sentex marker suffix exceeds {MAX_SENTEX_MARKER_BYTES} bytes")]
    MarkerBytes,
}
/// A rejected canonical positive decimal handle, with its numeric cause retained.
#[derive(Debug, Error)]
#[error("invalid sentex handle: {kind}")]
pub struct HandleError {
    #[source]
    /// Whether the decimal spelling or its numeric range was invalid.
    pub kind: HandleErrorKind,
}
/// Failures while decoding one externally supplied decimal handle.
#[derive(Debug, Error)]
pub enum HandleErrorKind {
    /// The value is not an unpadded ASCII decimal string beginning with 1 through 9.
    #[error("expected a canonical positive decimal string")]
    Noncanonical,
    /// The canonical decimal spelling does not fit the nonzero u64 representation.
    #[error("decimal handle is out of range")]
    Number(#[source] ParseIntError),
}
