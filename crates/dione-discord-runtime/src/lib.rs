//! Registered Discord gateway verification, admission, lifecycle and source runtime.
#![warn(missing_docs)]
mod client;
mod effects;
mod events;
#[cfg(test)]
mod harness_test_support;
mod ingress;
#[cfg(test)]
mod introspection_test_support;
mod observation;
mod rook_observation;
mod source;
mod verified_action;
mod verified_action_runtime;
pub use client::build_client;
pub use effects::create_dm_channel;
pub use events::{GatewayHandler, GatewayServices};
pub use ingress::{IngressLedger, SourceDeliveryFailure, VerifyResult};
pub use source::{SourceFailure, SourceResolver, now_ms};
