//! Shared provider replay identities and descriptive resident delivery contracts.

//! Valid syntax and sink construction do not expose invariant-bearing storage.
//!
//! ```compile_fail
//! use dione_provider_delivery::ReplyHandle;
//! let handle = ReplyHandle("teams-unchecked".to_owned());
//! ```
//!
//! ```compile_fail
//! use dione_provider_delivery::TeamsDelivery;
//! fn expose(delivery: TeamsDelivery) { let _ = delivery.0; }
//! ```
//!
//! ```compile_fail
//! use dione_provider_delivery::ReplyHandle;
//! let handle = ReplyHandle::generate();
//! ```

#![warn(missing_docs)]

mod key;
mod reply_handle;
mod teams;

pub use key::ProviderEventKey;
pub use reply_handle::{ReplyHandle, ReplyHandleError, ReplyHandleErrorKind};
pub use teams::{
    TeamsAdmissionOutcome, TeamsDelivery, TeamsDeliveryError, TeamsDeliveryErrorKind,
    TeamsEventSink, TeamsResidentEvent,
};
