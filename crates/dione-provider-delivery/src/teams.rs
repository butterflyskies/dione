//! Teams events and acceptance receipts at the resident delivery boundary.

use crate::{ProviderEventKey, ReplyHandle};
use dione_messages::timestamp::Timestamp;
use dione_teams_identities::{ActivityId, ActorId, BotId, ConversationId, TenantId};
use std::{future::Future, pin::Pin, sync::Arc};
use thiserror::Error;

/// Descriptive delivery data; native authority remains with the producing adapter.
pub struct TeamsResidentEvent {
    /// The persisted replay identity, independently checked by consumers.
    pub provider_key: ProviderEventKey,
    /// The content delivered to the resident.
    pub content: String,
    /// The native activity identity.
    pub message_id: ActivityId,
    /// The native conversation identity.
    pub conversation_id: ConversationId,
    /// The descriptive delivery channel.
    pub channel_id: String,
    /// The native actor identity.
    pub user_id: ActorId,
    /// The receiving bot identity.
    pub bot_id: BotId,
    /// The containing tenant identity.
    pub tenant_id: TenantId,
    /// A syntactically valid reference, not reply authority.
    pub reply_handle: ReplyHandle,
    /// The activity timestamp.
    pub timestamp: Timestamp,
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

/// Distinct resident acceptance and uncertainty receipts.
pub enum TeamsAdmissionOutcome {
    /// The event was committed.
    Committed,
    /// The replay identity was already accepted.
    Duplicate,
    /// The event is visible, but persistence durability is uncertain.
    CommittedDurabilityUncertain(TeamsDeliveryError),
    /// Retain authority without claiming either insertion or rejection.
    CompletionUnknown(TeamsDeliveryError),
}

/// An owner-provided resident admission sink.
pub trait TeamsEventSink: Send + Sync {
    /// Delivers descriptive event data while borrowing the sink.
    fn deliver<'a>(
        &'a self,
        event: TeamsResidentEvent,
    ) -> Pin<Box<dyn Future<Output = Result<TeamsAdmissionOutcome, TeamsDeliveryError>> + Send + 'a>>;
}

#[derive(Clone)]
/// Cloneable resident delivery capability supplied when constructing a Teams bridge.
pub struct TeamsDelivery(Arc<dyn TeamsEventSink>);

impl TeamsDelivery {
    /// Wraps a concrete resident sink in a cloneable capability.
    pub fn new(sink: impl TeamsEventSink + 'static) -> Self {
        Self(Arc::new(sink))
    }

    /// Delivers through the same sink while retaining its classified outcome.
    pub async fn deliver(
        &self,
        event: TeamsResidentEvent,
    ) -> Result<TeamsAdmissionOutcome, TeamsDeliveryError> {
        self.0.deliver(event).await
    }
}
