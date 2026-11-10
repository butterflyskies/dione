//! Sealed, dormant authority-receipt fixture and exact reaction-command parser.
//!
//! No production ingress or effect is registered. Source and policy evidence
//! construction remains private until a trusted runtime integration is designed.
#![warn(missing_docs)]

#[expect(dead_code, reason = "sealed receipt-gate fixture has no registered runtime caller")]
mod store;
