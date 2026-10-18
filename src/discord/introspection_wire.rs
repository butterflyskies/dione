//! MCP result projection for Discord metadata reads.

use crate::discord::{
    introspection::{
        ChannelObservation, EmojiObservation, GuildObservation, IntrospectionError,
        IntrospectionErrorKind, ListedChannel, MemberObservation, RoleObservation, UserObservation,
    },
    visibility::{CanView, Reason, ViewVerdict},
    visibility_read::{VisibilityError, VisibilityObservation, VisibilityOutcome},
};
use serde_json::{Value, json};
use serenity::model::{
    channel::{PermissionOverwrite, PermissionOverwriteType},
    id::UserId,
};

fn error(error: IntrospectionError) -> Value {
    match error.kind {
        IntrospectionErrorKind::Discord(cause) => json!({"error": cause.to_string()}),
    }
}

/// Projects a guild listing into the MCP result shape.
pub(crate) fn guilds(result: Result<Vec<GuildObservation>, IntrospectionError>) -> Value {
    match result {
        Ok(guilds) => json!({"guilds": guilds.into_iter().map(|guild| json!({
            "id": guild.id.to_string(), "name": guild.name,
        })).collect::<Vec<_>>()}),
        Err(cause) => error(cause),
    }
}

/// Projects channel subtypes using the historical SDK Debug vocabulary.
pub(crate) fn channels(result: Result<Vec<ListedChannel>, IntrospectionError>) -> Value {
    match result {
        Ok(channels) => json!({"channels": channels.into_iter().map(|channel| json!({
            "id": channel.id.to_string(), "name": channel.name, "kind": format!("{:?}", channel.kind),
        })).collect::<Vec<_>>()}),
        Err(cause) => error(cause),
    }
}

/// Projects a channel's coarse category and name.
pub(crate) fn channel(result: Result<ChannelObservation, IntrospectionError>) -> Value {
    match result {
        Ok(channel) => match channel {
            ChannelObservation::Guild {
                id,
                name,
                permission_overwrites,
                thread_parent,
            } => {
                let mut value = json!({"id":id.to_string(),"name":name,"kind":"guild",
                        "permission_overwrites":permission_overwrites.iter().map(overwrite_json).collect::<Vec<_>>()});
                if let Some(parent) = thread_parent {
                    value["parent_id"] = json!(parent.to_string());
                }
                value
            }
            ChannelObservation::Private { id, name } => {
                json!({"id":id.to_string(),"name":name,"kind":"private"})
            }
            ChannelObservation::Unknown { id } => {
                json!({"id":id.to_string(),"name":"(unknown)","kind":"unknown"})
            }
        },
        Err(cause) => error(cause),
    }
}

/// Projects a user's optional discriminator without padding.
pub(crate) fn user(result: Result<UserObservation, IntrospectionError>) -> Value {
    match result {
        Ok(user) => json!({
            "id": user.id.to_string(), "name": user.name,
            "discriminator": user.discriminator.map(|discriminator| discriminator.to_string()),
            "bot": user.bot,
        }),
        Err(cause) => error(cause),
    }
}

/// Projects a member, retaining null keys for absent metadata.
pub(crate) fn member(result: Result<MemberObservation, IntrospectionError>) -> Value {
    match result {
        Ok(member) => json!({
            "user_id": member.user_id.to_string(), "username": member.username, "nick": member.nick,
            "roles": member.roles.into_iter().map(|role| role.to_string()).collect::<Vec<_>>(),
            "joined_at": member.joined_at,
        }),
        Err(cause) => error(cause),
    }
}

/// Projects a guild's role metadata.
pub(crate) fn roles(result: Result<Vec<RoleObservation>, IntrospectionError>) -> Value {
    match result {
        Ok(roles) => json!({"roles": roles.into_iter().map(|role| json!({
            "id": role.id.to_string(), "name": role.name,
            "position": role.position.get(), "mentionable": role.mentionable,
            "permissions": role.permissions.bits().to_string(),
        })).collect::<Vec<_>>()}),
        Err(cause) => error(cause),
    }
}

fn overwrite_json(overwrite: &PermissionOverwrite) -> Value {
    let (kind, id) = match overwrite.kind {
        PermissionOverwriteType::Role(role) => ("role", role.get()),
        PermissionOverwriteType::Member(user) => ("member", user.get()),
        _ => ("unknown", 0),
    };
    json!({"type":kind,"id":id.to_string(),"allow":overwrite.allow.bits().to_string(),"deny":overwrite.deny.bits().to_string()})
}

fn verdict_json(user: UserId, verdict: ViewVerdict) -> Value {
    let can_view = match verdict.can_view() {
        CanView::Yes => json!(true),
        CanView::No => json!(false),
        CanView::Unknown => json!("unknown"),
    };
    json!({"user_id":user.to_string(),"can_view":can_view,"reason":verdict.reason().as_str()})
}

/// Projects lookup uncertainty without turning it into a permission denial.
pub(crate) fn visibility(result: Result<VisibilityObservation, VisibilityError>) -> Value {
    match result {
        Err(cause) => json!({"error":cause.kind.to_string()}),
        Ok(observation) => json!({
            "channel_id":observation.channel.to_string(),
            "results":observation.users.into_iter().map(|entry| match entry.outcome {
                VisibilityOutcome::Determined(verdict) => verdict_json(entry.user, verdict),
                VisibilityOutcome::MemberLookupFailed(cause) => json!({
                    "user_id":entry.user.to_string(), "can_view":"unknown",
                    "reason":Reason::MemberLookupFailed.as_str(), "error":cause.to_string(),
                }),
                VisibilityOutcome::ThreadMembershipUnknown(cause) => json!({
                    "user_id":entry.user.to_string(), "can_view":"unknown",
                    "reason":Reason::PrivateThreadMembershipUnknown.as_str(), "error":cause.to_string(),
                }),
            }).collect::<Vec<_>>(),
        }),
    }
}

/// Projects emoji metadata, deriving its mention text at the wire boundary.
pub(crate) fn emojis(result: Result<Vec<EmojiObservation>, IntrospectionError>) -> Value {
    match result {
        Ok(emojis) => json!({"emojis": emojis.into_iter().map(|emoji| json!({
            "id": emoji.id.to_string(), "name": emoji.name, "animated": emoji.animated,
            "use_as": if emoji.animated {
                format!("<a:{}:{}>", emoji.name, emoji.id)
            } else {
                format!("<:{}:{}>", emoji.name, emoji.id)
            },
        })).collect::<Vec<_>>()}),
        Err(cause) => error(cause),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        config::{Config, LoadedConfig},
        discord::introspection::{self, IntrospectionCtx},
    };
    use serenity::model::{
        Permissions,
        id::{ChannelId, GuildId, RoleId, UserId},
    };
    use std::{sync::Arc, time::Duration};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
        time::timeout,
    };

    pub(crate) async fn fixture(
        responses: Vec<(&'static str, Value)>,
    ) -> (IntrospectionCtx, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            timeout(Duration::from_secs(5), async move {
                let mut requests = Vec::new();
                for (status, body) in responses {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut reader = tokio::io::BufReader::new(stream);
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).await.unwrap(), 0, "request line ended at EOF");
                    let mut length = 0;
                    loop {
                        let mut header = String::new();
                        assert_ne!(reader.read_line(&mut header).await.unwrap(), 0, "request headers ended at EOF");
                        if header == "\r\n" { break; }
                        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = value.trim().parse::<usize>().unwrap();
                        }
                    }
                    let mut bytes = vec![0; length];
                    reader.read_exact(&mut bytes).await.unwrap();
                    assert!(bytes.is_empty(), "metadata reads must have no request body");
                    requests.push(line.trim_end().to_owned());
                    let body = body.to_string();
                    let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    reader.get_mut().write_all(response.as_bytes()).await.unwrap();
                }
                requests
            }).await.expect("fake Discord exchange deadline")
        });
        let ctx = IntrospectionCtx {
            http: Arc::new(
                serenity::http::HttpBuilder::new("fake")
                    .proxy(format!("http://{address}"))
                    .ratelimiter_disabled(true)
                    .build(),
            ),
            config: Arc::new(LoadedConfig::from_raw(Config {
                timezone: Some("America/Chicago".into()),
                ..Config::default()
            })),
        };
        (ctx, server)
    }

    #[tokio::test]
    async fn introspection_contract_lists_preserve_order_fields_and_empty_results() {
        let (ctx, server) = fixture(vec![
            ("200 OK", json!([])),
            ("200 OK", json!([
                {"id":"90","name":"Zulu","icon":null,"owner":false,"permissions":"0","features":[]},
                {"id":"12","name":"Alpha","icon":null,"owner":true,"permissions":"8","features":[]}
            ])),
            ("200 OK", json!([])),
            ("200 OK", json!([
                {"id":"90","guild_id":"7","name":"voice","type":2,"position":8,"permission_overwrites":[]},
                {"id":"12","guild_id":"7","name":"text","type":0,"position":0,"permission_overwrites":[]}
            ])),
            ("200 OK", json!([])),
            ("200 OK", json!([
                {"id":"90","name":"Higher","color":0,"colors":{"primary_color":0},"hoist":false,"managed":false,"position":8,"permissions":"1024","mentionable":true},
                {"id":"12","name":"Lower","color":0,"colors":{"primary_color":0},"hoist":false,"managed":false,"position":0,"permissions":"17179869184","mentionable":false}
            ])),
            ("200 OK", json!([])),
            ("200 OK", json!([
                {"id":"90","name":"dance","animated":true,"roles":[],"require_colons":true,"managed":false,"available":true},
                {"id":"12","name":"wave","animated":false,"roles":[],"require_colons":true,"managed":false,"available":true}
            ])),
        ]).await;
        assert_eq!(
            guilds(introspection::list_guilds(&ctx).await),
            json!({"guilds":[]})
        );
        assert_eq!(
            guilds(introspection::list_guilds(&ctx).await),
            json!({"guilds":[{"id":"90","name":"Zulu"},{"id":"12","name":"Alpha"}]})
        );
        assert_eq!(
            channels(introspection::list_channels(&ctx, GuildId::new(7)).await),
            json!({"channels":[]})
        );
        assert_eq!(
            channels(introspection::list_channels(&ctx, GuildId::new(7)).await),
            json!({"channels":[{"id":"90","name":"voice","kind":"Voice"},{"id":"12","name":"text","kind":"Text"}]})
        );
        assert_eq!(
            roles(introspection::list_roles(&ctx, GuildId::new(7)).await),
            json!({"roles":[]})
        );
        assert_eq!(
            roles(introspection::list_roles(&ctx, GuildId::new(7)).await),
            json!({"roles":[{"id":"90","name":"Higher","position":8,"mentionable":true,"permissions":"1024"},{"id":"12","name":"Lower","position":0,"mentionable":false,"permissions":"17179869184"}]})
        );
        assert_eq!(
            emojis(introspection::list_emojis(&ctx, GuildId::new(7)).await),
            json!({"emojis":[]})
        );
        assert_eq!(
            emojis(introspection::list_emojis(&ctx, GuildId::new(7)).await),
            json!({"emojis":[{"id":"90","name":"dance","animated":true,"use_as":"<a:dance:90>"},{"id":"12","name":"wave","animated":false,"use_as":"<:wave:12>"}]})
        );
        assert_eq!(
            server.await.unwrap(),
            [
                "GET /api/v10/users/@me/guilds? HTTP/1.1",
                "GET /api/v10/users/@me/guilds? HTTP/1.1",
                "GET /api/v10/guilds/7/channels HTTP/1.1",
                "GET /api/v10/guilds/7/channels HTTP/1.1",
                "GET /api/v10/guilds/7/roles HTTP/1.1",
                "GET /api/v10/guilds/7/roles HTTP/1.1",
                "GET /api/v10/guilds/7/emojis HTTP/1.1",
                "GET /api/v10/guilds/7/emojis HTTP/1.1",
            ]
        );
    }

    #[tokio::test]
    async fn introspection_contract_channel_categories_and_sdk_private_names() {
        let (ctx, server) = fixture(vec![
            ("200 OK", json!({"id":"91","guild_id":"7","name":"text","type":0,"position":0,"permission_overwrites":[]})),
            ("200 OK", json!({"id":"92","guild_id":"7","name":"voice","type":2,"position":1,"permission_overwrites":[]})),
            ("200 OK", json!({"id":"93","type":1,"recipients":[{"id":"80","username":"legacy","discriminator":"7","avatar":null}]})),
            ("200 OK", json!({"id":"94","type":1,"recipients":[{"id":"81","username":"modern","global_name":"Display name","avatar":null}]})),
        ]).await;
        for expected in [
            json!({"id":"91","name":"text","kind":"guild","permission_overwrites":[]}),
            json!({"id":"92","name":"voice","kind":"guild","permission_overwrites":[]}),
            json!({"id":"93","name":"DM with legacy#0007","kind":"private"}),
            json!({"id":"94","name":"DM with modern","kind":"private"}),
        ] {
            assert_eq!(
                channel(introspection::get_channel(&ctx, ChannelId::new(42)).await),
                expected
            );
        }
        assert_eq!(
            server.await.unwrap(),
            ["GET /api/v10/channels/42 HTTP/1.1"; 4]
        );
    }

    #[tokio::test]
    async fn introspection_contract_user_discriminator_and_returned_identity() {
        let (ctx, server) = fixture(vec![
            (
                "200 OK",
                json!({"id":"91","username":"legacy","discriminator":"7","bot":true,"avatar":null}),
            ),
            (
                "200 OK",
                json!({"id":"92","username":"zero","discriminator":"0","bot":false,"avatar":null}),
            ),
            (
                "200 OK",
                json!({"id":"93","username":"modern","avatar":null}),
            ),
        ])
        .await;
        for expected in [
            json!({"id":"91","name":"legacy","discriminator":"7","bot":true}),
            json!({"id":"92","name":"zero","discriminator":null,"bot":false}),
            json!({"id":"93","name":"modern","discriminator":null,"bot":false}),
        ] {
            assert_eq!(
                user(introspection::get_user(&ctx, UserId::new(80)).await),
                expected
            );
        }
        assert_eq!(server.await.unwrap(), ["GET /api/v10/users/80 HTTP/1.1"; 3]);
    }

    #[tokio::test]
    async fn introspection_contract_member_localization_nulls_and_role_order() {
        let (ctx, server) = fixture(vec![
            ("200 OK", json!({"user":{"id":"91","username":"returned","avatar":null},"nick":"Nickname","roles":["90","12"],"joined_at":"2026-07-10T12:30:00Z","deaf":false,"mute":false,"flags":0})),
            ("200 OK", json!({"user":{"id":"92","username":"no-nick","avatar":null},"nick":null,"roles":[],"joined_at":null,"deaf":false,"mute":false,"flags":0})),
        ]).await;
        assert_eq!(
            member(introspection::get_member(&ctx, GuildId::new(7), UserId::new(80)).await),
            json!({"user_id":"91","username":"returned","nick":"Nickname","roles":["90","12"],"joined_at":"2026-07-10T07:30:00-05:00"})
        );
        assert_eq!(
            member(introspection::get_member(&ctx, GuildId::new(7), UserId::new(80)).await),
            json!({"user_id":"92","username":"no-nick","nick":null,"roles":[],"joined_at":null})
        );
        assert_eq!(
            server.await.unwrap(),
            ["GET /api/v10/guilds/7/members/80 HTTP/1.1"; 2]
        );
    }

    #[tokio::test]
    async fn introspection_contract_all_http_errors_and_malformed_success() {
        let (ctx, server) = fixture(
            vec![
                (
                    "403 Forbidden",
                    json!({"code":50013,"message":"Missing Permissions"})
                );
                7
            ]
            .into_iter()
            .chain([("200 OK", json!({"id":"not-a-snowflake"}))])
            .collect(),
        )
        .await;
        for result in [
            guilds(introspection::list_guilds(&ctx).await),
            channels(introspection::list_channels(&ctx, GuildId::new(7)).await),
            channel(introspection::get_channel(&ctx, ChannelId::new(42)).await),
            user(introspection::get_user(&ctx, UserId::new(80)).await),
            member(introspection::get_member(&ctx, GuildId::new(7), UserId::new(80)).await),
            roles(introspection::list_roles(&ctx, GuildId::new(7)).await),
            emojis(introspection::list_emojis(&ctx, GuildId::new(7)).await),
        ] {
            assert_eq!(result, json!({"error":"Missing Permissions"}));
        }
        let malformed = user(introspection::get_user(&ctx, UserId::new(80)).await);
        assert_eq!(malformed.as_object().unwrap().len(), 1);
        let error = malformed["error"].as_str().unwrap();
        assert!(error.contains("invalid digit found in string"), "{error}");
        assert_eq!(
            server.await.unwrap(),
            [
                "GET /api/v10/users/@me/guilds? HTTP/1.1",
                "GET /api/v10/guilds/7/channels HTTP/1.1",
                "GET /api/v10/channels/42 HTTP/1.1",
                "GET /api/v10/users/80 HTTP/1.1",
                "GET /api/v10/guilds/7/members/80 HTTP/1.1",
                "GET /api/v10/guilds/7/roles HTTP/1.1",
                "GET /api/v10/guilds/7/emojis HTTP/1.1",
                "GET /api/v10/users/80 HTTP/1.1",
            ]
        );
    }

    #[test]
    fn introspection_unknown_channel_projects_fixed_name_and_category() {
        assert_eq!(
            channel(Ok(ChannelObservation::Unknown {
                id: ChannelId::new(42)
            })),
            json!({"id":"42","name":"(unknown)","kind":"unknown"})
        );
    }

    #[tokio::test]
    async fn introspection_sdk_negative_role_position_remains_zero() {
        let (ctx, server) = fixture(vec![("200 OK", json!([
            {"id":"12","name":"Unpositioned","color":0,"colors":{"primary_color":0},"hoist":false,"managed":false,"position":-1,"permissions":"0","mentionable":false}
        ]))]).await;
        assert_eq!(
            roles(introspection::list_roles(&ctx, GuildId::new(7)).await),
            json!({"roles":[{"id":"12","name":"Unpositioned","position":0,"mentionable":false,"permissions":"0"}]})
        );
        assert_eq!(
            server.await.unwrap(),
            ["GET /api/v10/guilds/7/roles HTTP/1.1"]
        );
    }

    #[tokio::test]
    async fn introspection_failures_retain_operation_targets_and_sdk_cause() {
        let (ctx, server) = fixture(vec![
            (
                "403 Forbidden",
                json!({"code":50013,"message":"Missing Permissions"})
            );
            7
        ])
        .await;
        let failures = [
            introspection::list_guilds(&ctx).await.unwrap_err(),
            introspection::list_channels(&ctx, GuildId::new(7))
                .await
                .unwrap_err(),
            introspection::get_channel(&ctx, ChannelId::new(42))
                .await
                .unwrap_err(),
            introspection::get_user(&ctx, UserId::new(80))
                .await
                .unwrap_err(),
            introspection::get_member(&ctx, GuildId::new(7), UserId::new(80))
                .await
                .unwrap_err(),
            introspection::list_roles(&ctx, GuildId::new(7))
                .await
                .unwrap_err(),
            introspection::list_emojis(&ctx, GuildId::new(7))
                .await
                .unwrap_err(),
        ];
        let expected = [
            introspection::IntrospectionOperation::Guilds,
            introspection::IntrospectionOperation::Channels(GuildId::new(7)),
            introspection::IntrospectionOperation::Channel(ChannelId::new(42)),
            introspection::IntrospectionOperation::User(UserId::new(80)),
            introspection::IntrospectionOperation::Member {
                guild: GuildId::new(7),
                user: UserId::new(80),
            },
            introspection::IntrospectionOperation::Roles(GuildId::new(7)),
            introspection::IntrospectionOperation::Emojis(GuildId::new(7)),
        ];
        for (failure, operation) in failures.into_iter().zip(expected) {
            assert_eq!(failure.operation, operation);
            let IntrospectionErrorKind::Discord(cause) = failure.kind;
            assert!(matches!(*cause, serenity::Error::Http(_)));
            assert_eq!(cause.to_string(), "Missing Permissions");
        }
        assert_eq!(
            server.await.unwrap(),
            [
                "GET /api/v10/users/@me/guilds? HTTP/1.1",
                "GET /api/v10/guilds/7/channels HTTP/1.1",
                "GET /api/v10/channels/42 HTTP/1.1",
                "GET /api/v10/users/80 HTTP/1.1",
                "GET /api/v10/guilds/7/members/80 HTTP/1.1",
                "GET /api/v10/guilds/7/roles HTTP/1.1",
                "GET /api/v10/guilds/7/emojis HTTP/1.1",
            ]
        );
    }
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
}
