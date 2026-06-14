use std::sync::Arc;

use serde_json::{Value, json};
use serenity::model::id::{ChannelId, GuildId, UserId};

use crate::config::LoadedConfig;

/// Context for introspection tools.
pub struct IntrospectionCtx {
    pub http: Arc<serenity::http::Http>,
    pub config: Arc<LoadedConfig>,
}

// ── list_guilds ───────────────────────────────────────────────────────────────

pub async fn list_guilds(ctx: &IntrospectionCtx) -> Value {
    match ctx.http.get_guilds(None, None).await {
        Ok(guilds) => {
            let list: Vec<Value> = guilds
                .iter()
                .map(|g| {
                    json!({
                        "id": g.id.get().to_string(),
                        "name": g.name,
                    })
                })
                .collect();
            json!({ "guilds": list })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── list_channels ─────────────────────────────────────────────────────────────

pub async fn list_channels(ctx: &IntrospectionCtx, guild_id: GuildId) -> Value {
    match ctx.http.get_channels(guild_id).await {
        Ok(channels) => {
            let list: Vec<Value> = channels
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id.get().to_string(),
                        "name": c.name,
                        "kind": format!("{:?}", c.kind),
                    })
                })
                .collect();
            json!({ "channels": list })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── get_channel ───────────────────────────────────────────────────────────────

pub async fn get_channel(ctx: &IntrospectionCtx, channel_id: ChannelId) -> Value {
    match ctx.http.get_channel(channel_id).await {
        Ok(channel) => {
            json!({
                "id": channel.id().get().to_string(),
                "name": channel_name(&channel),
                "kind": channel_kind_str(&channel),
            })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── get_user ──────────────────────────────────────────────────────────────────

pub async fn get_user(ctx: &IntrospectionCtx, user_id: UserId) -> Value {
    match ctx.http.get_user(user_id).await {
        Ok(user) => json!({
            "id": user.id.get().to_string(),
            "name": user.name,
            "discriminator": user.discriminator.map(|d| d.to_string()),
            "bot": user.bot,
        }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── get_member ────────────────────────────────────────────────────────────────

pub async fn get_member(ctx: &IntrospectionCtx, guild_id: GuildId, user_id: UserId) -> Value {
    match ctx.http.get_member(guild_id, user_id).await {
        Ok(member) => {
            json!({
                "user_id": member.user.id.get().to_string(),
                "username": member.user.name,
                "nick": member.nick,
                "roles": member.roles.iter().map(|r| r.get().to_string()).collect::<Vec<_>>(),
                "joined_at": member.joined_at.and_then(|t| {
                    match t.to_rfc3339() {
                        Some(ts) => Some(ctx.config.localize_rfc3339(&ts)),
                        None => {
                            tracing::warn!(
                                user_id = user_id.get(),
                                "joined_at Timestamp failed to_rfc3339(); omitting field"
                            );
                            None
                        }
                    }
                }),
            })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── list_roles ────────────────────────────────────────────────────────────────

pub async fn list_roles(ctx: &IntrospectionCtx, guild_id: GuildId) -> Value {
    match ctx.http.get_guild_roles(guild_id).await {
        Ok(roles) => {
            let list: Vec<Value> = roles
                .iter()
                .map(|r| {
                    json!({
                        "id": r.id.get().to_string(),
                        "name": r.name,
                        "position": r.position,
                        "mentionable": r.mentionable,
                    })
                })
                .collect();
            json!({ "roles": list })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn channel_name(channel: &serenity::model::channel::Channel) -> String {
    use serenity::model::channel::Channel;
    match channel {
        Channel::Guild(c) => c.name.clone(),
        Channel::Private(c) => c.name(),
        _ => "(unknown)".to_string(),
    }
}

fn channel_kind_str(channel: &serenity::model::channel::Channel) -> &'static str {
    use serenity::model::channel::Channel;
    match channel {
        Channel::Guild(_) => "guild",
        Channel::Private(_) => "private",
        _ => "unknown",
    }
}

// ── list_emojis ──────────────────────────────────────────────────────────────

pub async fn list_emojis(ctx: &IntrospectionCtx, guild_id: GuildId) -> Value {
    match ctx.http.get_emojis(guild_id).await {
        Ok(emojis) => {
            let list: Vec<Value> = emojis
                .iter()
                .map(|e| {
                    json!({
                        "id": e.id.get().to_string(),
                        "name": e.name,
                        "animated": e.animated,
                        "use_as": if e.animated {
                            format!("<a:{}:{}>", e.name, e.id.get())
                        } else {
                            format!("<:{}:{}>", e.name, e.id.get())
                        },
                    })
                })
                .collect();
            json!({ "emojis": list })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}
