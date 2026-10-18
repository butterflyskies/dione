//! Causal failures while opening and operating the attention store.

use super::{store::StoreError, types::RecordId};
use std::sync::Arc;
use thiserror::Error;
use tokio::task::JoinError;

/// The store could not be initialized.
#[derive(Debug, Error)]
#[error("attention initialization failed: {kind}")]
pub struct AttentionInitializationError {
    /// The original initialization failure.
    #[source]
    pub kind: AttentionInitializationErrorKind,
}

/// Initialization failure sources.
#[derive(Debug, Error)]
pub enum AttentionInitializationErrorKind {
    /// Opening durable metadata failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The initialization worker failed.
    #[error(transparent)]
    Worker(#[from] JoinError),
}

/// An attention store operation could not complete.
#[derive(Debug, Error)]
#[error("attention store operation failed: {kind}")]
pub struct AttentionStoreAccessError<E: std::error::Error + 'static> {
    /// The failed access or operation.
    #[source]
    pub kind: AttentionStoreAccessErrorKind<E>,
}

/// Store access and operation failure sources.
#[derive(Debug, Error)]
pub enum AttentionStoreAccessErrorKind<E: std::error::Error + 'static> {
    /// The store's initialization failed.
    #[error(transparent)]
    Initialization(Arc<AttentionInitializationError>),
    /// A preceding panic poisoned the store lock.
    #[error("attention store lock is poisoned")]
    LockPoisoned,
    /// The blocking worker failed.
    #[error(transparent)]
    Worker(JoinError),
    /// The operation returned its original error.
    #[error(transparent)]
    Operation(E),
}

/// Persisting a transport outcome failed; an accepted transport must not be replayed.
#[derive(Debug, Error)]
#[error("failed to persist attention receipt for {record}")]
pub struct AttentionReceiptError {
    record: RecordId,
    #[source]
    kind: AttentionReceiptErrorKind,
}

/// Receipt persistence failure sources.
#[derive(Debug, Error)]
pub enum AttentionReceiptErrorKind {
    /// The durable state update failed.
    #[error(transparent)]
    Store(AttentionStoreAccessError<StoreError>),
}

impl AttentionReceiptError {
    pub(crate) fn store(record: RecordId, source: AttentionStoreAccessError<StoreError>) -> Self {
        Self {
            record,
            kind: AttentionReceiptErrorKind::Store(source),
        }
    }
    /// Borrows the record whose receipt was not persisted.
    pub fn record(&self) -> &RecordId {
        &self.record
    }
    /// Borrows the concrete failure.
    pub fn kind(&self) -> &AttentionReceiptErrorKind {
        &self.kind
    }
}

#[derive(Debug, Error)]
#[error("{0}")]
pub(crate) struct LegacyOperationError(pub(crate) String);
