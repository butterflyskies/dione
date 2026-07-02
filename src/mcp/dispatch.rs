//! Tool dispatch: routes `tools/call` requests to the correct handler.

use serde_json::{Value, json};

use crate::config_store::{ConfigStore, DiscordId};
use crate::mcp::ids::Snowflake;
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
        download_attachment, edit_message, fetch_messages, fetch_new_since, get_message,
        react as discord_react, reply, send_dm, send_file,
    },
    render::{render_latex, render_latex_to_channel},
};
use crate::mcp::transport::TransportMode;

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
            reply(&ctx, channel_id, content, reply_to, suppress_ping).await
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
            edit_message(&ctx, channel_id, message_id, content).await
        }
        "fetch_messages" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?.channel();
            let limit = parse_limit(&args, 20);
            fetch_messages(&ctx, channel_id, limit).await
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
        "send_file" => {
            let ctx = server.messaging_ctx(config.clone());
            let channel_id = parse_id(&args, "channel_id")?.channel();
            let file_path = args
                .get("file_path")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing file_path".to_string())?;
            let caption = args.get("caption").and_then(Value::as_str);
            send_file(&ctx, channel_id, file_path, caption).await
        }

        "send_dm" => {
            let ctx = server.messaging_ctx(config.clone());
            let user_id = parse_id(&args, "user_id")?.user();
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
            render_latex_to_channel(&ctx, channel_id, latex, caption).await
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

        // Codex transport: long-poll for queued notifications
        "wait_for_push" if server.mode == TransportMode::Codex => {
            wait_for_push(server, &args).await
        }
        "wait_for_push" => {
            return Err("wait_for_push is only available in codex mode".to_string());
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

// ── Codex transport: wait_for_push ──────────────────────────────────────────

/// Maximum allowed timeout for `wait_for_push` (60 seconds).
const WAIT_FOR_PUSH_MAX_TIMEOUT_MS: u64 = 60_000;
/// Default timeout when none is specified (30 seconds).
const WAIT_FOR_PUSH_DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Block until one or more notifications are available, then return them.
///
/// Drains all pending notifications from the push queue. If the queue is empty,
/// blocks until at least one arrives or the timeout expires.
async fn wait_for_push(server: &DioneServer, args: &Value) -> Value {
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(WAIT_FOR_PUSH_DEFAULT_TIMEOUT_MS)
        .min(WAIT_FOR_PUSH_MAX_TIMEOUT_MS);

    let rx = match server.push_queue.as_ref() {
        Some(rx) => rx,
        None => return json!({ "error": "push queue not initialized" }),
    };

    let mut notifications = Vec::new();

    // First, drain any already-queued notifications without blocking.
    {
        let mut rx_guard = rx.lock().await;
        while let Ok(notif) = rx_guard.try_recv() {
            notifications.push(notif);
        }
    }

    // If we got some, return immediately (don't wait for more).
    if !notifications.is_empty() {
        return json!({ "notifications": notifications });
    }

    // Nothing pending — block until one arrives or timeout.
    let timeout = tokio::time::Duration::from_millis(timeout_ms);
    {
        let mut rx_guard = rx.lock().await;
        match tokio::time::timeout(timeout, rx_guard.recv()).await {
            Ok(Some(notif)) => {
                notifications.push(notif);
                // Drain any additional notifications that arrived while we were waiting.
                while let Ok(notif) = rx_guard.try_recv() {
                    notifications.push(notif);
                }
            }
            Ok(None) => {
                // Channel closed — server shutting down.
                return json!({ "notifications": [], "closed": true });
            }
            Err(_) => {
                // Timeout — no notifications arrived.
            }
        }
    }

    json!({ "notifications": notifications })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

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
}
