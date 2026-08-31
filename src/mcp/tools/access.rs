use crate::config::LoadedConfig;
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
    let mutation_user_id = user_id_str.clone();
    let result = crate::config::ConfigRuntime::new(ctx.state_dir.clone())
        .mutate(move |editor| editor.ensure_in_allow_from(&mutation_user_id))
        .await;

    let outcome = match result {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::warn!(user_id = uid, error = %e, "failed to persist allow_from update");
            return json!({
                "error": format!("failed to persist config: {e}"),
            });
        }
    };

    if let crate::config::ConfigDurability::Unknown { warning } = outcome.durability {
        // The config rename and live publication applied, but a crash may
        // still lose the directory entry. Keep the durable queue request so
        // a retry can re-persist the id and complete the acknowledgement.
        tracing::warn!(
            user_id = uid,
            generation = outcome.generation,
            warning,
            "access approval applied with unknown durability; retaining pending request"
        );
        return json!({
            "ok": true,
            "username": request.username,
            "user_id": user_id_str,
            "generation": outcome.generation,
            "durability": "unknown",
            "warning": warning,
            "request_retained": true,
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
        "generation": outcome.generation,
        "durability": "durable",
        "request_retained": false,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::AccessRequest;
    use chrono::Utc;

    #[tokio::test]
    async fn unknown_config_durability_retains_the_access_request_for_retry() {
        let dir = tempfile::TempDir::new().unwrap();
        let state_dir = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let config_path = state_dir.join("config.toml");
        std::fs::write(&config_path, "").unwrap();

        let mut queue = crate::queue::AccessQueue::load(&state_dir);
        assert!(queue.enqueue(
            AccessRequest {
                user_id: 42,
                username: "pending-user".to_owned(),
                message_preview: "please".to_owned(),
                timestamp: Utc::now(),
            },
            50,
        ));
        let queue = Arc::new(tokio::sync::Mutex::new(queue));
        let ctx = AccessCtx {
            queue: Arc::clone(&queue),
            config: Arc::new(LoadedConfig::try_from_raw(crate::config::Config::default()).unwrap()),
            state_dir: state_dir.clone(),
        };
        let _failure = crate::config::fail_mutation_dir_fsync_for_test(config_path);

        let receipt = approve_access(&ctx, UserId::new(42)).await;

        assert_eq!(receipt["ok"], true);
        assert_eq!(receipt["durability"], "unknown");
        assert_eq!(receipt["request_retained"], true);
        assert!(receipt["warning"].as_str().is_some());
        assert!(
            queue.lock().await.peek(42).is_some(),
            "unknown durability must preserve the durable retry handle"
        );
        assert!(
            crate::config::load_config(&state_dir)
                .access
                .allow_from
                .contains(&"42".to_owned()),
            "the rename-applied config remains live while the queue is retained"
        );
    }
}
