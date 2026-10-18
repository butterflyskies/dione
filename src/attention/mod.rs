//! Recipient-controlled source-bound attention.

pub mod admission;
pub mod config;
pub mod control;
pub mod learning;
pub mod provider;
pub mod runtime;
pub mod source;
pub mod store;
pub mod types;

pub mod delivery;

mod record_id;
pub mod store_access;
