//! Source-bound attention judgments, metadata, learning, and delivery contracts.
//!
//! The application retains current source access, export consent, scheduling,
//! and transport. Retained source identities do not establish current authority.
#![warn(missing_docs)]

pub mod config;
pub mod delivery;
pub mod learning;
pub mod provider;
mod record_id;
pub mod store;
pub mod store_access;
pub mod types;
