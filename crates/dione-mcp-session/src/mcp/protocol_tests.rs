//! Existing protocol contracts through current application harness fixtures.

use dione_mcp_protocol::tools_list;
use serde_json::{Value, json};

fn fetch_messages_schema() -> Value {
    dione_discord_tool_contracts::query::definitions()
        .find(|tool| tool["name"] == "fetch_messages")
        .expect("fetch_messages must be in owner definitions")
}

/// Boundary pin for the presence tools: deleting either from the
/// advertised list must fail here, not in a reviewer's client.
#[test]
fn selected_evidence_version_is_advertised_consistently_by_reply_and_dm_for_both_harnesses() {
    let (_queue_dir, codex) = crate::mcp::server::test_codex_connection();
    for harness in [
        &dione_claude::connect(
            dione_harness::ProtocolOutput::new(tokio::io::stdout()),
            dione_codex::TOOL_NAMES,
        ),
        &codex,
    ] {
        for (version, marker, encoding) in [
            (
                dione_evidence::EvidenceMarkerVersion::V2,
                "[🔍=v2:<role>:<token>]",
                "all eight big-endian",
            ),
            (
                dione_evidence::EvidenceMarkerVersion::V3,
                "[🔍=v3:<role>:<token>]",
                "minimal-width big-endian",
            ),
        ] {
            for enabled in [false, true] {
                let list = tools_list(harness.protocol().tools(), enabled, version);
                let dm = list["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|t| t["name"] == "send_dm")
                    .unwrap();
                let reply = dione_discord_tool_contracts::reply::definition(enabled, version);
                for tool in [dm, &reply] {
                    for field in ["claim_handles", "citation_handles"] {
                        let property = &tool["inputSchema"]["properties"][field];
                        if enabled {
                            let description = property["description"].as_str().unwrap();
                            assert!(description.contains(marker), "{description}");
                            assert!(description.contains(encoding), "{description}");
                            assert_eq!(property["maxItems"], 4);
                        } else {
                            assert!(property.is_null());
                        }
                    }
                    assert!(
                        tool["inputSchema"]["properties"]
                            .get("evidence_marker_version")
                            .is_none()
                    );
                }
            }
        }
    }
}

#[test]
fn unknown_delivery_outcome_is_queryable_in_advertised_journal_schema() {
    let list = tools_list(
        dione_claude::connect(
            dione_harness::ProtocolOutput::new(tokio::io::stdout()),
            dione_codex::TOOL_NAMES,
        )
        .protocol()
        .tools(),
        false,
        dione_evidence::EvidenceMarkerVersion::V3,
    );
    let stats = list["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "no_rly_stats")
        .unwrap();
    assert_eq!(
        stats["inputSchema"]["properties"]["outcome"]["enum"],
        json!(["released", "rephrased", "expired", "unknown"])
    );
    assert_eq!(
        dione_outbound_consent::journal::Outcome::parse("unknown"),
        Some(dione_outbound_consent::journal::Outcome::Unknown)
    );
}

#[test]
fn presence_tools_are_listed_with_their_schemas() {
    let list = tools_list(
        dione_claude::connect(
            dione_harness::ProtocolOutput::new(tokio::io::stdout()),
            dione_codex::TOOL_NAMES,
        )
        .protocol()
        .tools(),
        false,
        dione_evidence::EvidenceMarkerVersion::V3,
    );
    let tools = list["tools"].as_array().unwrap();
    let set = tools
        .iter()
        .find(|t| t["name"] == "set_presence")
        .expect("set_presence must be in tools list");
    for field in ["online_status", "activity_type", "activity_name"] {
        assert!(
            set["inputSchema"]["properties"].get(field).is_some(),
            "set_presence schema must carry {field}"
        );
    }
    assert!(
        tools.iter().any(|t| t["name"] == "get_presence"),
        "get_presence must be in tools list"
    );
}

#[test]
fn claim_and_citation_handle_schemas_are_exposed_only_when_enabled() {
    for enabled in [false, true] {
        let (_queue_dir, harness) = crate::mcp::server::test_codex_connection();
        let list = tools_list(
            harness.protocol().tools(),
            enabled,
            dione_evidence::EvidenceMarkerVersion::V3,
        );
        for name in ["send_dm"] {
            let tool = list["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap();
            let properties = &tool["inputSchema"]["properties"];
            for field in ["claim_handles", "citation_handles"] {
                assert_eq!(
                    properties.get(field).is_some(),
                    enabled,
                    "{name} {field} schema must follow the config gate"
                );
            }
            assert!(
                properties.get("evidence_keys").is_none(),
                "{name} must not advertise the removed evidence_keys alias"
            );
        }
    }
}

#[test]
fn fetch_messages_schema_has_cursor_fields() {
    let tool = fetch_messages_schema();
    let props = &tool["inputSchema"]["properties"];
    assert!(
        props.get("before").is_some(),
        "schema must include 'before'"
    );
    assert!(props.get("after").is_some(), "schema must include 'after'");
}

#[test]
fn fetch_messages_schema_documents_mutual_exclusion() {
    let tool = fetch_messages_schema();
    let desc = tool["description"].as_str().unwrap();
    assert!(
        desc.contains("mutually exclusive") || desc.contains("Mutually exclusive"),
        "tool description must document mutual exclusion of before/after"
    );
}

#[test]
fn fetch_messages_schema_documents_ordering() {
    let tool = fetch_messages_schema();
    let desc = tool["description"].as_str().unwrap();
    assert!(
        desc.contains("oldest-first"),
        "tool description must document oldest-first ordering"
    );
}

#[test]
fn fetch_messages_schema_cursor_descriptions_mention_exclusivity() {
    let tool = fetch_messages_schema();
    let props = &tool["inputSchema"]["properties"];
    let before_desc = props["before"]["description"].as_str().unwrap();
    let after_desc = props["after"]["description"].as_str().unwrap();
    assert!(
        before_desc.contains("Mutually exclusive"),
        "before description must mention mutual exclusivity"
    );
    assert!(
        after_desc.contains("Mutually exclusive"),
        "after description must mention mutual exclusivity"
    );
}
