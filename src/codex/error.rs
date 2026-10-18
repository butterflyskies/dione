//! Typed legacy-adapter failures without losing common storage causes.
use super::{
    LegacyDeliveryError,
    queue_codec::{LegacyCodecError, LegacyCodecErrorKind},
};
use crate::durable::{CodecErrorKind, QueueError, QueueErrorKind};
use thiserror::Error;

/// A Codex adapter or shared durable operation failed.
#[derive(Debug, Error)]
#[error("{kind}")]
pub struct CodexQueueError {
    /// Exhaustive category retaining the original concrete cause.
    #[source]
    pub kind: CodexQueueErrorKind,
}
/// Legacy payload/host boundary and shared durable failures.
#[derive(Debug, Error)]
pub enum CodexQueueErrorKind {
    /// Shared queue failure, including path and operation context.
    #[error("{}", legacy_queue_message(.0))]
    Durable(#[source] QueueError),
    /// Managed payload markers do not describe a valid delivery.
    #[error("malformed legacy attention metadata")]
    LegacyAttention(#[source] LegacyDeliveryError),
    /// The Codex host thread identifier is invalid.
    #[error("invalid Codex thread id")]
    InvalidThreadId,
}
impl From<CodexQueueErrorKind> for CodexQueueError {
    fn from(kind: CodexQueueErrorKind) -> Self {
        Self { kind }
    }
}
impl From<QueueError> for CodexQueueError {
    fn from(error: QueueError) -> Self {
        CodexQueueErrorKind::Durable(error).into()
    }
}

/// Live admission rejected before insertion.
#[derive(Debug, Error)]
#[error("{kind}")]
pub(crate) struct LiveQueueError {
    #[source]
    pub(crate) kind: LiveQueueErrorKind,
}
#[derive(Debug, Error)]
pub(crate) enum LiveQueueErrorKind {
    #[error("{0}")]
    Queue(#[source] CodexQueueError),
    #[error("no exact live resident route is ready")]
    ResidentUnavailable(#[source] QueueError),
    #[error("teams replay-identity capacity is full")]
    ReplayCapacity(#[source] QueueError),
}
impl From<CodexQueueError> for LiveQueueError {
    fn from(error: CodexQueueError) -> Self {
        Self {
            kind: LiveQueueErrorKind::Queue(error),
        }
    }
}

fn legacy_queue_message(error: &QueueError) -> String {
    let path = error.path.as_deref();
    match &error.kind {
        QueueErrorKind::Io(_) => path.map_or_else(
            || "failed to access Codex inbox".into(),
            |path| format!("failed to access Codex inbox `{path}`"),
        ),
        QueueErrorKind::Locked(_) => path.map_or_else(
            || "another Dione process owns Codex inbox".into(),
            |path| format!("another Dione process owns Codex inbox `{path}`"),
        ),
        QueueErrorKind::PriorSync(_) | QueueErrorKind::DurabilityUncertain(_) => path.map_or_else(
            || "visible Codex inbox commit has unconfirmed crash durability".into(),
            |path| {
                format!("visible Codex inbox commit at `{path}` has unconfirmed crash durability")
            },
        ),
        QueueErrorKind::CompletionUnknown(_) => {
            "Codex inbox operation completion is unknown".into()
        }
        QueueErrorKind::UnknownConsumer => "consumer is unknown or expired".into(),
        QueueErrorKind::NotPrimaryConsumer => "consumer is not the active primary".into(),
        QueueErrorKind::PrimaryConsumerExists => "an active primary consumer already exists".into(),
        QueueErrorKind::UnknownDeliveryToken => {
            "delivery token is unknown or its lease expired".into()
        }
        QueueErrorKind::NotDeferrable => {
            "delivery token does not identify an attention-managed event".into()
        }
        QueueErrorKind::ResidentUnavailable => "no exact live resident route is ready".into(),
        QueueErrorKind::ReplayCapacity => "teams replay-identity capacity is full".into(),
        QueueErrorKind::Identity(source) => source.to_string(),
        QueueErrorKind::InvalidStorageLayout => "invalid durable storage layout".into(),
        QueueErrorKind::Integrity(source) => format!("invalid durable snapshot: {source}"),
        QueueErrorKind::AllocationExhausted { allocation } => {
            format!("allocation exhausted: {allocation:?}")
        }
        QueueErrorKind::Codec(codec) => match &codec.kind {
            CodecErrorKind::Decode(source) => {
                let operation = match source
                    .downcast_ref::<LegacyCodecError>()
                    .map(|error| &error.kind)
                {
                    Some(LegacyCodecErrorKind::Payload(source)) => return source.to_string(),
                    Some(LegacyCodecErrorKind::Trailing(_)) => "unexpected trailing input in",
                    Some(
                        LegacyCodecErrorKind::Decode(_)
                        | LegacyCodecErrorKind::Encode(_)
                        | LegacyCodecErrorKind::Endpoint(_)
                        | LegacyCodecErrorKind::ReplayShape,
                    )
                    | None => "failed to decode",
                };
                path.map_or_else(
                    || format!("{operation} Codex inbox"),
                    |path| format!("{operation} Codex inbox `{path}`"),
                )
            }
            CodecErrorKind::Encode(_) => path.map_or_else(
                || "failed to encode Codex inbox".into(),
                |path| format!("failed to encode Codex inbox `{path}`"),
            ),
            CodecErrorKind::UnsupportedVersion => "unsupported durable snapshot version".into(),
        },
    }
}

impl From<QueueError> for LiveQueueError {
    fn from(error: QueueError) -> Self {
        match &error.kind {
            QueueErrorKind::ResidentUnavailable => LiveQueueError {
                kind: LiveQueueErrorKind::ResidentUnavailable(error),
            },
            QueueErrorKind::ReplayCapacity => LiveQueueError {
                kind: LiveQueueErrorKind::ReplayCapacity(error),
            },
            QueueErrorKind::Io(_)
            | QueueErrorKind::Locked(_)
            | QueueErrorKind::Codec(_)
            | QueueErrorKind::PriorSync(_)
            | QueueErrorKind::DurabilityUncertain(_)
            | QueueErrorKind::CompletionUnknown(_)
            | QueueErrorKind::Identity(_)
            | QueueErrorKind::Integrity(_)
            | QueueErrorKind::AllocationExhausted { .. }
            | QueueErrorKind::UnknownConsumer
            | QueueErrorKind::NotPrimaryConsumer
            | QueueErrorKind::PrimaryConsumerExists
            | QueueErrorKind::UnknownDeliveryToken
            | QueueErrorKind::NotDeferrable
            | QueueErrorKind::InvalidStorageLayout => CodexQueueError::from(error).into(),
        }
    }
}
