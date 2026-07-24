//! Tool dispatch: routes `tools/call` requests to the correct handler.

use crate::{
    codex::{
        CodexThreadId, ConsumerId, DeliveryToken, consumer_ttl, lease_duration, timeout_response,
        wait_duration,
    },
    config_store::{ConfigStore, DiscordId},
    mcp::{
        ids::Snowflake,
        server::DioneServer,
        tools::{
            access::{approve_access, deny_access, list_access_requests},
            bot_state::{send_typing, set_presence},
            diagnostics::{get_version, set_stderr_level, set_trace_level},
            introspection::{
                get_channel, get_member, get_user, list_channels, list_emojis, list_guilds,
                list_roles,
            },
            management::{create_thread, delete_message, pin_message, unpin_message},
            messaging::{
                download_attachment, edit_message_with_hook_overrides, fetch_messages,
                fetch_new_since, get_message, react as discord_react, reply_with_hook_overrides,
                send_dm_with_hook_overrides, send_file_with_hook_overrides,
            },
            render::{render_latex, render_latex_to_channel_with_hook_overrides},
            search::{SearchParams, search_messages},
        },
    },
    pre_send::HookName,
};
use serde_json::{Value, json};

fn check_admin_gate(config: &crate::config::LoadedConfig) -> Result<(), String> {
    if config.access.admin_only_mutations {
        Err("admin_only_mutations is enabled; config mutation tools are disabled".to_string())
    } else {
        Ok(())
    }
}

/// Dispatch a `tools/call` request to the appropriate handler.
///
/// Returns an MCP tool-result `Value` on success, or a `String` error message
/// that the caller wraps into a JSON-RPC error response.
pub(crate) async fn call_tool(
    server: &DioneServer,
    name: &str,
    args: Value,
) -> Result<Value, String> {
    let config = crate::config::load_config(&server.state_dir);

    let result = match name {
        // Durable Codex receive loop
        "bind_codex_thread" => {
            let binder = server
                .codex_thread_binder
                .as_ref()
                .ok_or_else(|| "bind_codex_thread is only available in Codex mode".to_string())?;
            let thread_id = CodexThreadId::parse(parse_str(&args, "thread_id")?)
                .map_err(|error| error.to_string())?;
            binder
                .bind(thread_id.clone())
                .await
                .map_err(|error| error.to_string())?;
            json!({ "bound": true, "thread_id": thread_id })
        }
        "next_event" => {
            let queue = server
                .codex_queue
                .as_ref()
                .ok_or_else(|| "next_event is only available in Codex mode".to_string())?;
            let consumer_id = ConsumerId::parse(parse_str(&args, "consumer_id")?)
                .map_err(|error| error.to_string())?;
            let wait = wait_duration(args.get("wait_seconds").and_then(Value::as_u64));
            let lease = lease_duration(args.get("lease_seconds").and_then(Value::as_u64));
            match queue
                .next_event(&consumer_id, wait, lease)
                .await
                .map_err(|error| error.to_string())?
            {
                Some(event) => serde_json::to_value(event)
                    .map_err(|error| format!("failed to serialize leased event: {error}"))?,
                None => timeout_response(),
            }
        }
        "ack_event" => {
            let queue = server
                .codex_queue
                .as_ref()
                .ok_or_else(|| "ack_event is only available in Codex mode".to_string())?;
            let consumer_id = ConsumerId::parse(parse_str(&args, "consumer_id")?)
                .map_err(|error| error.to_string())?;
            let raw_token = parse_str(&args, "delivery_token")?;
            let token = DeliveryToken::parse(raw_token).map_err(|error| error.to_string())?;
            queue
                .acknowledge(&consumer_id, &token)
                .await
                .map_err(|error| error.to_string())?;
            json!({ "acknowledged": true })
        }
        "event_queue_status" => {
            let queue = server
                .codex_queue
                .as_ref()
                .ok_or_else(|| "event_queue_status is only available in Codex mode".to_string())?;
            serde_json::to_value(queue.status().await)
                .map_err(|error| format!("failed to serialize queue status: {error}"))?
        }
        "register_event_consumer" => {
            let queue = server.codex_queue.as_ref().ok_or_else(|| {
                "register_event_consumer is only available in Codex mode".to_string()
            })?;
            let label = parse_str(&args, "label")?.trim();
            if label.is_empty() || label.len() > 120 {
                return Err("label must contain 1 to 120 characters".to_string());
            }
            let ttl = consumer_ttl(args.get("ttl_seconds").and_then(Value::as_u64));
            let make_primary = args
                .get("make_primary")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let claim_unassigned = args
                .get("claim_unassigned")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let registration = queue
                .register_consumer(label.to_owned(), ttl, make_primary, claim_unassigned)
                .await
                .map_err(|error| error.to_string())?;
            serde_json::to_value(registration)
                .map_err(|error| format!("failed to serialize consumer registration: {error}"))?
        }
        "handoff_event_consumer" => {
            let queue = server.codex_queue.as_ref().ok_or_else(|| {
                "handoff_event_consumer is only available in Codex mode".to_string()
            })?;
            let from = ConsumerId::parse(parse_str(&args, "from_consumer_id")?)
                .map_err(|error| error.to_string())?;
            let to = ConsumerId::parse(parse_str(&args, "to_consumer_id")?)
                .map_err(|error| error.to_string())?;
            let move_pending = args
                .get("move_pending")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let result = queue
                .handoff(&from, &to, move_pending)
                .await
                .map_err(|error| error.to_string())?;
            serde_json::to_value(result)
                .map_err(|error| format!("failed to serialize consumer handoff: {error}"))?
        }
        "claim_event_consumer" => {
            let queue = server.codex_queue.as_ref().ok_or_else(|| {
                "claim_event_consumer is only available in Codex mode".to_string()
            })?;
            let consumer_id = ConsumerId::parse(parse_str(&args, "consumer_id")?)
                .map_err(|error| error.to_string())?;
            let claim_orphaned = args
                .get("claim_orphaned")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let claimed_events = queue
                .claim_primary(&consumer_id, claim_orphaned)
                .await
                .map_err(|error| error.to_string())?;
            json!({
                "primary_consumer_id": consumer_id,
                "claimed_events": claimed_events
            })
        }

        // Messaging
        "reply" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?.channel();
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing content".to_string())?;
            let reply_to = parse_optional_id(&args, "reply_to_message_id")?.map(Snowflake::message);
            let suppress_ping = args
                .get("suppress_ping")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let no_rly = args.get("no_rly").and_then(Value::as_bool).unwrap_or(false);
            let no_rly_hooks = parse_hook_overrides(&args)?;
            reply_with_hook_overrides(
                &ctx,
                channel_id,
                content,
                reply_to,
                suppress_ping,
                no_rly,
                &no_rly_hooks,
            )
            .await
        }
        "react" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?.channel();
            let message_id = parse_id(&args, "message_id")?.message();
            let emoji = args
                .get("emoji")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing emoji".to_string())?;
            discord_react(&ctx, channel_id, message_id, emoji).await
        }
        "edit_message" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?.channel();
            let message_id = parse_id(&args, "message_id")?.message();
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing content".to_string())?;
            let no_rly_hooks = parse_hook_overrides(&args)?;
            edit_message_with_hook_overrides(&ctx, channel_id, message_id, content, &no_rly_hooks)
                .await
        }
        "fetch_messages" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?.channel();
            let before = parse_strict_optional_id(&args, "before")?.map(|s| s.message());
            let after = parse_strict_optional_id(&args, "after")?.map(|s| s.message());
            let limit = parse_limit(&args, 20);
            fetch_messages(&ctx, channel_id, before, after, limit).await
        }
        "fetch_new_since" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?.channel();
            let after_message_id = parse_id(&args, "after_message_id")?.message();
            let limit = parse_limit(&args, 20);
            fetch_new_since(&ctx, channel_id, after_message_id, limit).await
        }
        "download_attachment" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?.channel();
            let message_id = parse_id(&args, "message_id")?.message();
            download_attachment(&ctx, channel_id, message_id).await
        }
        "get_message" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?.channel();
            let message_id = parse_id(&args, "message_id")?.message();
            get_message(&ctx, channel_id, message_id).await
        }
        "search_messages" => {
            let ctx = server.messaging_ctx(config.clone());
            let guild_id = parse_id(&args, "guild_id")?.guild();
            let params: SearchParams =
                serde_json::from_value(args).map_err(|e| format!("invalid search params: {e}"))?;
            search_messages(&ctx, guild_id, params).await
        }

        "send_file" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?.channel();
            let file_path = args
                .get("file_path")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing file_path".to_string())?;
            let caption = args.get("caption").and_then(Value::as_str);
            let no_rly_hooks = parse_hook_overrides(&args)?;
            send_file_with_hook_overrides(&ctx, channel_id, file_path, caption, &no_rly_hooks).await
        }

        "send_dm" => {
            let ctx = server.messaging_ctx(config.clone());
            let user_id = parse_id(&args, "user_id")?.user();
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing content".to_string())?;
            let no_rly_hooks = parse_hook_overrides(&args)?;
            send_dm_with_hook_overrides(&ctx, user_id, content, &no_rly_hooks).await
        }

        // Introspection
        "list_guilds" => {
            let ctx = server.introspection_ctx(config.clone());
            list_guilds(&ctx).await
        }
        "list_channels" => {
            let ctx = server.introspection_ctx(config.clone());
            let guild_id = parse_id(&args, "guild_id")?.guild();
            list_channels(&ctx, guild_id).await
        }
        "get_channel" => {
            let ctx = server.introspection_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?.channel();
            get_channel(&ctx, channel_id).await
        }
        "get_user" => {
            let ctx = server.introspection_ctx(config.clone());
            let user_id = parse_id(&args, "user_id")?.user();
            get_user(&ctx, user_id).await
        }
        "get_member" => {
            let ctx = server.introspection_ctx(config.clone());
            let guild_id = parse_id(&args, "guild_id")?.guild();
            let user_id = parse_id(&args, "user_id")?.user();
            get_member(&ctx, guild_id, user_id).await
        }
        "list_roles" => {
            let ctx = server.introspection_ctx(config.clone());
            let guild_id = parse_id(&args, "guild_id")?.guild();
            list_roles(&ctx, guild_id).await
        }
        "list_emojis" => {
            let ctx = server.introspection_ctx(config.clone());
            let guild_id = parse_id(&args, "guild_id")?.guild();
            list_emojis(&ctx, guild_id).await
        }

        // Management
        "pin_message" => {
            let ctx = server.management_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?.channel();
            let message_id = parse_id(&args, "message_id")?.message();
            pin_message(&ctx, channel_id, message_id).await
        }
        "unpin_message" => {
            let ctx = server.management_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?.channel();
            let message_id = parse_id(&args, "message_id")?.message();
            unpin_message(&ctx, channel_id, message_id).await
        }
        "create_thread" => {
            let ctx = server.management_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?.channel();
            let message_id = parse_optional_id(&args, "message_id")?.map(Snowflake::message);
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing name".to_string())?;
            create_thread(&ctx, channel_id, message_id, name).await
        }
        "delete_message" => {
            let ctx = server.management_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?.channel();
            let message_id = parse_id(&args, "message_id")?.message();
            delete_message(&ctx, channel_id, message_id).await
        }

        // Access
        "list_access_requests" => {
            let ctx = server.access_ctx(config.clone());
            list_access_requests(&ctx).await
        }
        "approve_access" => {
            check_admin_gate(&config)?;
            let ctx = server.access_ctx(config.clone());
            let user_id = parse_id(&args, "user_id")?.user();
            approve_access(&ctx, user_id).await
        }
        "deny_access" => {
            check_admin_gate(&config)?;
            let ctx = server.access_ctx(config.clone());
            let user_id = parse_id(&args, "user_id")?.user();
            deny_access(&ctx, user_id).await
        }

        // Config management — read-only (no ConfigStore mutation needed)
        "list_config_channels" => ConfigStore::list_channels(&server.state_dir),
        "get_access_config" => ConfigStore::get_access(&server.state_dir),

        // Config management — mutations (ConfigStore, admin-gated)
        "add_channel" => {
            check_admin_gate(&config)?;
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
            check_admin_gate(&config)?;
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
            check_admin_gate(&config)?;
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
            check_admin_gate(&config)?;
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
            check_admin_gate(&config)?;
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
            check_admin_gate(&config)?;
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
            let channel_id = parse_id(&args, "channel_id")?.channel();
            send_typing(&ctx, channel_id).await
        }
        "set_presence" => {
            let ctx = server.bot_state_ctx(config.clone());
            let online_status = args
                .get("online_status")
                .and_then(Value::as_str)
                .unwrap_or("online");
            let activity_type = args.get("activity_type").and_then(Value::as_str);
            let activity_name = args.get("activity_name").and_then(Value::as_str);
            set_presence(&ctx, online_status, activity_type, activity_name).await
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
            let channel_id = parse_id(&args, "channel_id")?.channel();
            let latex = args
                .get("latex")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing latex".to_string())?;
            let caption = args.get("caption").and_then(Value::as_str);
            let no_rly_hooks = parse_hook_overrides(&args)?;
            render_latex_to_channel_with_hook_overrides(
                &ctx,
                channel_id,
                latex,
                caption,
                &no_rly_hooks,
            )
            .await
        }

        // Diagnostics
        "reload_config" => {
            check_admin_gate(&config)?;
            let (_, error) = crate::config::reload_config(&server.state_dir);
            match error {
                Some(e) => json!({ "error": e }),
                None => json!({ "ok": true }),
            }
        }
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

pub(crate) fn parse_id(args: &Value, key: &str) -> Result<Snowflake, String> {
    // Accept both numeric and string IDs.
    let id = if let Some(n) = args.get(key).and_then(Value::as_u64) {
        n
    } else if let Some(s) = args.get(key).and_then(Value::as_str) {
        s.parse::<u64>()
            .map_err(|_| format!("invalid {key}: not a valid u64"))?
    } else if args.get(key).is_some() {
        return Err(format!("invalid {key}: expected a numeric or string ID"));
    } else {
        return Err(format!("missing {key}"));
    };
    Snowflake::new(id).ok_or_else(|| format!("invalid {key}: must be a nonzero Discord snowflake"))
}

/// Parses an optional `limit` argument, clamping it into Discord's accepted
/// `1..=100` range.
///
/// Clamping the lower bound matters for cursor-based pagination: a `limit` of
/// 0 would always satisfy `count == limit`, producing an empty page that
/// claims `has_more: true` — a pagination dead end.
pub(crate) fn parse_limit(args: &Value, default: u8) -> u8 {
    args.get("limit")
        .and_then(Value::as_u64)
        .map(|v| v.clamp(1, 100) as u8)
        .unwrap_or(default)
}

pub(crate) fn parse_optional_id(args: &Value, key: &str) -> Result<Option<Snowflake>, String> {
    // A value of zero is explicitly wrong — silently promoting it to "absent"
    // would hide the caller's bug; return an error so they know their ID is invalid.
    if let Some(n) = args.get(key).and_then(Value::as_u64) {
        return Snowflake::new(n)
            .map(Some)
            .ok_or_else(|| format!("invalid {key}: must be a nonzero Discord snowflake"));
    }
    if let Some(s) = args.get(key).and_then(Value::as_str) {
        if s.is_empty() {
            return Ok(None);
        }
        let n = s
            .parse::<u64>()
            .map_err(|_| format!("invalid {key}: not a valid u64"))?;
        return Snowflake::new(n)
            .map(Some)
            .ok_or_else(|| format!("invalid {key}: must be a nonzero Discord snowflake"));
    }
    Ok(None)
}

fn parse_strict_optional_id(args: &Value, key: &str) -> Result<Option<Snowflake>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(v) if v.is_null() => Err(format!(
            "invalid {key}: null is not a valid snowflake; omit the field instead"
        )),
        Some(v) if v.is_u64() => {
            let n = v.as_u64().unwrap();
            Snowflake::new(n)
                .map(Some)
                .ok_or_else(|| format!("invalid {key}: must be a nonzero Discord snowflake"))
        }
        Some(v) if v.is_string() => {
            let s = v.as_str().unwrap();
            if s.is_empty() {
                return Err(format!(
                    "invalid {key}: empty string is not a valid snowflake"
                ));
            }
            let n = s
                .parse::<u64>()
                .map_err(|_| format!("invalid {key}: not a valid u64"))?;
            Snowflake::new(n)
                .map(Some)
                .ok_or_else(|| format!("invalid {key}: must be a nonzero Discord snowflake"))
        }
        Some(_) => Err(format!(
            "invalid {key}: expected a Discord snowflake (string or integer), got unexpected type"
        )),
    }
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

fn parse_hook_overrides(args: &Value) -> Result<Vec<HookName>, String> {
    let Some(value) = args.get("no_rly_hooks") else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| "no_rly_hooks must be an array of hook-name strings".to_owned())?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(|name| HookName::parse(name).map_err(|error| error.to_string()))
                .ok_or_else(|| "no_rly_hooks must contain only strings".to_owned())
                .and_then(std::convert::identity)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn hook_override_boundary_parses_canonical_names() {
        let parsed = parse_hook_overrides(&serde_json::json!({
            "no_rly_hooks": ["tier-1", "contradictionary"]
        }))
        .unwrap();
        assert_eq!(
            parsed.iter().map(HookName::as_str).collect::<Vec<_>>(),
            ["tier-1", "contradictionary"]
        );
    }

    #[test]
    fn hook_override_boundary_rejects_invalid_names_and_shapes() {
        assert!(
            parse_hook_overrides(&serde_json::json!({ "no_rly_hooks": ["Not Valid"] })).is_err()
        );
        assert!(parse_hook_overrides(&serde_json::json!({ "no_rly_hooks": "tier-1" })).is_err());
        assert!(parse_hook_overrides(&serde_json::json!({ "no_rly_hooks": [1] })).is_err());
    }

    /// Edge-weighted snowflake strategy. A uniform `0u64..` hits 0 with
    /// probability ~1/2^64, so the zero arms in the tests below were dead
    /// code — the edge case the tests exist for was never exercised. This
    /// forces 0 in ~10% of cases.
    fn id_strategy() -> impl Strategy<Value = u64> {
        prop_oneof![1 => Just(0u64), 9 => 1u64..]
    }

    proptest! {
        /// Zero IDs are rejected before they can reach serenity's
        /// NonZeroU64-backed Id wrappers (which panic on 0); every other u64
        /// parses, in both numeric and string form.
        #[test]
        fn parse_id_rejects_zero_accepts_nonzero(id in id_strategy(), as_string: bool) {
            let args = if as_string {
                json!({ "id": id.to_string() })
            } else {
                json!({ "id": id })
            };
            let result = parse_id(&args, "id");
            if id == 0 {
                prop_assert!(result.is_err(), "zero must be rejected, got: {:?}", result);
            } else {
                prop_assert_eq!(result.map(|s| s.get()), Ok(id));
            }
        }

        /// Optional IDs: nonzero values parse successfully; zero is rejected
        /// (serenity's Id wrappers panic on NonZeroU64(0)).  Absent/empty
        /// values yield Ok(None).
        #[test]
        fn parse_optional_id_rejects_zero_accepts_nonzero(
            id in id_strategy(),
            as_string: bool,
        ) {
            let args = if as_string {
                json!({ "id": id.to_string() })
            } else {
                json!({ "id": id })
            };
            let result = parse_optional_id(&args, "id");
            if id == 0 {
                prop_assert!(
                    result.is_err(),
                    "zero must be an error, got: {:?}",
                    result
                );
            } else {
                prop_assert_eq!(result.map(|opt| opt.map(|s| s.get())), Ok(Some(id)));
            }
        }

        /// Absent key yields Ok(None); empty string also yields Ok(None).
        #[test]
        fn parse_optional_id_absent_yields_none(_id in id_strategy()) {
            // Completely absent key
            let args_absent = json!({});
            prop_assert_eq!(parse_optional_id(&args_absent, "id").map(|opt| opt.map(|s| s.get())), Ok(None));
            // Empty string
            let args_empty = json!({ "id": "" });
            prop_assert_eq!(parse_optional_id(&args_empty, "id").map(|opt| opt.map(|s| s.get())), Ok(None));
        }

        /// The parsed limit always lands in Discord's accepted 1..=100 window
        /// (a limit of 0 would make `has_more: count == limit` hold vacuously
        /// on an empty page), and an absent limit yields the default.
        #[test]
        fn parse_limit_always_in_range(
            // Edge-weighted for the same reason as id_strategy: uniform u64
            // almost never lands on 0 (clamp-to-1 branch) or in 1..=100
            // (exact passthrough branch), leaving both untested.
            limit in proptest::option::of(prop_oneof![
                1 => Just(0u64),
                5 => 1u64..=100,
                4 => 101u64..,
            ]),
            default in 1u8..=100,
        ) {
            let args = match limit {
                Some(l) => json!({ "limit": l }),
                None => json!({}),
            };
            let parsed = parse_limit(&args, default);
            prop_assert!((1..=100).contains(&parsed));
            match limit {
                None => prop_assert_eq!(parsed, default),
                Some(l) => prop_assert_eq!(u64::from(parsed), l.clamp(1, 100)),
            }
        }
    }

    #[test]
    fn strict_optional_id_absent_yields_none() {
        assert_eq!(
            parse_strict_optional_id(&json!({}), "id").map(|o| o.map(|s| s.get())),
            Ok(None)
        );
    }

    #[test]
    fn strict_optional_id_valid_snowflake() {
        let result = parse_strict_optional_id(&json!({"id": "1234567890"}), "id");
        assert_eq!(result.map(|o| o.map(|s| s.get())), Ok(Some(1234567890)));

        let result = parse_strict_optional_id(&json!({"id": 1234567890u64}), "id");
        assert_eq!(result.map(|o| o.map(|s| s.get())), Ok(Some(1234567890)));
    }

    #[test]
    fn strict_optional_id_rejects_null() {
        assert!(parse_strict_optional_id(&json!({"id": null}), "id").is_err());
    }

    #[test]
    fn strict_optional_id_rejects_bool() {
        assert!(parse_strict_optional_id(&json!({"id": true}), "id").is_err());
    }

    #[test]
    fn strict_optional_id_rejects_object() {
        assert!(parse_strict_optional_id(&json!({"id": {}}), "id").is_err());
    }

    #[test]
    fn strict_optional_id_rejects_array() {
        assert!(parse_strict_optional_id(&json!({"id": []}), "id").is_err());
    }

    #[test]
    fn strict_optional_id_rejects_empty_string() {
        assert!(parse_strict_optional_id(&json!({"id": ""}), "id").is_err());
    }

    #[test]
    fn strict_optional_id_rejects_zero() {
        assert!(parse_strict_optional_id(&json!({"id": 0}), "id").is_err());
        assert!(parse_strict_optional_id(&json!({"id": "0"}), "id").is_err());
    }

    #[test]
    fn strict_optional_id_rejects_non_numeric_string() {
        assert!(parse_strict_optional_id(&json!({"id": "abc"}), "id").is_err());
    }
}
