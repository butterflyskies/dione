//! Bounded inverse channel visibility using an observed guild member census.

use super::{
    introspection::IntrospectionCtx,
    visibility::{CanView, ViewVerdict, compute_can_view},
    visibility_read::{PreparedVisibility, VisibilityError, prepare_visibility},
};
use crate::timestamp::Timestamp;
use serenity::model::{
    guild::Member,
    id::{ChannelId, GuildId, UserId},
};
use std::time::Duration;
use thiserror::Error;
use tokio::time::{Instant, timeout_at};

/// Maximum number of members examined by one audience read.
pub(crate) const MAX_AUDIENCE_MEMBERS: usize = 1_000;
const MEMBER_PAGE_SIZE: u8 = 100;
const MAX_MEMBER_PAGES: usize = 11;
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// A member's permission verdict, or missing role data that prevents a verdict.
#[derive(Debug, Clone, Copy)]
pub(crate) enum AudiencePermission {
    Determined(ViewVerdict),
    MissingRoles,
}

/// Minimal member metadata and effective visibility in the observed context.
#[derive(Debug)]
pub(crate) struct AudienceMember {
    pub(crate) user: UserId,
    pub(crate) username: String,
    pub(crate) nick: Option<String>,
    pub(crate) permission: AudiencePermission,
}

impl AudienceMember {
    pub(crate) fn is_unknown(&self) -> bool {
        match self.permission {
            AudiencePermission::Determined(verdict) => verdict.can_view() == CanView::Unknown,
            AudiencePermission::MissingRoles => true,
        }
    }
}

/// Why enumeration stopped; only exhaustion establishes a complete observed census.
#[derive(Debug)]
pub(crate) enum CensusEnd {
    Exhausted,
    MemberLimit,
    PageLimit,
    Deadline,
    InvalidPage,
    OwnerNotObserved,
    LookupFailed(Box<serenity::Error>),
}

impl CensusEnd {
    pub(crate) fn is_complete(&self) -> bool {
        matches!(self, Self::Exhausted)
    }

    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::Exhausted => "end_of_members",
            Self::MemberLimit => "member_limit",
            Self::PageLimit => "page_limit",
            Self::Deadline => "deadline",
            Self::InvalidPage => "invalid_member_page",
            Self::OwnerNotObserved => "guild_owner_not_observed",
            Self::LookupFailed(_) => "member_listing_failed",
        }
    }
}

/// A non-atomic observation, never a grant to disclose content to another room.
#[derive(Debug)]
pub(crate) struct AudienceObservation {
    pub(crate) channel: ChannelId,
    pub(crate) guild: GuildId,
    pub(crate) observed_at: Timestamp,
    pub(crate) completed_at: Timestamp,
    pub(crate) members: Vec<AudienceMember>,
    pub(crate) census_end: CensusEnd,
}

impl AudienceObservation {
    pub(crate) fn is_complete(&self) -> bool {
        self.census_end.is_complete() && !self.members.iter().any(AudienceMember::is_unknown)
    }
}

/// An audience read failed before a usable permission context was available.
#[derive(Debug, Error)]
pub(crate) enum AudienceError {
    #[error(transparent)]
    Visibility(#[from] VisibilityError),
    #[error("channel audience read deadline elapsed")]
    Deadline,
}

/// Enumerates observed members and reuses the candidate visibility evaluator.
pub(crate) async fn channel_effective_audience(
    ctx: &IntrospectionCtx,
    channel: ChannelId,
) -> Result<AudienceObservation, AudienceError> {
    let observed_at = Timestamp::now();
    let deadline = Instant::now() + READ_TIMEOUT;
    let prepared = timeout_at(deadline, prepare_visibility(ctx, channel))
        .await
        .map_err(|_| AudienceError::Deadline)??;
    let mut members = Vec::new();
    let mut after = None;
    let mut pages = 0;
    let census_end = loop {
        if pages == MAX_MEMBER_PAGES {
            break CensusEnd::PageLimit;
        }
        pages += 1;
        let remaining = MAX_AUDIENCE_MEMBERS - members.len();
        // Saturating larger counts preserves the page cap; subsequent conversions widen.
        let limit = u8::try_from(remaining)
            .unwrap_or(u8::MAX)
            .clamp(1, MEMBER_PAGE_SIZE);
        let page = match timeout_at(
            deadline,
            ctx.http.get_guild_members(
                prepared.guild,
                Some(u64::from(limit)),
                after.map(UserId::get),
            ),
        )
        .await
        {
            Err(_) => break CensusEnd::Deadline,
            Ok(Err(cause)) => break CensusEnd::LookupFailed(Box::new(cause)),
            Ok(Ok(page)) => page,
        };
        if !valid_page(&page, usize::from(limit), after) {
            break CensusEnd::InvalidPage;
        }
        if page.is_empty() {
            break if members
                .iter()
                .any(|member: &AudienceMember| member.user == prepared.owner)
            {
                CensusEnd::Exhausted
            } else {
                CensusEnd::OwnerNotObserved
            };
        }
        if remaining == 0 {
            break CensusEnd::MemberLimit;
        }
        after = page.iter().map(|member| member.user.id).max();
        members.extend(page.into_iter().map(|member| classify(&prepared, member)));
    };
    Ok(AudienceObservation {
        channel,
        guild: prepared.guild,
        observed_at,
        completed_at: Timestamp::now(),
        members,
        census_end,
    })
}

fn valid_page(page: &[Member], limit: usize, after: Option<UserId>) -> bool {
    page.len() <= limit
        && page.iter().enumerate().all(|(index, member)| {
            after.is_none_or(|after| member.user.id > after)
                && !page[..index]
                    .iter()
                    .any(|previous| previous.user.id == member.user.id)
        })
}

fn classify(prepared: &PreparedVisibility, member: Member) -> AudienceMember {
    let missing_roles = !prepared.roles.contains_key(&prepared.guild.everyone_role())
        || member
            .roles
            .iter()
            .any(|role| !prepared.roles.contains_key(role));
    let permission = if member.user.id != prepared.owner && missing_roles {
        AudiencePermission::MissingRoles
    } else {
        AudiencePermission::Determined(compute_can_view(
            &prepared.context(),
            member.user.id,
            Some(&member.roles),
            prepared.initial_access(),
        ))
    };
    AudienceMember {
        user: member.user.id,
        username: member.user.name,
        nick: member.nick,
        permission,
    }
}
