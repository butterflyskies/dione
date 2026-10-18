//! MCP projections of typed access requests and decisions.

use crate::{
    config::ConfigDurability,
    discord::access::{
        AccessApproval, AccessDenial, AccessError, AccessErrorKind, PendingAccessRequest,
    },
};
use serde_json::{Value, json};

pub(super) fn requests(requests: Vec<PendingAccessRequest>) -> Value {
    let requests: Vec<Value> = requests
        .iter()
        .map(|request| {
            json!({
                "user_id": request.user_id().to_string(),
                "username": request.username(),
                "message_preview": request.message_preview(),
                "timestamp": request.timestamp(),
            })
        })
        .collect();
    json!({ "requests": requests })
}

pub(super) fn approval(result: Result<AccessApproval, AccessError>) -> Value {
    match result {
        Ok(receipt) => match receipt.durability() {
            ConfigDurability::Durable => json!({
                "ok": true,
                "username": receipt.username(),
                "user_id": receipt.user_id().to_string(),
                "generation": receipt.generation(),
                "durability": "durable",
                "request_retained": false,
            }),
            ConfigDurability::Unknown { warning } => json!({
                "ok": true,
                "username": receipt.username(),
                "user_id": receipt.user_id().to_string(),
                "generation": receipt.generation(),
                "durability": "unknown",
                "warning": warning,
                "request_retained": true,
            }),
        },
        Err(error) => access_error(error),
    }
}

pub(super) fn denial(result: Result<AccessDenial, AccessError>) -> Value {
    match result {
        Ok(receipt) => json!({
            "ok": true,
            "username": receipt.username(),
            "user_id": receipt.user_id().to_string(),
        }),
        Err(error) => access_error(error),
    }
}

fn access_error(error: AccessError) -> Value {
    let message = match error.kind {
        AccessErrorKind::NoPendingRequest => {
            format!("no pending request for user {}", error.user_id)
        }
        AccessErrorKind::PersistConfig(source) => format!("failed to persist config: {source}"),
    };
    json!({ "error": message })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, LoadedConfig},
        discord::access::AccessCtx,
        queue::{AccessQueue, AccessRequest},
    };
    use camino::Utf8PathBuf;
    use serenity::model::id::UserId;
    use std::sync::Arc;
    use tempfile::TempDir;

    struct AccessFixture {
        access: AccessCtx,
        queue: Arc<tokio::sync::Mutex<AccessQueue>>,
        state_dir: Utf8PathBuf,
    }

    fn access_fixture(config: &str) -> (TempDir, AccessFixture) {
        let dir = TempDir::new().unwrap();
        let state_dir = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        std::fs::write(state_dir.join("config.toml"), config).unwrap();
        let queue = Arc::new(tokio::sync::Mutex::new(AccessQueue::load(&state_dir)));
        let access = AccessCtx::new(
            queue.clone(),
            Arc::new(LoadedConfig::try_from_raw(Config::default()).unwrap()),
            state_dir.clone(),
        );
        let ctx = AccessFixture {
            access,
            queue,
            state_dir,
        };
        (dir, ctx)
    }

    fn pending(user_id: u64) -> AccessRequest {
        AccessRequest {
            user_id,
            username: "pending-user".to_owned(),
            message_preview: "please".to_owned(),
            timestamp: "2026-01-02T12:00:00Z".parse().unwrap(),
        }
    }

    fn normalize_generation(receipt: &mut Value) -> u64 {
        let generation = receipt["generation"].as_u64().unwrap();
        assert!(generation > 0);
        receipt["generation"] = json!("<generation>");
        generation
    }

    #[tokio::test]
    async fn access_contract_list_preserves_zero_ids_timezone_and_tie_order() {
        let (_dir, mut ctx) = access_fixture("");
        assert_eq!(requests(ctx.access.list().await), json!({"requests":[]}));
        ctx.access = AccessCtx::new(
            ctx.queue.clone(),
            Arc::new(
                LoadedConfig::try_from_raw(Config {
                    timezone: Some("America/Chicago".to_owned()),
                    ..Config::default()
                })
                .unwrap(),
            ),
            ctx.state_dir.clone(),
        );
        std::fs::write(
            ctx.state_dir.join("queue.json"),
            include_str!("../../tests/fixtures/access-requests.json"),
        )
        .unwrap();
        *ctx.queue.lock().await = AccessQueue::load(&ctx.state_dir);
        assert_eq!(
            requests(ctx.access.list().await),
            json!({"requests":[
                {"user_id":"99","username":"pending-user","message_preview":"please","timestamp":"2026-01-01T06:00:00-06:00"},
                {"user_id":"0","username":"pending-user","message_preview":"please","timestamp":"2026-01-02T06:00:00-06:00"},
                {"user_id":"18446744073709551615","username":"pending-user","message_preview":"please","timestamp":"2026-01-02T06:00:00-06:00"},
                {"user_id":"42","username":"pending-user","message_preview":"please","timestamp":"2026-01-02T06:00:00-06:00"}
            ]})
        );
    }

    #[tokio::test]
    async fn access_contract_durable_approval_reloads_and_is_idempotent() {
        for config in ["", "[access]\nallow_from = [\"42\"]\n"] {
            let (_dir, ctx) = access_fixture(config);
            assert!(ctx.queue.lock().await.enqueue(pending(42), 50));
            let mut receipt = approval(ctx.access.approve(UserId::new(42)).await);
            normalize_generation(&mut receipt);
            assert_eq!(
                receipt,
                json!({
                    "ok":true,"username":"pending-user","user_id":"42",
                    "generation":"<generation>","durability":"durable","request_retained":false
                })
            );
            assert!(ctx.queue.lock().await.list().is_empty());
            assert!(AccessQueue::load(&ctx.state_dir).list().is_empty());
            assert_eq!(
                crate::config::load_config(&ctx.state_dir).access.allow_from,
                vec!["42"]
            );
            assert_eq!(
                approval(ctx.access.approve(UserId::new(42)).await),
                json!({"error":"no pending request for user 42"})
            );
        }
    }

    #[tokio::test]
    async fn access_contract_missing_precedes_config_failure_and_failed_write_retains_request() {
        let (_dir, ctx) = access_fixture("[access]\nallow_from = 7\n");
        assert_eq!(
            approval(ctx.access.approve(UserId::new(42)).await),
            json!({"error":"no pending request for user 42"})
        );
        assert!(ctx.queue.lock().await.enqueue(pending(42), 50));
        assert_eq!(
            approval(ctx.access.approve(UserId::new(42)).await),
            json!({"error":"failed to persist config: allow_from is not an array"})
        );
        assert!(ctx.queue.lock().await.peek(42).is_some());
        assert!(AccessQueue::load(&ctx.state_dir).peek(42).is_some());
        assert_eq!(
            std::fs::read_to_string(ctx.state_dir.join("config.toml")).unwrap(),
            "[access]\nallow_from = 7\n"
        );
    }

    #[tokio::test]
    async fn access_contract_unknown_durability_retains_request_and_retry_completes() {
        let (_dir, ctx) = access_fixture("");
        assert!(ctx.queue.lock().await.enqueue(pending(42), 50));
        let failure =
            crate::config::fail_mutation_dir_fsync_for_test(ctx.state_dir.join("config.toml"));
        let mut receipt = approval(ctx.access.approve(UserId::new(42)).await);
        let first_generation = normalize_generation(&mut receipt);
        assert_eq!(
            receipt["warning"],
            format!(
                "config applied at generation {first_generation}, but parent-directory fsync failed: injected mutation parent-directory fsync failure"
            )
        );
        receipt["warning"] = json!("<warning with verified generation>");
        assert_eq!(
            receipt,
            json!({
                "ok":true,"username":"pending-user","user_id":"42",
                "generation":"<generation>","durability":"unknown",
                "warning":"<warning with verified generation>","request_retained":true
            })
        );
        assert!(ctx.queue.lock().await.peek(42).is_some());
        assert!(AccessQueue::load(&ctx.state_dir).peek(42).is_some());
        assert_eq!(
            crate::config::load_config(&ctx.state_dir).access.allow_from,
            vec!["42"]
        );
        drop(failure);
        let mut retry = approval(ctx.access.approve(UserId::new(42)).await);
        assert!(normalize_generation(&mut retry) > first_generation);
        assert_eq!(
            retry,
            json!({
                "ok":true,"username":"pending-user","user_id":"42",
                "generation":"<generation>","durability":"durable","request_retained":false
            })
        );
        assert!(AccessQueue::load(&ctx.state_dir).list().is_empty());
        assert_eq!(
            crate::config::load_config(&ctx.state_dir).access.allow_from,
            vec!["42"]
        );
    }

    #[tokio::test]
    async fn access_contract_denial_persists_and_repeat_is_missing() {
        let (_dir, ctx) = access_fixture("");
        assert!(ctx.queue.lock().await.enqueue(pending(42), 50));
        assert_eq!(
            denial(ctx.access.deny(UserId::new(42)).await),
            json!({"ok":true,"username":"pending-user","user_id":"42"})
        );
        assert!(AccessQueue::load(&ctx.state_dir).list().is_empty());
        assert_eq!(
            denial(ctx.access.deny(UserId::new(42)).await),
            json!({"error":"no pending request for user 42"})
        );
        assert_eq!(
            std::fs::read_to_string(ctx.state_dir.join("config.toml")).unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn access_contract_queue_persistence_failure_is_best_effort() {
        for approve in [false, true] {
            let (_dir, ctx) = access_fixture("");
            assert!(ctx.queue.lock().await.enqueue(pending(42), 50));
            std::fs::create_dir(ctx.state_dir.join("queue.json.tmp")).unwrap();
            if approve {
                let mut receipt = approval(ctx.access.approve(UserId::new(42)).await);
                normalize_generation(&mut receipt);
                assert_eq!(
                    receipt,
                    json!({
                        "ok":true,"username":"pending-user","user_id":"42",
                        "generation":"<generation>","durability":"durable","request_retained":false
                    })
                );
                assert_eq!(
                    crate::config::load_config(&ctx.state_dir).access.allow_from,
                    vec!["42"]
                );
            } else {
                let receipt = denial(ctx.access.deny(UserId::new(42)).await);
                assert_eq!(
                    receipt,
                    json!({"ok":true,"username":"pending-user","user_id":"42"})
                );
            }
            assert!(ctx.queue.lock().await.list().is_empty());
            assert!(AccessQueue::load(&ctx.state_dir).peek(42).is_some());
        }
    }
}
