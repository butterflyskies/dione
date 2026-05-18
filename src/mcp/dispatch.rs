//! Tool dispatch: routes `tools/call` requests to the correct handler.

use serde_json::{Value, json};

use crate::mcp::server::DioneServer;
use crate::mcp::tools::{
    access::{approve_access, deny_access, list_access_requests},
    bot_state::send_typing,
    diagnostics::{get_version, set_stderr_level, set_trace_level},
    introspection::{
        get_channel, get_member, get_user, list_channels, list_emojis, list_guilds, list_roles,
    },
    management::{create_thread, delete_message, pin_message, unpin_message},
    messaging::{
        download_attachment, edit_message, fetch_messages, get_message, react as discord_react,
        reply,
    },
};

/// Dispatch a `tools/call` request to the appropriate handler.
///
/// Returns an MCP tool-result `Value` on success, or a `String` error message
/// that the caller wraps into a JSON-RPC error response.
pub(crate) async fn call_tool(
    server: &DioneServer,
    name: &str,
    args: Value,
) -> Result<Value, String> {
    let result = match name {
        // Messaging
        "reply" => {
            let ctx = server.messaging_ctx();
            let channel_id = parse_id(&args, "channel_id")?;
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing content".to_string())?;
            let reply_to = parse_optional_id(&args, "reply_to_message_id");
            reply(&ctx, channel_id, content, reply_to).await
        }
        "react" => {
            let ctx = server.messaging_ctx();
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            let emoji = args
                .get("emoji")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing emoji".to_string())?;
            discord_react(&ctx, channel_id, message_id, emoji).await
        }
        "edit_message" => {
            let ctx = server.messaging_ctx();
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing content".to_string())?;
            edit_message(&ctx, channel_id, message_id, content).await
        }
        "fetch_messages" => {
            let ctx = server.messaging_ctx();
            let channel_id = parse_id(&args, "channel_id")?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map(|v| v.min(100) as u8)
                .unwrap_or(20);
            fetch_messages(&ctx, channel_id, limit).await
        }
        "download_attachment" => {
            let ctx = server.messaging_ctx();
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            download_attachment(&ctx, channel_id, message_id).await
        }
        "get_message" => {
            let ctx = server.messaging_ctx();
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            get_message(&ctx, channel_id, message_id).await
        }

        // Introspection
        "list_guilds" => {
            let ctx = server.introspection_ctx();
            list_guilds(&ctx).await
        }
        "list_channels" => {
            let ctx = server.introspection_ctx();
            let guild_id = parse_id(&args, "guild_id")?;
            list_channels(&ctx, guild_id).await
        }
        "get_channel" => {
            let ctx = server.introspection_ctx();
            let channel_id = parse_id(&args, "channel_id")?;
            get_channel(&ctx, channel_id).await
        }
        "get_user" => {
            let ctx = server.introspection_ctx();
            let user_id = parse_id(&args, "user_id")?;
            get_user(&ctx, user_id).await
        }
        "get_member" => {
            let ctx = server.introspection_ctx();
            let guild_id = parse_id(&args, "guild_id")?;
            let user_id = parse_id(&args, "user_id")?;
            get_member(&ctx, guild_id, user_id).await
        }
        "list_roles" => {
            let ctx = server.introspection_ctx();
            let guild_id = parse_id(&args, "guild_id")?;
            list_roles(&ctx, guild_id).await
        }
        "list_emojis" => {
            let ctx = server.introspection_ctx();
            let guild_id = parse_id(&args, "guild_id")?;
            list_emojis(&ctx, guild_id).await
        }

        // Management
        "pin_message" => {
            let ctx = server.management_ctx();
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            pin_message(&ctx, channel_id, message_id).await
        }
        "unpin_message" => {
            let ctx = server.management_ctx();
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            unpin_message(&ctx, channel_id, message_id).await
        }
        "create_thread" => {
            let ctx = server.management_ctx();
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_optional_id(&args, "message_id");
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing name".to_string())?;
            create_thread(&ctx, channel_id, message_id, name).await
        }
        "delete_message" => {
            let ctx = server.management_ctx();
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            delete_message(&ctx, channel_id, message_id).await
        }

        // Access
        "list_access_requests" => {
            let ctx = server.access_ctx();
            list_access_requests(&ctx).await
        }
        "approve_access" => {
            let ctx = server.access_ctx();
            let user_id = parse_id(&args, "user_id")?;
            approve_access(&ctx, user_id).await
        }
        "deny_access" => {
            let ctx = server.access_ctx();
            let user_id = parse_id(&args, "user_id")?;
            deny_access(&ctx, user_id).await
        }

        // Bot state
        "send_typing" => {
            let ctx = server.bot_state_ctx();
            let channel_id = parse_id(&args, "channel_id")?;
            send_typing(&ctx, channel_id).await
        }

        // Diagnostics
        "get_version" => get_version().await,
        "set_trace_level" => {
            let ctx = server.diagnostics_ctx();
            let filter = args
                .get("filter")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing filter".to_string())?;
            set_trace_level(&ctx, filter).await
        }
        "set_stderr_level" => {
            let ctx = server.diagnostics_ctx();
            let filter = args
                .get("filter")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing filter".to_string())?;
            set_stderr_level(&ctx, filter).await
        }

        unknown => return Err(format!("unknown tool: {unknown}")),
    };

    // Wrap result in MCP tool result format.
    // If the result contains an "error" key, set isError: true per MCP spec.
    let is_error = result.get("error").is_some();
    let mut response = json!({
        "content": [
            { "type": "text", "text": result.to_string() }
        ]
    });
    if is_error {
        response["isError"] = json!(true);
    }
    Ok(response)
}

// ── Parameter parsing helpers ─────────────────────────────────────────────────

pub(crate) fn parse_id(args: &Value, key: &str) -> Result<u64, String> {
    // Accept both numeric and string IDs.
    if let Some(n) = args.get(key).and_then(Value::as_u64) {
        return Ok(n);
    }
    if let Some(s) = args.get(key).and_then(Value::as_str) {
        return s
            .parse::<u64>()
            .map_err(|_| format!("invalid {key}: not a valid u64"));
    }
    Err(format!("missing required parameter: {key}"))
}

pub(crate) fn parse_optional_id(args: &Value, key: &str) -> Option<u64> {
    if let Some(n) = args.get(key).and_then(Value::as_u64) {
        return Some(n);
    }
    if let Some(s) = args.get(key).and_then(Value::as_str) {
        return s.parse::<u64>().ok();
    }
    None
}
