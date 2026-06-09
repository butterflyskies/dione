//! Integration tests for the notification delivery pipeline.
//!
//! Tests the full path: event → rate limiter → delivery buffer → notification
//! output, verifying coalescing, rate limiting, bypass behavior, and config
//! integration.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use dione::config::{ChannelConfig, Config, LoadedConfig, RateLimitTomlConfig};
use dione::delivery_buffer::{BufferResult, DeliveryBuffer};
use dione::discord::events::NotificationEvent;
use dione::mcp::server::test_helpers;
use dione::rate_limiter::{
    ChannelRef, OverflowPolicy, ParticipantId, RateLimitConfig, RateLimitDecision, RateLimiter,
    ScopeConfig,
};

// ── Helpers ─────────────────────────────────────────────────────────────────

fn msg_event(chat_id: &str, user_id: &str, content: &str) -> NotificationEvent {
    NotificationEvent::Message {
        chat_id: chat_id.to_string(),
        message_id: "1".to_string(),
        user: format!("user-{user_id}"),
        user_id: user_id.to_string(),
        content: content.to_string(),
        timestamp: "2026-01-01T00:00:00Z".to_string(),
        attachments: vec![],
        is_voice_message: false,
        thread_parent_id: None,
    }
}

fn reaction_event(chat_id: &str, user_id: &str) -> NotificationEvent {
    NotificationEvent::Reaction {
        chat_id: chat_id.to_string(),
        message_id: "1".to_string(),
        user: format!("user-{user_id}"),
        user_id: user_id.to_string(),
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

fn edit_event(chat_id: &str, user_id: &str) -> NotificationEvent {
    NotificationEvent::MessageEdit {
        chat_id: chat_id.to_string(),
        message_id: "1".to_string(),
        user: format!("user-{user_id}"),
        user_id: user_id.to_string(),
        new_content: "edited content".to_string(),
        timestamp: "2026-01-01T00:00:01Z".to_string(),
        thread_parent_id: None,
    }
}

fn delete_event(chat_id: &str) -> NotificationEvent {
    NotificationEvent::MessageDelete {
        chat_id: chat_id.to_string(),
        message_id: "1".to_string(),
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
    if let NotificationEvent::Message {
        ref user_id,
        ref chat_id,
        ..
    } = event
    {
        let sender = ParticipantId::new(user_id.as_str());
        let channel = ChannelRef::new(chat_id.as_str());
        match rate_limiter.check_message(&sender, &channel, &[], now) {
            RateLimitDecision::Allowed { .. } => {}
            RateLimitDecision::Denied { .. } => return None,
        }
    }

    // Delivery buffer: coalesce message events per channel.
    match delivery_buffer.buffer_event(event, delay_ms) {
        BufferResult::Immediate(event) => Some(event),
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

    let event = msg_event("ch1", "100", "hello");
    let result = pipeline_step(event, &mut limiter, &mut buffer, 0, now);

    assert!(result.is_some(), "message should pass through immediately");
    let event = result.unwrap();
    assert!(matches!(event, NotificationEvent::Message { ref content, .. } if content == "hello"));
}

/// Rate limiting drops messages after token exhaustion.
#[test]
fn rate_limiter_drops_messages_after_exhaustion() {
    let mut limiter = make_limiter(true, 2);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    // First two messages pass.
    let r1 = pipeline_step(
        msg_event("ch1", "100", "msg1"),
        &mut limiter,
        &mut buffer,
        0,
        now,
    );
    let r2 = pipeline_step(
        msg_event("ch1", "100", "msg2"),
        &mut limiter,
        &mut buffer,
        0,
        now,
    );
    assert!(r1.is_some(), "first message must be allowed");
    assert!(r2.is_some(), "second message must be allowed (budget=2)");

    // Third message is denied.
    let r3 = pipeline_step(
        msg_event("ch1", "100", "msg3"),
        &mut limiter,
        &mut buffer,
        0,
        now,
    );
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
        msg_event("ch1", "100", "first"),
        &mut limiter,
        &mut buffer,
        500,
        now,
    );
    let r2 = pipeline_step(
        msg_event("ch1", "200", "second"),
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
            NotificationEvent::Message { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(contents, vec!["first", "second"]);
}

/// Non-message events bypass both rate limiter and delivery buffer.
#[test]
fn non_message_events_bypass_pipeline() {
    let mut limiter = make_limiter(true, 1);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    // Exhaust the rate limiter for this sender/channel.
    let _ = pipeline_step(
        msg_event("ch1", "100", "exhaust"),
        &mut limiter,
        &mut buffer,
        0,
        now,
    );
    // Verify messages are now denied.
    let denied = pipeline_step(
        msg_event("ch1", "100", "denied"),
        &mut limiter,
        &mut buffer,
        0,
        now,
    );
    assert!(
        denied.is_none(),
        "messages should be denied after exhaustion"
    );

    // Non-message events must still pass through, even with a delivery delay.
    let reaction = pipeline_step(
        reaction_event("ch1", "100"),
        &mut limiter,
        &mut buffer,
        500,
        now,
    );
    assert!(
        reaction.is_some(),
        "reaction must bypass rate limiter and buffer"
    );

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

/// Edit and delete events bypass rate limiter but are subject to the
/// delivery buffer (they are message-type events).
#[test]
fn edit_and_delete_bypass_rate_limiter_but_buffer() {
    let mut limiter = make_limiter(true, 1);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    // Exhaust the rate limiter.
    let _ = pipeline_step(
        msg_event("ch1", "100", "exhaust"),
        &mut limiter,
        &mut buffer,
        0,
        now,
    );

    // Edit events are not rate-limited (the rate limiter only checks
    // NotificationEvent::Message). With a delivery delay, they get buffered.
    let edit = pipeline_step(
        edit_event("ch1", "100"),
        &mut limiter,
        &mut buffer,
        500,
        now,
    );
    assert!(
        edit.is_none(),
        "edit event with delay should be buffered, not immediately forwarded"
    );

    // Delete events similarly bypass rate limiter.
    let delete = pipeline_step(delete_event("ch1"), &mut limiter, &mut buffer, 500, now);
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

    let edit = pipeline_step(edit_event("ch1", "100"), &mut limiter, &mut buffer, 0, now);
    assert!(edit.is_some(), "edit with no delay should pass immediately");

    let delete = pipeline_step(delete_event("ch1"), &mut limiter, &mut buffer, 0, now);
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
            msg_event("ch1", "100", &format!("msg{i}")),
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
        msg_event("ch1", "100", "instant"),
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
        msg_event("ch1", "100", "u100-ch1"),
        &mut limiter,
        &mut buffer,
        0,
        now,
    );
    assert!(r1.is_some());

    // User 100 in ch1: denied (exhausted).
    let r2 = pipeline_step(
        msg_event("ch1", "100", "u100-ch1-again"),
        &mut limiter,
        &mut buffer,
        0,
        now,
    );
    assert!(r2.is_none(), "same sender+channel should be denied");

    // User 200 in ch1: allowed (different sender).
    let r3 = pipeline_step(
        msg_event("ch1", "200", "u200-ch1"),
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
        msg_event("ch2", "100", "u100-ch2"),
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
        msg_event("ch1", "100", "ch1-msg"),
        &mut limiter,
        &mut buffer,
        50,
        now,
    );

    // Buffer a message in ch2 with longer delay.
    pipeline_step(
        msg_event("ch2", "200", "ch2-msg"),
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
        NotificationEvent::Message { content, .. } if content == "ch1-msg"
    ));

    // ch2 still pending.
    assert!(buffer.next_flush_deadline().is_some());

    // After 600ms: ch2 flushes.
    let after_600 = tokio::time::Instant::now() + tokio::time::Duration::from_millis(600);
    let flushed2 = buffer.flush_ready(after_600);
    assert_eq!(flushed2.len(), 1, "ch2 should have flushed");
    assert!(matches!(
        &flushed2[0],
        NotificationEvent::Message { content, .. } if content == "ch2-msg"
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
        msg_event("ch1", "100", "allowed"),
        &mut limiter,
        &mut buffer,
        500,
        now,
    );

    // Second message is rate-limited — should never enter the buffer.
    pipeline_step(
        msg_event("ch1", "100", "denied"),
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
        NotificationEvent::Message { content, .. } if content == "allowed"
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
                delivery_delay_ms: 500,
                ..Default::default()
            },
            ChannelConfig {
                id: "222".to_string(),
                delivery_delay_ms: 0,
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

    let event = NotificationEvent::Message {
        chat_id: "ch1".to_string(),
        message_id: "msg-42".to_string(),
        user: "alice".to_string(),
        user_id: "100".to_string(),
        content: "hello world".to_string(),
        timestamp: "2026-06-08T12:00:00Z".to_string(),
        attachments: vec![],
        is_voice_message: false,
        thread_parent_id: Some("parent-1".to_string()),
    };

    let result = pipeline_step(event, &mut limiter, &mut buffer, 0, now);
    let event = result.expect("message should pass through");

    // Convert to notification JSON and verify.
    let notification = test_helpers::make_notification(event);
    assert_eq!(notification["method"], "notifications/claude/channel");
    assert_eq!(notification["params"]["content"], "hello world");
    assert_eq!(notification["params"]["meta"]["chat_id"], "ch1");
    assert_eq!(notification["params"]["meta"]["message_id"], "msg-42");
    assert_eq!(notification["params"]["meta"]["user"], "alice");
    assert_eq!(notification["params"]["meta"]["user_id"], "100");
    assert_eq!(
        notification["params"]["meta"]["thread_parent_id"],
        "parent-1"
    );
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

    // "Reload" config with higher budget.
    let cfg2 = RateLimitTomlConfig {
        enabled: true,
        max_tokens: Some(10),
        ..Default::default()
    };
    let mut limiter = RateLimiter::new(cfg2.into_runtime());

    // New limiter allows 10 messages.
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
/// buffered, non-message events pass through regardless.
#[test]
fn mixed_event_stream() {
    let mut limiter = make_limiter(true, 2);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();
    let delay = 500;

    let mut immediate_results: Vec<&str> = Vec::new();

    // Message 1: allowed, buffered.
    let r = pipeline_step(
        msg_event("ch1", "100", "msg1"),
        &mut limiter,
        &mut buffer,
        delay,
        now,
    );
    if r.is_some() {
        immediate_results.push("msg1");
    }

    // Reaction: passes through immediately.
    let r = pipeline_step(
        reaction_event("ch1", "100"),
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
        msg_event("ch1", "100", "msg2"),
        &mut limiter,
        &mut buffer,
        delay,
        now,
    );
    if r.is_some() {
        immediate_results.push("msg2");
    }

    // Trace: passes through immediately.
    let r = pipeline_step(trace_event(), &mut limiter, &mut buffer, delay, now);
    if r.is_some() {
        immediate_results.push("trace");
    }

    // Message 3: denied (rate limit exhausted), never buffered.
    let r = pipeline_step(
        msg_event("ch1", "100", "msg3"),
        &mut limiter,
        &mut buffer,
        delay,
        now,
    );
    if r.is_some() {
        immediate_results.push("msg3");
    }

    // Permission response: passes through immediately.
    let r = pipeline_step(permission_event(), &mut limiter, &mut buffer, delay, now);
    if r.is_some() {
        immediate_results.push("permission");
    }

    // Only non-message events should have passed through immediately.
    assert_eq!(
        immediate_results,
        vec!["reaction", "trace", "permission"],
        "only non-message events should pass through immediately"
    );

    // Flush: only msg1 and msg2 should be in the buffer (msg3 was denied).
    let after_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(600);
    let flushed = buffer.flush_ready(after_deadline);
    assert_eq!(flushed.len(), 2, "only allowed messages should be buffered");

    let contents: Vec<String> = flushed
        .iter()
        .filter_map(|e| match e {
            NotificationEvent::Message { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(contents, vec!["msg1", "msg2"]);
}

/// Buffered messages are flushed in order.
#[test]
fn buffer_preserves_message_order() {
    let mut limiter = make_limiter(false, 100);
    let mut buffer = DeliveryBuffer::new();
    let now = Instant::now();

    for i in 0..5 {
        pipeline_step(
            msg_event("ch1", &format!("{i}"), &format!("msg-{i}")),
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
            NotificationEvent::Message { content, .. } => Some(content.clone()),
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
        msg_event("ch1", "100", "batch1"),
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
        msg_event("ch1", "200", "batch2"),
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
        NotificationEvent::Message { content, .. } if content == "batch2"
    ));
}
