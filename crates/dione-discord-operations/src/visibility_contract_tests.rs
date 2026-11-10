//! Registered metadata tools retain root resident admission and argument validation.
use super::{introspection_test_support::fixture, introspection_tools, reply_tool::ReplyServices};
use camino::Utf8PathBuf;
use dione_discord_introspection::introspection::IntrospectionCtx;
use dione_mcp_tools::ToolRegistry;
use serde_json::{Value, json};
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

#[tokio::test]
async fn audience_lists_viewers_and_denials_through_registered_tool() {
    let mut observed = channel(0);
    observed["permission_overwrites"] = json!([{"id":"3","type":1,"allow":"0","deny":"1024"}]);
    let (ctx, server) = fixture(vec![
        ("200 OK", observed),
        ("200 OK", guild()),
        (
            "200 OK",
            json!([
                member("1", json!([])),
                member("2", json!(["9"])),
                member("3", json!([])),
                member("4", json!([]))
            ]),
        ),
        ("200 OK", json!([])),
    ])
    .await;
    let directory = tempfile::tempdir().unwrap();
    let state_dir = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let registry = registry(&ctx, &state_dir);
    assert!(registry.resident_allowed("channel_effective_audience"));
    let result = registry
        .find("channel_effective_audience")
        .unwrap()
        .call(&json!({"channel_id":"42"}))
        .await
        .unwrap();
    assert_eq!(result["complete"], true);
    assert_eq!(result["member_census_complete"], true);
    assert_eq!(result["snapshot_consistent"], false);
    assert_eq!(result["members_examined"], 4);
    assert_eq!(result["non_viewer_count"], 1);
    assert_eq!(
        result["members"],
        json!([
            {"user_id":"1","username":"fixture","nick":null,"can_view":true,"reason":"owner"},
            {"user_id":"2","username":"fixture","nick":null,"can_view":true,"reason":"administrator"},
            {"user_id":"4","username":"fixture","nick":null,"can_view":true,"reason":"base_permissions"}
        ])
    );
    assert_eq!(result["unknown_members"], json!([]));
    assert!(
        result["observed_at"]
            .as_str()
            .is_some_and(|value| dione_messages::timestamp::Timestamp::parse(value).is_ok())
    );
    assert!(
        result["completed_at"]
            .as_str()
            .is_some_and(|value| dione_messages::timestamp::Timestamp::parse(value).is_ok())
    );
    assert_eq!(
        server.await.unwrap(),
        [
            "GET /api/v10/channels/42 HTTP/1.1",
            "GET /api/v10/guilds/7 HTTP/1.1",
            "GET /api/v10/guilds/7/members?&limit=100 HTTP/1.1",
            "GET /api/v10/guilds/7/members?&limit=100&after=4 HTTP/1.1",
        ]
    );
}

#[tokio::test]
async fn audience_rejects_invalid_channel_before_http() {
    let (ctx, server) = fixture(vec![]).await;
    let directory = tempfile::tempdir().unwrap();
    let state_dir = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let registry = registry(&ctx, &state_dir);
    assert!(
        registry
            .find("channel_effective_audience")
            .unwrap()
            .call(&json!({"channel_id":"0"}))
            .await
            .is_err()
    );
    assert!(server.await.unwrap().is_empty());
}

fn channel(kind: u8) -> Value {
    json!({"id":"42","guild_id":"7","name":"channel","type":kind,"position":0,"permission_overwrites":[]})
}

fn member(id: &str, roles: Value) -> Value {
    json!({"user":{"id":id,"username":"fixture","avatar":null},"roles":roles,"joined_at":null,"deaf":false,"mute":false,"flags":0})
}

fn registry(ctx: &IntrospectionCtx, state_dir: &camino::Utf8Path) -> ToolRegistry {
    let services = Arc::new(ReplyServices::new(
        ctx.http().clone(),
        dione_discord_state::new_state(),
        state_dir.to_owned(),
        Arc::new(dione_outbound_consent::consent::ConsentGate::new(state_dir)),
        Arc::new(dione_discord_runtime::IngressLedger::new()),
        None,
    ));
    ToolRegistry::new(
        &crate::test_support::reserved_tool_names(&dione_claude::connect(
            dione_harness::ProtocolOutput::new(tokio::io::stdout()),
            dione_codex::TOOL_NAMES,
        )),
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
