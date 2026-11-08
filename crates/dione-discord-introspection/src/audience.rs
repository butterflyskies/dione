//! Bounded inverse channel visibility using an observed guild member census.

use super::{
    introspection::IntrospectionCtx,
    visibility::{CanView, ViewVerdict, compute_can_view},
    visibility_read::{PreparedVisibility, VisibilityError, prepare_visibility},
};
use dione_messages::timestamp::Timestamp;
use serenity::model::{
    guild::Member,
    id::{ChannelId, GuildId, UserId},
};
use std::time::Duration;
use thiserror::Error;
use tokio::time::{Instant, timeout_at};

/// Maximum number of members examined by one audience read.
pub const MAX_AUDIENCE_MEMBERS: usize = 1_000;
const MEMBER_PAGE_SIZE: u8 = 100;
const MAX_MEMBER_PAGES: usize = 11;
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// A member's permission verdict, or missing role data that prevents a verdict.
#[derive(Debug, Clone, Copy)]
pub enum AudiencePermission {
    /// Observed role facts determine a permission verdict.
    Determined(ViewVerdict),
    /// Missing role facts prevent a verdict.
    MissingRoles,
}

/// Minimal member metadata and effective visibility in the observed context.
#[derive(Debug)]
pub struct AudienceMember {
    pub(crate) user: UserId,
    pub(crate) username: String,
    pub(crate) nick: Option<String>,
    pub(crate) permission: AudiencePermission,
}

impl AudienceMember {
    /// Whether missing facts prevent a definite permission answer.
    pub fn is_unknown(&self) -> bool {
        match self.permission {
            AudiencePermission::Determined(verdict) => verdict.can_view() == CanView::Unknown,
            AudiencePermission::MissingRoles => true,
        }
    }
}

/// Why enumeration stopped; only exhaustion establishes a complete observed census.
#[derive(Debug)]
pub enum CensusEnd {
    /// The observed member listing was exhausted.
    Exhausted,
    /// The member cap stopped enumeration.
    MemberLimit,
    /// The page cap stopped enumeration.
    PageLimit,
    /// The shared read deadline elapsed.
    Deadline,
    /// The SDK page violated ordering or size requirements.
    InvalidPage,
    /// The exhausted listing did not include the guild owner.
    OwnerNotObserved,
    /// The member-listing request failed.
    LookupFailed(Box<serenity::Error>),
}

impl CensusEnd {
    /// Whether this observation reached its completeness contract.
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Exhausted)
    }

    /// Returns the stable tool-facing census stop reason.
    pub fn reason(&self) -> &'static str {
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
pub struct AudienceObservation {
    pub(crate) channel: ChannelId,
    pub(crate) guild: GuildId,
    pub(crate) observed_at: Timestamp,
    pub(crate) completed_at: Timestamp,
    pub(crate) members: Vec<AudienceMember>,
    pub(crate) census_end: CensusEnd,
}

impl AudienceObservation {
    /// Whether this observation reached its completeness contract.
    pub fn is_complete(&self) -> bool {
        self.census_end.is_complete() && !self.members.iter().any(AudienceMember::is_unknown)
    }
}

/// An audience read failed before a usable permission context was available.
#[derive(Debug, Error)]
pub enum AudienceError {
    #[error(transparent)]
    /// Required channel permission facts could not be observed.
    Visibility(#[from] VisibilityError),
    #[error("channel audience read deadline elapsed")]
    /// The shared deadline elapsed before permission facts were available.
    Deadline,
}

/// Enumerates observed members and reuses the candidate visibility evaluator.
pub async fn channel_effective_audience(
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

impl AudienceMember {
    /// The observed member ID.
    pub fn user(&self) -> UserId {
        self.user
    }
    /// The observed username.
    pub fn username(&self) -> &str {
        &self.username
    }
    /// The optional observed nickname.
    pub fn nick(&self) -> Option<&str> {
        self.nick.as_deref()
    }
    /// The observed permission or missing-role uncertainty.
    pub fn permission(&self) -> AudiencePermission {
        self.permission
    }
}

impl AudienceObservation {
    /// The channel whose audience was observed.
    pub fn channel(&self) -> ChannelId {
        self.channel
    }
    /// The observed guild ID.
    pub fn guild(&self) -> GuildId {
        self.guild
    }
    /// When the bounded observation began.
    pub fn observed_at(&self) -> &Timestamp {
        &self.observed_at
    }
    /// When the bounded observation ended.
    pub fn completed_at(&self) -> &Timestamp {
        &self.completed_at
    }
    /// Members examined before the census stopped.
    pub fn members(&self) -> &[AudienceMember] {
        &self.members
    }
    /// Why the non-atomic member census stopped.
    pub fn census_end(&self) -> &CensusEnd {
        &self.census_end
    }
}
