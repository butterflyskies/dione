//! Context-bearing storage, codec and transition failures.

use super::identity::IdentityError;
use camino::Utf8PathBuf;
use std::{error::Error as StdError, io};
use thiserror::Error;

/// Failure at the serialization adapter boundary, retaining its original cause.
#[derive(Debug, Error)]
#[error("{kind}")]
pub struct CodecError {
    /// The failed codec operation.
    #[source]
    pub kind: CodecErrorKind,
}
/// Format interpretation failures; concrete adapter causes remain downcastable.
#[derive(Debug, Error)]
pub enum CodecErrorKind {
    /// Stored bytes could not be decoded or validated.
    #[error("failed to decode durable snapshot")]
    Decode(#[source] Box<dyn StdError + Send + Sync>),
    /// Snapshot encoding failed before visibility.
    #[error("failed to encode durable snapshot")]
    Encode(#[source] Box<dyn StdError + Send + Sync>),
    /// An explicit version is not understood by this codec.
    #[error("unsupported durable snapshot version")]
    UnsupportedVersion,
}
/// Counter whose next value cannot be allocated without identity reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocationKind {
    /// Event insertion.
    Event,
    /// New or renewed lease token.
    Lease,
    /// New consumer registration.
    Consumer,
}
/// Invalid retained state found before a queue becomes usable.
#[derive(Debug, Clone, Copy, Error)]
pub enum SnapshotIssue {
    /// A retained event identity occurs twice.
    #[error("duplicate event identity")]
    DuplicateEvent,
    /// A retained registration identity occurs twice.
    #[error("duplicate consumer identity")]
    DuplicateConsumer,
    /// A retained active lease token occurs twice.
    #[error("duplicate lease identity")]
    DuplicateLease,
    /// One replay identity belongs to two different queued records.
    #[error("replay identity belongs to multiple queued entries")]
    DuplicateReplay,
    /// A next counter can collide with a retained generated identity.
    #[error("next allocation counter conflicts with retained identity")]
    StaleCounter,
    /// A new generated identity already exists in retained state.
    #[error("generated identity already exists")]
    IdentityCollision,
    /// One entry repeats an identical replay key.
    #[error("entry repeats a replay identity")]
    RepeatedReplay,
}
/// An owned queue operation failed with its concrete cause and path.
#[derive(Debug, Error)]
#[error("{operation}: {kind}")]
pub struct QueueError {
    /// Failed operation; never includes payload text.
    pub operation: &'static str,
    /// Storage path when the operation touches disk.
    pub path: Option<Utf8PathBuf>,
    /// Exhaustive failure cause.
    #[source]
    pub kind: QueueErrorKind,
}
/// Durable storage and authorization failure categories.
#[derive(Debug, Error)]
pub enum QueueErrorKind {
    /// Snapshot, temporary and lock names must be distinct single path components.
    #[error("invalid durable storage filename layout")]
    InvalidStorageLayout,
    /// Filesystem operation failed before visibility.
    #[error("failed to access durable store")]
    Io(#[source] io::Error),
    /// A competing process owns the store lock.
    #[error("another process owns the durable store")]
    Locked(#[source] io::Error),
    /// Codec rejection leaves the original file untouched.
    #[error("durable snapshot codec failed")]
    Codec(#[source] CodecError),
    /// A prior visible operation still cannot establish its crash durability.
    #[error("prior visible commit has unconfirmed crash durability")]
    PriorSync(#[source] io::Error),
    /// This operation is visible but its directory entry is not known durable.
    #[error("visible commit has unconfirmed crash durability")]
    DurabilityUncertain(#[source] io::Error),
    /// Accepted owned operation lost its completion response.
    #[error("operation completion is unknown")]
    CompletionUnknown(#[source] tokio::task::JoinError),
    /// Invalid exact identity supplied at a boundary.
    #[error("{0}")]
    Identity(#[source] IdentityError),
    /// Explicitly conflicting or duplicate retained identities.
    #[error("invalid durable snapshot: {0}")]
    Integrity(#[source] SnapshotIssue),
    /// The next counter is the exhausted sentinel.
    #[error("allocation exhausted: {allocation:?}")]
    AllocationExhausted {
        /// Allocation family that cannot issue another identity.
        allocation: AllocationKind,
    },
    /// The consumer is unknown, expired, or has the wrong capability.
    #[error("consumer is unknown or expired")]
    UnknownConsumer,
    /// Only the current primary can hand off pending work.
    #[error("consumer is not the active primary")]
    NotPrimaryConsumer,
    /// A second registration cannot silently replace the primary.
    #[error("an active primary consumer already exists")]
    PrimaryConsumerExists,
    /// Acknowledgement did not identify an owned lease.
    #[error("delivery token is unknown or its lease expired")]
    UnknownDeliveryToken,
    /// The immutable admission metadata does not permit deferral.
    #[error("delivery token does not identify a deferrable event")]
    NotDeferrable,
    /// No currently usable resident endpoint exists.
    #[error("no exact live resident route is ready")]
    ResidentUnavailable,
    /// The configured timed-fence class cannot retain another new key.
    #[error("replay-identity capacity is full")]
    ReplayCapacity,
}
impl QueueError {
    /// Constructs a non-filesystem queue failure without losing its typed cause.
    pub fn new(operation: &'static str, kind: QueueErrorKind) -> Self {
        Self {
            operation,
            path: None,
            kind,
        }
    }
    pub(super) fn at(
        operation: &'static str,
        path: &camino::Utf8Path,
        kind: QueueErrorKind,
    ) -> Self {
        Self {
            operation,
            path: Some(path.to_owned()),
            kind,
        }
    }
}
