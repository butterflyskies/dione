use super::*;
use chrono_tz::Tz;
use dione_contracts::{
    ActorId, ConversationId, MessageId, ModuleInstanceId, ModuleSlotId, ProviderId,
};
use dione_messages::{
    events::{
        AuthorOrigin, ByteSize, MessageCreated, ObservedAttachment, ObservedAuthor,
        ReplyObservation, Targeting,
    },
    timestamp::Timestamp,
};
use std::num::NonZeroU64;

fn module() -> ModuleInstanceId {
    ModuleInstanceId::new(
        ProviderId::new("discord").unwrap(),
        ModuleSlotId::new("primary").unwrap(),
    )
}
fn conversation(id: u64) -> ConversationRef {
    ConversationRef::new(
        module(),
        ConversationId::numeric(NonZeroU64::new(id).unwrap()),
    )
}
fn source(channel: u64, message: u64) -> MessageRef {
    MessageRef::new(
        conversation(channel),
        MessageId::numeric(NonZeroU64::new(message).unwrap()),
    )
}
fn actor(id: u64) -> ActorRef {
    ActorRef::new(module(), ActorId::numeric(NonZeroU64::new(id).unwrap()))
}
struct BatchContext {
    channel_id: ConversationRef,
    thread_id: Option<ConversationRef>,
    tz: Option<Tz>,
}
fn serialize_batch(
    events: &[MessageCreated],
    context: &BatchContext,
) -> Result<String, &'static str> {
    let mut converted = Vec::new();
    for message in events.iter().cloned() {
        if message.source().conversation() != &context.channel_id {
            return Err("channel fixture mismatch");
        }
        let reply = message.reply().clone();
        let message = message.with_context(context.thread_id.clone(), reply);
        converted.push(OrdinaryEvent::Message(Box::new(message)));
    }
    let Some((first, rest)) = converted.split_first() else {
        return Err("empty fixture");
    };
    let rest: Vec<_> = rest.iter().collect();
    Ok(group(
        first.source().conversation(),
        first,
        &rest,
        PresentationContext::new(context.tz, EvidenceVisibility::Enabled),
    )
    .content)
}
fn msg(message_id: u64, user: &str, user_id: u64, content: &str) -> MessageCreated {
    MessageCreated::new(
        source(100, message_id),
        ObservedAuthor::new(actor(user_id), user.to_string(), AuthorOrigin::DirectHuman),
        content.to_string(),
        Targeting::Ambient,
        Timestamp::parse("2026-06-19T15:30:00+00:00").unwrap(),
    )
}

fn msg_with_reply(
    message_id: u64,
    user: &str,
    user_id: u64,
    content: &str,
    reply_to: u64,
) -> MessageCreated {
    MessageCreated::new(
        source(100, message_id),
        ObservedAuthor::new(actor(user_id), user.to_string(), AuthorOrigin::DirectHuman),
        content.to_string(),
        Targeting::Ambient,
        Timestamp::parse("2026-06-19T15:30:00+00:00").unwrap(),
    )
    .with_context(
        None,
        ReplyObservation::new(Some(source(100, reply_to)), None, None, None),
    )
}

fn msg_with_attachments(
    message_id: u64,
    user: &str,
    user_id: u64,
    content: &str,
    attachment_count: usize,
) -> MessageCreated {
    let attachments = (0..attachment_count)
        .map(|i| {
            ObservedAttachment::new(
                format!("file{i}.png"),
                Some("image/png".to_string()),
                ByteSize::new(1024),
            )
        })
        .collect();

    MessageCreated::new(
        source(100, message_id),
        ObservedAuthor::new(actor(user_id), user.to_string(), AuthorOrigin::DirectHuman),
        content.to_string(),
        Targeting::Ambient,
        Timestamp::parse("2026-06-19T15:30:00+00:00").unwrap(),
    )
    .with_attachments(attachments, false)
}

fn ctx_basic() -> BatchContext {
    BatchContext {
        channel_id: conversation(100),
        thread_id: None,
        tz: None,
    }
}

fn ctx_with_thread() -> BatchContext {
    BatchContext {
        channel_id: conversation(100),
        thread_id: Some(conversation(200)),
        tz: None,
    }
}

fn ctx_with_tz() -> BatchContext {
    BatchContext {
        channel_id: conversation(100),
        thread_id: None,
        tz: Some("America/Los_Angeles".parse().expect("valid tz")),
    }
}

#[test]
fn batch_header_has_no_application_transport_suffix() {
    let rendered =
        serialize_batch(&[msg(1, "alice", 4242, "hello")], &ctx_basic()).expect("serialize");
    assert!(rendered.contains("1|15:30|alice|L=1\n"));
    assert!(!rendered.contains("|A="));
    assert!(!rendered.contains("|@"));
}

#[test]
fn same_effective_user_preserves_each_visible_message_name() {
    let rendered = serialize_batch(
        &[msg(1, "alice", 4242, "one"), msg(2, "bob", 4242, "two")],
        &ctx_basic(),
    )
    .expect("serialize");
    assert!(rendered.contains("[users 4242=alice 4242=bob]"));
    assert!(rendered.contains("1|15:30|alice|L=1\none"));
    assert!(rendered.contains("2|15:30|bob|L=1\ntwo"));
}

// ── Basic serialization ──────────────────────────────────────────────────

#[test]
fn single_message() {
    let events = vec![msg(1001, "alice", 42, "hello world")];
    let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

    assert!(result.starts_with("[batch ch=100 n=1 latest=1001]\n"));
    assert!(result.contains("[users 42=alice]\n"));
    assert!(result.contains("1001|"));
    assert!(result.contains("|alice|L=1\n"));
    assert!(result.contains("hello world\n"));
}

#[test]
fn multiple_messages_multiple_users() {
    let events = vec![
        msg(1001, "alice", 42, "hello"),
        msg(1002, "bob", 99, "hi there"),
        msg(1003, "alice", 42, "how are you?"),
    ];
    let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

    // Header should show n=3 and latest=1003.
    assert!(result.contains("n=3"));
    assert!(result.contains("latest=1003"));

    // Roster should have both users (BTreeMap: ordered by user_id).
    assert!(result.contains("[users 42=alice 99=bob]"));

    // All three messages should be present.
    assert!(result.contains("hello\n"));
    assert!(result.contains("hi there\n"));
    assert!(result.contains("how are you?\n"));
}

#[test]
fn user_deduplication() {
    let events = vec![
        msg(1001, "alice", 42, "first"),
        msg(1002, "alice", 42, "second"),
        msg(1003, "alice", 42, "third"),
    ];
    let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

    // Roster should only list alice once.
    let roster_line = result.lines().nth(1).expect("roster line");
    assert_eq!(roster_line, "[users 42=alice]");
}

// ── Thread context ───────────────────────────────────────────────────────

#[test]
fn thread_id_in_header() {
    let events = vec![msg(1001, "alice", 42, "in a thread")];
    let result = serialize_batch(&events, &ctx_with_thread()).expect("should serialize");

    assert!(result.starts_with("[batch ch=100 thread=200 n=1 latest=1001]\n"));
}

// ── Timezone handling ────────────────────────────────────────────────────

#[test]
fn timezone_in_header_and_timestamps() {
    let events = vec![msg(1001, "alice", 42, "morning!")];
    let result = serialize_batch(&events, &ctx_with_tz()).expect("should serialize");

    // Header should include tz.
    assert!(result.contains("tz=America/Los_Angeles"));

    // 15:30 UTC -> 08:30 PDT (June = daylight saving).
    assert!(result.contains("08:30"));
}

#[test]
fn utc_fallback_when_no_tz() {
    let events = vec![msg(1001, "alice", 42, "utc time")];
    let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

    // Should show UTC time: 15:30.
    assert!(result.contains("15:30"));
    // Should NOT contain tz= in header.
    assert!(!result.contains("tz="));
}

#[test]
fn timestamp_with_nonzero_seconds() {
    let events = vec![MessageCreated::new(
        source(100, 1001),
        ObservedAuthor::new(actor(42), "alice".to_string(), AuthorOrigin::DirectHuman),
        "with seconds".to_string(),
        Targeting::Ambient,
        Timestamp::parse("2026-06-19T15:30:45+00:00").unwrap(),
    )];
    let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

    // Non-zero seconds should show HH:MM:SS.
    assert!(result.contains("15:30:45"));
}

// ── Reply-to ─────────────────────────────────────────────────────────────

#[test]
fn reply_to_suffix() {
    let events = vec![msg_with_reply(1002, "bob", 99, "yes!", 1001)];
    let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

    assert!(result.contains("|L=1|>1001\n"));
}

// ── Attachments ──────────────────────────────────────────────────────────

#[test]
fn attachment_count_suffix() {
    let events = vec![msg_with_attachments(1003, "carol", 77, "look at this", 3)];
    let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

    assert!(result.contains("|L=1|+3\n"));
}

#[test]
fn reply_and_attachments_both_present() {
    let events = vec![
        MessageCreated::new(
            source(100, 1004),
            ObservedAuthor::new(actor(55), "dave".to_string(), AuthorOrigin::DirectHuman),
            "replying with files".to_string(),
            Targeting::Ambient,
            Timestamp::parse("2026-06-19T15:30:00+00:00").unwrap(),
        )
        .with_attachments(
            vec![ObservedAttachment::new(
                "doc.pdf".to_string(),
                Some("application/pdf".to_string()),
                ByteSize::new(2048),
            )],
            false,
        )
        .with_context(
            None,
            ReplyObservation::new(Some(source(100, 1000)), None, None, None),
        ),
    ];
    let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

    // Should have both suffixes.
    assert!(result.contains("|L=1|>1000|+1\n"));
}

// ── Error cases ──────────────────────────────────────────────────────────

// ── Full round-trip ──────────────────────────────────────────────────────

#[test]
fn full_conversation_batch() {
    let tz: Tz = "US/Eastern".parse().expect("valid tz");
    let ctx = BatchContext {
        channel_id: conversation(555),
        thread_id: Some(conversation(666)),
        tz: Some(tz),
    };

    let events = vec![
        MessageCreated::new(
            source(555, 2001),
            ObservedAuthor::new(actor(10), "lina".to_string(), AuthorOrigin::DirectHuman),
            "hey, check this out".to_string(),
            Targeting::Ambient,
            Timestamp::parse("2026-06-19T20:00:00+00:00").unwrap(),
        )
        .with_context(
            Some(conversation(666)),
            ReplyObservation::new(None, None, None, None),
        ),
        MessageCreated::new(
            source(555, 2002),
            ObservedAuthor::new(actor(20), "ros".to_string(), AuthorOrigin::DirectHuman),
            "oh nice!".to_string(),
            Targeting::Ambient,
            Timestamp::parse("2026-06-19T20:01:30+00:00").unwrap(),
        )
        .with_context(
            Some(conversation(666)),
            ReplyObservation::new(
                Some(source(555, 2001)),
                Some(actor(10)),
                Some("lina".to_string()),
                Some("hey, check this out".to_string()),
            ),
        ),
        MessageCreated::new(
            source(555, 2003),
            ObservedAuthor::new(actor(10), "lina".to_string(), AuthorOrigin::DirectHuman),
            "here's the file".to_string(),
            Targeting::Ambient,
            Timestamp::parse("2026-06-19T20:02:00+00:00").unwrap(),
        )
        .with_attachments(
            vec![ObservedAttachment::new(
                "design.png".to_string(),
                Some("image/png".to_string()),
                ByteSize::new(4096),
            )],
            false,
        )
        .with_context(
            Some(conversation(666)),
            ReplyObservation::new(None, None, None, None),
        ),
    ];

    let result = serialize_batch(&events, &ctx).expect("should serialize");

    // Verify structure.
    let lines: Vec<&str> = result.lines().collect();

    // Header: ch=555, thread=666, n=3, tz=US/Eastern, latest=2003
    assert_eq!(
        lines[0],
        "[batch ch=555 thread=666 n=3 tz=US/Eastern latest=2003]"
    );

    // Roster: keyed by user_id (numeric order).
    assert_eq!(lines[1], "[users 10=lina 20=ros]");

    // Blank separator.
    assert_eq!(lines[2], "");

    // 20:00 UTC -> 16:00 EDT (June = daylight saving, US/Eastern = -4).
    assert!(lines[3].starts_with("2001|16:00|lina|L=1"));
    assert_eq!(lines[4], "hey, check this out");

    // Reply: 20:01:30 UTC -> 16:01:30 EDT.
    assert!(lines[6].contains("|L=1|>2001"));
    assert!(lines[6].contains("16:01:30"));
    assert_eq!(lines[7], "oh nice!");

    // Attachment: 20:02 UTC -> 16:02 EDT.
    assert!(lines[9].contains("|L=1|+1"));
    assert_eq!(lines[10], "here's the file");
}

// ── Multiline content ────────────────────────────────────────────────────

#[test]
fn multiline_message_content() {
    let events = vec![MessageCreated::new(
        source(100, 1001),
        ObservedAuthor::new(actor(42), "alice".to_string(), AuthorOrigin::DirectHuman),
        "line one\nline two\nline three".to_string(),
        Targeting::Ambient,
        Timestamp::parse("2026-06-19T15:30:00+00:00").unwrap(),
    )];
    let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

    // Line count should reflect the three lines in content.
    assert!(result.contains("|L=3\n"));
    // The content should be preserved as-is (multiline).
    assert!(result.contains("line one\nline two\nline three\n"));
}

#[test]
fn empty_content_in_batch() {
    // P1: empty content should produce L=0 with no content line, and the
    // next message in the batch must still parse correctly.
    let events = vec![
        msg(1001, "alice", 42, ""),
        msg(1002, "bob", 99, "second message"),
    ];
    let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

    // First message header has L=0.
    assert!(result.contains("1001|15:30|alice|L=0\n"));

    // No content line after the L=0 header — the very next non-blank line
    // should be the second message's header.
    let lines: Vec<&str> = result.lines().collect();
    let l0_idx = lines
        .iter()
        .position(|l| l.contains("L=0"))
        .expect("L=0 line");
    // After the L=0 header, next line is blank separator, then second
    // message header.
    assert_eq!(lines[l0_idx + 1], "");
    assert!(lines[l0_idx + 2].starts_with("1002|"));

    // Second message parses fine.
    assert!(result.contains("|L=1\n"));
    assert!(result.contains("second message\n"));
}

#[test]
fn content_with_trailing_newline() {
    // P2: content ending in \n should be trimmed so L matches actual lines.
    let events = vec![msg(1001, "alice", 42, "text\n")];
    let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

    // "text\n" trimmed to "text" -> L=1, not L=2.
    assert!(result.contains("|L=1\n"));
    // Content line should be "text" (no double newline).
    assert!(result.contains("text\n"));
    // Verify no double newline from the content itself.
    assert!(!result.contains("text\n\n\n"));
}

#[test]
fn multiline_then_next_message() {
    // P2: the exact ambiguity case L=LINECOUNT was added to resolve.
    // A multiline message followed by another message must serialize
    // with correct separation — L tells the parser exactly how many
    // content lines to consume.
    let events = vec![
        msg(1001, "alice", 42, "a\nb\nc"),
        msg(1002, "bob", 99, "next"),
    ];
    let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

    let lines: Vec<&str> = result.lines().collect();

    // First message: L=3 for three content lines.
    let header_idx = lines
        .iter()
        .position(|l| l.contains("1001|"))
        .expect("first header");
    assert!(lines[header_idx].contains("|L=3"));
    assert_eq!(lines[header_idx + 1], "a");
    assert_eq!(lines[header_idx + 2], "b");
    assert_eq!(lines[header_idx + 3], "c");

    // Blank separator between messages.
    assert_eq!(lines[header_idx + 4], "");

    // Second message: L=1.
    assert!(lines[header_idx + 5].contains("1002|"));
    assert!(lines[header_idx + 5].contains("|L=1"));
    assert_eq!(lines[header_idx + 6], "next");
}

// ── Channel validation ──────────────────────────────────────────────────

// ── Wire-contract snapshot ───────────────────────────────────────────────

#[test]
fn wire_format_snapshot() {
    let tz: Tz = "US/Eastern".parse().expect("valid tz");
    let ctx = BatchContext {
        channel_id: conversation(555),
        thread_id: Some(conversation(666)),
        tz: Some(tz),
    };

    let events = vec![
        MessageCreated::new(
            source(555, 2001),
            ObservedAuthor::new(actor(10), "lina".to_string(), AuthorOrigin::DirectHuman),
            "hey, check this out".to_string(),
            Targeting::Ambient,
            Timestamp::parse("2026-06-19T20:00:00+00:00").unwrap(),
        )
        .with_context(
            Some(conversation(666)),
            ReplyObservation::new(None, None, None, None),
        ),
        MessageCreated::new(
            source(555, 2002),
            ObservedAuthor::new(actor(20), "ros".to_string(), AuthorOrigin::DirectHuman),
            "oh nice!".to_string(),
            Targeting::Ambient,
            Timestamp::parse("2026-06-19T20:01:30+00:00").unwrap(),
        )
        .with_context(
            Some(conversation(666)),
            ReplyObservation::new(
                Some(source(555, 2001)),
                Some(actor(10)),
                Some("lina".to_string()),
                Some("hey, check this out".to_string()),
            ),
        ),
        MessageCreated::new(
            source(555, 2003),
            ObservedAuthor::new(actor(10), "lina".to_string(), AuthorOrigin::DirectHuman),
            "here's the file".to_string(),
            Targeting::Ambient,
            Timestamp::parse("2026-06-19T20:02:00+00:00").unwrap(),
        )
        .with_attachments(
            vec![ObservedAttachment::new(
                "design.png".to_string(),
                Some("image/png".to_string()),
                ByteSize::new(4096),
            )],
            false,
        )
        .with_context(
            Some(conversation(666)),
            ReplyObservation::new(None, None, None, None),
        ),
    ];

    let expected = "\
[batch ch=555 thread=666 n=3 tz=US/Eastern latest=2003]
[users 10=lina 20=ros]

2001|16:00|lina|L=1
hey, check this out

2002|16:01:30|ros|L=1|>2001
oh nice!

2003|16:02|lina|L=1|+1
here's the file
";

    let result = serialize_batch(&events, &ctx).expect("should serialize");
    assert_eq!(result, expected);
}

#[test]
fn public_project_preserves_ordinary_single_shape() {
    let delivery = Delivery::single(
        OrdinaryEvent::Message(Box::new(msg(1001, "alice", 42, "hello"))),
        PresentationContext::new(None, EvidenceVisibility::Enabled),
    );
    let Presentation::Channel(channel) = project(&delivery) else {
        panic!("channel projection");
    };
    assert_eq!(channel.content, "hello");
    assert_eq!(
        serde_json::to_value(channel.meta).unwrap(),
        serde_json::json!({
            "chat_id": "100", "message_id": "1001", "user_id": "42", "user": "42=alice",
            "ts": "2026-06-19T15:30:00+00:00"
        })
    );
}
#[test]
fn public_project_groups_an_actual_two_event_batch() {
    let delivery = Delivery::batch(
        OrdinaryEvent::Message(Box::new(msg(1001, "alice", 42, "one"))),
        vec![OrdinaryEvent::Message(Box::new(msg(
            1002, "bob", 99, "two",
        )))],
        PresentationContext::new(None, EvidenceVisibility::Enabled),
    );
    let Presentation::Channel(channel) = project(&delivery) else {
        panic!("channel projection");
    };
    assert_eq!(
        channel.content,
        "[batch ch=100 n=2 latest=1002]\n[users 42=alice 99=bob]\n\n1001|15:30|alice|L=1\none\n\n1002|15:30|bob|L=1\ntwo\n"
    );
    assert_eq!(
        serde_json::to_value(channel.meta).unwrap(),
        serde_json::json!({
            "chat_id": "100", "type": "batch", "format": "batch_v1"
        })
    );
}
