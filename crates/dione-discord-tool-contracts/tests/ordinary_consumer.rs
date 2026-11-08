use dione_discord_tool_contracts::{
    management::{self, ManagementRequest, ManagementTool},
    metadata::{self, MetadataTool},
    query::{self, QueryRequest, QueryTool},
    reply::{self, ReplyRequest},
};
use dione_evidence::EvidenceMarkerVersion;
use serde_json::json;
use std::error::Error;

#[test]
fn ordinary_catalog_preserves_twenty_names_and_current_reply_presentation() {
    let names: Vec<String> = metadata::definitions()
        .chain(query::definitions())
        .chain(management::definitions())
        .chain(std::iter::once(reply::definition(
            true,
            EvidenceMarkerVersion::V3,
        )))
        .map(|v| v["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        names,
        [
            "list_guilds",
            "list_channels",
            "get_channel",
            "get_user",
            "get_member",
            "list_roles",
            "list_emojis",
            "can_view_channel",
            "channel_effective_audience",
            "fetch_messages",
            "fetch_new_since",
            "fetch_pins",
            "download_attachment",
            "get_message",
            "search_messages",
            "pin_message",
            "unpin_message",
            "create_thread",
            "delete_message",
            "reply"
        ]
    );
    assert_eq!(
        MetadataTool::ALL.len() + QueryTool::ALL.len() + ManagementTool::ALL.len() + 1,
        20
    );
    for version in [EvidenceMarkerVersion::V2, EvidenceMarkerVersion::V3] {
        let enabled = reply::definition(true, version);
        let disabled = reply::definition(false, version);
        assert_eq!(
            enabled["inputSchema"]["required"],
            json!(["channel_id", "content"])
        );
        assert_eq!(
            enabled["inputSchema"]["properties"]["suppress_ping"]["type"],
            "boolean"
        );
        assert!(
            enabled["inputSchema"]["properties"]
                .get("claim_handles")
                .is_some()
        );
        assert!(
            disabled["inputSchema"]["properties"]
                .get("claim_handles")
                .is_none()
        );
        assert!(
            disabled["inputSchema"]["properties"]
                .get("citation_handles")
                .is_none()
        );
    }
}

#[test]
fn ordinary_requests_preserve_precedence_sources_borrowing_and_evidence_gate() {
    let bad = QueryTool::Messages
        .parse(&json!({"response_version":3,"channel_id":"bad","before":null}))
        .err()
        .unwrap();
    assert_eq!(
        bad.to_string(),
        "response_version must be the integer 1 or 2"
    );
    let bad = QueryTool::Messages
        .parse(&json!({"response_version":1,"channel_id":"42","before":null}))
        .err()
        .unwrap();
    assert_eq!(
        bad.to_string(),
        "invalid before: null is not a valid snowflake; omit the field instead"
    );
    let bad = QueryTool::Messages
        .parse(&json!({"channel_id":"42","before":"bad"}))
        .err()
        .unwrap();
    assert_eq!(bad.to_string(), "invalid before: not a valid u64");
    let mut cause = bad.source();
    let mut numeric = false;
    while let Some(error) = cause {
        numeric |= error.is::<std::num::ParseIntError>();
        cause = error.source();
    }
    assert!(numeric);
    match QueryTool::Messages
        .parse(&json!({"channel_id":"42","limit":"legacy ignored"}))
        .unwrap()
    {
        QueryRequest::Messages { limit, .. } => assert_eq!(limit, 20),
        _ => panic!("history request expected"),
    }
    for version in [EvidenceMarkerVersion::V2, EvidenceMarkerVersion::V3] {
        let input = json!({"channel_id":"42","content":"fixture text","claim_handles":[]});
        let result = ReplyRequest::parse(&input, false, version);
        assert!(
            result.is_err(),
            "disabled evidence fields must be rejected before decoding"
        );
        let error = result.err().unwrap();
        assert_eq!(
            error.to_string(),
            "claim_handles and citation_handles require evidence_markers_enabled"
        );
    }
    let input = json!({"channel_id":"42","content":"fixture text","reply_to_message_id":{},"suppress_ping":"legacy ignored"});
    let parsed = ReplyRequest::parse(&input, true, EvidenceMarkerVersion::V3).unwrap();
    assert_eq!(parsed.channel.get(), 42);
    assert_eq!(parsed.reply_to, None);
    assert!(!parsed.ping.suppressed());
    assert!(std::ptr::eq(
        parsed.content,
        input["content"].as_str().unwrap()
    ));
    let input = json!({"channel_id":"42","message_id":{},"name":"fixture-thread"});
    match ManagementTool::CreateThread.parse(&input).unwrap() {
        ManagementRequest::Thread {
            channel,
            message,
            name,
        } => {
            assert_eq!(channel.get(), 42);
            assert_eq!(message, None);
            assert_eq!(name, "fixture-thread");
        }
        _ => panic!("thread request expected"),
    }
}
