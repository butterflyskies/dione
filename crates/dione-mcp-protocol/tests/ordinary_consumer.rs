use dione_evidence::EvidenceMarkerVersion;
use dione_mcp_protocol::{initialize_response, tools_list};
use serde_json::{Value, json};

#[test]
fn all_supported_protocol_versions_and_application_metadata_are_preserved() {
    let capabilities = json!({"tools":{"listChanged":true},"fixture":{"epoch":7}});
    for version in ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"] {
        assert_eq!(
            initialize_response("fixture-app-43.7", capabilities.clone(), Some(version)),
            json!({"protocolVersion":version,"capabilities":capabilities,
                "serverInfo":{"name":"dione","version":"fixture-app-43.7"}})
        );
    }
}

#[test]
fn unknown_and_missing_protocol_versions_use_the_same_latest_fallback() {
    for requested in [None, Some("fixture-unsupported-43"), Some("")] {
        assert_eq!(
            initialize_response("fixture-app-43.8", Value::Null, requested),
            json!({"protocolVersion":"2025-11-25","capabilities":null,
                "serverInfo":{"name":"dione","version":"fixture-app-43.8"}})
        );
    }
}

#[test]
fn all_thirty_three_builtins_precede_ordered_harness_snapshots() {
    let harness = vec![
        json!({"name":"fixture_tool_z","description":"fixture metadata z","inputSchema":{"type":"object"},"fixture_epoch":2}),
        json!({"name":"fixture_tool_a","description":"fixture metadata a","inputSchema":{"type":"object"},"fixture_epoch":1}),
    ];
    let result = tools_list(harness.clone(), false, EvidenceMarkerVersion::V3);
    let tools = result["tools"].as_array().unwrap();
    let expected = [
        "attention",
        "no_rly",
        "rephrase",
        "no_rly_stats",
        "no_rly_condense",
        "no_rly_vacuum",
        "react",
        "edit_message",
        "send_file",
        "send_dm",
        "list_access_requests",
        "approve_access",
        "deny_access",
        "get_presence",
        "set_presence",
        "render_latex",
        "render_latex_to_channel",
        "send_typing",
        "get_version",
        "reload_config",
        "set_trace_level",
        "set_stderr_level",
        "list_config_channels",
        "get_access_config",
        "add_channel",
        "remove_channel",
        "update_channel",
        "update_dm_policy",
        "add_allow_from",
        "remove_allow_from",
        "mute_server",
        "unmute_server",
        "list_muted_servers",
    ];
    assert_eq!(tools.len(), 35);
    assert_eq!(
        tools[..33]
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(&tools[33..], harness.as_slice());
    assert_eq!(tools[0]["inputSchema"], dione_attention::control::schema());
    assert_eq!(
        tools[3]["inputSchema"]["properties"]["outcome"]["enum"],
        json!(["released", "rephrased", "expired", "unknown"])
    );
}

#[test]
fn current_evidence_enablement_and_version_control_only_the_dm_projection() {
    for (version, marker) in [
        (EvidenceMarkerVersion::V2, "[🔍=v2:<role>:<token>]"),
        (EvidenceMarkerVersion::V3, "[🔍=v3:<role>:<token>]"),
    ] {
        for enabled in [false, true] {
            let result = tools_list(Vec::new(), enabled, version);
            let dm = result["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == "send_dm")
                .unwrap();
            let properties = &dm["inputSchema"]["properties"];
            for field in ["claim_handles", "citation_handles"] {
                assert_eq!(properties.get(field).is_some(), enabled);
                if enabled {
                    assert_eq!(properties[field]["maxItems"], 4);
                    assert!(
                        properties[field]["description"]
                            .as_str()
                            .unwrap()
                            .contains(marker)
                    );
                }
            }
            assert!(properties.get("evidence_marker_version").is_none());
            assert_eq!(result["tools"][0]["name"], "attention");
        }
    }
}
