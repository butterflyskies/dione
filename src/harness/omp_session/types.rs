//! Exact OMP lifecycle identities and immutable Teams event payloads.
use crate::{
    delivery::ProviderEventKey,
    durable::{EndpointId, QueueError},
    teams::{
        ActivityId, ActorId, BotId, ConversationId, ReplyHandle, TenantId,
        delivery::TeamsResidentEvent,
    },
    timestamp::Timestamp,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct SavedSessionId(EndpointId);

impl SavedSessionId {
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, OmpSessionError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(OmpSessionErrorKind::InvalidSession.into());
        }
        EndpointId::new(value)
            .map(Self)
            .map_err(|_| OmpSessionErrorKind::InvalidSession.into())
    }
    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }
    pub(super) fn endpoint(&self) -> &EndpointId {
        &self.0
    }
}
impl TryFrom<String> for SavedSessionId {
    type Error = OmpSessionError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}
impl From<SavedSessionId> for String {
    fn from(value: SavedSessionId) -> Self {
        value.as_str().to_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct BindingId(String);
impl BindingId {
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, OmpSessionError> {
        let value = value.into();
        let parsed = Uuid::parse_str(&value).map_err(|_| OmpSessionErrorKind::InvalidBinding)?;
        if parsed.to_string() != value {
            return Err(OmpSessionErrorKind::InvalidBinding.into());
        }
        Ok(Self(value))
    }
    pub(super) fn generate() -> Self {
        Self(Uuid::new_v4().to_string())
    }
}
impl TryFrom<String> for BindingId {
    type Error = OmpSessionError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}
impl From<BindingId> for String {
    fn from(value: BindingId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OmpBinding {
    pub(crate) session_id: SavedSessionId,
    pub(crate) binding_id: BindingId,
}

/// Current authority status for one exact caller-supplied generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum BindingStatus {
    Active,
    Inactive,
    Stale,
    Unavailable,
}

/// A fenced recovery never reveals a replacement controller's identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum BindingRecovery {
    Active(OmpBinding),
    Inactive(OmpBinding),
    Stale,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum OmpPayload {
    Teams {
        content: String,
        message_id: ActivityId,
        conversation_id: ConversationId,
        channel_id: String,
        user_id: ActorId,
        bot_id: BotId,
        tenant_id: TenantId,
        reply_handle: ReplyHandle,
        timestamp: Timestamp,
    },
}
impl OmpPayload {
    pub(super) fn validate(&self) -> Result<(), OmpSessionError> {
        let Self::Teams {
            content,
            channel_id,
            ..
        } = self;
        if content.len() > 262_144
            || channel_id.is_empty()
            || channel_id.len() > 512
            || channel_id.chars().any(char::is_control)
        {
            return Err(OmpSessionErrorKind::EventShape.into());
        }
        Ok(())
    }
    pub(super) fn key(&self) -> ProviderEventKey {
        match self {
            Self::Teams {
                tenant_id,
                bot_id,
                conversation_id,
                message_id,
                ..
            } => ProviderEventKey::teams(
                tenant_id.clone(),
                bot_id.clone(),
                conversation_id.clone(),
                message_id.clone(),
            ),
        }
    }
    pub(super) fn from_event(event: TeamsResidentEvent) -> Result<Self, OmpSessionError> {
        let payload = Self::Teams {
            content: event.content,
            message_id: event.message_id,
            conversation_id: event.conversation_id,
            channel_id: event.channel_id,
            user_id: event.user_id,
            bot_id: event.bot_id,
            tenant_id: event.tenant_id,
            reply_handle: event.reply_handle,
            timestamp: event.timestamp,
        };
        if payload.key() != event.provider_key {
            return Err(OmpSessionErrorKind::EventIdentity.into());
        }
        payload.validate()?;
        Ok(payload)
    }
}

#[derive(Debug, Error)]
#[error("OMP session failed: {kind}")]
pub(crate) struct OmpSessionError {
    #[source]
    pub(crate) kind: OmpSessionErrorKind,
}
#[derive(Debug, Error)]
pub(crate) enum OmpSessionErrorKind {
    #[error("invalid saved session identity")]
    InvalidSession,
    #[error("invalid binding identity")]
    InvalidBinding,
    #[error("event identity disagrees with its native coordinates")]
    EventIdentity,
    #[error("event exceeds the OMP notification contract")]
    EventShape,
    #[error("no initialized OMP binding is ready")]
    Unavailable,
    #[error("binding is stale")]
    StaleBinding,
    #[error("accepted command completion is unknown")]
    CompletionUnknown(#[source] tokio::sync::oneshot::error::RecvError),
    #[error("durable inbox operation failed")]
    Queue(#[source] QueueError),
    #[error("session worker failed")]
    Worker(#[source] tokio::task::JoinError),
    #[error("protocol output failed")]
    Output(#[source] crate::harness::output::ProtocolOutputError),
    #[error("protocol output is unavailable")]
    OutputUnavailable,
}
impl From<OmpSessionErrorKind> for OmpSessionError {
    fn from(kind: OmpSessionErrorKind) -> Self {
        Self { kind }
    }
}
impl From<QueueError> for OmpSessionError {
    fn from(error: QueueError) -> Self {
        OmpSessionErrorKind::Queue(error).into()
    }
}
