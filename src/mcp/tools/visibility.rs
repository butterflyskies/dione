//! Channel visibility: whether a guild member can view a channel.
//!
//! This follows Discord's documented permission algorithm
//! (<https://discord.com/developers/docs/topics/permissions#permission-overwrites>).
//! The guild owner and members with `ADMINISTRATOR` can view everything.
//! Otherwise, base permissions are the `@everyone` role's permissions OR'd
//! with each of the member's roles. Channel overwrites then apply in order:
//! the `@everyone` overwrite, the member's role overwrites aggregated (all
//! denies, then all allows), and the member's own overwrite. `VIEW_CHANNEL`
//! decides. A thread uses its parent channel's overwrites, and a private
//! thread also requires thread membership or `MANAGE_THREADS`.
//!
//! The computation performs no I/O so the rules can be tested directly.

use serenity::model::{
    Permissions,
    channel::{PermissionOverwrite, PermissionOverwriteType},
    id::{GuildId, RoleId, UserId},
};
use std::collections::HashMap;

/// Whether the visibility computation reached a definite answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CanView {
    Yes,
    No,
    Unknown,
}

/// The rule that decided a visibility answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    NotAMember,
    Owner,
    Administrator,
    BasePermissions,
    EveryoneOverwrite,
    RoleOverwrite,
    MemberOverwrite,
    ManageThreads,
    PrivateThreadMember,
    NotPrivateThreadMember,
    PrivateThreadMembershipUnknown,
    MemberLookupFailed,
}

impl Reason {
    /// The name reported to tool callers.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NotAMember => "not_a_member",
            Self::Owner => "owner",
            Self::Administrator => "administrator",
            Self::BasePermissions => "base_permissions",
            Self::EveryoneOverwrite => "everyone_overwrite",
            Self::RoleOverwrite => "role_overwrite",
            Self::MemberOverwrite => "member_overwrite",
            Self::ManageThreads => "manage_threads",
            Self::PrivateThreadMember => "private_thread_member",
            Self::NotPrivateThreadMember => "not_private_thread_member",
            Self::PrivateThreadMembershipUnknown => "private_thread_membership_unknown",
            Self::MemberLookupFailed => "member_lookup_failed",
        }
    }
}

/// The answer for one user and the rule that decided it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ViewVerdict {
    pub can_view: CanView,
    pub reason: Reason,
}

/// Thread membership, which only matters for a private thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadAccess {
    /// The channel is not a private thread.
    NotRequired,
    /// The channel is a private thread and the user is a thread member.
    Member,
    /// The channel is a private thread and the user is not a thread member.
    NotMember,
    /// The channel is a private thread and membership could not be read.
    Unknown,
}

/// Guild-wide facts shared by every user checked against one channel.
pub(crate) struct ChannelPermissionContext<'a> {
    /// The guild ID, which is also the `@everyone` role ID.
    pub guild_id: GuildId,
    pub owner_id: UserId,
    pub role_permissions: &'a HashMap<RoleId, Permissions>,
    /// The overwrites of the channel, or of the parent channel for a thread.
    pub overwrites: &'a [PermissionOverwrite],
}

/// Decides whether `user_id` can view the channel described by `channel`.
///
/// `member_roles` is `None` when the user is not a guild member.
pub(crate) fn compute_can_view(
    channel: &ChannelPermissionContext<'_>,
    user_id: UserId,
    member_roles: Option<&[RoleId]>,
    thread_access: ThreadAccess,
) -> ViewVerdict {
    let Some(member_roles) = member_roles else {
        return verdict(CanView::No, Reason::NotAMember);
    };
    if user_id == channel.owner_id {
        return verdict(CanView::Yes, Reason::Owner);
    }

    let everyone_role = channel.guild_id.everyone_role();
    let role_permissions = |role_id: &RoleId| {
        channel
            .role_permissions
            .get(role_id)
            .copied()
            .unwrap_or_else(Permissions::empty)
    };
    let base = member_roles
        .iter()
        .map(role_permissions)
        .fold(role_permissions(&everyone_role), |acc, bits| acc | bits);
    if base.contains(Permissions::ADMINISTRATOR) {
        return verdict(CanView::Yes, Reason::Administrator);
    }

    let mut permissions = base;
    let mut view_reason = Reason::BasePermissions;
    let mut apply = |allow: Permissions, deny: Permissions, reason: Reason| {
        if (allow | deny).contains(Permissions::VIEW_CHANNEL) {
            view_reason = reason;
        }
        permissions.remove(deny);
        permissions.insert(allow);
    };

    let overwrite_for = |target: PermissionOverwriteType| {
        channel
            .overwrites
            .iter()
            .find(|overwrite| overwrite.kind == target)
    };
    if let Some(overwrite) = overwrite_for(PermissionOverwriteType::Role(everyone_role)) {
        apply(overwrite.allow, overwrite.deny, Reason::EveryoneOverwrite);
    }
    let (role_allow, role_deny) = channel
        .overwrites
        .iter()
        .filter(|overwrite| match overwrite.kind {
            PermissionOverwriteType::Role(role_id) => {
                role_id != everyone_role && member_roles.contains(&role_id)
            }
            _ => false,
        })
        .fold(
            (Permissions::empty(), Permissions::empty()),
            |(allow, deny), overwrite| (allow | overwrite.allow, deny | overwrite.deny),
        );
    apply(role_allow, role_deny, Reason::RoleOverwrite);
    if let Some(overwrite) = overwrite_for(PermissionOverwriteType::Member(user_id)) {
        apply(overwrite.allow, overwrite.deny, Reason::MemberOverwrite);
    }

    if !permissions.contains(Permissions::VIEW_CHANNEL) {
        return verdict(CanView::No, view_reason);
    }
    match thread_access {
        ThreadAccess::NotRequired => verdict(CanView::Yes, view_reason),
        _ if permissions.contains(Permissions::MANAGE_THREADS) => {
            verdict(CanView::Yes, Reason::ManageThreads)
        }
        ThreadAccess::Member => verdict(CanView::Yes, Reason::PrivateThreadMember),
        ThreadAccess::NotMember => verdict(CanView::No, Reason::NotPrivateThreadMember),
        ThreadAccess::Unknown => verdict(CanView::Unknown, Reason::PrivateThreadMembershipUnknown),
    }
}

fn verdict(can_view: CanView, reason: Reason) -> ViewVerdict {
    ViewVerdict { can_view, reason }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const GUILD: u64 = 100;
    const OWNER: u64 = 1;
    const USER: u64 = 2;
    const ROLE: u64 = 50;
    const OTHER_ROLE: u64 = 51;
    const ADMIN_ROLE: u64 = 52;

    fn role_overwrite(role: u64, allow: Permissions, deny: Permissions) -> PermissionOverwrite {
        PermissionOverwrite {
            allow,
            deny,
            kind: PermissionOverwriteType::Role(RoleId::new(role)),
        }
    }

    fn member_overwrite(user: u64, allow: Permissions, deny: Permissions) -> PermissionOverwrite {
        PermissionOverwrite {
            allow,
            deny,
            kind: PermissionOverwriteType::Member(UserId::new(user)),
        }
    }

    const VIEW: Permissions = Permissions::VIEW_CHANNEL;
    const NONE: Permissions = Permissions::empty();

    fn check(
        everyone: Permissions,
        roles: &[u64],
        overwrites: &[PermissionOverwrite],
        user: u64,
        thread_access: ThreadAccess,
    ) -> (CanView, &'static str) {
        let role_permissions = HashMap::from([
            (RoleId::new(GUILD), everyone),
            (RoleId::new(ROLE), NONE),
            (RoleId::new(OTHER_ROLE), NONE),
            (RoleId::new(ADMIN_ROLE), Permissions::ADMINISTRATOR),
        ]);
        let channel = ChannelPermissionContext {
            guild_id: GuildId::new(GUILD),
            owner_id: UserId::new(OWNER),
            role_permissions: &role_permissions,
            overwrites,
        };
        let member_roles: Vec<RoleId> = roles.iter().copied().map(RoleId::new).collect();
        let result = compute_can_view(
            &channel,
            UserId::new(user),
            Some(&member_roles),
            thread_access,
        );
        (result.can_view, result.reason.as_str())
    }

    #[test_case(VIEW, &[], vec![], CanView::Yes, "base_permissions"; "no overwrites and everyone can view")]
    #[test_case(NONE, &[], vec![], CanView::No, "base_permissions"; "no overwrites and everyone cannot view")]
    #[test_case(
        VIEW, &[ROLE],
        vec![role_overwrite(GUILD, NONE, VIEW), role_overwrite(ROLE, VIEW, NONE)],
        CanView::Yes, "role_overwrite";
        "everyone deny then role allow"
    )]
    #[test_case(
        VIEW, &[],
        vec![role_overwrite(GUILD, NONE, VIEW), role_overwrite(ROLE, VIEW, NONE)],
        CanView::No, "everyone_overwrite";
        "role allow does not apply without the role"
    )]
    #[test_case(
        VIEW, &[ROLE],
        vec![role_overwrite(ROLE, VIEW, NONE), member_overwrite(USER, NONE, VIEW)],
        CanView::No, "member_overwrite";
        "role allow then member deny"
    )]
    #[test_case(
        VIEW, &[ROLE],
        vec![role_overwrite(ROLE, NONE, VIEW), member_overwrite(USER, VIEW, NONE)],
        CanView::Yes, "member_overwrite";
        "role deny then member allow"
    )]
    #[test_case(
        VIEW, &[ROLE, OTHER_ROLE],
        vec![role_overwrite(ROLE, NONE, VIEW), role_overwrite(OTHER_ROLE, VIEW, NONE)],
        CanView::Yes, "role_overwrite";
        "role allow wins over another role deny"
    )]
    #[test_case(
        NONE, &[ADMIN_ROLE],
        vec![role_overwrite(ADMIN_ROLE, NONE, VIEW), member_overwrite(USER, NONE, VIEW)],
        CanView::Yes, "administrator";
        "administrator bypasses deny overwrites"
    )]
    fn overwrites_decide_view_channel(
        everyone: Permissions,
        roles: &[u64],
        overwrites: Vec<PermissionOverwrite>,
        expected: CanView,
        reason: &str,
    ) {
        assert_eq!(
            check(
                everyone,
                roles,
                &overwrites,
                USER,
                ThreadAccess::NotRequired
            ),
            (expected, reason)
        );
    }

    #[test]
    fn owner_bypasses_deny_overwrites() {
        let overwrites = [
            role_overwrite(GUILD, NONE, VIEW),
            member_overwrite(OWNER, NONE, VIEW),
        ];
        assert_eq!(
            check(NONE, &[], &overwrites, OWNER, ThreadAccess::NotRequired),
            (CanView::Yes, "owner")
        );
        assert_eq!(
            check(NONE, &[], &overwrites, USER, ThreadAccess::NotRequired),
            (CanView::No, "everyone_overwrite")
        );
    }

    #[test]
    fn non_member_cannot_view() {
        let role_permissions = HashMap::from([(RoleId::new(GUILD), VIEW)]);
        let channel = ChannelPermissionContext {
            guild_id: GuildId::new(GUILD),
            owner_id: UserId::new(OWNER),
            role_permissions: &role_permissions,
            overwrites: &[],
        };
        let result = compute_can_view(&channel, UserId::new(USER), None, ThreadAccess::NotRequired);
        assert_eq!(
            (result.can_view, result.reason.as_str()),
            (CanView::No, "not_a_member")
        );
    }

    #[test]
    fn everyone_overwrite_is_not_aggregated_with_role_overwrites() {
        // Discord never lists `@everyone` in a member's roles, so on real data
        // the `@everyone` guard in the role aggregate is unobservable and a
        // mutant that drops it is equivalent. The guard matters only if a
        // role list does include the `@everyone` ID: the `@everyone`
        // overwrite must then still apply once, before role overwrites, and
        // its allow must not join the role aggregate where it would override
        // a role deny.
        let overwrites = [
            role_overwrite(GUILD, VIEW, NONE),
            role_overwrite(ROLE, NONE, VIEW),
        ];
        assert_eq!(
            check(
                NONE,
                &[GUILD, ROLE],
                &overwrites,
                USER,
                ThreadAccess::NotRequired
            ),
            (CanView::No, "role_overwrite")
        );
    }

    #[test]
    fn thread_inherits_parent_overwrites() {
        // The caller passes the parent channel's overwrites for a public thread.
        let parent = [role_overwrite(ROLE, VIEW, NONE)];
        assert_eq!(
            check(NONE, &[ROLE], &parent, USER, ThreadAccess::NotRequired),
            (CanView::Yes, "role_overwrite")
        );
        assert_eq!(
            check(NONE, &[], &parent, USER, ThreadAccess::NotRequired),
            (CanView::No, "base_permissions")
        );
    }

    #[test_case(ThreadAccess::Unknown, CanView::Unknown, "private_thread_membership_unknown"; "membership unknown")]
    #[test_case(ThreadAccess::Member, CanView::Yes, "private_thread_member"; "thread member")]
    #[test_case(ThreadAccess::NotMember, CanView::No, "not_private_thread_member"; "not a thread member")]
    fn private_thread_requires_membership(access: ThreadAccess, expected: CanView, reason: &str) {
        assert_eq!(check(VIEW, &[], &[], USER, access), (expected, reason));
    }

    #[test]
    fn private_thread_needs_parent_view_before_membership() {
        assert_eq!(
            check(NONE, &[], &[], USER, ThreadAccess::Member),
            (CanView::No, "base_permissions")
        );
    }

    #[test]
    fn manage_threads_sees_private_threads_without_membership() {
        let overwrites = [role_overwrite(ROLE, Permissions::MANAGE_THREADS, NONE)];
        assert_eq!(
            check(VIEW, &[ROLE], &overwrites, USER, ThreadAccess::Unknown),
            (CanView::Yes, "manage_threads")
        );
    }
}
