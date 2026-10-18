//! Discord metadata reads with typed observations and failure targets.

use super::visibility::VisibilityRequest;
use crate::{config::LoadedConfig, timestamp::Timestamp};
use serenity::{
    http::Http,
    model::{
        Permissions,
        channel::{Channel, ChannelType, PermissionOverwrite},
        id::{ChannelId, EmojiId, GuildId, RoleId, UserId},
    },
};
use std::{fmt, num::NonZeroU16, sync::Arc};
use thiserror::Error;

/// Dependencies selected at tool-call entry.
pub(crate) struct IntrospectionCtx {
    pub(crate) http: Arc<Http>,
    pub(crate) config: Arc<LoadedConfig>,
}

/// A guild visible to the bot.
#[derive(Debug)]
pub(crate) struct GuildObservation {
    pub(crate) id: GuildId,
    pub(crate) name: String,
}

/// A channel returned by a guild listing.
#[derive(Debug)]
pub(crate) struct ListedChannel {
    pub(crate) id: ChannelId,
    pub(crate) name: String,
    pub(crate) kind: ChannelType,
}

/// A channel's category and resolved name.
#[derive(Debug)]
pub(crate) enum ChannelObservation {
    Guild {
        id: ChannelId,
        name: String,
        permission_overwrites: Vec<PermissionOverwrite>,
        thread_parent: Option<ChannelId>,
    },
    Private {
        id: ChannelId,
        name: String,
    },
    Unknown {
        id: ChannelId,
    },
}

/// A Discord user, retaining the optional legacy discriminator.
#[derive(Debug)]
pub(crate) struct UserObservation {
    pub(crate) id: UserId,
    pub(crate) name: String,
    pub(crate) discriminator: Option<NonZeroU16>,
    pub(crate) bot: bool,
}

/// A guild member with a join time localized using the call's configuration.
#[derive(Debug)]
pub(crate) struct MemberObservation {
    pub(crate) user_id: UserId,
    pub(crate) username: String,
    pub(crate) nick: Option<String>,
    pub(crate) roles: Vec<RoleId>,
    pub(crate) joined_at: Option<Timestamp>,
}

/// A role's hierarchy position as normalized by Discord's SDK.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RolePosition(u16);

impl RolePosition {
    /// Returns the numeric hierarchy position for protocol projection.
    pub(crate) fn get(self) -> u16 {
        self.0
    }
}

/// A guild role's displayed metadata.
#[derive(Debug)]
pub(crate) struct RoleObservation {
    pub(crate) id: RoleId,
    pub(crate) name: String,
    pub(crate) position: RolePosition,
    pub(crate) mentionable: bool,
    pub(crate) permissions: Permissions,
}

/// A guild emoji's identity and animation state.
#[derive(Debug)]
pub(crate) struct EmojiObservation {
    pub(crate) id: EmojiId,
    pub(crate) name: String,
    pub(crate) animated: bool,
}

/// A metadata read and its required targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IntrospectionOperation {
    Guilds,
    Channels(GuildId),
    Channel(ChannelId),
    User(UserId),
    Member { guild: GuildId, user: UserId },
    Roles(GuildId),
    Emojis(GuildId),
    Visibility(VisibilityRequest),
}

impl fmt::Display for IntrospectionOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Guilds => f.write_str("list guilds"),
            Self::Channels(guild) => write!(f, "list channels in guild {guild}"),
            Self::Channel(channel) => write!(f, "get channel {channel}"),
            Self::User(user) => write!(f, "get user {user}"),
            Self::Member { guild, user } => write!(f, "get member {user} in guild {guild}"),
            Self::Roles(guild) => write!(f, "list roles in guild {guild}"),
            Self::Emojis(guild) => write!(f, "list emojis in guild {guild}"),
            Self::Visibility(request) => {
                write!(f, "check visibility of channel {}", request.channel())
            }
        }
    }
}

/// A metadata read failure with its typed target and original cause.
#[derive(Debug, Error)]
#[error("failed to {operation}")]
pub(crate) struct IntrospectionError {
    pub(crate) operation: IntrospectionOperation,
    #[source]
    pub(crate) kind: IntrospectionErrorKind,
}

/// The cause of a metadata read failure.
#[derive(Debug, Error)]
pub(crate) enum IntrospectionErrorKind {
    #[error(transparent)]
    Discord(Box<serenity::Error>),
}

impl IntrospectionError {
    fn discord(operation: IntrospectionOperation, cause: serenity::Error) -> Self {
        Self {
            operation,
            kind: IntrospectionErrorKind::Discord(Box::new(cause)),
        }
    }
}

/// Lists the SDK's default page of guilds in response order.
pub(crate) async fn list_guilds(
    ctx: &IntrospectionCtx,
) -> Result<Vec<GuildObservation>, IntrospectionError> {
    let guilds = ctx
        .http
        .get_guilds(None, None)
        .await
        .map_err(|cause| IntrospectionError::discord(IntrospectionOperation::Guilds, cause))?;
    Ok(guilds
        .into_iter()
        .map(|guild| GuildObservation {
            id: guild.id,
            name: guild.name,
        })
        .collect())
}

/// Lists a guild's channels in response order.
pub(crate) async fn list_channels(
    ctx: &IntrospectionCtx,
    guild: GuildId,
) -> Result<Vec<ListedChannel>, IntrospectionError> {
    let channels = ctx.http.get_channels(guild).await.map_err(|cause| {
        IntrospectionError::discord(IntrospectionOperation::Channels(guild), cause)
    })?;
    Ok(channels
        .into_iter()
        .map(|channel| ListedChannel {
            id: channel.id,
            name: channel.name,
            kind: channel.kind,
        })
        .collect())
}

/// Reads a channel, resolving private-channel names through the SDK.
pub(crate) async fn get_channel(
    ctx: &IntrospectionCtx,
    channel: ChannelId,
) -> Result<ChannelObservation, IntrospectionError> {
    let channel = ctx.http.get_channel(channel).await.map_err(|cause| {
        IntrospectionError::discord(IntrospectionOperation::Channel(channel), cause)
    })?;
    Ok(match channel {
        Channel::Guild(channel) => ChannelObservation::Guild {
            id: channel.id,
            name: channel.name,
            permission_overwrites: channel.permission_overwrites,
            thread_parent: if is_thread(channel.kind) {
                channel.parent_id
            } else {
                None
            },
        },
        Channel::Private(channel) => ChannelObservation::Private {
            id: channel.id,
            name: channel.name(),
        },
        channel => ChannelObservation::Unknown { id: channel.id() },
    })
}

/// Reads a user's metadata.
pub(crate) async fn get_user(
    ctx: &IntrospectionCtx,
    user: UserId,
) -> Result<UserObservation, IntrospectionError> {
    let user =
        ctx.http.get_user(user).await.map_err(|cause| {
            IntrospectionError::discord(IntrospectionOperation::User(user), cause)
        })?;
    Ok(UserObservation {
        id: user.id,
        name: user.name,
        discriminator: user.discriminator,
        bot: user.bot,
    })
}

/// Reads a guild member and localizes the optional join time.
pub(crate) async fn get_member(
    ctx: &IntrospectionCtx,
    guild: GuildId,
    user: UserId,
) -> Result<MemberObservation, IntrospectionError> {
    let member = ctx.http.get_member(guild, user).await.map_err(|cause| {
        IntrospectionError::discord(IntrospectionOperation::Member { guild, user }, cause)
    })?;
    let joined_at = member
        .joined_at
        .and_then(|timestamp| match timestamp.to_rfc3339() {
            Some(timestamp) => Some(ctx.config.localize_rfc3339(&timestamp)),
            None => {
                tracing::warn!(
                    user_id = user.get(),
                    "joined_at Timestamp failed to_rfc3339(); omitting field"
                );
                None
            }
        });
    Ok(MemberObservation {
        user_id: member.user.id,
        username: member.user.name,
        nick: member.nick,
        roles: member.roles,
        joined_at,
    })
}

/// Lists a guild's roles in response order.
pub(crate) async fn list_roles(
    ctx: &IntrospectionCtx,
    guild: GuildId,
) -> Result<Vec<RoleObservation>, IntrospectionError> {
    let roles = ctx.http.get_guild_roles(guild).await.map_err(|cause| {
        IntrospectionError::discord(IntrospectionOperation::Roles(guild), cause)
    })?;
    Ok(roles
        .into_iter()
        .map(|role| RoleObservation {
            id: role.id,
            name: role.name,
            position: RolePosition(role.position),
            mentionable: role.mentionable,
            permissions: role.permissions,
        })
        .collect())
}

pub(crate) fn is_thread(kind: ChannelType) -> bool {
    matches!(
        kind,
        ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
    )
}

/// Lists a guild's custom emojis in response order.
pub(crate) async fn list_emojis(
    ctx: &IntrospectionCtx,
    guild: GuildId,
) -> Result<Vec<EmojiObservation>, IntrospectionError> {
    let emojis = ctx.http.get_emojis(guild).await.map_err(|cause| {
        IntrospectionError::discord(IntrospectionOperation::Emojis(guild), cause)
    })?;
    Ok(emojis
        .into_iter()
        .map(|emoji| EmojiObservation {
            id: emoji.id,
            name: emoji.name,
            animated: emoji.animated,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_thread_kinds_count_as_threads() {
        assert!(is_thread(ChannelType::PublicThread));
        assert!(is_thread(ChannelType::PrivateThread));
        assert!(is_thread(ChannelType::NewsThread));
        assert!(!is_thread(ChannelType::Text));
        assert!(!is_thread(ChannelType::Category));
    }
}
