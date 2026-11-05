//! Source-qualified provider identities and descriptive context observations.
//!
//! These values describe provider data. They do not authenticate recipients,
//! prove current access, or authorize actions.
#![warn(missing_docs)]

pub mod context;
mod identity;
pub mod observation;

pub use identity::{
    ActorId, ActorRef, ConversationId, ConversationRef, IdentityError, IdentityErrorKind,
    LocalIdentityRef, MessageId, MessageRef, ModuleInstanceId, ModuleSlotId, ProviderId,
    WorkspaceId, WorkspaceRef,
};
