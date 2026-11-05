//! Child-owned OMP inbox, lifecycle and event adapter.
mod actor;
mod adapter;
mod codec;
mod types;

pub use actor::{OmpAdmission, OmpControl, OmpSession, RunningOmpSession};
pub use adapter::connect;
pub use types::{OmpSessionError, OmpSessionErrorKind};
