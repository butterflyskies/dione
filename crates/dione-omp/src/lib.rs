//! Child-owned OMP inbox, lifecycle and event adapter.
#![warn(missing_docs)]
mod actor;
mod adapter;
mod codec;
mod types;

pub use actor::{OmpAdmission, OmpControl, OmpSession, RunningOmpSession};
pub use adapter::connect;
pub use types::{OmpSessionError, OmpSessionErrorKind};

mod teams_delivery;
pub use teams_delivery::teams_delivery;
