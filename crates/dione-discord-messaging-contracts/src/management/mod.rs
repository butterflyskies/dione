//! Descriptive management results and typed errors without execution authority.

use crate::TargetError;
use serenity::model::id::ChannelId;
use thiserror::Error;

/// Management result and error JSON projection.
pub mod wire;

/// Descriptive Discord-returned thread metadata, not authority to act on the thread.
#[derive(Debug)]
pub struct CreatedThread {
    /// The returned thread identifier.
    pub id: ChannelId,
    /// The returned thread name.
    pub name: String,
}

/// A management failure retaining its requested channel and original cause.
#[derive(Debug, Error)]
#[error("management operation failed in channel {channel}")]
pub struct ManagementError {
    /// The requested channel.
    pub channel: ChannelId,
    /// The typed failure and source chain.
    #[source]
    pub kind: ManagementErrorKind,
}

/// Categories of management failure with their original causes.
#[derive(Debug, Error)]
pub enum ManagementErrorKind {
    /// Outbound configuration denied the requested channel.
    #[error("channel not in allowlist")]
    OutboundDenied,
    /// Target verification rejected the request.
    #[error(transparent)]
    Target(TargetError),
    /// The Discord SDK rejected the operation.
    #[error(transparent)]
    Discord(Box<serenity::Error>),
}
