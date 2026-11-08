//! Descriptive Discord messaging outcomes, typed failures and MCP JSON projections.
//! These values grant no ingress, consent or delivery authority.
#![warn(missing_docs)]
mod types;
pub use types::{
    ChannelRef, DeliveryReceipt, DmDeliveryRoute, DmOutcome, LinkPreviews, MessagingError,
    MessagingErrorKind, PartialReceipt, RedirectRestrictedSurface, ReplyOutcome, ReplyPing, Report,
    TargetError, TargetOperation,
};
pub mod wire;
