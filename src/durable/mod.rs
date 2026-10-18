//! Generic durable admission, replay, leasing and accepted-operation ownership.
mod codec;
mod engine;
mod error;
mod identity;
mod options;
mod queue;
pub(crate) mod state;
mod store;
#[cfg(test)]
mod tests;
pub use codec::QueueCodec;
pub use error::{
    AllocationKind, CodecError, CodecErrorKind, QueueError, QueueErrorKind, SnapshotIssue,
};
pub use identity::{
    ConsumerId, DeliveryToken, EndpointId, EventId, IdentityError, IdentityErrorKind,
};
pub use options::QueueOptions;
pub(crate) use queue::BindingController;
pub use queue::{Admission, AdmissionOutcome, DurableQueue, ResidentHandle};
pub use state::{
    ConsumerRegistrationResult, ConsumerStatus, EntryVisibility, HandoffResult, LeasedEvent,
    QueueSnapshot, QueueStatus, RemovalResult, ReplayClass,
};
#[cfg(test)]
pub(crate) use store::PersistFailure;
