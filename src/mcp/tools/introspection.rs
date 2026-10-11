use crate::{
    config::LoadedConfig,
    mcp::tools::visibility::{
        CanView, ChannelPermissionContext, Reason, ThreadAccess, ViewVerdict, compute_can_view,
    },
};
use serde_json::{Value, json};
use serenity::{
    http::HttpError,
    model::{
        Permissions,
        channel::{Channel, ChannelType, PermissionOverwrite, PermissionOverwriteType},
        guild::{Member, ThreadMember},
        id::{ChannelId, GuildId, RoleId, UserId},
    },
};
use std::{collections::HashMap, sync::Arc};

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
            let mut details = json!({
                "id": channel.id().get().to_string(),
                "name": channel_name(&channel),
                "kind": channel_kind_str(&channel),
            });
            if let Channel::Guild(guild_channel) = &channel {
                details["permission_overwrites"] = guild_channel
                    .permission_overwrites
                    .iter()
                    .map(overwrite_json)
                    .collect();
                if is_thread(guild_channel.kind)
                    && let Some(parent_id) = guild_channel.parent_id
                {
                    details["parent_id"] = json!(parent_id.get().to_string());
                }
            }
            details
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
                        "permissions": r.permissions.bits().to_string(),
                    })
                })
                .collect();
            json!({ "roles": list })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── can_view_channel ──────────────────────────────────────────────────────────

/// Upper bound on user IDs per `can_view_channel` call; each costs a member
/// lookup.
pub(crate) const MAX_CAN_VIEW_USER_IDS: usize = 25;

/// Reports whether each user can view a guild channel or thread.
///
/// See [`compute_can_view`] for the rules. A thread is checked against its
/// parent channel's overwrites. Thread membership is read only for a private
/// thread whose answer depends on it.
pub async fn can_view_channel(
    ctx: &IntrospectionCtx,
    channel_id: ChannelId,
    user_ids: &[UserId],
) -> Value {
    let channel = match ctx.http.get_channel(channel_id).await {
        Ok(Channel::Guild(channel)) => channel,
        Ok(_) => return json!({ "error": "can_view_channel requires a guild channel or thread" }),
        Err(e) => return json!({ "error": e.to_string() }),
    };
    let overwrites = if is_thread(channel.kind) {
        let Some(parent_id) = channel.parent_id else {
            return json!({ "error": "thread has no parent channel" });
        };
        match ctx.http.get_channel(parent_id).await {
            Ok(Channel::Guild(parent)) => parent.permission_overwrites,
            Ok(_) => return json!({ "error": "thread parent is not a guild channel" }),
            Err(e) => return json!({ "error": format!("failed to fetch thread parent: {e}") }),
        }
    } else {
        channel.permission_overwrites
    };
    let guild = match ctx.http.get_guild(channel.guild_id).await {
        Ok(guild) => guild,
        Err(e) => return json!({ "error": format!("failed to fetch guild: {e}") }),
    };
    let role_permissions: HashMap<RoleId, Permissions> = guild
        .roles
        .iter()
        .map(|(id, role)| (*id, role.permissions))
        .collect();
    let context = ChannelPermissionContext {
        guild_id: channel.guild_id,
        owner_id: guild.owner_id,
        role_permissions: &role_permissions,
        overwrites: &overwrites,
    };
    let private_thread = channel.kind == ChannelType::PrivateThread;

    let mut results = Vec::with_capacity(user_ids.len());
    for &user_id in user_ids {
        let member_roles = match member_roles(ctx.http.get_member(channel.guild_id, user_id).await)
        {
            Ok(roles) => roles,
            Err(error) => {
                results.push(json!({
                    "user_id": user_id.get().to_string(),
                    "can_view": "unknown",
                    "reason": Reason::MemberLookupFailed.as_str(),
                    "error": error,
                }));
                continue;
            }
        };
        let initial_access = if private_thread {
            ThreadAccess::Unknown
        } else {
            ThreadAccess::NotRequired
        };
        let mut verdict =
            compute_can_view(&context, user_id, member_roles.as_deref(), initial_access);
        let mut lookup_error = None;
        if verdict.can_view == CanView::Unknown {
            let access = thread_access(ctx, channel_id, user_id)
                .await
                .unwrap_or_else(|e| {
                    lookup_error = Some(e);
                    ThreadAccess::Unknown
                });
            verdict = compute_can_view(&context, user_id, member_roles.as_deref(), access);
        }
        let mut result = verdict_json(user_id, verdict);
        if let Some(error) = lookup_error {
            result["error"] = json!(error);
        }
        results.push(result);
    }
    json!({
        "channel_id": channel_id.get().to_string(),
        "results": results,
    })
}

/// Reads private thread membership.
///
/// A failed lookup yields its error text.
async fn thread_access(
    ctx: &IntrospectionCtx,
    thread_id: ChannelId,
    user_id: UserId,
) -> Result<ThreadAccess, String> {
    thread_membership(
        ctx.http
            .get_thread_channel_member(thread_id, user_id, false)
            .await,
    )
}

/// Maps a guild member lookup to the member's roles, or `None` for a user
/// who is not a guild member.
///
/// A 404 saying the guild itself is gone is a failure, not a non-member.
fn member_roles(lookup: serenity::Result<Member>) -> Result<Option<Vec<RoleId>>, String> {
    match lookup {
        Ok(member) => Ok(Some(member.roles)),
        Err(e) if is_absent_user(&e) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Maps a thread member lookup to the user's private thread access.
///
/// A 404 saying the thread itself is gone, for example deleted mid-call, is a
/// failure, not a non-member.
fn thread_membership(lookup: serenity::Result<ThreadMember>) -> Result<ThreadAccess, String> {
    match lookup {
        Ok(_) => Ok(ThreadAccess::Member),
        Err(e) if is_absent_user(&e) => Ok(ThreadAccess::NotMember),
        Err(e) => Err(e.to_string()),
    }
}

fn verdict_json(user_id: UserId, verdict: ViewVerdict) -> Value {
    let can_view = match verdict.can_view {
        CanView::Yes => json!(true),
        CanView::No => json!(false),
        CanView::Unknown => json!("unknown"),
    };
    json!({
        "user_id": user_id.get().to_string(),
        "can_view": can_view,
        "reason": verdict.reason.as_str(),
    })
}

/// Discord's JSON error code for a channel or thread that does not exist.
const UNKNOWN_CHANNEL: isize = 10003;
/// Discord's JSON error code for a guild that does not exist.
const UNKNOWN_GUILD: isize = 10004;

/// Whether a lookup failed because the user is absent, as opposed to the
/// guild or channel being looked up in no longer existing.
fn is_absent_user(error: &serenity::Error) -> bool {
    matches!(
        error,
        serenity::Error::Http(HttpError::UnsuccessfulRequest(response))
            if response.status_code.as_u16() == 404
                && !matches!(response.error.code, UNKNOWN_CHANNEL | UNKNOWN_GUILD)
    )
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Renders one channel permission overwrite with Discord's field names.
///
/// `allow` and `deny` are decimal strings, as Discord sends them, because the
/// bitfields exceed the integer range that JSON clients handle exactly.
fn overwrite_json(overwrite: &PermissionOverwrite) -> Value {
    let (kind, id) = match overwrite.kind {
        PermissionOverwriteType::Role(role_id) => ("role", role_id.get()),
        PermissionOverwriteType::Member(user_id) => ("member", user_id.get()),
        _ => ("unknown", 0),
    };
    json!({
        "type": kind,
        "id": id.to_string(),
        "allow": overwrite.allow.bits().to_string(),
        "deny": overwrite.deny.bits().to_string(),
    })
}

fn is_thread(kind: ChannelType) -> bool {
    matches!(
        kind,
        ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
    )
}

fn channel_name(channel: &Channel) -> String {
    match channel {
        Channel::Guild(c) => c.name.clone(),
        Channel::Private(c) => c.name(),
        _ => "(unknown)".to_string(),
    }
}

fn channel_kind_str(channel: &Channel) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overwrite_json_reports_role_and_member_targets_with_decimal_bitfields() {
        let role = PermissionOverwrite {
            allow: Permissions::VIEW_CHANNEL,
            deny: Permissions::SEND_MESSAGES,
            kind: PermissionOverwriteType::Role(RoleId::new(11)),
        };
        let member = PermissionOverwrite {
            allow: Permissions::empty(),
            deny: Permissions::VIEW_CHANNEL | Permissions::MANAGE_THREADS,
            kind: PermissionOverwriteType::Member(UserId::new(22)),
        };
        assert_eq!(
            overwrite_json(&role),
            json!({ "type": "role", "id": "11", "allow": "1024", "deny": "2048" })
        );
        assert_eq!(
            overwrite_json(&member),
            json!({ "type": "member", "id": "22", "allow": "0", "deny": "17179870208" })
        );
    }

    #[test]
    fn verdict_json_reports_true_false_or_unknown() {
        let user = UserId::new(7);
        let cases = [
            (CanView::Yes, Reason::Owner, json!(true), "owner"),
            (
                CanView::No,
                Reason::NotAMember,
                json!(false),
                "not_a_member",
            ),
            (
                CanView::Unknown,
                Reason::PrivateThreadMembershipUnknown,
                json!("unknown"),
                "private_thread_membership_unknown",
            ),
        ];
        for (can_view, reason, expected, reason_name) in cases {
            assert_eq!(
                verdict_json(user, ViewVerdict { can_view, reason }),
                json!({ "user_id": "7", "can_view": expected, "reason": reason_name })
            );
        }
    }

    /// Builds the error serenity returns for a Discord 404 with `code`.
    async fn not_found(code: isize) -> serenity::Error {
        let body = json!({ "code": code, "message": "not found" }).to_string();
        let response = axum::http::Response::builder()
            .status(404)
            .body(body)
            .expect("valid response");
        let response = serenity::http::ErrorResponse::from_response(
            reqwest::Response::from(response),
            reqwest::Method::GET,
        )
        .await;
        serenity::Error::Http(HttpError::UnsuccessfulRequest(response))
    }

    const UNKNOWN_MEMBER: isize = 10007;

    #[tokio::test]
    async fn member_404_means_not_a_member() {
        assert_eq!(member_roles(Err(not_found(UNKNOWN_MEMBER).await)), Ok(None));
    }

    #[tokio::test]
    async fn member_404_for_a_missing_guild_is_a_lookup_failure() {
        let result = member_roles(Err(not_found(UNKNOWN_GUILD).await));
        assert!(
            result
                .as_ref()
                .is_err_and(|error| error.contains("not found")),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn thread_member_404_means_not_a_thread_member() {
        assert_eq!(
            thread_membership(Err(not_found(UNKNOWN_MEMBER).await)),
            Ok(ThreadAccess::NotMember)
        );
    }

    #[tokio::test]
    async fn thread_member_404_for_a_deleted_thread_is_a_lookup_failure() {
        let result = thread_membership(Err(not_found(UNKNOWN_CHANNEL).await));
        assert!(
            result
                .as_ref()
                .is_err_and(|error| error.contains("not found")),
            "{result:?}"
        );
    }

    #[test]
    fn only_thread_kinds_count_as_threads() {
        assert!(is_thread(ChannelType::PublicThread));
        assert!(is_thread(ChannelType::PrivateThread));
        assert!(is_thread(ChannelType::NewsThread));
        assert!(!is_thread(ChannelType::Text));
        assert!(!is_thread(ChannelType::Category));
    }
}
