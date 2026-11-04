//! Final attention delivery fencing and receipts.
pub use super::store_access::AttentionReceiptError;
use super::types::RecordId;
use dione_contracts::{ActorRef, MessageRef};
use futures_util::future::BoxFuture;
use std::{fmt::Debug, sync::Arc};

/// Result known at the app-server request/response boundary. Accepted means
/// Codex acknowledged the turn/start request, not that the recipient saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionDeliveryReceipt {
    /// Codex acknowledged the turn/start request at the transport boundary.
    Accepted,
    /// The transport cannot establish whether the turn/start request was accepted.
    Uncertain,
}

/// Final dispatch denial: temporary evidence loss defers work; invalid evidence rejects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AttentionGuardFailure {
    /// Temporary evidence loss requires deferring the dispatch.
    #[error("attention evidence is temporarily unavailable")]
    Unavailable,
    /// Current evidence no longer permits dispatch of the retained work.
    #[error("attention evidence was invalidated")]
    Invalidated,
}

/// Final lifecycle fence and durable outcome observer for managed work.
pub trait AttentionDeliveryGuard: Send + Sync + Debug {
    /// Checks current authority immediately before dispatch; must not wait for blocking I/O.
    fn check(&self, descriptor: &AttentionDescriptor<'_>) -> Result<(), AttentionGuardFailure>;
    /// Durably records a transport-boundary outcome before queue acknowledgement continues.
    ///
    /// The owned guard lets implementations move blocking persistence into an
    /// awaited worker without borrowing the delivery task.
    fn receipt<'a>(
        self: Arc<Self>,
        record: &'a RecordId,
        outcome: AttentionDeliveryReceipt,
    ) -> BoxFuture<'a, Result<(), AttentionReceiptError>>;
}

/// The scheduling directive bound to the recorded admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionDirective {
    /// Prompt the recipient at a safe idle boundary.
    Prompt,
    /// Deliver at the recipient's next safe turn.
    NextTurn,
}

/// Observed source evidence for a final authority check, not an authorization token.
#[derive(Debug, Clone, Copy)]
pub struct AttentionDescriptor<'a> {
    record: &'a RecordId,
    directive: AttentionDirective,
    source: &'a MessageRef,
    author: &'a ActorRef,
    original_text: &'a str,
}

impl<'a> AttentionDescriptor<'a> {
    /// Combines the retained source observation with its recorded admission.
    pub fn new(
        record: &'a RecordId,
        directive: AttentionDirective,
        source: &'a MessageRef,
        author: &'a ActorRef,
        original_text: &'a str,
    ) -> Self {
        Self {
            record,
            directive,
            source,
            author,
            original_text,
        }
    }
    /// Borrows the validated decision identity.
    pub fn record(&self) -> &'a RecordId {
        self.record
    }
    /// Returns the recorded scheduling request.
    pub fn directive(&self) -> AttentionDirective {
        self.directive
    }
    /// Borrows the complete message scope.
    pub fn source(&self) -> &'a MessageRef {
        self.source
    }
    /// Borrows the observed author and its module scope.
    pub fn author(&self) -> &'a ActorRef {
        self.author
    }
    /// Borrows the original UTF-8 source text.
    pub fn original_text(&self) -> &'a str {
        self.original_text
    }
}

/// Owned source evidence for a final guard check; this is not an authorization token.
#[derive(Debug, Clone)]
pub struct OwnedAttentionDescriptor {
    record: RecordId,
    directive: AttentionDirective,
    source: MessageRef,
    author: ActorRef,
    original_text: String,
}

impl OwnedAttentionDescriptor {
    /// Retains the source observation and the recorded scheduling directive.
    pub fn new(
        record: RecordId,
        directive: AttentionDirective,
        source: MessageRef,
        author: ActorRef,
        original_text: String,
    ) -> Self {
        Self {
            record,
            directive,
            source,
            author,
            original_text,
        }
    }
    /// Borrows the retained evidence without changing its identity or authority.
    pub fn descriptor(&self) -> AttentionDescriptor<'_> {
        AttentionDescriptor::new(
            &self.record,
            self.directive,
            &self.source,
            &self.author,
            &self.original_text,
        )
    }
}
