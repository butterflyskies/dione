//! Teams authentication, ingress, reply authority, and resident delivery.

mod auth;
pub mod delivery;
pub mod edge;
mod identities;
mod json;
mod reply_handle;
pub(crate) mod reply_store;
pub mod runtime;
mod tools;

pub use auth::{AdmissionPolicy, ChannelPolicy, ProbeError, ProbeErrorKind};
pub(crate) use auth::{
    AuthenticatedConversationReference, AuthenticatedTeamsEnvelope, CorrelatedReplyRequest,
    authenticate_activity, correlated_reply, parse_connector_metadata,
};
pub use edge::TeamsEdgeErrorKind;
pub use identities::{
    ActivityId, ActorId, ApplicationId, BotId, ConversationId, IdentityError, IdentityErrorKind,
    IdentityKind, TenantId,
};
pub use reply_handle::{ReplyHandle, ReplyHandleError, ReplyHandleErrorKind};
pub use reply_store::{InvalidRecordKind, ReplyStoreError, ReplyStoreErrorKind};
pub use runtime::TeamsRuntimeErrorKind;
pub use tools::mcp_tools;
