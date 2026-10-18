//! Typed failures while validating externally supplied sentex handles.

use super::locators::{MAX_SENTEX_MARKER_BYTES, MAX_SENTEX_REFS};
use std::{fmt, num::ParseIntError};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SentexField {
    Claims,
    Citations,
}
impl SentexField {
    pub(crate) fn as_str(self) -> &'static str {
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
#[derive(Debug, Error)]
#[error("{kind}")]
pub(crate) struct SentexError {
    #[source]
    pub(crate) kind: SentexErrorKind,
}
#[derive(Debug, Error)]
pub(crate) enum SentexErrorKind {
    #[error("evidence_keys was removed; use claim_handles and/or citation_handles")]
    RemovedLegacyField,
    #[error("sentex handle count overflowed")]
    CountOverflow,
    #[error(
        "claim_handles and citation_handles support at most {MAX_SENTEX_REFS} total references"
    )]
    TooManyReferences,
    #[error("{0} must be an array")]
    ExpectedArray(SentexField),
    #[error("{field} must contain canonical positive decimal u64 strings")]
    InvalidHandle {
        field: SentexField,
        #[source]
        cause: HandleError,
    },
    #[error("sentex marker suffix exceeds {MAX_SENTEX_MARKER_BYTES} bytes")]
    MarkerBytes,
}
#[derive(Debug, Error)]
#[error("invalid sentex handle: {kind}")]
pub(crate) struct HandleError {
    #[source]
    pub(crate) kind: HandleErrorKind,
}
#[derive(Debug, Error)]
pub(crate) enum HandleErrorKind {
    #[error("expected a canonical positive decimal string")]
    Noncanonical,
    #[error("decimal handle is out of range")]
    Number(#[source] ParseIntError),
}
