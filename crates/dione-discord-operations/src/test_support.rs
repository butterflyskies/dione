//! Frozen original reserved-name setup for member registry consumers.
use dione_harness::HarnessConnection;
use dione_mcp_protocol::tools_list;
use std::collections::BTreeSet;
const PRIVATE_CONTROL_TOOLS: &[&str] = &[
    "teams_bind_session",
    "teams_check_session",
    "teams_recover_session",
    "teams_unbind_session",
    "teams_ack_event",
];

pub(crate) fn reserved_tool_names(harness: &HarnessConnection) -> BTreeSet<String> {
    let definitions = tools_list(
        harness.protocol().tools(),
        true,
        dione_evidence::EvidenceMarkerVersion::V3,
    );
    let mut reserved: BTreeSet<String> = dione_codex::TOOL_NAMES
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    if let Some(tools) = definitions["tools"].as_array() {
        reserved.extend(
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .map(str::to_owned),
        );
    }
    reserved.extend(PRIVATE_CONTROL_TOOLS.iter().map(|name| (*name).to_owned()));
    reserved
}
