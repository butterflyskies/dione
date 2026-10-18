//! Child-owned OMP inbox with exclusive, cancellation-independent command ownership.
mod actor;
mod codec;
mod types;

pub(crate) use actor::{OmpAdmission, OmpControl, OmpSession, RunningOmpSession};
pub(crate) use types::{BindingId, OmpSessionError, OmpSessionErrorKind, SavedSessionId};
