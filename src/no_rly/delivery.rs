//! Typed publication failures and the private state required to resume a send.

use crate::{contradictionary::DiaryRecord, discord::chunker::FenceChunkError};
use serenity::model::id::{ChannelId, MessageId};
use thiserror::Error;

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

/// A failed attempt with structurally valid retry state.
#[derive(Debug, Error)]
pub enum DeliverError {
    /// Nothing was published.
    #[error(transparent)]
    Total(TotalFailure),
    /// Some chunks were published; retry state is retained privately.
    #[error(transparent)]
    Partial(PartialFailure),
}

impl DeliverError {
    /// Construct a failure before publication.
    pub fn total(cause: DeliveryCause) -> Self {
        Self::total_with_diary(cause, Vec::new())
    }

    pub(crate) fn total_with_diary(cause: DeliveryCause, diary: Vec<DiaryRecord>) -> Self {
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

    /// The original failure cause.
    pub fn cause(&self) -> &DeliveryCause {
        match self {
            Self::Total(failure) => &failure.cause,
            Self::Partial(failure) => &failure.cause,
        }
    }

    /// Progress made by this attempt.
    pub fn progress(&self) -> DeliveryProgress {
        match self {
            Self::Total(_) => DeliveryProgress::None,
            Self::Partial(failure) => DeliveryProgress::Partial(failure.sent.clone()),
        }
    }
}
