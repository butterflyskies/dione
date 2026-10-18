//! Tool dispatch: routes `tools/call` requests to the correct handler.
use crate::{
    config_store::{ConfigStore, DiscordId},
    discord::{
        messaging::{
            LinkPreviews, edit_message_with_embed_control, react as discord_react, release_held,
            rephrase_held, send_dm_with_evidence_and_hook_overrides, send_file_with_hook_overrides,
        },
        messaging_wire,
        rendering::render_to_channel,
        tool_arguments::{
            parse_enabled_sentex_handles, parse_hook_overrides, parse_id, parse_optional_bool,
        },
        typing::send_typing,
    },
    mcp::{
        access_wire, presence_wire, render_wire,
        server::DioneServer,
        tools::{
            diagnostics::{get_version, set_stderr_level, set_trace_level},
            no_rly::{no_rly_condense, no_rly_stats, no_rly_vacuum},
        },
    },
    no_rly::queue::HoldHandle,
    rendering,
};
use serde_json::{Value, json};

fn check_admin_gate(config: &crate::config::LoadedConfig) -> Result<(), String> {
    if config.access.admin_only_mutations {
        Err("admin_only_mutations is enabled; config mutation tools are disabled".to_string())
    } else {
        Ok(())
    }
}

fn mutation_receipt(outcome: crate::config::ConfigMutationOutcome) -> Value {
    match outcome.durability {
        crate::config::ConfigDurability::Durable => json!({
            "ok": true,
            "generation": outcome.generation,
            "durability": "durable",
        }),
        crate::config::ConfigDurability::Unknown { warning } => json!({
            "ok": true,
            "generation": outcome.generation,
            "durability": "unknown",
            "warning": warning,
        }),
    }
}

fn mutation_receipt_with(
    outcome: crate::config::ConfigMutationOutcome,
    key: &str,
    value: impl Into<Value>,
) -> Value {
    let mut receipt = mutation_receipt(outcome);
    receipt[key] = value.into();
    receipt
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
    let config = crate::config::load_config(server.state_dir());

    let result = if let Some(tool) = server.module_tools.find(name) {
        tool.call(&args).await.map_err(|error| error.to_string())?
    } else if let Some(result) = server.harness().protocol().call(name, &args).await {
        result.map_err(|error| error.to_string())?
    } else {
        match name {
            // Messaging
            "no_rly" => {
                let ctx = server.messaging_ctx(config.clone());
                let handle = HoldHandle::new(parse_str(&args, "handle")?);
                messaging_wire::release(&handle, release_held(&ctx, &handle).await)
            }
            "rephrase" => {
                let ctx = server.messaging_ctx(config.clone());
                let handle = HoldHandle::new(parse_str(&args, "handle")?);
                let content = args
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "missing content".to_string())?;
                messaging_wire::rephrase(&handle, rephrase_held(&ctx, &handle, content).await)
            }
            "no_rly_stats" => {
                let ctx = server.no_rly_ctx(config.clone());
                // A present-but-wrong-type filter arg is a caller error, not a
                // silent "no filter" — otherwise `since_days: "7"` returns all-time
                // stats presented as if filtered.
                let since_days = optional_u64(&args, "since_days")?;
                let outcome = optional_str(&args, "outcome")?;
                let pattern = optional_str(&args, "pattern")?;
                no_rly_stats(&ctx, since_days, outcome, pattern).await
            }
            "no_rly_condense" => {
                let ctx = server.no_rly_ctx(config.clone());
                let older_than_days = args.get("older_than_days").and_then(Value::as_u64);
                no_rly_condense(&ctx, older_than_days).await
            }
            "no_rly_vacuum" => {
                let ctx = server.no_rly_ctx(config.clone());
                let older_than_days = args.get("older_than_days").and_then(Value::as_u64);
                no_rly_vacuum(&ctx, older_than_days).await
            }
            "react" => {
                let ctx = server.messaging_ctx(config.clone());
                let channel_id = parse_id(&args, "channel_id")?.channel();
                let message_id = parse_id(&args, "message_id")?.message();
                let emoji = args
                    .get("emoji")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "missing emoji".to_string())?;
                messaging_wire::unit(discord_react(&ctx, channel_id, message_id, emoji).await)
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
                messaging_wire::message(
                    edit_message_with_embed_control(
                        &ctx,
                        channel_id,
                        message_id,
                        content,
                        &no_rly_hooks,
                        parse_optional_bool(&args, "suppress_embeds")?,
                    )
                    .await,
                )
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
                messaging_wire::message(
                    send_file_with_hook_overrides(
                        &ctx,
                        channel_id,
                        file_path,
                        caption,
                        &no_rly_hooks,
                    )
                    .await,
                )
            }

            "send_dm" => {
                let ctx = server.messaging_ctx(config.clone());
                let user_id = parse_id(&args, "user_id")?.user();
                let content = args
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "missing content".to_string())?;
                let no_rly_hooks = parse_hook_overrides(&args)?;
                let sentex_handles =
                    parse_enabled_sentex_handles(&args, config.delivery.evidence_markers_enabled)?;
                messaging_wire::dm(
                    send_dm_with_evidence_and_hook_overrides(
                        &ctx,
                        user_id,
                        content,
                        &no_rly_hooks,
                        &sentex_handles,
                        LinkPreviews::from_suppressed(
                            parse_optional_bool(&args, "suppress_embeds")?.unwrap_or(false),
                        ),
                    )
                    .await,
                )
            }

            // Access
            "list_access_requests" => {
                let ctx = server.access_ctx(config.clone());
                access_wire::requests(ctx.list().await)
            }
            "approve_access" => {
                check_admin_gate(&config)?;
                let ctx = server.access_ctx(config.clone());
                let user_id = parse_id(&args, "user_id")?.user();
                access_wire::approval(ctx.approve(user_id).await)
            }
            "deny_access" => {
                check_admin_gate(&config)?;
                let ctx = server.access_ctx(config.clone());
                let user_id = parse_id(&args, "user_id")?.user();
                access_wire::denial(ctx.deny(user_id).await)
            }

            // Config management — read-only (no ConfigStore mutation needed)
            "list_config_channels" => {
                let mut result = ConfigStore::list_channels(server.state_dir());
                // Annotate with active guild mutes so the caller sees muted guilds
                // alongside the channel list (channels don't carry guild_id, so
                // per-channel annotation isn't possible).
                if let Some(store) = crate::mute_store::global() {
                    let now = store.now();
                    let mutes: Vec<Value> = store
                        .list_muted()
                        .into_iter()
                        .map(|m| {
                            json!({
                                "guild_id": m.guild_id.to_string(),
                                "muted_until": m.muted_until.to_rfc3339(),
                                "remaining_seconds": m.remaining_seconds(now),
                                "cutoff_event_id": m.cutoff_event_id,
                            })
                        })
                        .collect();
                    if !mutes.is_empty() {
                        result["guild_mutes"] = json!(mutes);
                    }
                }
                result
            }
            "get_access_config" => ConfigStore::get_access(server.state_dir()),
            "attention" => {
                let command: crate::attention::control::AttentionCommand =
                    serde_json::from_value(args.clone())
                        .map_err(|error| format!("invalid attention command: {error}"))?;
                crate::attention::control::execute(
                    server.attention.clone(),
                    command,
                    !config.access.admin_only_mutations,
                )
                .await?
            }

            // Config management — mutations (serialized through ConfigRuntime, admin-gated)
            "add_channel" => {
                check_admin_gate(&config)?;
                let id = parse_str(&args, "id")?.to_owned();
                let response_id = id.clone();
                DiscordId::parse(&id)?;
                let require_mention = args.get("require_mention").and_then(Value::as_bool);
                let allow_from = parse_string_array(&args, "allow_from").unwrap_or_default();
                for af in &allow_from {
                    DiscordId::parse(af)?;
                }
                match crate::config::ConfigRuntime::new(server.state_dir().clone())
                    .mutate(move |editor| {
                        editor.add_channel_entry(&id, require_mention.unwrap_or(true), allow_from)
                    })
                    .await
                {
                    Ok(outcome) => mutation_receipt_with(outcome, "id", response_id),
                    Err(e) => json!({ "error": e.to_string() }),
                }
            }
            "remove_channel" => {
                check_admin_gate(&config)?;
                let id = parse_str(&args, "id")?.to_owned();
                let response_id = id.clone();
                DiscordId::parse(&id)?;
                match crate::config::ConfigRuntime::new(server.state_dir().clone())
                    .mutate(move |editor| editor.remove_channel_entry(&id))
                    .await
                {
                    Ok(outcome) => mutation_receipt_with(outcome, "id", response_id),
                    Err(e) => json!({ "error": e.to_string() }),
                }
            }
            "update_channel" => {
                check_admin_gate(&config)?;
                let id = parse_str(&args, "id")?.to_owned();
                let response_id = id.clone();
                DiscordId::parse(&id)?;
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
                    match crate::config::ConfigRuntime::new(server.state_dir().clone())
                        .mutate(move |editor| {
                            editor.update_channel_entry(&id, require_mention, allow_from)
                        })
                        .await
                    {
                        Ok(outcome) => mutation_receipt_with(outcome, "id", response_id),
                        Err(e) => json!({ "error": e.to_string() }),
                    }
                }
            }
            "update_dm_policy" => {
                check_admin_gate(&config)?;
                let policy = parse_str(&args, "policy")?.to_owned();
                let response_policy = policy.clone();
                if !matches!(policy.as_str(), "drop" | "queue" | "disabled") {
                    json!({ "error": format!("invalid dm_policy: {policy}; must be one of: drop, queue, disabled") })
                } else {
                    match crate::config::ConfigRuntime::new(server.state_dir().clone())
                        .mutate(move |editor| {
                            editor.set_dm_policy(&policy);
                            Ok(())
                        })
                        .await
                    {
                        Ok(outcome) => mutation_receipt_with(outcome, "dm_policy", response_policy),
                        Err(e) => json!({ "error": e.to_string() }),
                    }
                }
            }
            "add_allow_from" => {
                check_admin_gate(&config)?;
                let user_id = parse_str(&args, "user_id")?.to_owned();
                let response_user_id = user_id.clone();
                DiscordId::parse(&user_id)?;
                match crate::config::ConfigRuntime::new(server.state_dir().clone())
                    .mutate(move |editor| editor.add_to_allow_from(&user_id))
                    .await
                {
                    Ok(outcome) => mutation_receipt_with(outcome, "user_id", response_user_id),
                    Err(e) => json!({ "error": e.to_string() }),
                }
            }
            "remove_allow_from" => {
                check_admin_gate(&config)?;
                let user_id = parse_str(&args, "user_id")?.to_owned();
                let response_user_id = user_id.clone();
                DiscordId::parse(&user_id)?;
                match crate::config::ConfigRuntime::new(server.state_dir().clone())
                    .mutate(move |editor| editor.remove_from_allow_from(&user_id))
                    .await
                {
                    Ok(outcome) => mutation_receipt_with(outcome, "user_id", response_user_id),
                    Err(e) => json!({ "error": e.to_string() }),
                }
            }

            // Bot state
            "send_typing" => {
                let ctx = server.typing_ctx(config.clone());
                let channel_id = parse_id(&args, "channel_id")?.channel();
                presence_wire::typing(send_typing(&ctx, channel_id).await)
            }
            "get_presence" => presence_wire::get(server.presence.snapshot().await),
            "set_presence" => match presence_wire::parse_request(&args) {
                Ok(request) => presence_wire::set(server.presence.set(request).await),
                Err(error) => json!({ "error": error.to_string() }),
            },

            // Rendering
            "render_latex" => {
                let latex = args
                    .get("latex")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "missing latex".to_string())?;
                render_wire::file(rendering::render_file(latex))
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
                render_wire::channel(
                    render_to_channel(&ctx, channel_id, latex, caption, &no_rly_hooks).await,
                )
            }

            // Diagnostics
            "reload_config" => {
                check_admin_gate(&config)?;
                // Async entry: serialized under the config writer, file I/O on
                // the blocking pool — never blocks the async runtime.
                let (_, error) = crate::config::ConfigRuntime::new(server.state_dir().clone())
                    .reload()
                    .await;
                match error {
                    Some(e) => json!({ "error": e }),
                    None => json!({ "ok": true }),
                }
            }
            "get_version" => get_version().await,

            // Guild mute management (admin-gated)
            "mute_server" => {
                check_admin_gate(&config)?;
                let guild_id_str = parse_str(&args, "guild_id")?;
                DiscordId::parse(guild_id_str)?;
                let guild_id: u64 = guild_id_str
                    .parse()
                    .map_err(|_| format!("invalid guild_id: {guild_id_str}"))?;
                let ttl_minutes = args
                    .get("ttl_minutes")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| "missing or invalid ttl_minutes".to_string())?;
                if ttl_minutes == 0 {
                    return Err("ttl_minutes must be at least 1".to_string());
                }
                if ttl_minutes > crate::mute_store::MAX_TTL_MINUTES {
                    return Err(format!(
                        "ttl_minutes ({ttl_minutes}) exceeds maximum ({})",
                        crate::mute_store::MAX_TTL_MINUTES,
                    ));
                }
                let reason = args.get("reason").and_then(Value::as_str).map(String::from);
                let store = crate::mute_store::global()
                    .ok_or_else(|| "mute store not initialized".to_string())?;
                match store
                    .mute_guild(guild_id, ttl_minutes, "admin".into(), reason)
                    .await
                {
                    Ok(mute) => json!({
                        "ok": true,
                        "guild_id": guild_id.to_string(),
                        "muted_until": mute.muted_until.to_rfc3339(),
                        "remaining_seconds": mute.remaining_seconds(store.now()),
                        "cutoff_event_id": mute.cutoff_event_id,
                    }),
                    Err(e) => json!({ "error": e }),
                }
            }
            "unmute_server" => {
                check_admin_gate(&config)?;
                let guild_id_str = parse_str(&args, "guild_id")?;
                DiscordId::parse(guild_id_str)?;
                let guild_id: u64 = guild_id_str
                    .parse()
                    .map_err(|_| format!("invalid guild_id: {guild_id_str}"))?;
                let store = crate::mute_store::global()
                    .ok_or_else(|| "mute store not initialized".to_string())?;
                match store.unmute_guild(guild_id).await {
                    Ok(mute) => json!({
                        "ok": true,
                        "guild_id": guild_id.to_string(),
                        "was_muted_until": mute.muted_until.to_rfc3339(),
                    }),
                    Err(e) => json!({ "error": e }),
                }
            }
            "list_muted_servers" => {
                check_admin_gate(&config)?;
                let store = crate::mute_store::global()
                    .ok_or_else(|| "mute store not initialized".to_string())?;
                let now = store.now();
                let mutes: Vec<Value> = store
                    .list_muted()
                    .into_iter()
                    .map(|m| {
                        json!({
                            "guild_id": m.guild_id.to_string(),
                            "muted_until": m.muted_until.to_rfc3339(),
                            "muted_at": m.muted_at.to_rfc3339(),
                            "muted_by": m.muted_by,
                            "reason": m.reason,
                            "remaining_seconds": m.remaining_seconds(now),
                            "cutoff_event_id": m.cutoff_event_id,
                        })
                    })
                    .collect();
                json!({ "muted_servers": mutes })
            }
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
        }
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

fn parse_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}

/// An optional integer argument: absent or null yields `None`, a present value
/// of the wrong type is an error (rather than silently dropped).
fn optional_u64(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("invalid {key}: expected an integer")),
    }
}

/// An optional string argument: absent or null yields `None`, a present value
/// of the wrong type is an error (rather than silently dropped).
fn optional_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| format!("invalid {key}: expected a string")),
    }
}

fn parse_string_array(args: &Value, key: &str) -> Option<Vec<String>> {
    args.get(key).and_then(Value::as_array).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{discord::tool_arguments::parse_optional_id, pre_send::HookName};
    use proptest::prelude::*;

    #[tokio::test]
    async fn access_contract_dispatch_gate_parser_and_tool_error_envelopes() {
        let (_dir, server) = presence_test_server().await;
        std::fs::write(
            server.state_dir().join("config.toml"),
            "[access]\nadmin_only_mutations = true\n",
        )
        .unwrap();
        assert!(
            crate::config::reload_config(server.state_dir())
                .await
                .1
                .is_none()
        );
        for tool in ["approve_access", "deny_access"] {
            assert_eq!(
                call_tool(&server, tool, json!({"user_id":"bad"}))
                    .await
                    .unwrap_err(),
                "admin_only_mutations is enabled; config mutation tools are disabled"
            );
        }
        assert_eq!(
            call_tool(&server, "list_access_requests", json!({}))
                .await
                .unwrap(),
            json!({"content":[{"type":"text","text":"{\"requests\":[]}"}]})
        );
        std::fs::write(
            server.state_dir().join("config.toml"),
            "[access]\nadmin_only_mutations = false\n",
        )
        .unwrap();
        assert!(
            crate::config::reload_config(server.state_dir())
                .await
                .1
                .is_none()
        );
        for tool in ["approve_access", "deny_access"] {
            for (args, error) in [
                (json!({}), "missing user_id"),
                (
                    json!({"user_id":0}),
                    "invalid user_id: must be a nonzero Discord snowflake",
                ),
                (json!({"user_id":"bad"}), "invalid user_id: not a valid u64"),
                (
                    json!({"user_id":false}),
                    "invalid user_id: expected a numeric or string ID",
                ),
            ] {
                assert_eq!(call_tool(&server, tool, args).await.unwrap_err(), error);
            }
            for args in [json!({"user_id":42}), json!({"user_id":"42"})] {
                assert_eq!(
                    call_tool(&server, tool, args).await.unwrap(),
                    json!({
                        "content":[{"type":"text","text":"{\"error\":\"no pending request for user 42\"}"}],
                        "isError":true
                    })
                );
            }
        }
    }

    #[tokio::test]
    async fn render_contract_dispatch_argument_order_and_error_envelopes() {
        let (_dir, server) = presence_test_server().await;
        for latex in [Value::Null, json!(false), json!(3), json!([]), json!({})] {
            assert_eq!(
                call_tool(&server, "render_latex", json!({"latex":latex}))
                    .await
                    .unwrap_err(),
                "missing latex"
            );
        }
        for (tool, args, expected) in [
            ("render_latex", json!({}), "missing latex"),
            (
                "render_latex_to_channel",
                json!({"no_rly_hooks":false}),
                "missing channel_id",
            ),
            (
                "render_latex_to_channel",
                json!({"channel_id":0,"latex":false}),
                "invalid channel_id: must be a nonzero Discord snowflake",
            ),
            (
                "render_latex_to_channel",
                json!({"channel_id":"bad","latex":"x"}),
                "invalid channel_id: not a valid u64",
            ),
            (
                "render_latex_to_channel",
                json!({"channel_id":42,"no_rly_hooks":false}),
                "missing latex",
            ),
            (
                "render_latex_to_channel",
                json!({"channel_id":42,"latex":"x","no_rly_hooks":false}),
                "no_rly_hooks must be an array of hook-name strings",
            ),
            (
                "render_latex_to_channel",
                json!({"channel_id":42,"latex":"x","no_rly_hooks":[3]}),
                "no_rly_hooks must contain only strings",
            ),
        ] {
            assert_eq!(call_tool(&server, tool, args).await.unwrap_err(), expected);
        }
        assert_presence_tool_result(
            call_tool(
                &server,
                "render_latex",
                json!({"latex":"\\iftypst","ignored":true}),
            )
            .await
            .unwrap(),
            json!({"error":"\\iftypst is not supported"}),
            true,
        );
        for caption in [Value::Null, json!(false), json!(8), json!([]), json!({})] {
            assert_presence_tool_result(
                call_tool(
                    &server,
                    "render_latex_to_channel",
                    json!({
                        "channel_id":"+42",
                        "latex":"\\iftypst",
                        "caption":caption,
                        "no_rly_hooks":["tier-1"]
                    }),
                )
                .await
                .unwrap(),
                json!({"error":"no_rly_hooks cannot be used when no caption is sent"}),
                true,
            );
        }
        assert_presence_tool_result(
            call_tool(
                &server,
                "render_latex_to_channel",
                json!({"channel_id":42,"latex":"\\iftypst","caption":"","no_rly_hooks":["tier-1"]}),
            )
            .await
            .unwrap(),
            json!({"error":"channel 42 is not a permitted outbound target"}),
            true,
        );
    }

    #[tokio::test]
    async fn render_contract_dispatch_file_success_envelope() {
        let (_dir, server) = presence_test_server().await;
        let response = call_tool(&server, "render_latex", json!({"latex":"x","ignored":true}))
            .await
            .unwrap();
        let result: Value =
            serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
        let path = result["path"].as_str().unwrap();
        let bytes = std::fs::read(path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_presence_tool_result(
            response,
            json!({"ok":true,"path":path,"size_bytes":bytes.len()}),
            false,
        );
    }

    #[tokio::test]
    async fn introspection_contract_registered_member_uses_reloaded_configuration() {
        let member = json!({
            "user":{"id":"91","username":"returned"}, "nick":null,
            "roles":[], "joined_at":"2026-07-10T12:30:00Z",
            "deaf":false,"mute":false,"flags":0
        });
        let (ctx, requests) = crate::discord::introspection_wire::tests::fixture(vec![
            ("200 OK", member.clone()),
            ("200 OK", member),
        ])
        .await;
        let (_dir, server) = presence_test_server_with_http(ctx.http).await;
        for (timezone, expected) in [
            ("America/Chicago", "2026-07-10T07:30:00-05:00"),
            ("Asia/Tokyo", "2026-07-10T21:30:00+09:00"),
        ] {
            std::fs::write(
                server.state_dir().join("config.toml"),
                format!("timezone = \"{timezone}\"\n"),
            )
            .unwrap();
            assert!(
                crate::config::reload_config(server.state_dir())
                    .await
                    .1
                    .is_none()
            );
            let response = call_tool(
                &server,
                "get_member",
                json!({"guild_id":"7","user_id":"80"}),
            )
            .await
            .unwrap();
            let value: Value =
                serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(
                value,
                json!({"user_id":"91","username":"returned","nick":null,"roles":[],"joined_at":expected})
            );
        }
        assert_eq!(
            requests.await.unwrap(),
            ["GET /api/v10/guilds/7/members/80 HTTP/1.1"; 2]
        );
    }

    #[tokio::test]
    async fn introspection_contract_dispatch_parsing_and_http_envelopes() {
        let (ctx, requests) = crate::discord::introspection_wire::tests::fixture(vec![
            ("200 OK", json!([])),
            ("200 OK", json!([])),
            ("200 OK", json!({"id":"91","type":0,"name":"returned-channel"})),
            ("200 OK", json!({"id":"92","username":"returned-user","discriminator":"7","bot":true})),
            ("200 OK", json!({"user":{"id":"93","username":"returned-member"},"nick":null,"roles":["9","2"],"joined_at":null,"deaf":false,"mute":false,"flags":0})),
            ("200 OK", json!([])),
            ("200 OK", json!([])),
            ("403 Forbidden", json!({"code":50013,"message":"Missing Permissions"})),
        ]).await;
        let (_dir, server) = presence_test_server_with_http(ctx.http).await;
        for (tool, args, expected) in [
            ("list_channels", json!({}), "missing guild_id"),
            (
                "list_roles",
                json!({"guild_id":null}),
                "invalid guild_id: expected a numeric or string ID",
            ),
            (
                "list_emojis",
                json!({"guild_id":0}),
                "invalid guild_id: must be a nonzero Discord snowflake",
            ),
            (
                "get_channel",
                json!({"channel_id":"bad"}),
                "invalid channel_id: not a valid u64",
            ),
            (
                "get_channel",
                json!({"channel_id":-1}),
                "invalid channel_id: expected a numeric or string ID",
            ),
            (
                "get_user",
                json!({"user_id":1.5}),
                "invalid user_id: expected a numeric or string ID",
            ),
            (
                "get_user",
                json!({"user_id":[]}),
                "invalid user_id: expected a numeric or string ID",
            ),
            (
                "get_user",
                json!({"user_id":{}}),
                "invalid user_id: expected a numeric or string ID",
            ),
            (
                "get_user",
                json!({"user_id":true}),
                "invalid user_id: expected a numeric or string ID",
            ),
            (
                "get_member",
                json!({"guild_id":0,"user_id":"bad"}),
                "invalid guild_id: must be a nonzero Discord snowflake",
            ),
            ("get_member", json!({"guild_id":7}), "missing user_id"),
            (
                "get_member",
                json!({"guild_id":7,"user_id":"bad"}),
                "invalid user_id: not a valid u64",
            ),
        ] {
            assert_eq!(call_tool(&server, tool, args).await.unwrap_err(), expected);
        }
        for (tool, args, expected) in [
            (
                "list_guilds",
                json!({"guild_id":false,"ignored":[1,2]}),
                json!({"content":[{"type":"text","text":"{\"guilds\":[]}"}]}),
            ),
            (
                "list_channels",
                json!({"guild_id":7,"ignored":true}),
                json!({"content":[{"type":"text","text":"{\"channels\":[]}"}]}),
            ),
            (
                "get_channel",
                json!({"channel_id":"42"}),
                json!({"content":[{"type":"text","text":"{\"id\":\"91\",\"kind\":\"guild\",\"name\":\"returned-channel\",\"permission_overwrites\":[]}"}]}),
            ),
            (
                "get_user",
                json!({"user_id":"+80"}),
                json!({"content":[{"type":"text","text":"{\"bot\":true,\"discriminator\":\"7\",\"id\":\"92\",\"name\":\"returned-user\"}"}]}),
            ),
            (
                "get_member",
                json!({"guild_id":"+7","user_id":80}),
                json!({"content":[{"type":"text","text":"{\"joined_at\":null,\"nick\":null,\"roles\":[\"9\",\"2\"],\"user_id\":\"93\",\"username\":\"returned-member\"}"}]}),
            ),
            (
                "list_roles",
                json!({"guild_id":"7"}),
                json!({"content":[{"type":"text","text":"{\"roles\":[]}"}]}),
            ),
            (
                "list_emojis",
                json!({"guild_id":7}),
                json!({"content":[{"type":"text","text":"{\"emojis\":[]}"}]}),
            ),
            (
                "get_user",
                json!({"user_id":80}),
                json!({"content":[{"type":"text","text":"{\"error\":\"Missing Permissions\"}"}],"isError":true}),
            ),
        ] {
            assert_eq!(call_tool(&server, tool, args).await.unwrap(), expected);
        }
        assert_eq!(
            requests.await.unwrap(),
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
    #[tokio::test]
    async fn search_contract_dispatch_guild_then_arguments_and_tool_errors() {
        let (_dir, server) = presence_test_server().await;
        for (args, expected) in [
            (json!({"limit":0}), "missing guild_id"),
            (
                json!({"guild_id":0,"limit":0}),
                "invalid guild_id: must be a nonzero Discord snowflake",
            ),
            (
                json!({"guild_id":"bad","has":null}),
                "invalid guild_id: not a valid u64",
            ),
            (
                json!({"guild_id":"7","limit":0}),
                "invalid search params: search limit must be 1..=25, got 0",
            ),
            (
                json!({"guild_id":"7","before":"bad","after":"also-bad"}),
                "invalid search params: expected snowflake ID or YYYY-MM-DD, got 'also-bad'",
            ),
            (
                json!({"guild_id":"7","has":null}),
                "invalid search params: invalid type: null, expected a sequence",
            ),
        ] {
            assert_eq!(
                call_tool(&server, "search_messages", args)
                    .await
                    .unwrap_err(),
                expected
            );
        }
        for args in [
            json!({"guild_id":7}),
            json!({"guild_id":"7","sort_by":"timestamp","sort_order":"desc","author_id":null}),
        ] {
            assert_eq!(
                call_tool(&server, "search_messages", args).await.unwrap(),
                json!({
                    "content":[{"type":"text","text":"{\"error\":\"at least one search filter is required (content, author_id, mentions, has, before, after, or channel_id)\"}"}],
                    "isError":true
                })
            );
        }
        assert_eq!(
            call_tool(
                &server,
                "search_messages",
                json!({
                    "guild_id":"7","before":"2014-01-01","after":"2013-01-01"
                })
            )
            .await
            .unwrap(),
            json!({
                "content":[{"type":"text","text":"{\"error\":\"invalid date '2014-01-01: date is before Discord epoch (2015-01-01)': expected YYYY-MM-DD\"}"}],
                "isError":true
            })
        );
    }

    #[tokio::test]
    async fn management_contract_dispatch_parse_order_and_error_envelopes() {
        let (_dir, server) = presence_test_server().await;
        for tool in [
            "pin_message",
            "unpin_message",
            "delete_message",
            "create_thread",
        ] {
            for (args, expected) in [
                (json!({}), "missing channel_id"),
                (
                    json!({"channel_id":0,"message_id":"bad"}),
                    "invalid channel_id: must be a nonzero Discord snowflake",
                ),
                (
                    json!({"channel_id":"bad","message_id":0}),
                    "invalid channel_id: not a valid u64",
                ),
                (
                    json!({"channel_id":42,"message_id":0}),
                    "invalid message_id: must be a nonzero Discord snowflake",
                ),
                (
                    json!({"channel_id":"42","message_id":"bad"}),
                    "invalid message_id: not a valid u64",
                ),
            ] {
                assert_eq!(call_tool(&server, tool, args).await.unwrap_err(), expected);
            }
            assert_eq!(
                call_tool(
                    &server,
                    tool,
                    json!({"channel_id":42,"message_id":"9001","name":"","extra":true})
                )
                .await
                .unwrap(),
                json!({
                    "content":[{"type":"text","text":"{\"error\":\"channel not in allowlist\"}"}], "isError":true
                })
            );
        }
        for tool in ["pin_message", "unpin_message", "delete_message"] {
            assert_eq!(
                call_tool(&server, tool, json!({"channel_id":42}))
                    .await
                    .unwrap_err(),
                "missing message_id"
            );
        }
        for message in [
            Value::Null,
            json!(""),
            json!(false),
            json!(-1),
            json!(1.5),
            json!([]),
            json!({}),
        ] {
            let args = json!({"channel_id":"42","message_id":message});
            assert_eq!(parse_optional_id(&args, "message_id").unwrap(), None);
            assert_eq!(
                call_tool(&server, "create_thread", args.clone())
                    .await
                    .unwrap_err(),
                "missing name"
            );
            let mut args = args;
            args["name"] = json!("");
            assert_eq!(
                call_tool(&server, "create_thread", args).await.unwrap(),
                json!({
                    "content":[{"type":"text","text":"{\"error\":\"channel not in allowlist\"}"}], "isError":true
                })
            );
        }
        for args in [
            json!({"channel_id":42}),
            json!({"channel_id":42,"name":7}),
            json!({"channel_id":42,"message_id":9001}),
        ] {
            assert_eq!(
                call_tool(&server, "create_thread", args).await.unwrap_err(),
                "missing name"
            );
        }
    }
    async fn presence_test_server() -> (tempfile::TempDir, DioneServer) {
        presence_test_server_with_http(std::sync::Arc::new(serenity::http::Http::new("fake"))).await
    }
    async fn presence_test_server_with_http(
        http: std::sync::Arc<serenity::http::Http>,
    ) -> (tempfile::TempDir, DioneServer) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let state_dir =
            camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 path");
        let (notification_tx, _notification_rx) = tokio::sync::mpsc::channel(1);
        let server = DioneServer::new(
            crate::state::new_state(),
            std::sync::Arc::new(tokio::sync::Mutex::new(crate::queue::AccessQueue::load(
                &state_dir,
            ))),
            http,
            state_dir.clone(),
            notification_tx,
            crate::tracing_channel::TraceLevelController::noop(),
            crate::harness::HarnessConnection::claude(),
            std::sync::Arc::new(crate::no_rly::consent::ConsentGate::new(&state_dir)),
            std::sync::Arc::new(crate::ingress_ledger::IngressLedger::new()),
            crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            ),
            None,
        )
        .await
        .expect("Discord reply registration");
        (dir, server)
    }

    fn assert_presence_tool_result(response: Value, expected: Value, is_error: bool) {
        let expected = if is_error {
            json!({
                "content": [{"type": "text", "text": expected.to_string()}],
                "isError": true
            })
        } else {
            json!({"content": [{"type": "text", "text": expected.to_string()}]})
        };
        assert_eq!(response, expected);
    }

    #[tokio::test]
    async fn presence_wire_defaults_and_activity_text() {
        let (_dir, server) = presence_test_server().await;
        let (_, presence) = crate::discord::presence::wire_shared_presence();
        let server = server.with_presence(presence);
        assert_presence_tool_result(
            call_tool(&server, "get_presence", json!({})).await.unwrap(),
            json!({
                "set_this_process": false,
                "sink_installed": false,
                "note": "no presence has been requested since this process started; Discord is showing the gateway default. sink_installed reports whether a shard messenger has been installed since the last gateway ready — disconnects are not tracked"
            }),
            false,
        );

        for (args, status, kind, name) in [
            (json!({}), "online", Value::Null, Value::Null),
            (
                json!({"online_status": null, "activity_type": null, "activity_name": null}),
                "online",
                Value::Null,
                Value::Null,
            ),
            (
                json!({"online_status": 17, "activity_type": false, "activity_name": []}),
                "online",
                Value::Null,
                Value::Null,
            ),
            (
                json!({"activity_name": "ignored"}),
                "online",
                Value::Null,
                Value::Null,
            ),
            (
                json!({"online_status": "idle", "activity_type": "playing", "activity_name": ""}),
                "idle",
                json!("playing"),
                json!(""),
            ),
            (
                json!({"online_status": "dnd", "activity_type": "custom", "activity_name": "human text"}),
                "dnd",
                json!("custom"),
                json!("human text"),
            ),
            (
                json!({"online_status": "invisible", "activity_type": "watching", "activity_name": "  "}),
                "invisible",
                json!("watching"),
                json!("  "),
            ),
            (
                json!({"activity_type": "listening", "activity_name": "music"}),
                "online",
                json!("listening"),
                json!("music"),
            ),
            (
                json!({"activity_type": "competing", "activity_name": "chess"}),
                "online",
                json!("competing"),
                json!("chess"),
            ),
        ] {
            let before = chrono::Utc::now();
            assert_presence_tool_result(
                call_tool(&server, "set_presence", args).await.unwrap(),
                json!({"ok": true, "status": "applied"}),
                false,
            );
            let after = chrono::Utc::now();
            let mut response = call_tool(&server, "get_presence", json!({})).await.unwrap();
            let mut payload: Value =
                serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
            let timestamp =
                chrono::DateTime::parse_from_rfc3339(payload["set_at"].as_str().unwrap())
                    .unwrap()
                    .with_timezone(&chrono::Utc);
            assert!(timestamp >= before && timestamp <= after);
            payload["set_at"] = json!("2026-10-10T00:00:00+00:00");
            response["content"][0]["text"] = json!(payload.to_string());
            assert_presence_tool_result(
                response,
                json!({
                    "set_this_process": true,
                    "online_status": status,
                    "activity_type": kind,
                    "activity_name": name,
                    "set_at": "2026-10-10T00:00:00+00:00",
                    "sink_installed": false,
                    "source": "last requested presence (replayed on reconnect), not a Discord read-back; sink_installed is not a live connection probe"
                }),
                false,
            );
        }
    }

    #[tokio::test]
    async fn presence_wire_validation_precedes_capability_and_preserves_state() {
        let (_dir, unavailable) = presence_test_server().await;
        let (_direct_dir, direct) = presence_test_server().await;
        let (_, presence) = crate::discord::presence::wire_shared_presence();
        let direct = direct.with_presence(presence);
        call_tool(&direct, "set_presence", json!({"online_status": "idle"}))
            .await
            .unwrap();
        let original = call_tool(&direct, "get_presence", json!({})).await.unwrap();
        for server in [&unavailable, &direct] {
            for (args, error) in [
                (
                    json!({"online_status": "bad", "activity_type": "bad"}),
                    "invalid online_status: bad; must be one of: online, idle, dnd, invisible",
                ),
                (
                    json!({"activity_type": "bad"}),
                    "invalid activity_type: bad; must be one of: playing, watching, listening, competing, custom",
                ),
                (
                    json!({"activity_type": "custom"}),
                    "activity_name is required when activity_type is set",
                ),
                (
                    json!({"activity_type": "custom", "activity_name": null}),
                    "activity_name is required when activity_type is set",
                ),
                (
                    json!({"activity_type": "custom", "activity_name": 7}),
                    "activity_name is required when activity_type is set",
                ),
            ] {
                assert_presence_tool_result(
                    call_tool(server, "set_presence", args)
                        .await
                        .expect("validation is a tool error"),
                    json!({"error": error}),
                    true,
                );
            }
        }
        assert_eq!(
            call_tool(&direct, "get_presence", json!({})).await.unwrap(),
            original
        );
        assert_presence_tool_result(
            call_tool(&unavailable, "set_presence", json!({}))
                .await
                .unwrap(),
            json!({"error": "presence updates require the discord gateway command channel"}),
            true,
        );
        assert_presence_tool_result(
            call_tool(&unavailable, "get_presence", json!({}))
                .await
                .unwrap(),
            json!({"error": "presence state is not wired on this transport"}),
            true,
        );
    }

    #[tokio::test]
    async fn presence_wire_queue_acceptance_and_closed_channel() {
        let (_dir, server) = presence_test_server().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let server = server.with_presence(crate::discord::presence::PresenceCapability::Queued(tx));
        assert_presence_tool_result(
            call_tool(&server, "set_presence", json!({"activity_type": "custom"}))
                .await
                .unwrap(),
            json!({"error": "activity_name is required when activity_type is set"}),
            true,
        );
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert_presence_tool_result(
            call_tool(&server, "set_presence", json!({})).await.unwrap(),
            json!({"ok": true, "status": "accepted"}),
            false,
        );
        assert_presence_tool_result(
            call_tool(&server, "get_presence", json!({})).await.unwrap(),
            json!({"error": "presence state is not wired on this transport"}),
            true,
        );
        drop(rx);
        assert_presence_tool_result(
            call_tool(&server, "set_presence", json!({})).await.unwrap(),
            json!({"error": "failed to send presence command: channel closed"}),
            true,
        );
    }

    #[tokio::test]
    async fn presence_round_trip_crosses_dispatch_and_composition() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let state_dir =
            camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 path");
        let (notification_tx, _notification_rx) = tokio::sync::mpsc::channel(1);
        let (gateway_handle, server_presence) = crate::discord::presence::wire_shared_presence();
        let crate::discord::presence::PresenceCapability::Direct(server_handle) = &server_presence
        else {
            panic!("assembly wires the direct MCP handle");
        };
        assert!(gateway_handle.is_same_authority(server_handle));
        let server = DioneServer::new(
            crate::state::new_state(),
            std::sync::Arc::new(tokio::sync::Mutex::new(crate::queue::AccessQueue::load(
                &state_dir,
            ))),
            std::sync::Arc::new(serenity::http::Http::new("fake")),
            state_dir.clone(),
            notification_tx,
            crate::tracing_channel::TraceLevelController::noop(),
            crate::harness::HarnessConnection::claude(),
            std::sync::Arc::new(crate::no_rly::consent::ConsentGate::new(&state_dir)),
            std::sync::Arc::new(crate::ingress_ledger::IngressLedger::new()),
            crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            ),
            None,
        )
        .await
        .expect("Discord reply registration")
        .with_presence(server_presence);

        fn unwrap_tool_result(response: &Value) -> Value {
            assert!(
                response.get("isError").is_none(),
                "tool call must not error: {response}"
            );
            let text = response["content"][0]["text"]
                .as_str()
                .expect("tool result carries a text payload");
            serde_json::from_str(text).expect("tool result text is JSON")
        }

        let set = call_tool(
            &server,
            "set_presence",
            json!({
                "online_status": "dnd",
                "activity_type": "custom",
                "activity_name": "boundary proof"
            }),
        )
        .await
        .expect("set_presence dispatches");
        let set = unwrap_tool_result(&set);
        assert_eq!(set["ok"], true);
        assert_eq!(set["status"], "applied");

        let get = call_tool(&server, "get_presence", json!({}))
            .await
            .expect("get_presence dispatches");
        let get = unwrap_tool_result(&get);
        assert_eq!(get["set_this_process"], true);
        assert_eq!(get["online_status"], "dnd");
        assert_eq!(get["activity_name"], "boundary proof");
        assert_eq!(get["sink_installed"], false);

        let observed = gateway_handle.snapshot().await;
        assert_eq!(
            observed.desired().map(|d| d.request().status()),
            Some(crate::discord::presence::OnlineStatus::Dnd)
        );
    }

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
                prop_assert_eq!(result.unwrap().get(), id);
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
                prop_assert_eq!(result.unwrap().map(|s| s.get()), Some(id));
            }
        }

        /// Absent key yields Ok(None); empty string also yields Ok(None).
        #[test]
        fn parse_optional_id_absent_yields_none(_id in id_strategy()) {
            // Completely absent key
            let args_absent = json!({});
            prop_assert_eq!(parse_optional_id(&args_absent, "id").unwrap(), None);
            // Empty string
            let args_empty = json!({ "id": "" });
            prop_assert_eq!(parse_optional_id(&args_empty, "id").unwrap(), None);
        }
    }
}

#[cfg(test)]
mod embed_control_tests {
    use super::*;
    #[test]
    fn suppress_embeds_requires_boolean_when_present() {
        assert_eq!(
            parse_optional_bool(&json!({}), "suppress_embeds").unwrap(),
            None
        );
        for value in [true, false] {
            assert_eq!(
                parse_optional_bool(&json!({"suppress_embeds": value}), "suppress_embeds").unwrap(),
                Some(value)
            );
        }
        for value in [Value::Null, json!("true"), json!(1), json!([]), json!({})] {
            assert!(
                parse_optional_bool(&json!({"suppress_embeds": value}), "suppress_embeds").is_err()
            );
        }
    }
}
