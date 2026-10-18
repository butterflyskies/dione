//! Platform-independent scheduling identities.

pub(crate) mod context;
mod identity;
pub(crate) mod identity_display;
pub(crate) mod observation;

pub use identity::{
    ActorId, ActorRef, ConversationId, ConversationRef, IdentityError, IdentityErrorKind,
    MessageId, MessageRef, ModuleInstanceId, ModuleSlotId, ProviderId, WorkspaceId, WorkspaceRef,
};

pub(crate) mod delivery;
pub(crate) mod events;

pub(crate) use identity::LocalIdentityRef;
