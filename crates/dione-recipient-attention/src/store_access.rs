//! String failures retained by the application attention adapter.

use thiserror::Error;

#[derive(Debug, Error)]
#[error("{0}")]
pub(crate) struct LegacyOperationError(pub(crate) String);
