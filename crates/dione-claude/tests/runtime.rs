use dione_harness::{AttentionPolicy, ProtocolOutput};
use dione_messages::{
    delivery::{Delivery, EvidenceVisibility, PresentationContext},
    events::{ControlEvent, PermissionDecision, PermissionRequestId, TraceLevel},
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn public_connection_projects_typed_delivery_and_retains_response_output() {
    let (writer, reader) = tokio::io::duplex(4096);
    let connection = dione_claude::connect(ProtocolOutput::new(writer), &["reserved-operation"]);
    let delivery = Delivery::control(
        ControlEvent::Trace {
            level: TraceLevel::INFO,
            target: "consumer".into(),
            message: "line one\n雪 🛰".into(),
            fields: Vec::new(),
        },
        PresentationContext::new(None, EvidenceVisibility::Enabled),
    );
    connection.deliver(&delivery).await.unwrap();
    connection
        .output()
        .write(&json!({"jsonrpc":"2.0","id":3,"result":{}}))
        .await
        .unwrap();
    let mut lines = BufReader::new(reader).lines();
    let first: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    let second: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(
        first,
        json!({"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":"line one\n雪 🛰","meta":{"level":"INFO","target":"consumer","type":"trace"}}})
    );
    assert_eq!(second, json!({"jsonrpc":"2.0","id":3,"result":{}}));
}

#[tokio::test]
async fn public_protocol_uses_supplied_catalog_and_exact_capabilities() {
    let connection = dione_claude::connect(
        ProtocolOutput::new(tokio::io::sink()),
        &["new-provider-tool", " spaced-tool "],
    );
    assert_eq!(connection.attention_policy(), AttentionPolicy::Direct);
    assert_eq!(dione_claude::ATTENTION_POLICY, AttentionPolicy::Direct);
    let protocol = connection.protocol();
    assert_eq!(
        protocol.capabilities(),
        json!({"tools":{},"experimental":{"claude/channel":{},"claude/channel/permission":{}}})
    );
    assert!(protocol.tools().is_empty());
    assert!(!protocol.resident_control());
    assert!(protocol.permission_relay());
    for name in ["new-provider-tool", " spaced-tool "] {
        let error = protocol.call(name, &json!({})).await.unwrap().unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("{name} is only available in Codex mode")
        );
    }
    assert!(protocol.call("unknown-tool", &json!({})).await.is_none());
}

#[tokio::test]
async fn public_permission_envelope_preserves_decision_and_request_whitespace() {
    for (decision, behavior) in [
        (PermissionDecision::Granted, "allow"),
        (PermissionDecision::Denied, "deny"),
    ] {
        let (writer, reader) = tokio::io::duplex(1024);
        let connection = dione_claude::connect(ProtocolOutput::new(writer), &[]);
        let delivery = Delivery::control(
            ControlEvent::Permission {
                request: PermissionRequestId::new(" request-token ".into()).unwrap(),
                decision,
            },
            PresentationContext::new(None, EvidenceVisibility::Enabled),
        );
        connection.deliver(&delivery).await.unwrap();
        let mut lines = BufReader::new(reader).lines();
        let actual: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            actual,
            json!({"jsonrpc":"2.0","method":"notifications/claude/channel/permission","params":{"request_id":" request-token ","behavior":behavior}})
        );
    }
}
