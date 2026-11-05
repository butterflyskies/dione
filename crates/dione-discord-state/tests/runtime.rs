mod support;
use chrono::Utc;
use dione_discord_state::{PendingPermission, new_state, observe_webhook_creator};
use serde_json::{Value, json};
use serenity::model::id::{ChannelId, MessageId, WebhookId};
use std::{sync::Arc, time::Duration};
use support::Fixture;
use tokio::time::timeout;

fn webhook(id: u64, creator: Option<u64>) -> Value {
    json!({"id":id.to_string(), "type":1, "guild_id":"30", "channel_id":"20", "name":"fixture", "user":creator.map(|creator| json!({"id":creator.to_string(), "username":"creator", "discriminator":"0", "bot":true}))})
}

#[tokio::test]
async fn public_http_observations_cache_only_successful_creator_facts() {
    let mut fixture = Fixture::new(vec![
        ("200 OK", webhook(40, Some(200))),
        ("200 OK", webhook(50, None)),
        ("200 OK", webhook(50, Some(300))),
        (
            "404 Not Found",
            json!({"message":"Unknown Webhook", "code":10015}),
        ),
        ("200 OK", webhook(60, Some(400))),
    ])
    .await;
    let state = new_state();
    let alias = Arc::clone(&state);
    assert!(Arc::ptr_eq(&state, &alias));
    assert_eq!(
        timeout(
            Duration::from_secs(5),
            observe_webhook_creator(&fixture.http, &state, WebhookId::new(40))
        )
        .await
        .unwrap(),
        Some(200)
    );
    assert_eq!(fixture.count(), 1);
    assert_eq!(
        timeout(
            Duration::from_secs(5),
            observe_webhook_creator(&fixture.http, &alias, WebhookId::new(40))
        )
        .await
        .unwrap(),
        Some(200)
    );
    assert_eq!(fixture.count(), 1);
    assert_eq!(
        timeout(
            Duration::from_secs(5),
            observe_webhook_creator(&fixture.http, &state, WebhookId::new(50))
        )
        .await
        .unwrap(),
        None
    );
    assert_eq!(fixture.count(), 2);
    assert_eq!(
        timeout(
            Duration::from_secs(5),
            observe_webhook_creator(&fixture.http, &state, WebhookId::new(50))
        )
        .await
        .unwrap(),
        Some(300)
    );
    assert_eq!(fixture.count(), 3);
    assert_eq!(
        timeout(
            Duration::from_secs(5),
            observe_webhook_creator(&fixture.http, &state, WebhookId::new(60))
        )
        .await
        .unwrap(),
        None
    );
    assert_eq!(fixture.count(), 4);
    assert_eq!(
        timeout(
            Duration::from_secs(5),
            observe_webhook_creator(&fixture.http, &state, WebhookId::new(60))
        )
        .await
        .unwrap(),
        Some(400)
    );
    assert_eq!(fixture.count(), 5);
    fixture.finish().await;
    assert_eq!(
        *fixture.requests.lock().unwrap(),
        [
            "GET /api/v10/webhooks/40 HTTP/1.1",
            "GET /api/v10/webhooks/50 HTTP/1.1",
            "GET /api/v10/webhooks/50 HTTP/1.1",
            "GET /api/v10/webhooks/60 HTTP/1.1",
            "GET /api/v10/webhooks/60 HTTP/1.1",
        ]
    );
}

#[tokio::test]
async fn public_state_indices_and_permission_cleanup_share_one_allocation() {
    let state = new_state();
    let alias = Arc::clone(&state);
    {
        let mut state = state.write().await;
        state.record_dm_channel(1, 2);
        state.record_thread_parent(3, Some(4));
        state.record_thread_parent(5, None);
        state.cache_username(1, "fixture".into());
        state.note_non_bot(100);
        state.note_sent(5001);
        state.prune_sent_ids(0);
        assert!(!state.is_own_send(5001));
        for (id, channel, request) in [
            (5001, 9001, "shared"),
            (5002, 9002, "shared"),
            (5003, 9003, "other"),
        ] {
            state.pending_permissions.insert(
                id,
                PendingPermission {
                    request_id: request.into(),
                    channel_id: ChannelId::new(channel),
                    created_at: Utc::now(),
                },
            );
        }
    }
    let mut view = alias.write().await;
    assert_eq!(view.dm_channel_map.get(&1), Some(&2));
    assert!(view.dm_channel_ids.contains(&2));
    assert_eq!(view.thread_parents.get(&3), Some(&Some(4)));
    assert_eq!(view.thread_parents.get(&5), Some(&None));
    assert_eq!(view.user_names.get(&1).map(String::as_str), Some("fixture"));
    assert!(view.non_bot_message_ids.contains(&100));
    assert_eq!(
        view.remove_permissions_by_request_id("shared"),
        [
            (ChannelId::new(9001), MessageId::new(5001)),
            (ChannelId::new(9002), MessageId::new(5002))
        ]
    );
    assert!(view.is_own_send(5001));
    assert!(view.is_own_send(5002));
    assert!(!view.is_own_send(5003));
    assert_eq!(view.pending_permissions.len(), 1);
    assert!(view.pending_permissions.contains_key(&5003));
}
