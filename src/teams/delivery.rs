//! Teams events and acceptance receipts at the resident delivery boundary.

use super::{ActivityId, ActorId, BotId, ConversationId, ReplyHandle, TenantId};
use crate::{delivery::ProviderEventKey, timestamp::Timestamp};
use std::{future::Future, pin::Pin, sync::Arc};
use thiserror::Error;

pub(crate) struct TeamsResidentEvent {
    pub(crate) provider_key: ProviderEventKey,
    pub(crate) content: String,
    pub(crate) message_id: ActivityId,
    pub(crate) conversation_id: ConversationId,
    pub(crate) channel_id: String,
    pub(crate) user_id: ActorId,
    pub(crate) bot_id: BotId,
    pub(crate) tenant_id: TenantId,
    pub(crate) reply_handle: ReplyHandle,
    pub(crate) timestamp: Timestamp,
}

/// A classified resident failure retaining the adapter's original cause.
#[derive(Debug, Error)]
#[error("{kind}")]
pub struct TeamsDeliveryError {
    /// The resident failure category and source.
    #[source]
    pub kind: TeamsDeliveryErrorKind,
}

/// Failures at the dynamic resident-sink boundary.
#[derive(Debug, Error)]
pub enum TeamsDeliveryErrorKind {
    /// The owner accepted the operation, but its completion response was lost.
    #[error("resident admission completion is unknown")]
    CompletionUnknown(#[source] tokio::task::JoinError),
    /// The command owner accepted admission, but its response channel closed.
    #[error("resident admission completion response was lost")]
    ResponseLost(#[source] tokio::sync::oneshot::error::RecvError),
    /// No current resident route accepts this event.
    #[error("resident route unavailable")]
    RouteUnavailable(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Replay retention has reached its bounded capacity.
    #[error("resident replay capacity exhausted")]
    ReplayCapacity(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Resident persistence failed; the receipt determines whether this event committed.
    #[error("resident storage failed")]
    Storage(#[source] Box<dyn std::error::Error + Send + Sync>),
}

pub(crate) enum TeamsAdmissionOutcome {
    Committed,
    Duplicate,
    CommittedDurabilityUncertain(TeamsDeliveryError),
    /// Retain authority without claiming either insertion or rejection.
    CompletionUnknown(TeamsDeliveryError),
}

pub(crate) trait TeamsEventSink: Send + Sync {
    fn deliver<'a>(
        &'a self,
        event: TeamsResidentEvent,
    ) -> Pin<Box<dyn Future<Output = Result<TeamsAdmissionOutcome, TeamsDeliveryError>> + Send + 'a>>;
}

#[derive(Clone)]
/// Cloneable resident delivery capability supplied when constructing a Teams bridge.
pub struct TeamsDelivery(Arc<dyn TeamsEventSink>);

impl TeamsDelivery {
    pub(crate) fn new(sink: impl TeamsEventSink + 'static) -> Self {
        Self(Arc::new(sink))
    }

    pub(crate) async fn deliver(
        &self,
        event: TeamsResidentEvent,
    ) -> Result<TeamsAdmissionOutcome, TeamsDeliveryError> {
        self.0.deliver(event).await
    }
}
