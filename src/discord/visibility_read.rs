//! Discord visibility observations with concrete lookup failures preserved until projection.

use super::{
    introspection::{IntrospectionCtx, is_thread},
    visibility::{
        CanView, ChannelPermissionContext, ThreadAccess, ViewVerdict, VisibilityRequest,
        compute_can_view,
    },
};
use serenity::{
    http::HttpError,
    model::{
        Permissions,
        channel::{Channel, ChannelType},
        guild::{Member, ThreadMember},
        id::{ChannelId, RoleId, UserId},
    },
};
use std::collections::HashMap;
use thiserror::Error;

/// Ordered visibility observations for one requested channel.
#[derive(Debug)]
pub(crate) struct VisibilityObservation {
    pub(crate) channel: ChannelId,
    pub(crate) users: Vec<UserVisibility>,
}

/// A user's visibility or the concrete cause preventing its determination.
#[derive(Debug)]
pub(crate) struct UserVisibility {
    pub(crate) user: UserId,
    pub(crate) outcome: VisibilityOutcome,
}

/// Lookup uncertainty is distinct from a permission denial.
#[derive(Debug)]
pub(crate) enum VisibilityOutcome {
    Determined(ViewVerdict),
    MemberLookupFailed(Box<serenity::Error>),
    ThreadMembershipUnknown(Box<serenity::Error>),
}

/// A channel visibility read failed before per-user evaluation.
#[derive(Debug, Error)]
#[error("failed to check visibility of channel {channel}: {kind}")]
pub(crate) struct VisibilityError {
    pub(crate) channel: ChannelId,
    #[source]
    pub(crate) kind: VisibilityErrorKind,
}

/// The failed visibility prerequisite and its original cause.
#[derive(Debug, Error)]
pub(crate) enum VisibilityErrorKind {
    #[error(transparent)]
    Channel(Box<serenity::Error>),
    #[error("can_view_channel requires a guild channel or thread")]
    NotGuildChannel,
    #[error("thread has no parent channel")]
    MissingParent,
    #[error("thread parent is not a guild channel")]
    NotGuildParent,
    #[error("failed to fetch thread parent: {0}")]
    Parent(#[source] Box<serenity::Error>),
    #[error("failed to fetch guild: {0}")]
    Guild(#[source] Box<serenity::Error>),
}

/// Evaluates users in request order and reads private membership only when decisive.
pub(crate) async fn can_view_channel(
    ctx: &IntrospectionCtx,
    request: &VisibilityRequest,
) -> Result<VisibilityObservation, VisibilityError> {
    let channel_id = request.channel();
    let fail = |kind| VisibilityError {
        channel: channel_id,
        kind,
    };
    let channel = match ctx.http.get_channel(channel_id).await {
        Ok(Channel::Guild(channel)) => channel,
        Ok(_) => return Err(fail(VisibilityErrorKind::NotGuildChannel)),
        Err(cause) => return Err(fail(VisibilityErrorKind::Channel(Box::new(cause)))),
    };
    let overwrites = if is_thread(channel.kind) {
        let parent = channel
            .parent_id
            .ok_or_else(|| fail(VisibilityErrorKind::MissingParent))?;
        match ctx.http.get_channel(parent).await {
            Ok(Channel::Guild(parent)) => parent.permission_overwrites,
            Ok(_) => return Err(fail(VisibilityErrorKind::NotGuildParent)),
            Err(cause) => return Err(fail(VisibilityErrorKind::Parent(Box::new(cause)))),
        }
    } else {
        channel.permission_overwrites
    };
    let guild = ctx
        .http
        .get_guild(channel.guild_id)
        .await
        .map_err(|cause| fail(VisibilityErrorKind::Guild(Box::new(cause))))?;
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
    let initial_access = if channel.kind == ChannelType::PrivateThread {
        ThreadAccess::Unknown
    } else {
        ThreadAccess::NotRequired
    };
    let mut results = Vec::with_capacity(request.users().len());
    for &user in request.users() {
        let outcome = match member_roles(ctx.http.get_member(channel.guild_id, user).await) {
            Err(cause) => VisibilityOutcome::MemberLookupFailed(cause),
            Ok(roles) => {
                let verdict = compute_can_view(&context, user, roles.as_deref(), initial_access);
                if verdict.can_view() == CanView::Unknown {
                    match thread_membership(
                        ctx.http
                            .get_thread_channel_member(channel_id, user, false)
                            .await,
                    ) {
                        Ok(access) => VisibilityOutcome::Determined(compute_can_view(
                            &context,
                            user,
                            roles.as_deref(),
                            access,
                        )),
                        Err(cause) => VisibilityOutcome::ThreadMembershipUnknown(cause),
                    }
                } else {
                    VisibilityOutcome::Determined(verdict)
                }
            }
        };
        results.push(UserVisibility { user, outcome });
    }
    Ok(VisibilityObservation {
        channel: channel_id,
        users: results,
    })
}

/// A missing guild is a failed lookup, not proof that the user is a non-member.
fn member_roles(
    lookup: serenity::Result<Member>,
) -> Result<Option<Vec<RoleId>>, Box<serenity::Error>> {
    match lookup {
        Ok(member) => Ok(Some(member.roles)),
        Err(cause) if is_absent_user(&cause) => Ok(None),
        Err(cause) => Err(Box::new(cause)),
    }
}

/// A deleted thread is uncertain visibility, not a negative membership answer.
fn thread_membership(
    lookup: serenity::Result<ThreadMember>,
) -> Result<ThreadAccess, Box<serenity::Error>> {
    match lookup {
        Ok(_) => Ok(ThreadAccess::Member),
        Err(cause) if is_absent_user(&cause) => Ok(ThreadAccess::NotMember),
        Err(cause) => Err(Box::new(cause)),
    }
}

const UNKNOWN_CHANNEL: isize = 10003;
const UNKNOWN_GUILD: isize = 10004;

fn is_absent_user(error: &serenity::Error) -> bool {
    matches!(error, serenity::Error::Http(HttpError::UnsuccessfulRequest(response))
        if response.status_code.as_u16() == 404 && !matches!(response.error.code, UNKNOWN_CHANNEL | UNKNOWN_GUILD))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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
        assert_eq!(
            member_roles(Err(not_found(UNKNOWN_MEMBER).await)).map_err(|e| e.to_string()),
            Ok(None)
        );
    }

    #[tokio::test]
    async fn member_404_for_a_missing_guild_is_a_lookup_failure() {
        let result = member_roles(Err(not_found(UNKNOWN_GUILD).await));
        assert!(
            result
                .as_ref()
                .is_err_and(|error| error.to_string().contains("not found")),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn thread_member_404_means_not_a_thread_member() {
        assert_eq!(
            thread_membership(Err(not_found(UNKNOWN_MEMBER).await)).map_err(|e| e.to_string()),
            Ok(ThreadAccess::NotMember)
        );
    }

    #[tokio::test]
    async fn thread_member_404_for_a_deleted_thread_is_a_lookup_failure() {
        let result = thread_membership(Err(not_found(UNKNOWN_CHANNEL).await));
        assert!(
            result
                .as_ref()
                .is_err_and(|error| error.to_string().contains("not found")),
            "{result:?}"
        );
    }
}
