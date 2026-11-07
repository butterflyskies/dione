//! Shared observed messages, typed deliveries, and presentation values.
//!
//! These values describe supplied observations and scheduling requests. The
//! application retains current access, permission, and dispatch authority.
#![warn(missing_docs)]

pub mod delivery;
pub mod events;
pub mod identity_display;
/// Fixed-offset timestamps and compact formatting without application configuration.
pub mod timestamp;
