//! Descriptive metadata tool catalog and bounded request decoding.
use dione_discord_identities::Snowflake;
use dione_discord_introspection::{
    introspection::IntrospectionOperation,
    visibility::{MAX_CAN_VIEW_USER_IDS, VisibilityRequest, VisibilityRequestError},
};
use dione_discord_tool_arguments::{ArgumentError, ArgumentErrorKind, parse_id};
use serde_json::{Value, json};
#[derive(Clone, Copy)]
/// Descriptive MetadataTool metadata; construction grants no execution authority.
pub enum MetadataTool {
    /// Describes the Guilds operation or request shape.
    Guilds,
    /// Describes the Channels operation or request shape.
    Channels,
    /// Describes the Channel operation or request shape.
    Channel,
    /// Describes the User operation or request shape.
    User,
    /// Describes the Member operation or request shape.
    Member,
    /// Describes the Roles operation or request shape.
    Roles,
    /// Describes the Emojis operation or request shape.
    Emojis,
    /// Describes the Visibility operation or request shape.
    Visibility,
    /// Describes the Audience operation or request shape.
    Audience,
}

impl MetadataTool {
    /// Every operation in the existing discovery order.
    pub const ALL: [Self; 9] = [
        Self::Guilds,
        Self::Channels,
        Self::Channel,
        Self::User,
        Self::Member,
        Self::Roles,
        Self::Emojis,
        Self::Visibility,
        Self::Audience,
    ];

    /// Returns the existing MCP tool name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Guilds => "list_guilds",
            Self::Channels => "list_channels",
            Self::Channel => "get_channel",
            Self::User => "get_user",
            Self::Member => "get_member",
            Self::Roles => "list_roles",
            Self::Emojis => "list_emojis",
            Self::Visibility => "can_view_channel",
            Self::Audience => "channel_effective_audience",
        }
    }

    /// Returns the existing discovery description.
    pub fn description(self) -> &'static str {
        match self {
            Self::Guilds => "List guilds the bot is in",
            Self::Channels => "List channels in a guild",
            Self::Channel => {
                "Get channel details: id, name, kind, and for guild channels the permission overwrites (allow/deny as decimal strings) and a thread's parent_id. A thread's permission_overwrites is empty because a thread inherits its parent channel's overwrites."
            }
            Self::User => "Get user information",
            Self::Member => "Get guild member information",
            Self::Roles => "List roles in a guild",
            Self::Emojis => {
                "List custom emoji available in a guild (name, id, animated flag, and the string to use in reactions/messages)"
            }
            Self::Visibility => {
                "Report whether each user can view a guild channel or thread, using Discord's permission overwrite rules; a thread is checked against its parent channel's overwrites, since a thread's own permission_overwrites is empty. Returns can_view true, false, or \"unknown\" with the deciding reason for each user, plus an error field describing the lookup failure when can_view is \"unknown\" because a member or thread-membership lookup failed."
            }
            Self::Audience => {
                "List observed members who can view a guild channel or thread without supplying candidate users. Returns members, unknown_members, and separate census/permission completeness with observation times. Classifies at most 1000 guild members within 30 seconds, fetching at most one additional member to distinguish exhaustion; REST member listing requires application GUILD_MEMBERS access. Private-thread membership is not enumerated: members needing that check remain unknown; owner/admin/MANAGE_THREADS can be definite. This is a non-atomic Discord permission observation, not connector reachability or relay authorization."
            }
        }
    }

    /// Returns the existing argument schema.
    pub fn schema(self) -> Value {
        match self {
            Self::Guilds => json!({"type":"object","properties":{}}),
            Self::Channels | Self::Roles | Self::Emojis => json!({
                "type":"object", "required":["guild_id"],
                "properties":{"guild_id":{"type":"string"}}
            }),
            Self::Channel | Self::Audience => json!({
                "type":"object", "required":["channel_id"],
                "properties":{"channel_id":{"type":"string"}}
            }),
            Self::User => json!({
                "type":"object", "required":["user_id"],
                "properties":{"user_id":{"type":"string"}}
            }),
            Self::Member => json!({
                "type":"object", "required":["guild_id","user_id"],
                "properties":{"guild_id":{"type":"string"},"user_id":{"type":"string"}}
            }),
            Self::Visibility => json!({
                "type":"object", "required":["channel_id","user_ids"],
                "properties":{
                    "channel_id":{"type":"string"},
                    "user_ids":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":MAX_CAN_VIEW_USER_IDS}
                }
            }),
        }
    }

    /// Decodes metadata without checking access or executing an operation.
    pub fn parse(self, arguments: &Value) -> Result<IntrospectionOperation, ArgumentError> {
        Ok(match self {
            Self::Guilds => IntrospectionOperation::Guilds,
            Self::Channels => {
                IntrospectionOperation::Channels(parse_id(arguments, "guild_id")?.guild())
            }
            Self::Channel => {
                IntrospectionOperation::Channel(parse_id(arguments, "channel_id")?.channel())
            }
            Self::User => IntrospectionOperation::User(parse_id(arguments, "user_id")?.user()),
            Self::Member => IntrospectionOperation::Member {
                guild: parse_id(arguments, "guild_id")?.guild(),
                user: parse_id(arguments, "user_id")?.user(),
            },
            Self::Roles => IntrospectionOperation::Roles(parse_id(arguments, "guild_id")?.guild()),
            Self::Emojis => {
                IntrospectionOperation::Emojis(parse_id(arguments, "guild_id")?.guild())
            }
            Self::Visibility => {
                IntrospectionOperation::Visibility(parse_visibility_request(arguments)?)
            }
            Self::Audience => {
                IntrospectionOperation::Audience(parse_id(arguments, "channel_id")?.channel())
            }
        })
    }
}

fn parse_visibility_request(arguments: &Value) -> Result<VisibilityRequest, ArgumentError> {
    let channel = parse_id(arguments, "channel_id")?.channel();
    let values = arguments
        .get("user_ids")
        .ok_or(ArgumentErrorKind::Missing("user_ids"))?
        .as_array()
        .ok_or(ArgumentErrorKind::UserIdsArray)?;
    if values.is_empty() {
        return Err(ArgumentErrorKind::VisibilityRequest(VisibilityRequestError::Empty).into());
    }
    if values.len() > MAX_CAN_VIEW_USER_IDS {
        return Err(ArgumentErrorKind::VisibilityRequest(VisibilityRequestError::TooMany).into());
    }
    let users = values
        .iter()
        .map(|value| {
            let id = match value {
                Value::Number(number) => number.as_u64(),
                Value::String(string) => string.parse::<u64>().ok(),
                _ => None,
            }
            .ok_or(ArgumentErrorKind::UserIdItem)?;
            Snowflake::new(id)
                .map(Snowflake::user)
                .ok_or_else(|| ArgumentErrorKind::UserIdZero.into())
        })
        .collect::<Result<Vec<_>, ArgumentError>>()?;
    VisibilityRequest::new(channel, users)
        .map_err(|error| ArgumentErrorKind::VisibilityRequest(error).into())
}

/// Returns metadata definitions for the protocol contract fixtures.
/// Projects this catalog in its existing discovery order.
pub fn definitions() -> impl Iterator<Item = Value> {
    MetadataTool::ALL.into_iter().map(|operation| {
        crate::tool(
            operation.name(),
            operation.description(),
            operation.schema(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serenity::model::id::{ChannelId, UserId};

    #[test]
    fn visibility_request_preserves_mixed_ids_duplicates_and_count_boundaries() {
        let request =
            parse_visibility_request(&json!({"channel_id":"42","user_ids":["12",34,"12"]}))
                .unwrap();
        assert_eq!(request.channel().get(), 42);
        assert_eq!(
            request
                .users()
                .iter()
                .map(|user| user.get())
                .collect::<Vec<_>>(),
            [12, 34, 12]
        );
        for size in [1, 25] {
            let request =
                parse_visibility_request(&json!({"channel_id":"42","user_ids":vec!["80";size]}))
                    .unwrap();
            assert_eq!(request.users().len(), size);
            assert!(request.users().iter().all(|user| user.get() == 80));
        }
        assert!(VisibilityRequest::new(ChannelId::new(42), vec![]).is_err());
        assert!(VisibilityRequest::new(ChannelId::new(42), vec![UserId::new(80); 26]).is_err());
    }
}
