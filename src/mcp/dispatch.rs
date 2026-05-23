//! Tool dispatch: routes `tools/call` requests to the correct handler.

use serde_json::{Value, json};

use crate::config_store::{ConfigStore, DiscordId};
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
        reply, send_dm, send_file,
    },
    render::{render_latex, render_latex_to_channel},
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
    let config = std::sync::Arc::new(crate::config::load_config(&server.state_dir));

    let result = match name {
        // Messaging
        "reply" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?;
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing content".to_string())?;
            let reply_to = parse_optional_id(&args, "reply_to_message_id");
            reply(&ctx, channel_id, content, reply_to).await
        }
        "react" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            let emoji = args
                .get("emoji")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing emoji".to_string())?;
            discord_react(&ctx, channel_id, message_id, emoji).await
        }
        "edit_message" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing content".to_string())?;
            edit_message(&ctx, channel_id, message_id, content).await
        }
        "fetch_messages" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map(|v| v.min(100) as u8)
                .unwrap_or(20);
            fetch_messages(&ctx, channel_id, limit).await
        }
        "download_attachment" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            download_attachment(&ctx, channel_id, message_id).await
        }
        "get_message" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            get_message(&ctx, channel_id, message_id).await
        }
        "send_file" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?;
            let file_path = args
                .get("file_path")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing file_path".to_string())?;
            let caption = args.get("caption").and_then(Value::as_str);
            send_file(&ctx, channel_id, file_path, caption).await
        }

        "send_dm" => {
            let ctx = server.messaging_ctx(config.clone());
            let user_id = parse_id(&args, "user_id")?;
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing content".to_string())?;
            send_dm(&ctx, user_id, content).await
        }

        // Introspection
        "list_guilds" => {
            let ctx = server.introspection_ctx(config.clone());
            list_guilds(&ctx).await
        }
        "list_channels" => {
            let ctx = server.introspection_ctx(config.clone());
            let guild_id = parse_id(&args, "guild_id")?;
            list_channels(&ctx, guild_id).await
        }
        "get_channel" => {
            let ctx = server.introspection_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?;
            get_channel(&ctx, channel_id).await
        }
        "get_user" => {
            let ctx = server.introspection_ctx(config.clone());
            let user_id = parse_id(&args, "user_id")?;
            get_user(&ctx, user_id).await
        }
        "get_member" => {
            let ctx = server.introspection_ctx(config.clone());
            let guild_id = parse_id(&args, "guild_id")?;
            let user_id = parse_id(&args, "user_id")?;
            get_member(&ctx, guild_id, user_id).await
        }
        "list_roles" => {
            let ctx = server.introspection_ctx(config.clone());
            let guild_id = parse_id(&args, "guild_id")?;
            list_roles(&ctx, guild_id).await
        }
        "list_emojis" => {
            let ctx = server.introspection_ctx(config.clone());
            let guild_id = parse_id(&args, "guild_id")?;
            list_emojis(&ctx, guild_id).await
        }

        // Management
        "pin_message" => {
            let ctx = server.management_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            pin_message(&ctx, channel_id, message_id).await
        }
        "unpin_message" => {
            let ctx = server.management_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            unpin_message(&ctx, channel_id, message_id).await
        }
        "create_thread" => {
            let ctx = server.management_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_optional_id(&args, "message_id");
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing name".to_string())?;
            create_thread(&ctx, channel_id, message_id, name).await
        }
        "delete_message" => {
            let ctx = server.management_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?;
            let message_id = parse_id(&args, "message_id")?;
            delete_message(&ctx, channel_id, message_id).await
        }

        // Access
        "list_access_requests" => {
            let ctx = server.access_ctx(config.clone());
            list_access_requests(&ctx).await
        }
        "approve_access" => {
            let ctx = server.access_ctx(config.clone());
            let user_id = parse_id(&args, "user_id")?;
            approve_access(&ctx, user_id).await
        }
        "deny_access" => {
            let ctx = server.access_ctx(config.clone());
            let user_id = parse_id(&args, "user_id")?;
            deny_access(&ctx, user_id).await
        }

        // Config management — read-only (no ConfigStore mutation needed)
        "list_config_channels" => ConfigStore::list_channels(&server.state_dir),
        "get_access_config" => ConfigStore::get_access(&server.state_dir),

        // Config management — mutations (ConfigStore)
        "add_channel" => {
            let id_str = parse_str(&args, "id")?;
            DiscordId::parse(id_str)?;
            let require_mention = args.get("require_mention").and_then(Value::as_bool);
            let allow_from = parse_string_array(&args, "allow_from").unwrap_or_default();
            for af in &allow_from {
                DiscordId::parse(af)?;
            }
            match async {
                let mut editor = ConfigStore::load(&server.state_dir).await?;
                editor.add_channel_entry(id_str, require_mention.unwrap_or(true), allow_from)?;
                editor.save().await
            }
            .await
            {
                Ok(()) => json!({ "ok": true, "id": id_str }),
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "remove_channel" => {
            let id_str = parse_str(&args, "id")?;
            DiscordId::parse(id_str)?;
            match async {
                let mut editor = ConfigStore::load(&server.state_dir).await?;
                editor.remove_channel_entry(id_str)?;
                editor.save().await
            }
            .await
            {
                Ok(()) => json!({ "ok": true, "id": id_str }),
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "update_channel" => {
            let id_str = parse_str(&args, "id")?;
            DiscordId::parse(id_str)?;
            let require_mention = args.get("require_mention").and_then(Value::as_bool);
            let allow_from = parse_string_array(&args, "allow_from");
            if require_mention.is_none() && allow_from.is_none() {
                json!({ "error": "at least one of require_mention or allow_from must be provided" })
            } else {
                if let Some(ref af) = allow_from {
                    for entry in af {
                        DiscordId::parse(entry)?;
                    }
                }
                match async {
                    let mut editor = ConfigStore::load(&server.state_dir).await?;
                    editor.update_channel_entry(id_str, require_mention, allow_from)?;
                    editor.save().await
                }
                .await
                {
                    Ok(()) => json!({ "ok": true, "id": id_str }),
                    Err(e) => json!({ "error": e.to_string() }),
                }
            }
        }
        "update_dm_policy" => {
            let policy = parse_str(&args, "policy")?;
            if !matches!(policy, "drop" | "queue" | "disabled") {
                json!({ "error": format!("invalid dm_policy: {policy}; must be one of: drop, queue, disabled") })
            } else {
                match async {
                    let mut editor = ConfigStore::load(&server.state_dir).await?;
                    editor.set_dm_policy(policy);
                    editor.save().await
                }
                .await
                {
                    Ok(()) => json!({ "ok": true, "dm_policy": policy }),
                    Err(e) => json!({ "error": e.to_string() }),
                }
            }
        }
        "add_allow_from" => {
            let user_id = parse_str(&args, "user_id")?;
            DiscordId::parse(user_id)?;
            match async {
                let mut editor = ConfigStore::load(&server.state_dir).await?;
                editor.add_to_allow_from(user_id)?;
                editor.save().await
            }
            .await
            {
                Ok(()) => json!({ "ok": true, "user_id": user_id }),
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "remove_allow_from" => {
            let user_id = parse_str(&args, "user_id")?;
            DiscordId::parse(user_id)?;
            match async {
                let mut editor = ConfigStore::load(&server.state_dir).await?;
                editor.remove_from_allow_from(user_id)?;
                editor.save().await
            }
            .await
            {
                Ok(()) => json!({ "ok": true, "user_id": user_id }),
                Err(e) => json!({ "error": e.to_string() }),
            }
        }

        // Bot state
        "send_typing" => {
            let ctx = server.bot_state_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?;
            send_typing(&ctx, channel_id).await
        }

        // Rendering
        "render_latex" => {
            let latex = args
                .get("latex")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing latex".to_string())?;
            render_latex(latex).await
        }
        "render_latex_to_channel" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?;
            let latex = args
                .get("latex")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing latex".to_string())?;
            let caption = args.get("caption").and_then(Value::as_str);
            render_latex_to_channel(&ctx, channel_id, latex, caption).await
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

fn parse_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}

fn parse_string_array(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key).and_then(Value::as_array).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    })
}
