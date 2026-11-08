//! Discord metadata observations, channel visibility, bounded audience census and wire projection.
#![warn(missing_docs)]
pub mod audience;
pub mod introspection;
pub mod visibility;
#[cfg(test)]
mod visibility_contract_tests;
pub mod visibility_read;
pub mod wire;
