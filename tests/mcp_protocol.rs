//! Integration tests for the MCP JSON-RPC protocol layer.
//!
//! These tests exercise `handle_request` (via `test_helpers::dispatch_request`)
//! and `event_to_notification` (via `test_helpers::make_notification`) without
//! a real Discord connection.  Tool calls that require Discord HTTP are tested
//! only up to the gate-rejection path.

use std::sync::Arc;

use dione::discord::events::{AttachmentMeta, NotificationEvent};
use dione::mcp::server::{DioneServer, test_helpers};
use dione::queue::AccessQueue;
use dione::state::new_state;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::{Mutex, mpsc};

// ── Test fixture ──────────────────────────────────────────────────────────────

fn temp_state_dir() -> (TempDir, camino::Utf8PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    (dir, path)
}

fn make_server(state_dir: &camino::Utf8PathBuf) -> DioneServer {
    let http = Arc::new(serenity::http::Http::new("fake-token-for-tests"));
    let state = new_state();
    let queue = Arc::new(Mutex::new(AccessQueue::load(state_dir)));
    let (tx, _rx) = mpsc::channel(4);
    DioneServer {
        state,
        queue,
        http,
        state_dir: state_dir.clone(),
        notification_tx: tx,
        discord_cmd_tx: None,
    }
}

// ── Initialize handshake ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_initialize_returns_capabilities() {
    let resp = test_helpers::get_initialize_response();
    // Must advertise the tools capability.
    assert!(
        resp.get("capabilities")
            .and_then(|c| c.get("tools"))
            .is_some(),
        "initialize response must include capabilities.tools"
    );
    // Must include server info.
    assert!(
        resp.get("serverInfo").and_then(|s| s.get("name")).is_some(),
        "initialize response must include serverInfo.name"
    );
    // Protocol version must be present.
    assert!(
        resp.get("protocolVersion").is_some(),
        "initialize response must include protocolVersion"
    );
}

#[tokio::test]
async fn test_initialize_request_dispatch() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2024-11-05" }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    // Result should have capabilities.
    assert!(
        resp["result"]["capabilities"]["tools"].is_object(),
        "dispatch initialize must return capabilities.tools"
    );
}

// ── tools/list ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_tools_list_contains_expected_tools() {
    let list = test_helpers::get_tools_list();
    let tools = list["tools"].as_array().expect("tools must be an array");

    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    // Core messaging tools.
    assert!(names.contains(&"reply"), "tools/list must include reply");
    assert!(names.contains(&"react"), "tools/list must include react");
    assert!(
        names.contains(&"edit_message"),
        "tools/list must include edit_message"
    );
    assert!(
        names.contains(&"fetch_messages"),
        "tools/list must include fetch_messages"
    );

    // Introspection tools.
    assert!(
        names.contains(&"list_guilds"),
        "tools/list must include list_guilds"
    );
    assert!(
        names.contains(&"list_channels"),
        "tools/list must include list_channels"
    );

    // Access management tools.
    assert!(
        names.contains(&"list_access_requests"),
        "tools/list must include list_access_requests"
    );
    assert!(
        names.contains(&"approve_access"),
        "tools/list must include approve_access"
    );
    assert!(
        names.contains(&"deny_access"),
        "tools/list must include deny_access"
    );

    // Bot state tools.
    assert!(
        names.contains(&"send_typing"),
        "tools/list must include send_typing"
    );
}

#[tokio::test]
async fn test_tools_list_dispatch() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 2);
    assert!(
        resp["result"]["tools"].is_array(),
        "tools/list result must have tools array"
    );
}

// ── Invalid method ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_unknown_method_returns_error() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "no_such_method/v99",
        "params": {}
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 3);
    // Must have an error field, not a result field.
    assert!(
        resp.get("error").is_some(),
        "unknown method must return an error response"
    );
    assert!(
        resp.get("result").is_none(),
        "unknown method must not return a result"
    );
}

// ── Client notification (no id) ───────────────────────────────────────────────

#[tokio::test]
async fn test_client_notification_no_response() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    // A JSON-RPC notification has no "id" field.
    let notif = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    });
    let resp = test_helpers::dispatch_request(&server, notif).await;
    // Server must return None (no response to a notification).
    assert!(
        resp.is_none(),
        "server must not respond to client notifications"
    );
}

// ── tools/call — gate rejection path ─────────────────────────────────────────

#[tokio::test]
async fn test_tools_call_send_typing_rejected_unknown_channel() {
    let (_dir, state_dir) = temp_state_dir();
    // Empty config → no opted-in channels, no DM map → gate will reject.
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "send_typing",
            "arguments": { "channel_id": "999999" }
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 4);
    // The tool result must be present and flagged as an error (channel not in allowlist).
    let result = &resp["result"];
    assert_eq!(
        result["isError"],
        json!(true),
        "send_typing to unknown channel should return isError: true"
    );
}

#[tokio::test]
async fn test_tools_call_unknown_tool_returns_error() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "nonexistent_tool",
            "arguments": {}
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();

    // The RPC layer should return a JSON-RPC error (not a tool result with isError).
    assert!(
        resp.get("error").is_some(),
        "unknown tool should produce a JSON-RPC error"
    );
}

#[tokio::test]
async fn test_tools_call_missing_tool_name_returns_error() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "arguments": { "channel_id": "123" }
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    assert!(
        resp.get("error").is_some(),
        "tools/call without name must return error"
    );
}

// ── list_access_requests tool — empty queue ───────────────────────────────────

#[tokio::test]
async fn test_tools_call_list_access_requests_empty() {
    let (_dir, state_dir) = temp_state_dir();
    let server = make_server(&state_dir);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "list_access_requests",
            "arguments": {}
        }
    });
    let resp = test_helpers::dispatch_request(&server, req).await.unwrap();
    assert_eq!(resp["id"], 7);
    // Should be a valid result, not an error.
    assert!(
        resp.get("result").is_some(),
        "list_access_requests should succeed"
    );
    assert!(
        resp.get("error").is_none(),
        "list_access_requests should not error"
    );
}

// ── Notification format tests ─────────────────────────────────────────────────

#[test]
fn test_notification_message_event_format() {
    let event = NotificationEvent::Message {
        chat_id: "111".to_string(),
        message_id: "222".to_string(),
        user: "alice".to_string(),
        user_id: "333".to_string(),
        content: "hello".to_string(),
        timestamp: "2026-01-01T00:00:00Z".to_string(),
        attachments: vec![],
        is_voice_message: false,
    };
    let notif = test_helpers::make_notification(event);

    assert_eq!(notif["jsonrpc"], "2.0");
    assert_eq!(notif["method"], "notifications/channel");
    let params = &notif["params"];
    assert_eq!(params["type"], "message");
    assert_eq!(params["chat_id"], "111");
    assert_eq!(params["message_id"], "222");
    assert_eq!(params["user"], "alice");
    assert_eq!(params["user_id"], "333");
    assert_eq!(params["content"], "hello");
    assert_eq!(params["is_voice_message"], false);
    assert!(params["attachments"].as_array().unwrap().is_empty());
    // No id field — it's a notification.
    assert!(notif.get("id").is_none());
}

#[test]
fn test_notification_message_event_with_attachment() {
    let event = NotificationEvent::Message {
        chat_id: "10".to_string(),
        message_id: "20".to_string(),
        user: "bob".to_string(),
        user_id: "30".to_string(),
        content: "see file".to_string(),
        timestamp: "2026-01-01T00:00:00Z".to_string(),
        attachments: vec![AttachmentMeta {
            name: "photo.png".to_string(),
            content_type: Some("image/png".to_string()),
            size: 1024,
        }],
        is_voice_message: false,
    };
    let notif = test_helpers::make_notification(event);
    let attachments = notif["params"]["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0]["name"], "photo.png");
    assert_eq!(attachments[0]["content_type"], "image/png");
    assert_eq!(attachments[0]["size"], 1024);
}

#[test]
fn test_notification_voice_message_flag() {
    let event = NotificationEvent::Message {
        chat_id: "10".to_string(),
        message_id: "20".to_string(),
        user: "carol".to_string(),
        user_id: "40".to_string(),
        content: String::new(),
        timestamp: "2026-01-01T00:00:00Z".to_string(),
        attachments: vec![],
        is_voice_message: true,
    };
    let notif = test_helpers::make_notification(event);
    assert_eq!(notif["params"]["is_voice_message"], true);
}

#[test]
fn test_notification_reaction_event_format() {
    let event = NotificationEvent::Reaction {
        chat_id: "500".to_string(),
        message_id: "600".to_string(),
        user: "dave".to_string(),
        user_id: "700".to_string(),
        emoji: "👍".to_string(),
    };
    let notif = test_helpers::make_notification(event);

    assert_eq!(notif["jsonrpc"], "2.0");
    assert_eq!(notif["method"], "notifications/channel");
    let params = &notif["params"];
    assert_eq!(params["type"], "reaction");
    assert_eq!(params["chat_id"], "500");
    assert_eq!(params["message_id"], "600");
    assert_eq!(params["user"], "dave");
    assert_eq!(params["user_id"], "700");
    assert_eq!(params["emoji"], "👍");
    assert!(notif.get("id").is_none());
}

#[test]
fn test_notification_permission_response_format() {
    let event = NotificationEvent::PermissionResponse {
        request_id: "req-abc-123".to_string(),
        granted: true,
    };
    let notif = test_helpers::make_notification(event);

    assert_eq!(notif["jsonrpc"], "2.0");
    assert_eq!(notif["method"], "notifications/channel");
    let params = &notif["params"];
    assert_eq!(params["type"], "permission_response");
    assert_eq!(params["request_id"], "req-abc-123");
    assert_eq!(params["granted"], true);
    assert!(notif.get("id").is_none());
}

#[test]
fn test_notification_permission_response_denied() {
    let event = NotificationEvent::PermissionResponse {
        request_id: "req-xyz-999".to_string(),
        granted: false,
    };
    let notif = test_helpers::make_notification(event);
    assert_eq!(notif["params"]["granted"], false);
    assert_eq!(notif["params"]["type"], "permission_response");
}

// ── Snapshot tests ────────────────────────────────────────────────────────────

#[test]
fn test_notification_message_snapshot() {
    let event = NotificationEvent::Message {
        chat_id: "1000".to_string(),
        message_id: "2000".to_string(),
        user: "snapuser".to_string(),
        user_id: "3000".to_string(),
        content: "snapshot content".to_string(),
        timestamp: "2026-01-01T00:00:00+00:00".to_string(),
        attachments: vec![],
        is_voice_message: false,
    };
    let notif = test_helpers::make_notification(event);
    insta::assert_json_snapshot!(notif);
}

#[test]
fn test_notification_reaction_snapshot() {
    let event = NotificationEvent::Reaction {
        chat_id: "1001".to_string(),
        message_id: "2001".to_string(),
        user: "reactor".to_string(),
        user_id: "3001".to_string(),
        emoji: "🎉".to_string(),
    };
    let notif = test_helpers::make_notification(event);
    insta::assert_json_snapshot!(notif);
}

#[test]
fn test_notification_permission_response_snapshot() {
    let event = NotificationEvent::PermissionResponse {
        request_id: "snap-req-42".to_string(),
        granted: true,
    };
    let notif = test_helpers::make_notification(event);
    insta::assert_json_snapshot!(notif);
}
