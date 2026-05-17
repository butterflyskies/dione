use std::sync::Arc;

use camino::Utf8PathBuf;
use serde_json::{Value, json};
use toml_edit::DocumentMut;

/// Context for access management tools.
pub struct AccessCtx {
    pub queue: Arc<tokio::sync::Mutex<crate::queue::AccessQueue>>,
    pub state_dir: Utf8PathBuf,
}

// ── list_access_requests ──────────────────────────────────────────────────────

pub async fn list_access_requests(ctx: &AccessCtx) -> Value {
    let queue = ctx.queue.lock().await;
    let requests: Vec<Value> = queue
        .list()
        .iter()
        .map(|r| {
            json!({
                "user_id": r.user_id.to_string(),
                "username": r.username,
                "message_preview": r.message_preview,
                "timestamp": r.timestamp.to_rfc3339(),
            })
        })
        .collect();
    json!({ "requests": requests })
}

// ── approve_access ────────────────────────────────────────────────────────────

pub async fn approve_access(ctx: &AccessCtx, user_id: u64) -> Value {
    // Check that the request exists before writing config.
    let request = {
        let queue = ctx.queue.lock().await;
        queue.peek(user_id).cloned()
    };

    let Some(request) = request else {
        return json!({ "error": format!("no pending request for user {user_id}") });
    };

    // Write config first — if this fails, leave the request in the queue.
    if let Err(e) = add_to_allow_from(&ctx.state_dir, user_id).await {
        tracing::warn!(user_id, error = %e, "failed to persist allow_from update");
        return json!({
            "error": format!("failed to persist config: {e}"),
        });
    }

    // Config write succeeded — now remove from queue.
    {
        let mut queue = ctx.queue.lock().await;
        queue.approve(user_id);
    }

    json!({
        "ok": true,
        "username": request.username,
        "user_id": user_id.to_string(),
    })
}

// ── deny_access ───────────────────────────────────────────────────────────────

pub async fn deny_access(ctx: &AccessCtx, user_id: u64) -> Value {
    let removed = {
        let mut queue = ctx.queue.lock().await;
        queue.deny(user_id)
    };

    match removed {
        Some(request) => json!({
            "ok": true,
            "username": request.username,
            "user_id": user_id.to_string(),
        }),
        None => json!({ "error": format!("no pending request for user {user_id}") }),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Adds `user_id` to `access.allow_from` in `config.toml`, using `toml_edit` for
/// structure-preserving edits.
async fn add_to_allow_from(
    state_dir: &Utf8PathBuf,
    user_id: u64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config_path = state_dir.join("config.toml");
    let user_id_str = user_id.to_string();

    // Read current config (if any). Missing file → start fresh.
    let contents = match tokio::fs::read_to_string(&config_path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };

    let mut doc: DocumentMut = contents.parse()?;

    // Ensure [access] table exists.
    if !doc.contains_key("access") {
        doc["access"] = toml_edit::table();
    }

    let access = &mut doc["access"];

    // Ensure allow_from array exists.
    if access.get("allow_from").is_none() {
        access["allow_from"] = toml_edit::array();
    }

    let allow_from = access["allow_from"]
        .as_array_mut()
        .ok_or("allow_from is not an array")?;

    // Dedup: don't add if already present.
    let already_present = allow_from.iter().any(|v| v.as_str() == Some(&user_id_str));

    if !already_present {
        allow_from.push(user_id_str);
    }

    // Write back atomically.
    let tmp_path = state_dir.join("config.toml.tmp");
    tokio::fs::write(&tmp_path, doc.to_string()).await?;
    tokio::fs::rename(&tmp_path, &config_path).await?;

    Ok(())
}
