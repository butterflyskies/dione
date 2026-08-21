use crate::{config::LoadedConfig, config_store::ConfigStore};
use camino::Utf8PathBuf;
use serde_json::{Value, json};
use serenity::model::id::UserId;
use std::sync::Arc;

/// Context for access management tools.
pub struct AccessCtx {
    pub queue: Arc<tokio::sync::Mutex<crate::queue::AccessQueue>>,
    pub config: Arc<LoadedConfig>,
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
                "timestamp": ctx.config.localize_utc(&r.timestamp),
            })
        })
        .collect();
    json!({ "requests": requests })
}

// ── approve_access ────────────────────────────────────────────────────────────

pub async fn approve_access(ctx: &AccessCtx, user_id: UserId) -> Value {
    let uid = user_id.get();
    let request = {
        let queue = ctx.queue.lock().await;
        queue.peek(uid).cloned()
    };

    let Some(request) = request else {
        return json!({ "error": format!("no pending request for user {uid}") });
    };

    let user_id_str = uid.to_string();
    let result = async {
        let mut editor = ConfigStore::load(&ctx.state_dir).await?;
        editor.ensure_in_allow_from(&user_id_str)?;
        editor.save().await
    }
    .await;

    if let Err(e) = result {
        tracing::warn!(user_id = uid, error = %e, "failed to persist allow_from update");
        return json!({
            "error": format!("failed to persist config: {e}"),
        });
    }

    {
        let mut queue = ctx.queue.lock().await;
        queue.approve(uid);
    }

    json!({
        "ok": true,
        "username": request.username,
        "user_id": user_id_str,
    })
}

// ── deny_access ───────────────────────────────────────────────────────────────

pub async fn deny_access(ctx: &AccessCtx, user_id: UserId) -> Value {
    let uid = user_id.get();
    let removed = {
        let mut queue = ctx.queue.lock().await;
        queue.deny(uid)
    };

    match removed {
        Some(request) => json!({
            "ok": true,
            "username": request.username,
            "user_id": uid.to_string(),
        }),
        None => json!({ "error": format!("no pending request for user {uid}") }),
    }
}
