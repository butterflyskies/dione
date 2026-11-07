//! Teams authentication, ingress, reply authority, and resident delivery.

#![warn(missing_docs)]

mod auth;
pub mod edge;
mod json;
mod reply_store;
pub mod runtime;
mod tools;

pub use auth::{AdmissionPolicy, ChannelPolicy, ProbeError, ProbeErrorKind};
pub(crate) use auth::{
    AuthenticatedConversationReference, AuthenticatedTeamsEnvelope, CorrelatedReplyRequest,
    authenticate_activity, correlated_reply, parse_connector_metadata,
};
pub use edge::TeamsEdgeErrorKind;
pub use reply_store::{InvalidRecordKind, ReplyStoreError, ReplyStoreErrorKind};
pub use runtime::TeamsRuntimeErrorKind;
pub use tools::mcp_tools;

#[cfg(test)]
pub(crate) mod fixture_process;
