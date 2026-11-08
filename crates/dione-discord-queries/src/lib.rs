//! Authorized Discord message reads, search, attachments and observation projections.
#![warn(missing_docs)]
pub mod queries;
pub mod search;
pub mod search_wire;
#[cfg(test)]
mod test_support;
pub mod wire;
use camino::Utf8PathBuf;
use dione_config::LoadedConfig;
use dione_discord_state::State;
use dione_pre_send::ConstructId;
use serenity::{http::Http, model::id::ChannelId};
use std::sync::Arc;
use thiserror::Error;
/// Call-entry read dependencies without delivery, consent or sealed ingress authority.
pub struct QueryCtx {
    http: Arc<Http>,
    state: State,
    config: Arc<LoadedConfig>,
    state_dir: Utf8PathBuf,
    construct_id: ConstructId,
}
impl QueryCtx {
    /// Binds the current configuration, live channel-policy state and receipt identity.
    pub fn new(
        http: Arc<Http>,
        state: State,
        config: Arc<LoadedConfig>,
        state_dir: Utf8PathBuf,
        construct_id: ConstructId,
    ) -> Self {
        Self {
            http,
            state,
            config,
            state_dir,
            construct_id,
        }
    }
}
/// A failed read, preserving its original typed source and displayed text.
#[derive(Debug, Error)]
#[error("{kind}")]
pub struct QueryError {
    /// The read failure and its original source chain.
    #[source]
    pub kind: QueryErrorKind,
}
impl From<QueryErrorKind> for QueryError {
    fn from(kind: QueryErrorKind) -> Self {
        Self { kind }
    }
}
impl From<queries::ObservationError> for QueryError {
    fn from(source: queries::ObservationError) -> Self {
        QueryErrorKind::Observation(source).into()
    }
}
/// Exhaustive failures reachable from the message read owner.
#[derive(Debug, Error)]
pub enum QueryErrorKind {
    /// Discord returned an invalid observation.
    #[error(transparent)]
    Observation(#[from] queries::ObservationError),
    /// Current channel policy denies the read.
    #[error("channel {0} is not a permitted outbound target")]
    OutboundDenied(ChannelId),
    /// Discord rejected the read.
    #[error(transparent)]
    Discord(#[from] Box<serenity::Error>),
    /// The attachment source message could not be fetched.
    #[error("failed to fetch message: {0}")]
    FetchMessage(#[source] Box<serenity::Error>),
    /// The inbox directory could not be created.
    #[error("failed to create inbox: {0}")]
    CreateInbox(#[source] std::io::Error),
    /// Pagination directions conflict.
    #[error("cannot specify both 'before' and 'after'")]
    ConflictingPagination,
    /// The observed message contains no attachments.
    #[error("message has no attachments")]
    NoAttachments,
}
async fn check_outbound(ctx: &QueryCtx, channel: ChannelId) -> Result<(), QueryError> {
    let state = ctx.state.read().await;
    if dione_discord_gate::OutboundGate::check_channel_with_threads(
        &ctx.config,
        channel.get(),
        &state.dm_channel_ids,
        &state.thread_parents,
    ) {
        Ok(())
    } else {
        Err(QueryErrorKind::OutboundDenied(channel).into())
    }
}
