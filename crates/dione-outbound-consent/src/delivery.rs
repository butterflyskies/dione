//! Typed publication failures and the private state required to resume a send.

use crate::chunker::FenceChunkError;
use crate::diary::DiaryRecord;
use serenity::model::id::{ChannelId, MessageId};
use thiserror::Error;

/// Whether a failed Discord message POST may already have created a message.
/// Response decoding and unclassified failures are conservative; only explicit
/// local construction/connect failures and deterministic 4xx responses prove
/// no effect. This does not authorize a retry or rely on nonce deduplication.
pub fn post_completion_is_unknown(error: &serenity::Error) -> bool {
    use serenity::http::HttpError;
    match error {
        serenity::Error::Http(HttpError::UnsuccessfulRequest(response)) => {
            !response.status_code.is_client_error()
        }
        serenity::Error::Http(HttpError::Request(error)) => !error.is_connect(),
        serenity::Error::Http(
            HttpError::Url(_)
            | HttpError::InvalidWebhook
            | HttpError::InvalidHeader(_)
            | HttpError::InvalidScheme
            | HttpError::InvalidPort
            | HttpError::ApplicationIdMissing,
        ) => false,
        _ => true,
    }
}

/// The cause of a failed publication attempt.
#[derive(Debug, Error)]
pub enum DeliveryCause {
    /// Current policy denied the destination.
    #[error("channel {0} is not a permitted outbound target")]
    OutboundDenied(ChannelId),
    /// Fence-preserving chunking could not fit the message.
    #[error(transparent)]
    Chunking(#[from] FenceChunkError),
    /// Discord rejected a chunk.
    #[error("failed to send chunk {index}: {source}")]
    ChunkSend {
        /// Zero-based chunk index.
        index: usize,
        /// Original Discord error.
        #[source]
        source: Box<serenity::Error>,
    },
}

/// At least one message published by an attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedMessages {
    first: MessageId,
    rest: Vec<MessageId>,
}

impl PostedMessages {
    /// Construct nonempty progress from its first message and remaining messages.
    pub fn new(first: MessageId, rest: Vec<MessageId>) -> Self {
        Self { first, rest }
    }

    /// The first published message.
    pub fn first(&self) -> MessageId {
        self.first
    }

    /// All published messages in delivery order.
    pub fn iter(&self) -> impl Iterator<Item = MessageId> + '_ {
        std::iter::once(self.first).chain(self.rest.iter().copied())
    }
}

/// Progress made by this attempt, excluding earlier retries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryProgress {
    /// No message was published.
    None,
    /// One or more messages were published.
    Partial(PostedMessages),
    /// The failing POST may have published, in addition to this confirmed prefix.
    Unknown(Option<PostedMessages>),
}

/// An attempt that published nothing.
#[derive(Debug, Error)]
#[error("{cause}")]
pub struct TotalFailure {
    #[source]
    pub(crate) cause: DeliveryCause,
    pub(crate) diary: Vec<DiaryRecord>,
}

/// An attempt that must resume from a private remainder.
#[derive(Debug, Error)]
#[error("{cause}")]
pub struct PartialFailure {
    #[source]
    pub(crate) cause: DeliveryCause,
    pub(crate) sent: PostedMessages,
    pub(crate) remainder: String,
    pub(crate) diary: Vec<DiaryRecord>,
}

/// A terminal uncertain attempt, with no authority to retry its failing chunk.
#[derive(Debug, Error)]
#[error("delivery completion unknown: {cause}")]
pub struct UnknownFailure {
    #[source]
    pub(crate) cause: DeliveryCause,
    pub(crate) sent: Option<PostedMessages>,
    pub(crate) diary: Vec<DiaryRecord>,
}

/// A failed attempt with structurally valid retry state.
#[derive(Debug, Error)]
pub enum DeliverError {
    /// Nothing was published.
    #[error(transparent)]
    Total(TotalFailure),
    /// Some chunks were published; retry state is retained privately.
    #[error(transparent)]
    Partial(PartialFailure),
    /// The failing POST may already have published; no remainder is retryable.
    #[error(transparent)]
    Unknown(UnknownFailure),
}

impl DeliverError {
    /// Construct an uncertain attempt, preserving only confirmed progress.
    pub fn unknown(
        cause: DeliveryCause,
        sent: Option<PostedMessages>,
        diary: Vec<DiaryRecord>,
    ) -> Self {
        Self::Unknown(UnknownFailure { cause, sent, diary })
    }

    /// Construct a failure before publication.
    pub fn total(cause: DeliveryCause) -> Self {
        Self::total_with_diary(cause, Vec::new())
    }

    /// Constructs a no-publication failure while retaining deferred diary records.
    pub fn total_with_diary(cause: DeliveryCause, diary: Vec<DiaryRecord>) -> Self {
        Self::Total(TotalFailure { cause, diary })
    }

    /// Construct a partial failure with required nonempty progress and retry content.
    pub fn partial(
        cause: DeliveryCause,
        sent: PostedMessages,
        remainder: String,
        diary: Vec<DiaryRecord>,
    ) -> Self {
        Self::Partial(PartialFailure {
            cause,
            sent,
            remainder,
            diary,
        })
    }

    /// Consumes terminal publication details, discarding private retry remainder.
    ///
    /// Consent retries retain their remainder inside the gate instead of consuming
    /// this terminal projection.
    pub fn into_parts(self) -> (DeliveryCause, DeliveryProgress, Vec<DiaryRecord>) {
        match self {
            Self::Unknown(failure) => (
                failure.cause,
                DeliveryProgress::Unknown(failure.sent),
                failure.diary,
            ),
            Self::Total(failure) => (failure.cause, DeliveryProgress::None, failure.diary),
            Self::Partial(failure) => (
                failure.cause,
                DeliveryProgress::Partial(failure.sent),
                failure.diary,
            ),
        }
    }

    /// The original failure cause.
    pub fn cause(&self) -> &DeliveryCause {
        match self {
            Self::Unknown(failure) => &failure.cause,
            Self::Total(failure) => &failure.cause,
            Self::Partial(failure) => &failure.cause,
        }
    }

    /// Progress made by this attempt.
    pub fn progress(&self) -> DeliveryProgress {
        match self {
            Self::Unknown(failure) => DeliveryProgress::Unknown(failure.sent.clone()),
            Self::Total(_) => DeliveryProgress::None,
            Self::Partial(failure) => DeliveryProgress::Partial(failure.sent.clone()),
        }
    }
}
