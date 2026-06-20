//! Integration tests for the notification delivery pipeline.
//!
//! Tests the full path: event → rate limiter → delivery buffer → notification
//! output, verifying coalescing, rate limiting, bypass behavior, and config
//! integration.

use dione::{
    config::{ChannelConfig, Config, DeliveryConfig, LoadedConfig, RateLimitTomlConfig},
    delivery_buffer::{BufferResult, DeliveryBuffer},
    discord::events::{MessageEvent, NotificationEvent},
    mcp::server::test_helpers,
    rate_limiter::{
        ChannelRef, OverflowPolicy, ParticipantId, RateLimitConfig, RateLimitDecision, RateLimiter,
        ScopeConfig,
    },
};
use serenity::model::id::{ChannelId, MessageId, UserId};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

// ── Helpers ─────────────────────────────────────────────────────────────────

fn msg_event(chat_id: u64, user_id: u64, content: &str) -> NotificationEvent {
    NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(chat_id),
        message_id: MessageId::new(1),
        user: format!("user-{user_id}"),
        user_id: UserId::new(user_id),
        content: content.to_string(),
        timestamp: "2026-01-01T00:00:00Z".to_string(),
        attachments: vec![],
        is_voice_message: false,
        thread_parent_id: None,
        reply_to_message_id: None,
        reply_to_user_id: None,
        reply_to_user: None,
        reply_to_content_preview: None,
    })
}

fn reaction_event(chat_id: u64, user_id: u64) -> NotificationEvent {
    NotificationEvent::Reaction {
        chat_id: ChannelId::new(chat_id),
        message_id: MessageId::new(1),
        user: format!("user-{user_id}"),
        user_id: UserId::new(user_id),
        emoji: "👍".to_string(),
    }
}

fn trace_event() -> NotificationEvent {
    NotificationEvent::Trace {
        level: "info".to_string(),
        target: "test".to_string(),
        message: "test trace".to_string(),
        fields: vec![],
    }
}

fn permission_event() -> NotificationEvent {
    NotificationEvent::PermissionResponse {
        request_id: "req-1".to_string(),
        granted: true,
    }
}

fn config_error_event() -> NotificationEvent {
    NotificationEvent::ConfigError {
        error: "bad toml".to_string(),
    }
}

fn edit_event(chat_id: u64, user_id: u64) -> NotificationEvent {
    NotificationEvent::MessageEdit {
        chat_id: ChannelId::new(chat_id),
        message_id: MessageId::new(1),
        user: format!("user-{user_id}"),
        user_id: UserId::new(user_id),
        new_content: "edited content".to_string(),
        timestamp: "2026-01-01T00:00:01Z".to_string(),
        thread_parent_id: None,
        reply_to_message_id: None,
    }
}

fn delete_event(chat_id: u64) -> NotificationEvent {
    NotificationEvent::MessageDelete {
        chat_id: ChannelId::new(chat_id),
        message_id: MessageId::new(1),
        thread_parent_id: None,
    }
}

/// Create a rate limiter with a small token budget for testing.
fn make_limiter(enabled: bool, max_tokens: u32) -> RateLimiter {
    RateLimiter::new(RateLimitConfig {
        enabled,
        default: ScopeConfig {
            max_tokens,
            window: Duration::from_secs(3600),
            cooldown: Duration::from_secs(3600),
            overflow: OverflowPolicy::Drop { notify: true },
        },
        classes: Vec::new(),
        individuals: HashMap::new(),
        channels: HashMap::new(),
    })
}

/// Simulate the notification forwarding pipeline for a single event.
///
/// Returns `Some(event)` if the event would be forwarded immediately,
/// `None` if it was rate-limited (dropped) or buffered.
fn pipeline_step(
    event: NotificationEvent,
    rate_limiter: &mut RateLimiter,
    delivery_buffer: &mut DeliveryBuffer,
    delay_ms: u64,
    now: Instant,
) -> Option<NotificationEvent> {
    // Rate-limit check for message events only.
    if let NotificationEvent::Message(MessageEvent {
        ref user_id,
        ref chat_id,
        ..
    }) = event
    {
        let user_id_str = user_id.get().to_string();
        let chat_id_str = chat_id.get().to_string();
        let sender = ParticipantId::new(&user_id_str);
        let channel = ChannelRef::new(&chat_id_str);
        match rate_limiter.check_message(&sender, &channel, &[], now) {
            RateLimitDecision::Allowed { .. } => {}
            RateLimitDecision::Denied { .. } => return None,
        }
    }

    // Delivery buffer: coalesce message events per channel.
    match delivery_buffer.buffer_event(event, delay_ms) {
        BufferResult::Immediate(event) => Some(*event),
        BufferResult::Buffered => None,
    }
}

// ── Full pipeline tests ─────────────────────────────────────────────────────

/// Test the full pipeline: message event → rate limiter (allow) → delivery
/// buffer (immediate) → notification output.
#[test]
fn full_pipeline_message_allowed_no_delay() {
    let mut limiter = make_limiter(true, 5);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    let event = msg_event(1, 100, "hello");
    let result = pipeline_step(event, &mut limiter, &mut buffer, 0, now);

    assert!(result.is_some(), "message should pass through immediately");
    let event = result.unwrap();
    assert!(
        matches!(event, NotificationEvent::Message(MessageEvent { ref content, .. }) if content == "hello")
    );
}

/// Rate limiting drops messages after token exhaustion.
#[test]
fn rate_limiter_drops_messages_after_exhaustion() {
    let mut limiter = make_limiter(true, 2);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    // First two messages pass.
    let r1 = pipeline_step(msg_event(1, 100, "msg1"), &mut limiter, &mut buffer, 0, now);
    let r2 = pipeline_step(msg_event(1, 100, "msg2"), &mut limiter, &mut buffer, 0, now);
    assert!(r1.is_some(), "first message must be allowed");
    assert!(r2.is_some(), "second message must be allowed (budget=2)");

    // Third message is denied.
    let r3 = pipeline_step(msg_event(1, 100, "msg3"), &mut limiter, &mut buffer, 0, now);
    assert!(
        r3.is_none(),
        "third message must be dropped after exhaustion"
    );
}

/// Delivery buffer coalescing: two messages within the window come out
/// together after the delay.
#[test]
fn delivery_buffer_coalesces_messages() {
    let mut limiter = make_limiter(false, 20);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    // Buffer two messages with 500ms delay.
    let r1 = pipeline_step(
        msg_event(1, 100, "first"),
        &mut limiter,
        &mut buffer,
        500,
        now,
    );
    let r2 = pipeline_step(
        msg_event(1, 200, "second"),
        &mut limiter,
        &mut buffer,
        500,
        now,
    );

    assert!(r1.is_none(), "first message must be buffered");
    assert!(r2.is_none(), "second message must be buffered");

    // Before deadline: nothing flushed.
    let too_early = tokio::time::Instant::now();
    let flushed_early = buffer.flush_ready(too_early);
    assert!(
        flushed_early.is_empty(),
        "must not flush before deadline expires"
    );

    // After deadline: both messages flushed together.
    let after_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(600);
    let flushed = buffer.flush_ready(after_deadline);
    assert_eq!(flushed.len(), 2, "both messages must flush together");

    // Verify the messages retained their content.
    let contents: Vec<String> = flushed
        .iter()
        .filter_map(|e| match e {
            NotificationEvent::Message(MessageEvent { content, .. }) => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(contents, vec!["first", "second"]);
}

/// Non-channel events bypass both rate limiter and delivery buffer.
/// Channel events (including reactions) are subject to the delivery buffer.
#[test]
fn non_channel_events_bypass_pipeline() {
    let mut limiter = make_limiter(true, 1);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    // Exhaust the rate limiter for this sender/channel.
    let _ = pipeline_step(
        msg_event(1, 100, "exhaust"),
        &mut limiter,
        &mut buffer,
        0,
        now,
    );
    // Verify messages are now denied.
    let denied = pipeline_step(
        msg_event(1, 100, "denied"),
        &mut limiter,
        &mut buffer,
        0,
        now,
    );
    assert!(
        denied.is_none(),
        "messages should be denied after exhaustion"
    );

    // Non-channel events must still pass through, even with a delivery delay.
    let trace = pipeline_step(trace_event(), &mut limiter, &mut buffer, 500, now);
    assert!(trace.is_some(), "trace must bypass rate limiter and buffer");

    let perm = pipeline_step(permission_event(), &mut limiter, &mut buffer, 500, now);
    assert!(
        perm.is_some(),
        "permission response must bypass rate limiter and buffer"
    );

    let cfg_err = pipeline_step(config_error_event(), &mut limiter, &mut buffer, 500, now);
    assert!(
        cfg_err.is_some(),
        "config error must bypass rate limiter and buffer"
    );
}

/// Reactions are channel events — they bypass the rate limiter but are
/// subject to the delivery buffer when a delay is configured.
#[test]
fn reactions_buffered_with_delay() {
    let mut limiter = make_limiter(true, 1);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    // Reaction with a delivery delay should be buffered.
    let result = pipeline_step(reaction_event(1, 100), &mut limiter, &mut buffer, 500, now);
    assert!(
        result.is_none(),
        "reaction with delay should be buffered, not immediate"
    );

    // Reaction with no delay should pass through immediately.
    let result = pipeline_step(reaction_event(2, 100), &mut limiter, &mut buffer, 0, now);
    assert!(
        result.is_some(),
        "reaction with no delay should pass through immediately"
    );

    // Flush the buffered reaction.
    let after_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(600);
    let flushed = buffer.flush_ready(after_deadline);
    assert_eq!(flushed.len(), 1, "buffered reaction should flush");
    assert!(matches!(flushed[0], NotificationEvent::Reaction { .. }));
}

/// Edit and delete events bypass rate limiter but are subject to the
/// delivery buffer (they are message-type events).
#[test]
fn edit_and_delete_bypass_rate_limiter_but_buffer() {
    let mut limiter = make_limiter(true, 1);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    // Exhaust the rate limiter.
    let _ = pipeline_step(
        msg_event(1, 100, "exhaust"),
        &mut limiter,
        &mut buffer,
        0,
        now,
    );

    // Edit events are not rate-limited (the rate limiter only checks
    // NotificationEvent::Message). With a delivery delay, they get buffered.
    let edit = pipeline_step(edit_event(1, 100), &mut limiter, &mut buffer, 500, now);
    assert!(
        edit.is_none(),
        "edit event with delay should be buffered, not immediately forwarded"
    );

    // Delete events similarly bypass rate limiter.
    let delete = pipeline_step(delete_event(1), &mut limiter, &mut buffer, 500, now);
    assert!(
        delete.is_none(),
        "delete event with delay should be buffered"
    );

    // Both should flush when deadline expires.
    let after_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(600);
    let flushed = buffer.flush_ready(after_deadline);
    assert_eq!(
        flushed.len(),
        2,
        "edit and delete events must flush together"
    );
}

/// Edit and delete events with no delay pass through immediately (no
/// buffering).
#[test]
fn edit_and_delete_immediate_when_no_delay() {
    let mut limiter = make_limiter(true, 10);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    let edit = pipeline_step(edit_event(1, 100), &mut limiter, &mut buffer, 0, now);
    assert!(edit.is_some(), "edit with no delay should pass immediately");

    let delete = pipeline_step(delete_event(1), &mut limiter, &mut buffer, 0, now);
    assert!(
        delete.is_some(),
        "delete with no delay should pass immediately"
    );
}

/// Disabled rate limiter lets all messages through.
#[test]
fn disabled_rate_limiter_passthrough() {
    let mut limiter = make_limiter(false, 1);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    // With max_tokens=1 but disabled, all messages should pass.
    for i in 0..10 {
        let result = pipeline_step(
            msg_event(1, 100, &format!("msg{i}")),
            &mut limiter,
            &mut buffer,
            0,
            now,
        );
        assert!(
            result.is_some(),
            "message {i} must pass when rate limiter is disabled"
        );
    }
}

/// Channels with no delivery delay pass messages immediately (no buffering).
#[test]
fn no_delay_means_immediate() {
    let mut limiter = make_limiter(true, 100);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    let result = pipeline_step(
        msg_event(1, 100, "instant"),
        &mut limiter,
        &mut buffer,
        0,
        now,
    );
    assert!(
        result.is_some(),
        "message with delay_ms=0 must pass immediately"
    );
    assert!(
        buffer.next_flush_deadline().is_none(),
        "no pending flush deadline for immediate messages"
    );
}

/// Rate limiter isolates different sender/channel combinations.
#[test]
fn rate_limiter_per_sender_per_channel_isolation() {
    let mut limiter = make_limiter(true, 1);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    // User 100 in ch1: allowed.
    let r1 = pipeline_step(
        msg_event(1, 100, "u100-ch1"),
        &mut limiter,
        &mut buffer,
        0,
        now,
    );
    assert!(r1.is_some());

    // User 100 in ch1: denied (exhausted).
    let r2 = pipeline_step(
        msg_event(1, 100, "u100-ch1-again"),
        &mut limiter,
        &mut buffer,
        0,
        now,
    );
    assert!(r2.is_none(), "same sender+channel should be denied");

    // User 200 in ch1: allowed (different sender).
    let r3 = pipeline_step(
        msg_event(1, 200, "u200-ch1"),
        &mut limiter,
        &mut buffer,
        0,
        now,
    );
    assert!(
        r3.is_some(),
        "different sender in same channel should be allowed"
    );

    // User 100 in ch2: allowed (different channel).
    let r4 = pipeline_step(
        msg_event(2, 100, "u100-ch2"),
        &mut limiter,
        &mut buffer,
        0,
        now,
    );
    assert!(
        r4.is_some(),
        "same sender in different channel should be allowed"
    );
}

/// Multiple channels buffer independently.
#[test]
fn multiple_channels_buffer_independently() {
    let mut limiter = make_limiter(false, 100);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    // Buffer a message in ch1 with short delay.
    pipeline_step(
        msg_event(1, 100, "ch1-msg"),
        &mut limiter,
        &mut buffer,
        50,
        now,
    );

    // Buffer a message in ch2 with longer delay.
    pipeline_step(
        msg_event(2, 200, "ch2-msg"),
        &mut limiter,
        &mut buffer,
        500,
        now,
    );

    // After 100ms: ch1 should flush, ch2 should not.
    let after_100 = tokio::time::Instant::now() + tokio::time::Duration::from_millis(100);
    let flushed = buffer.flush_ready(after_100);
    assert_eq!(flushed.len(), 1, "only ch1 should have flushed");
    assert!(matches!(
        &flushed[0],
        NotificationEvent::Message(MessageEvent { content, .. }) if content == "ch1-msg"
    ));

    // ch2 still pending.
    assert!(buffer.next_flush_deadline().is_some());

    // After 600ms: ch2 flushes.
    let after_600 = tokio::time::Instant::now() + tokio::time::Duration::from_millis(600);
    let flushed2 = buffer.flush_ready(after_600);
    assert_eq!(flushed2.len(), 1, "ch2 should have flushed");
    assert!(matches!(
        &flushed2[0],
        NotificationEvent::Message(MessageEvent { content, .. }) if content == "ch2-msg"
    ));
}

/// Rate-limited messages never reach the delivery buffer.
#[test]
fn rate_limited_messages_dont_reach_buffer() {
    let mut limiter = make_limiter(true, 1);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    // First message passes (buffered due to delay).
    pipeline_step(
        msg_event(1, 100, "allowed"),
        &mut limiter,
        &mut buffer,
        500,
        now,
    );

    // Second message is rate-limited — should never enter the buffer.
    pipeline_step(
        msg_event(1, 100, "denied"),
        &mut limiter,
        &mut buffer,
        500,
        now,
    );

    // Flush — only the first message should be present.
    let after_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(600);
    let flushed = buffer.flush_ready(after_deadline);
    assert_eq!(
        flushed.len(),
        1,
        "only the allowed message should be in the buffer"
    );
    assert!(matches!(
        &flushed[0],
        NotificationEvent::Message(MessageEvent { content, .. }) if content == "allowed"
    ));
}

// ── Config integration tests ─────────────────────────────────────────────

/// Config correctly parses `[rate_limit]` and converts to runtime config.
#[test]
fn config_rate_limit_round_trip() {
    let toml_cfg = RateLimitTomlConfig {
        enabled: true,
        max_tokens: Some(10),
        window_secs: Some(60),
        cooldown_secs: Some(30),
        overflow: Some("drop".to_string()),
    };
    let runtime = toml_cfg.into_runtime();

    assert!(runtime.enabled);
    assert_eq!(runtime.default.max_tokens, 10);
    assert_eq!(runtime.default.window, Duration::from_secs(60));
    assert_eq!(runtime.default.cooldown, Duration::from_secs(30));
    assert!(matches!(
        runtime.default.overflow,
        OverflowPolicy::Drop { notify: true }
    ));

    // Verify a limiter created from this config works.
    let mut limiter = RateLimiter::new(runtime);
    let sender = ParticipantId::new("test-user");
    let channel = ChannelRef::new("test-channel");
    let now = Instant::now();

    for _ in 0..10 {
        let decision = limiter.check_message(&sender, &channel, &[], now);
        assert!(
            matches!(decision, RateLimitDecision::Allowed { .. }),
            "should allow up to max_tokens"
        );
    }
    // 11th should be denied.
    let denied = limiter.check_message(&sender, &channel, &[], now);
    assert!(
        matches!(denied, RateLimitDecision::Denied { .. }),
        "must deny after max_tokens exhausted"
    );
}

/// Config correctly parses `delivery_delay_ms` per channel and provides
/// it through `LoadedConfig::delivery_delay_ms`.
#[test]
fn config_delivery_delay_per_channel() {
    let raw = Config {
        channels: vec![
            ChannelConfig {
                id: "111".to_string(),
                delivery_delay_ms: Some(500),
                ..Default::default()
            },
            ChannelConfig {
                id: "222".to_string(),
                delivery_delay_ms: Some(0),
                ..Default::default()
            },
            ChannelConfig {
                id: "333".to_string(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let loaded = LoadedConfig::from_raw(raw);

    assert_eq!(loaded.delivery_delay_ms(111), 500);
    assert_eq!(loaded.delivery_delay_ms(222), 0);
    assert_eq!(
        loaded.delivery_delay_ms(333),
        0,
        "default delivery_delay_ms is 0"
    );
    assert_eq!(
        loaded.delivery_delay_ms(999),
        0,
        "unknown channel returns 0"
    );
}

/// Config defaults produce a disabled rate limiter.
#[test]
fn config_default_rate_limiter_is_disabled() {
    let cfg = Config::default();
    assert!(!cfg.rate_limit.enabled);

    let runtime = cfg.rate_limit.into_runtime();
    assert!(!runtime.enabled);

    let mut limiter = RateLimiter::new(runtime);
    let sender = ParticipantId::new("any-user");
    let channel = ChannelRef::new("any-channel");
    let now = Instant::now();

    // Disabled limiter should allow unlimited messages.
    for _ in 0..100 {
        let decision = limiter.check_message(&sender, &channel, &[], now);
        assert!(matches!(decision, RateLimitDecision::Allowed { .. }));
    }
}

/// `RateLimitTomlConfig` buffer overflow converts correctly.
#[test]
fn config_buffer_overflow_policy() {
    let toml_cfg = RateLimitTomlConfig {
        enabled: true,
        max_tokens: Some(1),
        overflow: Some("buffer".to_string()),
        ..Default::default()
    };
    let runtime = toml_cfg.into_runtime();
    assert_eq!(runtime.default.overflow, OverflowPolicy::Buffer);
}

/// Notification conversion preserves all fields through the pipeline.
#[test]
fn notification_format_preserved_through_pipeline() {
    let mut limiter = make_limiter(false, 100);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(1),
        message_id: MessageId::new(42),
        user: "alice".to_string(),
        user_id: UserId::new(100),
        content: "hello world".to_string(),
        timestamp: "2026-06-08T12:00:00Z".to_string(),
        attachments: vec![],
        is_voice_message: false,
        thread_parent_id: Some(ChannelId::new(9001)),
        reply_to_message_id: Some(MessageId::new(777)),
        reply_to_user_id: None,
        reply_to_user: None,
        reply_to_content_preview: None,
    });

    let result = pipeline_step(event, &mut limiter, &mut buffer, 0, now);
    let event = result.expect("message should pass through");

    // Convert to notification JSON and verify.
    let notification = test_helpers::make_notification(event);
    assert_eq!(notification["method"], "notifications/claude/channel");
    assert_eq!(notification["params"]["content"], "hello world");
    assert_eq!(notification["params"]["meta"]["chat_id"], "1");
    assert_eq!(notification["params"]["meta"]["message_id"], "42");
    assert_eq!(notification["params"]["meta"]["user"], "alice");
    assert_eq!(notification["params"]["meta"]["user_id"], "100");
    assert_eq!(notification["params"]["meta"]["thread_parent_id"], "9001");
    assert_eq!(notification["params"]["meta"]["reply_to_message_id"], "777");
}

/// Config reload updates rate limiter behavior (simulated by creating a
/// new limiter from new config).
#[test]
fn config_reload_updates_rate_limiter() {
    // Start with a tight budget.
    let cfg1 = RateLimitTomlConfig {
        enabled: true,
        max_tokens: Some(1),
        ..Default::default()
    };
    let mut limiter = RateLimiter::new(cfg1.into_runtime());
    let sender = ParticipantId::new("user1");
    let channel = ChannelRef::new("ch1");
    let now = Instant::now();

    // First message allowed, second denied.
    assert!(matches!(
        limiter.check_message(&sender, &channel, &[], now),
        RateLimitDecision::Allowed { .. }
    ));
    assert!(matches!(
        limiter.check_message(&sender, &channel, &[], now),
        RateLimitDecision::Denied { .. }
    ));

    // "Reload" config with higher budget via update_config (not a new limiter).
    let cfg2 = RateLimitTomlConfig {
        enabled: true,
        max_tokens: Some(10),
        ..Default::default()
    };
    limiter.update_config(cfg2.into_runtime());

    // Existing bucket gets replaced on next check (config changed).
    // New budget allows 10 messages.
    for _ in 0..10 {
        assert!(matches!(
            limiter.check_message(&sender, &channel, &[], now),
            RateLimitDecision::Allowed { .. }
        ));
    }
    assert!(matches!(
        limiter.check_message(&sender, &channel, &[], now),
        RateLimitDecision::Denied { .. }
    ));
}

/// TOML deserialization round trip through a temp file.
#[test]
fn config_toml_file_round_trip() {
    use std::fs;
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let state_dir = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let config_path = state_dir.join("config.toml");

    let toml = r#"
[rate_limit]
enabled = true
max_tokens = 5
window_secs = 120
cooldown_secs = 60
overflow = "drop"

[[channels]]
id = "12345"
require_mention = false
delivery_delay_ms = 3000

[[channels]]
id = "67890"
require_mention = true
"#;
    fs::write(config_path.as_std_path(), toml.as_bytes()).unwrap();

    let (loaded, error) = dione::config::reload_config(&state_dir);
    assert!(error.is_none(), "config should parse without errors");

    // Rate limit config.
    assert!(loaded.rate_limit.enabled);
    assert_eq!(loaded.rate_limit.max_tokens, Some(5));
    assert_eq!(loaded.rate_limit.window_secs, Some(120));
    assert_eq!(loaded.rate_limit.cooldown_secs, Some(60));

    // Delivery delays.
    assert_eq!(loaded.delivery_delay_ms(12345), 3000);
    assert_eq!(
        loaded.delivery_delay_ms(67890),
        0,
        "channel without delay config defaults to 0"
    );

    // Runtime rate limiter works.
    let runtime = loaded.rate_limit.clone().into_runtime();
    let mut limiter = RateLimiter::new(runtime);
    let sender = ParticipantId::new("u1");
    let channel = ChannelRef::new("12345");
    let now = Instant::now();

    for i in 0..5 {
        assert!(
            matches!(
                limiter.check_message(&sender, &channel, &[], now),
                RateLimitDecision::Allowed { .. }
            ),
            "message {i} should be allowed"
        );
    }
    assert!(
        matches!(
            limiter.check_message(&sender, &channel, &[], now),
            RateLimitDecision::Denied { .. }
        ),
        "6th message should be denied"
    );
}

/// Mixed event types through the pipeline: messages are rate-limited and
/// buffered, reactions are buffered (channel events), non-channel events
/// pass through regardless.
#[test]
fn mixed_event_stream() {
    let mut limiter = make_limiter(true, 2);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();
    let delay = 500;

    let mut immediate_results: Vec<&str> = Vec::new();

    // Message 1: allowed, buffered.
    let r = pipeline_step(
        msg_event(1, 100, "msg1"),
        &mut limiter,
        &mut buffer,
        delay,
        now,
    );
    if r.is_some() {
        immediate_results.push("msg1");
    }

    // Reaction: buffered (channel event with delay).
    let r = pipeline_step(
        reaction_event(1, 100),
        &mut limiter,
        &mut buffer,
        delay,
        now,
    );
    if r.is_some() {
        immediate_results.push("reaction");
    }

    // Message 2: allowed, buffered.
    let r = pipeline_step(
        msg_event(1, 100, "msg2"),
        &mut limiter,
        &mut buffer,
        delay,
        now,
    );
    if r.is_some() {
        immediate_results.push("msg2");
    }

    // Trace: passes through immediately (non-channel event).
    let r = pipeline_step(trace_event(), &mut limiter, &mut buffer, delay, now);
    if r.is_some() {
        immediate_results.push("trace");
    }

    // Message 3: denied (rate limit exhausted), never buffered.
    let r = pipeline_step(
        msg_event(1, 100, "msg3"),
        &mut limiter,
        &mut buffer,
        delay,
        now,
    );
    if r.is_some() {
        immediate_results.push("msg3");
    }

    // Permission response: passes through immediately (non-channel event).
    let r = pipeline_step(permission_event(), &mut limiter, &mut buffer, delay, now);
    if r.is_some() {
        immediate_results.push("permission");
    }

    // Only non-channel events should have passed through immediately.
    assert_eq!(
        immediate_results,
        vec!["trace", "permission"],
        "only non-channel events should pass through immediately"
    );

    // Flush: msg1, reaction, and msg2 should be in the buffer (msg3 was denied).
    let after_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(600);
    let flushed = buffer.flush_ready(after_deadline);
    assert_eq!(
        flushed.len(),
        3,
        "allowed messages and reaction should be buffered"
    );

    // Verify order: msg1, reaction, msg2.
    assert!(
        matches!(&flushed[0], NotificationEvent::Message(MessageEvent { content, .. }) if content == "msg1")
    );
    assert!(matches!(&flushed[1], NotificationEvent::Reaction { .. }));
    assert!(
        matches!(&flushed[2], NotificationEvent::Message(MessageEvent { content, .. }) if content == "msg2")
    );
}

/// Buffered messages are flushed in order.
#[test]
fn buffer_preserves_message_order() {
    let mut limiter = make_limiter(false, 100);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    for i in 0..5 {
        pipeline_step(
            msg_event(1, i as u64 + 1, &format!("msg-{i}")),
            &mut limiter,
            &mut buffer,
            500,
            now,
        );
    }

    let after_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(600);
    let flushed = buffer.flush_ready(after_deadline);
    assert_eq!(flushed.len(), 5);

    let contents: Vec<String> = flushed
        .iter()
        .filter_map(|e| match e {
            NotificationEvent::Message(MessageEvent { content, .. }) => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(contents, vec!["msg-0", "msg-1", "msg-2", "msg-3", "msg-4"]);
}

/// Empty flush produces no events.
#[test]
fn empty_buffer_flush_produces_nothing() {
    let mut buffer = DeliveryBuffer::new();
    let now = tokio::time::Instant::now() + tokio::time::Duration::from_secs(1);
    let flushed = buffer.flush_ready(now);
    assert!(flushed.is_empty());
}

/// Buffer re-arms after flushing — new messages get a fresh deadline.
#[test]
fn buffer_rearms_after_flush() {
    let mut limiter = make_limiter(false, 100);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    // First batch.
    pipeline_step(
        msg_event(1, 100, "batch1"),
        &mut limiter,
        &mut buffer,
        50,
        now,
    );
    let after = tokio::time::Instant::now() + tokio::time::Duration::from_millis(100);
    let flushed1 = buffer.flush_ready(after);
    assert_eq!(flushed1.len(), 1);

    // Second batch — new deadline should be set.
    pipeline_step(
        msg_event(1, 200, "batch2"),
        &mut limiter,
        &mut buffer,
        50,
        now,
    );
    assert!(
        buffer.next_flush_deadline().is_some(),
        "new deadline should be set after re-buffering"
    );

    let after2 = tokio::time::Instant::now() + tokio::time::Duration::from_millis(200);
    let flushed2 = buffer.flush_ready(after2);
    assert_eq!(flushed2.len(), 1);
    assert!(matches!(
        &flushed2[0],
        NotificationEvent::Message(MessageEvent { content, .. }) if content == "batch2"
    ));
}

// ── Async integration tests (tokio::select loop) ──────────────────────────

/// Run the notification forwarding loop (mirrors the logic in server.rs
/// notif_task) using real channels and deterministic time.
///
/// Returns the collected output events.
async fn run_notif_loop(
    mut rx: tokio::sync::mpsc::Receiver<NotificationEvent>,
    cancel: tokio_util::sync::CancellationToken,
    delay_ms_fn: impl Fn(&NotificationEvent) -> u64 + Send + 'static,
) -> Vec<NotificationEvent> {
    let mut delivery_buffer = DeliveryBuffer::new();
    let (output_tx, mut output_rx) = tokio::sync::mpsc::channel::<NotificationEvent>(256);

    let loop_task = tokio::spawn(async move {
        loop {
            let flush_deadline = delivery_buffer.next_flush_deadline();

            tokio::select! {
                biased;

                _ = cancel.cancelled() => {
                    break;
                }

                _ = async {
                    match flush_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    let now = tokio::time::Instant::now();
                    let flushed = delivery_buffer.flush_ready(now);
                    for event in flushed {
                        let _ = output_tx.send(event).await;
                    }
                }

                event = rx.recv() => {
                    let Some(event) = event else { break };
                    let delay_ms = delay_ms_fn(&event);
                    match delivery_buffer.buffer_event(event, delay_ms) {
                        BufferResult::Immediate(event) => {
                            let _ = output_tx.send(*event).await;
                        }
                        BufferResult::Buffered => {}
                    }
                }
            }
        }

        // Drain remaining buffered events on exit.
        let remaining = delivery_buffer.flush_all();
        for event in remaining {
            let _ = output_tx.send(event).await;
        }
    });

    // Wait for the loop task to complete.
    let _ = loop_task.await;

    // Collect all output events.
    let mut events = Vec::new();
    output_rx.close();
    while let Some(event) = output_rx.recv().await {
        events.push(event);
    }
    events
}

/// Delayed message events appear only after the delivery delay elapses.
#[tokio::test(start_paused = true)]
async fn async_delayed_message_appears_after_delay() {
    let (tx, rx) = tokio::sync::mpsc::channel::<NotificationEvent>(16);
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();

    let handle = tokio::spawn(async move { run_notif_loop(rx, cancel_clone, |_| 500).await });

    // Send a message.
    tx.send(msg_event(1, 100, "delayed-msg")).await.unwrap();

    // Advance time to just before the deadline — event should not have flushed yet.
    tokio::time::advance(Duration::from_millis(400)).await;
    tokio::task::yield_now().await;

    // Advance past the deadline.
    tokio::time::advance(Duration::from_millis(200)).await;
    tokio::task::yield_now().await;

    // Cancel to collect results.
    cancel.cancel();
    let events = handle.await.unwrap();

    assert_eq!(
        events.len(),
        1,
        "exactly one event should have been emitted"
    );
    assert!(matches!(
        &events[0],
        NotificationEvent::Message(MessageEvent { content, .. }) if content == "delayed-msg"
    ));
}

/// Reaction events to a delayed channel are buffered along with messages.
#[tokio::test(start_paused = true)]
async fn async_reaction_buffered_with_delay() {
    let (tx, rx) = tokio::sync::mpsc::channel::<NotificationEvent>(16);
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();

    let handle = tokio::spawn(async move { run_notif_loop(rx, cancel_clone, |_| 500).await });

    // Send a message, then a reaction to the same channel.
    tx.send(msg_event(1, 100, "hello")).await.unwrap();
    tx.send(reaction_event(1, 200)).await.unwrap();

    // Allow the events to be processed.
    tokio::task::yield_now().await;

    // Advance past the deadline.
    tokio::time::advance(Duration::from_millis(600)).await;
    tokio::task::yield_now().await;

    cancel.cancel();
    let events = handle.await.unwrap();

    // Both message and reaction should have been buffered and flushed together.
    assert_eq!(events.len(), 2, "message and reaction should both flush");
    assert!(matches!(
        &events[0],
        NotificationEvent::Message(MessageEvent { .. })
    ));
    assert!(matches!(&events[1], NotificationEvent::Reaction { .. }));
}

/// Non-channel events pass through immediately even when delay is configured.
#[tokio::test(start_paused = true)]
async fn async_trace_bypasses_buffer() {
    let (tx, rx) = tokio::sync::mpsc::channel::<NotificationEvent>(16);
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();

    let handle = tokio::spawn(async move { run_notif_loop(rx, cancel_clone, |_| 500).await });

    // Send a trace event — should pass through immediately.
    tx.send(trace_event()).await.unwrap();
    tokio::task::yield_now().await;

    // Don't advance time — trace should already be emitted.
    cancel.cancel();
    let events = handle.await.unwrap();

    assert_eq!(
        events.len(),
        1,
        "trace event should pass through immediately"
    );
    assert!(matches!(&events[0], NotificationEvent::Trace { .. }));
}

/// Shutdown drain: when the CancellationToken is cancelled, all buffered
/// events are flushed via flush_all().
#[tokio::test(start_paused = true)]
async fn async_shutdown_drains_buffered_events() {
    let (tx, rx) = tokio::sync::mpsc::channel::<NotificationEvent>(16);
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();

    let handle = tokio::spawn(async move { run_notif_loop(rx, cancel_clone, |_| 5000).await });

    // Buffer several events with a long delay (won't flush naturally).
    tx.send(msg_event(1, 100, "drain-1")).await.unwrap();
    tx.send(msg_event(1, 200, "drain-2")).await.unwrap();
    tx.send(reaction_event(2, 100)).await.unwrap();
    tx.send(msg_event(2, 300, "drain-3")).await.unwrap();

    // Let events be processed by the loop.
    tokio::task::yield_now().await;
    // Small advance so events are consumed but deadline hasn't passed.
    tokio::time::advance(Duration::from_millis(100)).await;
    tokio::task::yield_now().await;

    // Cancel — should trigger flush_all drain.
    cancel.cancel();
    let events = handle.await.unwrap();

    // All 4 events should have been drained on shutdown.
    assert_eq!(
        events.len(),
        4,
        "all buffered events must be flushed on shutdown, got {}",
        events.len()
    );

    // Verify we got the right events (order: ch1 events first, then ch2,
    // because BTreeMap iterates in sorted key order).
    assert!(matches!(
        &events[0],
        NotificationEvent::Message(MessageEvent { content, chat_id, .. })
        if content == "drain-1" && *chat_id == 1
    ));
    assert!(matches!(
        &events[1],
        NotificationEvent::Message(MessageEvent { content, chat_id, .. })
        if content == "drain-2" && *chat_id == 1
    ));
    assert!(matches!(
        &events[2],
        NotificationEvent::Reaction { chat_id, .. }
        if *chat_id == 2
    ));
    assert!(matches!(
        &events[3],
        NotificationEvent::Message(MessageEvent { content, chat_id, .. })
        if content == "drain-3" && *chat_id == 2
    ));
}

/// Shutdown drain with empty buffer produces no events.
#[tokio::test(start_paused = true)]
async fn async_shutdown_empty_buffer() {
    let (_tx, rx) = tokio::sync::mpsc::channel::<NotificationEvent>(16);
    let cancel = tokio_util::sync::CancellationToken::new();
    let cancel_clone = cancel.clone();

    let handle = tokio::spawn(async move { run_notif_loop(rx, cancel_clone, |_| 500).await });

    // Cancel immediately with nothing buffered.
    cancel.cancel();
    let events = handle.await.unwrap();

    assert!(
        events.is_empty(),
        "empty buffer shutdown should produce no events"
    );
}

// ── Global delivery_delay_ms integration tests ─────────────────────────

/// Global default flows through the full pipeline: unconfigured channels
/// inherit the global delay.
#[test]
fn global_default_flows_through_pipeline() {
    let raw = Config {
        delivery: DeliveryConfig {
            delivery_delay_ms: 500,
            ..Default::default()
        },
        channels: vec![ChannelConfig {
            id: "100".to_string(),
            // No per-channel override — inherits global 500ms.
            ..Default::default()
        }],
        ..Default::default()
    };
    let loaded = LoadedConfig::from_raw(raw);

    let mut limiter = make_limiter(false, 100);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    // Use the loaded config delay for channel 100.
    let delay = loaded.delivery_delay_ms(100);
    assert_eq!(delay, 500, "channel 100 should inherit global 500ms");

    // Event should be buffered (not immediate) with inherited delay.
    let result = pipeline_step(
        msg_event(100, 1, "via-global"),
        &mut limiter,
        &mut buffer,
        delay,
        now,
    );
    assert!(
        result.is_none(),
        "message should be buffered with inherited global delay"
    );

    // Unconfigured channel also inherits global.
    let delay_unconfigured = loaded.delivery_delay_ms(999);
    assert_eq!(
        delay_unconfigured, 500,
        "unconfigured channel inherits global"
    );
    let result2 = pipeline_step(
        msg_event(999, 2, "unconfigured"),
        &mut limiter,
        &mut buffer,
        delay_unconfigured,
        now,
    );
    assert!(
        result2.is_none(),
        "unconfigured channel message should be buffered with global delay"
    );

    // Flush both.
    let after_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(600);
    let flushed = buffer.flush_ready(after_deadline);
    assert_eq!(flushed.len(), 2, "both channels should flush");
}

/// Per-channel override with global default set: override wins.
#[test]
fn per_channel_override_with_global_default() {
    let raw = Config {
        delivery: DeliveryConfig {
            delivery_delay_ms: 1000,
            ..Default::default()
        },
        channels: vec![
            ChannelConfig {
                id: "100".to_string(),
                delivery_delay_ms: Some(50),
                ..Default::default()
            },
            ChannelConfig {
                id: "200".to_string(),
                // No override — inherits 1000ms.
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let loaded = LoadedConfig::from_raw(raw);

    let mut limiter = make_limiter(false, 100);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    // Channel 100 has 50ms override.
    let delay_100 = loaded.delivery_delay_ms(100);
    assert_eq!(delay_100, 50);
    pipeline_step(
        msg_event(100, 1, "fast-channel"),
        &mut limiter,
        &mut buffer,
        delay_100,
        now,
    );

    // Channel 200 inherits 1000ms global.
    let delay_200 = loaded.delivery_delay_ms(200);
    assert_eq!(delay_200, 1000);
    pipeline_step(
        msg_event(200, 2, "slow-channel"),
        &mut limiter,
        &mut buffer,
        delay_200,
        now,
    );

    // After 100ms: channel 100 should flush, channel 200 should not.
    let after_100 = tokio::time::Instant::now() + tokio::time::Duration::from_millis(100);
    let flushed = buffer.flush_ready(after_100);
    assert_eq!(
        flushed.len(),
        1,
        "only channel 100 (50ms) should have flushed"
    );
    assert!(matches!(
        &flushed[0],
        NotificationEvent::Message(MessageEvent { content, .. }) if content == "fast-channel"
    ));

    // Channel 200 still pending.
    assert!(buffer.next_flush_deadline().is_some());
}

// ── Individual notification format tests ────────────────────────────────

/// Individual notifications have correct JSON-RPC structure.
#[test]
fn individual_notification_jsonrpc_structure() {
    let events = vec![
        msg_event(1, 100, "first"),
        reaction_event(1, 200),
        msg_event(1, 300, "second"),
    ];

    let notifications: Vec<serde_json::Value> = events
        .into_iter()
        .map(test_helpers::make_notification)
        .collect();

    assert_eq!(notifications.len(), 3);
    for n in &notifications {
        assert_eq!(n["jsonrpc"], "2.0");
        assert_eq!(n["method"], "notifications/claude/channel");
        assert!(n.get("id").is_none(), "notifications must not have an id");
        assert!(n["params"]["content"].is_string());
        assert!(n["params"]["meta"].is_object());
    }

    assert_eq!(notifications[0]["params"]["content"], "first");
    assert_eq!(notifications[1]["params"]["content"], "reacted with 👍");
    assert_eq!(notifications[2]["params"]["content"], "second");
}

/// Multi-channel flush preserves per-event chat_id in individual notifications.
#[test]
fn multi_channel_flush_preserves_chat_ids() {
    let mut limiter = make_limiter(false, 100);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    pipeline_step(
        msg_event(1, 100, "ch1-first"),
        &mut limiter,
        &mut buffer,
        200,
        now,
    );
    pipeline_step(
        msg_event(2, 200, "ch2-first"),
        &mut limiter,
        &mut buffer,
        200,
        now,
    );
    pipeline_step(
        msg_event(1, 300, "ch1-second"),
        &mut limiter,
        &mut buffer,
        200,
        now,
    );

    let after_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(300);
    let flushed = buffer.flush_ready(after_deadline);
    assert_eq!(
        flushed.len(),
        3,
        "all events from both channels should flush"
    );

    let notifications: Vec<serde_json::Value> = flushed
        .into_iter()
        .map(test_helpers::make_notification)
        .collect();

    // BTreeMap orders by channel key: "1" < "2", so ch1 events come first.
    assert_eq!(notifications[0]["params"]["meta"]["chat_id"], "1");
    assert_eq!(notifications[0]["params"]["content"], "ch1-first");
    assert_eq!(notifications[1]["params"]["meta"]["chat_id"], "1");
    assert_eq!(notifications[1]["params"]["content"], "ch1-second");
    assert_eq!(notifications[2]["params"]["meta"]["chat_id"], "2");
    assert_eq!(notifications[2]["params"]["content"], "ch2-first");
}
