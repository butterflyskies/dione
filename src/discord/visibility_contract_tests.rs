//! Main visibility contracts exercised through the current Discord registrations and wire path.

use super::{
    introspection::{self, IntrospectionCtx},
    introspection_tools,
    introspection_wire::{self, tests::fixture},
    reply_tool::ReplyServices,
    visibility::VisibilityRequest,
    visibility_read,
};
use crate::{harness::HarnessConnection, mcp::tool_registry::ToolRegistry};
use camino::Utf8PathBuf;
use serde_json::{Value, json};
use serenity::model::id::{ChannelId, UserId};
use std::sync::Arc;

fn guild() -> Value {
    json!({
        "id":"7", "name":"fixture", "owner_id":"1", "verification_level":0,
        "default_message_notifications":0, "explicit_content_filter":0,
        "roles":[
            {"id":"7","name":"@everyone","color":0,"colors":{"primary_color":0},"hoist":false,"managed":false,"position":0,"permissions":"1024","mentionable":false},
            {"id":"9","name":"admin","color":0,"colors":{"primary_color":0},"hoist":false,"managed":false,"position":1,"permissions":"8","mentionable":false}
        ],
        "emojis":[], "features":[], "mfa_level":0, "system_channel_flags":0,
        "premium_tier":0, "preferred_locale":"en-US", "nsfw_level":0,
        "stickers":[], "premium_progress_bar_enabled":false
    })
}

fn channel(kind: u8) -> Value {
    json!({"id":"42","guild_id":"7","name":"channel","type":kind,"position":0,"permission_overwrites":[]})
}

fn member(id: &str, roles: Value) -> Value {
    json!({"user":{"id":id,"username":"fixture","avatar":null},"roles":roles,"joined_at":null,"deaf":false,"mute":false,"flags":0})
}

fn request(ids: &[u64]) -> VisibilityRequest {
    VisibilityRequest::new(
        ChannelId::new(42),
        ids.iter().copied().map(UserId::new).collect(),
    )
    .unwrap()
}

fn registry(ctx: &IntrospectionCtx, state_dir: &camino::Utf8Path) -> ToolRegistry {
    let services = Arc::new(ReplyServices::new(
        ctx.http.clone(),
        crate::state::new_state(),
        state_dir.to_owned(),
        Arc::new(crate::no_rly::consent::ConsentGate::new(state_dir)),
        Arc::new(crate::ingress_ledger::IngressLedger::new()),
        None,
    ));
    ToolRegistry::new(
        &HarnessConnection::claude(),
        introspection_tools::registrations(services).unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn registered_visibility_preserves_order_owner_admin_denial_and_lookup_uncertainty() {
    let mut observed = channel(0);
    observed["permission_overwrites"] = json!([{"id":"3","type":1,"allow":"0","deny":"1024"}]);
    let (ctx, server) = fixture(vec![
        ("200 OK", observed),
        ("200 OK", guild()),
        ("200 OK", member("1", json!([]))),
        ("200 OK", member("2", json!(["9"]))),
        ("200 OK", member("3", json!([]))),
        (
            "404 Not Found",
            json!({"code":10007,"message":"Unknown Member"}),
        ),
        (
            "404 Not Found",
            json!({"code":10004,"message":"Unknown Guild"}),
        ),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let state_dir = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let registry = registry(&ctx, &state_dir);
    assert!(registry.resident_allowed("can_view_channel"));
    assert!(!registry.resident_allowed("unregistered_visibility"));
    let result = registry
        .find("can_view_channel")
        .unwrap()
        .call(&json!({
            "channel_id":"42", "user_ids":["1",2,"3",4,"5"]
        }))
        .await
        .unwrap();
    assert_eq!(
        result,
        json!({"channel_id":"42","results":[
            {"user_id":"1","can_view":true,"reason":"owner"},
            {"user_id":"2","can_view":true,"reason":"administrator"},
            {"user_id":"3","can_view":false,"reason":"member_overwrite"},
            {"user_id":"4","can_view":false,"reason":"not_a_member"},
            {"user_id":"5","can_view":"unknown","reason":"member_lookup_failed","error":"Unknown Guild"}
        ]})
    );
    assert_eq!(
        server.await.unwrap(),
        [
            "GET /api/v10/channels/42 HTTP/1.1",
            "GET /api/v10/guilds/7 HTTP/1.1",
            "GET /api/v10/guilds/7/members/1 HTTP/1.1",
            "GET /api/v10/guilds/7/members/2 HTTP/1.1",
            "GET /api/v10/guilds/7/members/3 HTTP/1.1",
            "GET /api/v10/guilds/7/members/4 HTTP/1.1",
            "GET /api/v10/guilds/7/members/5 HTTP/1.1",
        ]
    );
}

#[tokio::test]
async fn registered_visibility_rejects_invalid_arguments_before_discord_reads() {
    let (ctx, server) = fixture(vec![]).await;
    let directory = tempfile::tempdir().unwrap();
    let state_dir = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let registry = registry(&ctx, &state_dir);
    let tool = registry.find("can_view_channel").unwrap();
    let cases = [
        (json!({"channel_id":"42"}), "missing user_ids"),
        (
            json!({"channel_id":"42","user_ids":"1"}),
            "invalid user_ids: expected an array of IDs",
        ),
        (
            json!({"channel_id":"42","user_ids":[]}),
            "invalid user_ids: must contain at least one ID",
        ),
        (
            json!({"channel_id":"42","user_ids":vec!["1";26]}),
            "invalid user_ids: at most 25 IDs are allowed",
        ),
        (
            json!({"channel_id":"42","user_ids":["0"]}),
            "invalid user_ids: must be nonzero Discord snowflakes",
        ),
        (
            json!({"channel_id":"42","user_ids":["bad"]}),
            "invalid user_ids: each ID must be a numeric or string u64",
        ),
        (
            json!({"channel_id":"42","user_ids":[true]}),
            "invalid user_ids: each ID must be a numeric or string u64",
        ),
        (
            json!({"channel_id":"42","user_ids":[-1]}),
            "invalid user_ids: each ID must be a numeric or string u64",
        ),
        (
            json!({"channel_id":"42","user_ids":["18446744073709551616"]}),
            "invalid user_ids: each ID must be a numeric or string u64",
        ),
        (
            json!({"channel_id":"0","user_ids":["1"]}),
            "invalid channel_id: must be a nonzero Discord snowflake",
        ),
    ];
    for (arguments, expected) in cases {
        assert_eq!(
            tool.call(&arguments).await.unwrap_err().to_string(),
            expected
        );
    }
    assert!(server.await.unwrap().is_empty());
}

#[tokio::test]
async fn private_thread_uses_parent_and_reads_membership_only_when_decisive() {
    let mut thread = channel(12);
    thread["parent_id"] = json!("41");
    let mut parent = channel(0);
    parent["id"] = json!("41");
    parent["permission_overwrites"] = json!([
        {"id":"80","type":1,"allow":"0","deny":"1024"},
        {"id":"81","type":1,"allow":"17179869184","deny":"0"}
    ]);
    let (ctx, server) = fixture(vec![
        ("200 OK", thread),
        ("200 OK", parent),
        ("200 OK", guild()),
        ("200 OK", member("80", json!([]))),
        ("200 OK", member("81", json!([]))),
        ("200 OK", member("82", json!([]))),
        (
            "200 OK",
            json!({"id":"42","user_id":"82","join_timestamp":"2026-10-17T00:00:00Z","flags":0}),
        ),
        ("200 OK", member("83", json!([]))),
        (
            "404 Not Found",
            json!({"code":10007,"message":"Unknown Member"}),
        ),
        ("200 OK", member("84", json!([]))),
        (
            "404 Not Found",
            json!({"code":10003,"message":"Unknown Channel"}),
        ),
    ])
    .await;
    assert_eq!(
        introspection_wire::visibility(
            visibility_read::can_view_channel(&ctx, &request(&[80, 81, 82, 83, 84])).await
        ),
        json!({
            "channel_id":"42","results":[
                {"user_id":"80","can_view":false,"reason":"member_overwrite"},
                {"user_id":"81","can_view":true,"reason":"manage_threads"},
                {"user_id":"82","can_view":true,"reason":"private_thread_member"},
                {"user_id":"83","can_view":false,"reason":"not_private_thread_member"},
                {"user_id":"84","can_view":"unknown","reason":"private_thread_membership_unknown","error":"Unknown Channel"}
            ]
        })
    );
    assert_eq!(
        server.await.unwrap(),
        [
            "GET /api/v10/channels/42 HTTP/1.1",
            "GET /api/v10/channels/41 HTTP/1.1",
            "GET /api/v10/guilds/7 HTTP/1.1",
            "GET /api/v10/guilds/7/members/80 HTTP/1.1",
            "GET /api/v10/guilds/7/members/81 HTTP/1.1",
            "GET /api/v10/guilds/7/members/82 HTTP/1.1",
            "GET /api/v10/channels/42/thread-members/82?&with_member=false HTTP/1.1",
            "GET /api/v10/guilds/7/members/83 HTTP/1.1",
            "GET /api/v10/channels/42/thread-members/83?&with_member=false HTTP/1.1",
            "GET /api/v10/guilds/7/members/84 HTTP/1.1",
            "GET /api/v10/channels/42/thread-members/84?&with_member=false HTTP/1.1",
        ]
    );
}

#[tokio::test]
async fn public_thread_inherits_parent_overwrites_without_reading_membership() {
    let mut thread = channel(11);
    thread["parent_id"] = json!("41");
    let mut parent = channel(0);
    parent["id"] = json!("41");
    parent["permission_overwrites"] = json!([{"id":"80","type":1,"allow":"0","deny":"1024"}]);
    let (ctx, server) = fixture(vec![
        ("200 OK", thread),
        ("200 OK", parent),
        ("200 OK", guild()),
        ("200 OK", member("80", json!([]))),
        ("200 OK", member("81", json!([]))),
    ])
    .await;
    assert_eq!(
        introspection_wire::visibility(
            visibility_read::can_view_channel(&ctx, &request(&[80, 81])).await
        ),
        json!({"channel_id":"42","results":[
            {"user_id":"80","can_view":false,"reason":"member_overwrite"},
            {"user_id":"81","can_view":true,"reason":"base_permissions"}
        ]})
    );
    assert_eq!(
        server.await.unwrap(),
        [
            "GET /api/v10/channels/42 HTTP/1.1",
            "GET /api/v10/channels/41 HTTP/1.1",
            "GET /api/v10/guilds/7 HTTP/1.1",
            "GET /api/v10/guilds/7/members/80 HTTP/1.1",
            "GET /api/v10/guilds/7/members/81 HTTP/1.1",
        ]
    );
}

#[tokio::test]
async fn prerequisite_errors_preserve_main_wire_text() {
    let private =
        json!({"id":"41","type":1,"recipients":[{"id":"80","username":"dm","avatar":null}]});
    let mut thread = channel(12);
    thread["parent_id"] = json!("41");
    let cases = [
        (
            vec![("200 OK", private.clone())],
            "can_view_channel requires a guild channel or thread",
        ),
        (
            vec![(
                "403 Forbidden",
                json!({"code":50013,"message":"Missing Permissions"}),
            )],
            "Missing Permissions",
        ),
        (
            vec![("200 OK", channel(12))],
            "thread has no parent channel",
        ),
        (
            vec![("200 OK", thread.clone()), ("200 OK", private)],
            "thread parent is not a guild channel",
        ),
        (
            vec![
                ("200 OK", thread),
                (
                    "404 Not Found",
                    json!({"code":10003,"message":"Unknown Channel"}),
                ),
            ],
            "failed to fetch thread parent: Unknown Channel",
        ),
        (
            vec![
                ("200 OK", channel(0)),
                (
                    "404 Not Found",
                    json!({"code":10004,"message":"Unknown Guild"}),
                ),
            ],
            "failed to fetch guild: Unknown Guild",
        ),
    ];
    for (responses, expected) in cases {
        let count = responses.len();
        let (ctx, server) = fixture(responses).await;
        let error = visibility_read::can_view_channel(&ctx, &request(&[80]))
            .await
            .unwrap_err();
        assert_eq!(error.channel, ChannelId::new(42));
        assert_eq!(
            introspection_wire::visibility(Err(error)),
            json!({"error":expected})
        );
        assert_eq!(server.await.unwrap().len(), count);
    }
}

#[tokio::test]
async fn channel_metadata_carries_decimal_bitfields_and_thread_parent() {
    let mut observed = channel(0);
    observed["parent_id"] = json!("100");
    observed["permission_overwrites"] = json!([
        {"id":"11","type":0,"allow":"1024","deny":"2048"},
        {"id":"22","type":1,"allow":"0","deny":"17179870208"}
    ]);
    let mut thread = channel(11);
    thread["parent_id"] = json!("41");
    let (ctx, server) = fixture(vec![
        ("200 OK", observed),
        ("200 OK", thread),
        ("200 OK", channel(12)),
    ])
    .await;
    for expected in [
        json!({"id":"42","name":"channel","kind":"guild","permission_overwrites":[
            {"type":"role","id":"11","allow":"1024","deny":"2048"},
            {"type":"member","id":"22","allow":"0","deny":"17179870208"}
        ]}),
        json!({"id":"42","name":"channel","kind":"guild","permission_overwrites":[],"parent_id":"41"}),
        json!({"id":"42","name":"channel","kind":"guild","permission_overwrites":[]}),
    ] {
        assert_eq!(
            introspection_wire::channel(introspection::get_channel(&ctx, ChannelId::new(42)).await),
            expected
        );
    }
    assert_eq!(
        server.await.unwrap(),
        ["GET /api/v10/channels/42 HTTP/1.1"; 3]
    );
}
