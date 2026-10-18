//! Durable recipient handling for trusted application integrations.
//!
//! The embedding application authenticates recipients and operators, verifies source
//! observations, and supplies current authorization and callback transport. This
//! module does not interpret conversation content as configuration or credentials.
//! Production webhook registration is a separate adapter.

mod context;
mod context_observation;
mod model;
mod observation;
mod owner;
mod private_dir;
mod publication;
mod publication_tools;
mod store;
mod tools;

pub use model::{
    AccessDecision, AdmissionOutcome, AttemptId, AuthenticatedRecipient, AuthorizationRevision,
    AuthorizedOperator, BindingGeneration, BindingState, CallbackOutcome, ClaimGeneration,
    ClaimGrant, ClaimOutcome, DeliveryFailure, GrantToken, Limits, OwnerError, OwnerErrorKind,
    ParkReason, RecipientId, RecoveryGeneration, ReplayOutcome, RequestId, SenderFilter,
    Settlement, Subscription, SubscriptionId, WakeRequest,
};
pub use observation::{
    ActorKey, ConversationKey, EventKey, MessageKey, NativeSupplement, ObservationError,
    ObservationErrorKind, RetainedObservation, WorkspaceKey,
};
pub use owner::{
    Authorizer, Clock, EventDisposition, EventStatus, RecipientStatus, RequiredAction, RookHandle,
    RookService, WakeTransport,
};
pub use private_dir::{StorageError, StorageErrorKind};

#[cfg(all(test, unix))]
mod tests;

pub use context::{
    ContextCapabilities, ContextDirection, ContextError, ContextErrorKind, ContextPage,
    ContextProvider, ContextQuery, ContextRequest, ContextResponse, ContextService, PageLimit,
};
pub use context_observation::ContextObservation;
pub use tools::ContextTools;

pub use publication::{
    CanonicalPublication, DispatchOutcome, IdempotencyKey, MentionPolicy, PreparedPublication,
    PublicationBlock, PublicationCapabilities, PublicationDispatch, PublicationError,
    PublicationErrorKind, PublicationProvider, PublicationReceipt, PublicationRejection,
    PublicationState, PublicationStatus, PublicationTarget, PublicationUncertainty, RejectionPhase,
    TextPublication,
};
pub use publication_tools::PublicationTools;
