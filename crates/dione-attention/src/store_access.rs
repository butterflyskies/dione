//! Causal failures while opening and operating the attention store.

use super::{
    store::{AttentionStore, StoreError},
    types::RecordId,
};
use std::sync::{Arc, Mutex, TryLockError};
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
    /// A concurrent transaction currently owns the store lock.
    #[error("attention store is busy")]
    Busy,
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
    /// Retains the failed decision identity and its concrete persistence cause.
    pub fn store(record: RecordId, source: AttentionStoreAccessError<StoreError>) -> Self {
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

/// Serializes durable metadata operations without blocking asynchronous workers.
///
/// Initialization failures remain sticky. This controller owns no ingress,
/// provider-export, configuration or delivery authority.
pub struct AttentionStoreAccess {
    store: Mutex<Result<AttentionStore, Arc<AttentionInitializationError>>>,
}

impl AttentionStoreAccess {
    /// Takes the ordinary store result produced by application initialization.
    ///
    /// This does not retry, replace a failed store, or create an empty fallback.
    pub fn from_initialized(
        store: Result<AttentionStore, Arc<AttentionInitializationError>>,
    ) -> Self {
        Self {
            store: Mutex::new(store),
        }
    }

    /// Runs one serialized store operation on a blocking worker.
    ///
    /// Once started, cancellation of the awaiting task does not abort the worker.
    /// The controller and store remain alive until that worker exits.
    pub async fn transact<R, E, F>(
        self: &Arc<Self>,
        operation: F,
    ) -> Result<R, AttentionStoreAccessError<E>>
    where
        R: Send + 'static,
        E: std::error::Error + Send + Sync + 'static,
        F: FnOnce(&mut AttentionStore) -> Result<R, E> + Send + 'static,
    {
        let access = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = access.store.lock().map_err(|_| AttentionStoreAccessError {
                kind: AttentionStoreAccessErrorKind::LockPoisoned,
            })?;
            let store = guard.as_mut().map_err(|error| AttentionStoreAccessError {
                kind: AttentionStoreAccessErrorKind::Initialization(Arc::clone(error)),
            })?;
            operation(store).map_err(|error| AttentionStoreAccessError {
                kind: AttentionStoreAccessErrorKind::Operation(error),
            })
        })
        .await
        .map_err(|error| AttentionStoreAccessError {
            kind: AttentionStoreAccessErrorKind::Worker(error),
        })?
    }

    /// Runs a synchronous read while holding the same transaction mutex.
    ///
    /// Busy, poisoned and uninitialized state fail closed. No borrowed store or
    /// lock guard escapes; the operation's entire validation stays under the lock.
    pub fn try_read<R, E, F>(&self, operation: F) -> Result<R, AttentionStoreAccessError<E>>
    where
        E: std::error::Error + 'static,
        F: FnOnce(&AttentionStore) -> Result<R, E>,
    {
        let guard = self
            .store
            .try_lock()
            .map_err(|error| AttentionStoreAccessError {
                kind: match error {
                    TryLockError::WouldBlock => AttentionStoreAccessErrorKind::Busy,
                    TryLockError::Poisoned(_) => AttentionStoreAccessErrorKind::LockPoisoned,
                },
            })?;
        let store = guard.as_ref().map_err(|error| AttentionStoreAccessError {
            kind: AttentionStoreAccessErrorKind::Initialization(Arc::clone(error)),
        })?;
        operation(store).map_err(|error| AttentionStoreAccessError {
            kind: AttentionStoreAccessErrorKind::Operation(error),
        })
    }
}
