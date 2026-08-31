use camino::Utf8PathBuf;
use dione::{
    codex::TransportMode,
    config::{Config, DmPolicy, LoadedConfig},
    mcp::server::{DioneServer, test_helpers},
    no_rly::consent::ConsentGate,
    queue::AccessQueue,
    state::new_state,
    tracing_channel::TraceLevelController,
};
use serde_json::{Value, json};
use serenity::http::Http;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::{Mutex, mpsc};

static CONFIG_DISPATCH_LOCK: Mutex<()> = Mutex::const_new(());

// ── Test helpers ─────────────────────────────────────────────────────────────

fn temp_state_dir() -> (TempDir, Utf8PathBuf) {
    let dir = TempDir::new().unwrap();
    let state_dir = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
    (dir, state_dir)
}

/// Load config directly from disk, bypassing the global ArcSwap cache.
///
/// The global `load_config()` reads from a process-wide cache that is mutated
/// by `ConfigRuntime::mutate()`. When tests run in parallel each test's
/// mutation overwrites the same cache, causing flaky assertions. This helper
/// reads the TOML file from the test's own temp directory so each test is
/// isolated.
fn load_config_from_disk(state_dir: &Utf8PathBuf) -> LoadedConfig {
    let config_path = state_dir.join("config.toml");
    let contents = std::fs::read_to_string(&config_path).unwrap_or_default();
    let raw: Config = toml::from_str(&contents).unwrap_or_default();
    LoadedConfig::try_from_raw(raw).expect("test configuration generation")
}

fn make_server(state_dir: &Utf8PathBuf) -> DioneServer {
    let (notification_tx, _notification_rx) = mpsc::channel(1);
    DioneServer::new(
        new_state(),
        Arc::new(Mutex::new(AccessQueue::load(state_dir))),
        Arc::new(Http::new("fake-token-for-tests")),
        state_dir.clone(),
        notification_tx,
        TraceLevelController::noop(),
        TransportMode::ClaudeCode,
        Arc::new(ConsentGate::new(state_dir)),
        Arc::new(dione::ingress_ledger::IngressLedger::new()),
    )
}

fn config_projection(config: &LoadedConfig) -> Value {
    json!({
        "channels": config.raw.channels.iter().map(|channel| json!({
            "id": channel.id,
            "require_mention": channel.require_mention,
            "allow_from": channel.allow_from,
        })).collect::<Vec<_>>(),
        "dm_policy": match config.raw.access.dm_policy {
            DmPolicy::Queue => "queue",
            DmPolicy::Drop => "drop",
            DmPolicy::Disabled => "disabled",
            _ => "unknown",
        },
        "allow_from": config.raw.access.allow_from,
        "admins": config.raw.access.admins,
    })
}

async fn call_config_tool(state_dir: &Utf8PathBuf, name: &str, arguments: Value) -> Value {
    // ArcSwap is process-global. Keep the production dispatch and its
    // immediate disk/live oracle indivisible across parallel test cases.
    let _config_dispatch = CONFIG_DISPATCH_LOCK.lock().await;
    let server = make_server(state_dir);
    let response = test_helpers::dispatch_request(
        &server,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        }),
    )
    .await
    .expect("tools/call response");
    if let Some(error) = response.get("error") {
        return json!({ "error": error["message"].as_str().unwrap_or("JSON-RPC error") });
    }
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool result text");
    let result: Value = serde_json::from_str(text).expect("config tool JSON result");
    if result["ok"] == true {
        assert!(
            result["generation"].is_u64(),
            "an acknowledged mutation must identify its published generation: {result}"
        );
        assert_eq!(
            result["durability"], "durable",
            "the ordinary production path must return a typed durability receipt"
        );
        let disk = load_config_from_disk(state_dir);
        let live = dione::config::load_config(state_dir);
        assert_eq!(
            config_projection(&disk),
            config_projection(&live),
            "an acknowledged production-dispatch mutation must be visible on disk and in the immediate ArcSwap snapshot"
        );
    }
    result
}

#[tokio::test]
async fn malformed_sidecar_error_from_production_dispatch_is_snippet_free() {
    let (_dir, state_dir) = temp_state_dir();
    std::fs::write(
        state_dir.join("config.toml"),
        "[contradictionary]\nenabled = true\n",
    )
    .unwrap();
    std::fs::write(
        state_dir.join("contradictionary.toml"),
        "[[entry]]\npattern = \"sekrit-token\"\naction = [",
    )
    .unwrap();

    let result = add_allow_from(&state_dir, "424242").await;
    let rendered = result.to_string();
    assert!(result.get("error").is_some(), "got: {result}");
    assert!(
        !rendered.contains("sekrit-token") && !rendered.contains("\\n"),
        "MCP errors must not expose malformed sidecar source snippets: {rendered}"
    );
}

#[tokio::test]
async fn invalid_sidecar_semantic_value_from_production_dispatch_is_value_free() {
    let (_dir, state_dir) = temp_state_dir();
    std::fs::write(
        state_dir.join("config.toml"),
        "[contradictionary]\nenabled = true\n",
    )
    .unwrap();
    std::fs::write(
        state_dir.join("contradictionary.toml"),
        "[[entry]]\npattern = \"ordinary\"\naction = \"sekrit-semantic-token\"\n",
    )
    .unwrap();

    let result = add_allow_from(&state_dir, "424242").await;
    let rendered = result.to_string();
    assert!(result.get("error").is_some(), "got: {result}");
    assert!(
        !rendered.contains("sekrit-semantic-token"),
        "MCP errors must not echo attacker-controlled semantic values: {rendered}"
    );
    assert!(rendered.contains("invalid entry schema"), "got: {rendered}");
}

/// Read channel list through the production MCP dispatch.
async fn list_config_channels(state_dir: &Utf8PathBuf) -> Value {
    call_config_tool(state_dir, "list_config_channels", json!({})).await
}

/// Read access config through the production MCP dispatch.
async fn get_access_config(state_dir: &Utf8PathBuf) -> Value {
    call_config_tool(state_dir, "get_access_config", json!({})).await
}

async fn add_channel(
    state_dir: &Utf8PathBuf,
    id: &str,
    require_mention: Option<bool>,
    allow_from: Option<Vec<String>>,
) -> Value {
    call_config_tool(
        state_dir,
        "add_channel",
        json!({
            "id": id,
            "require_mention": require_mention,
            "allow_from": allow_from,
        }),
    )
    .await
}

async fn remove_channel(state_dir: &Utf8PathBuf, id: &str) -> Value {
    call_config_tool(state_dir, "remove_channel", json!({ "id": id })).await
}

async fn update_channel(
    state_dir: &Utf8PathBuf,
    id: &str,
    require_mention: Option<bool>,
    allow_from: Option<Vec<String>>,
) -> Value {
    call_config_tool(
        state_dir,
        "update_channel",
        json!({
            "id": id,
            "require_mention": require_mention,
            "allow_from": allow_from,
        }),
    )
    .await
}

async fn update_dm_policy(state_dir: &Utf8PathBuf, policy: &str) -> Value {
    call_config_tool(state_dir, "update_dm_policy", json!({ "policy": policy })).await
}

async fn add_allow_from(state_dir: &Utf8PathBuf, user_id: &str) -> Value {
    call_config_tool(state_dir, "add_allow_from", json!({ "user_id": user_id })).await
}

async fn remove_allow_from(state_dir: &Utf8PathBuf, user_id: &str) -> Value {
    call_config_tool(
        state_dir,
        "remove_allow_from",
        json!({ "user_id": user_id }),
    )
    .await
}

// ── Channel round-trips ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_add_channel_roundtrips_through_load_config() {
    let (_dir, state_dir) = temp_state_dir();

    let result = add_channel(&state_dir, "12345", Some(false), Some(vec!["111".into()])).await;
    assert_eq!(result["ok"], true);

    let config = load_config_from_disk(&state_dir);
    assert_eq!(config.raw.channels.len(), 1);
    assert_eq!(config.raw.channels[0].id, "12345");
    assert!(!config.raw.channels[0].require_mention);
    assert_eq!(config.raw.channels[0].allow_from, vec!["111"]);
    assert!(config.channel_policy(12345).is_some());
}

#[tokio::test]
async fn test_add_channel_defaults() {
    let (_dir, state_dir) = temp_state_dir();

    let result = add_channel(&state_dir, "99999", None, None).await;
    assert_eq!(result["ok"], true);

    let config = load_config_from_disk(&state_dir);
    assert!(config.raw.channels[0].require_mention);
    assert!(config.raw.channels[0].allow_from.is_empty());
}

#[tokio::test]
async fn test_add_channel_rejects_duplicate() {
    let (_dir, state_dir) = temp_state_dir();

    add_channel(&state_dir, "12345", None, None).await;
    let result = add_channel(&state_dir, "12345", None, None).await;
    assert!(result.get("error").is_some());

    let config = load_config_from_disk(&state_dir);
    assert_eq!(config.raw.channels.len(), 1);
}

#[tokio::test]
async fn test_add_channel_rejects_non_numeric_id() {
    let (_dir, state_dir) = temp_state_dir();

    let result = add_channel(&state_dir, "not-a-number", None, None).await;
    assert!(result.get("error").is_some());
}

#[tokio::test]
async fn test_add_channel_rejects_invalid_allow_from() {
    let (_dir, state_dir) = temp_state_dir();

    let result = add_channel(&state_dir, "12345", None, Some(vec!["bad".into()])).await;
    assert!(result.get("error").is_some());
}

#[tokio::test]
async fn test_remove_channel_roundtrip() {
    let (_dir, state_dir) = temp_state_dir();

    add_channel(&state_dir, "12345", None, None).await;
    let result = remove_channel(&state_dir, "12345").await;
    assert_eq!(result["ok"], true);

    let config = load_config_from_disk(&state_dir);
    assert!(config.raw.channels.is_empty());
}

#[tokio::test]
async fn test_remove_channel_not_found() {
    let (_dir, state_dir) = temp_state_dir();

    let result = remove_channel(&state_dir, "99999").await;
    assert!(result.get("error").is_some());
}

#[tokio::test]
async fn test_add_remove_add_sequence() {
    let (_dir, state_dir) = temp_state_dir();

    add_channel(&state_dir, "111", None, None).await;
    remove_channel(&state_dir, "111").await;
    add_channel(&state_dir, "222", None, None).await;

    let list = list_config_channels(&state_dir).await;
    let channels = list["channels"].as_array().unwrap();
    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0]["id"], "222");

    let config = load_config_from_disk(&state_dir);
    assert_eq!(config.raw.channels.len(), 1);
}

#[tokio::test]
async fn test_update_channel_roundtrip() {
    let (_dir, state_dir) = temp_state_dir();

    add_channel(&state_dir, "12345", Some(true), None).await;
    let result = update_channel(&state_dir, "12345", Some(false), Some(vec!["999".into()])).await;
    assert_eq!(result["ok"], true);

    let config = load_config_from_disk(&state_dir);
    assert!(!config.raw.channels[0].require_mention);
    assert_eq!(config.raw.channels[0].allow_from, vec!["999"]);
}

#[tokio::test]
async fn test_update_channel_not_found() {
    let (_dir, state_dir) = temp_state_dir();

    let result = update_channel(&state_dir, "99999", Some(false), None).await;
    assert!(result.get("error").is_some());
    assert!(result["error"].as_str().unwrap().contains("not found"));
}

#[tokio::test]
async fn test_update_channel_rejects_invalid_allow_from() {
    let (_dir, state_dir) = temp_state_dir();

    add_channel(&state_dir, "12345", None, None).await;
    let result = update_channel(&state_dir, "12345", None, Some(vec!["bad-id".into()])).await;
    assert!(result.get("error").is_some());

    // Config should be unchanged.
    let config = load_config_from_disk(&state_dir);
    assert!(config.raw.channels[0].allow_from.is_empty());
}

#[tokio::test]
async fn test_update_channel_requires_at_least_one_field() {
    let (_dir, state_dir) = temp_state_dir();

    add_channel(&state_dir, "12345", None, None).await;
    let result = update_channel(&state_dir, "12345", None, None).await;
    assert!(result.get("error").is_some());
}

// ── Access round-trips ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_add_allow_from_roundtrip() {
    let (_dir, state_dir) = temp_state_dir();

    let result = add_allow_from(&state_dir, "12345").await;
    assert_eq!(result["ok"], true);

    let config = load_config_from_disk(&state_dir);
    assert!(config.is_allowed(12345));
}

#[tokio::test]
async fn test_add_allow_from_rejects_duplicate() {
    let (_dir, state_dir) = temp_state_dir();

    add_allow_from(&state_dir, "12345").await;
    let result = add_allow_from(&state_dir, "12345").await;
    assert!(result.get("error").is_some());
}

#[tokio::test]
async fn test_add_allow_from_rejects_non_numeric() {
    let (_dir, state_dir) = temp_state_dir();

    let result = add_allow_from(&state_dir, "not-a-number").await;
    assert!(result.get("error").is_some());
}

#[tokio::test]
async fn test_remove_allow_from_roundtrip() {
    let (_dir, state_dir) = temp_state_dir();

    add_allow_from(&state_dir, "12345").await;
    let result = remove_allow_from(&state_dir, "12345").await;
    assert_eq!(result["ok"], true);

    let config = load_config_from_disk(&state_dir);
    assert!(!config.is_allowed(12345));
}

#[tokio::test]
async fn test_remove_allow_from_not_found() {
    let (_dir, state_dir) = temp_state_dir();

    let result = remove_allow_from(&state_dir, "99999").await;
    assert!(result.get("error").is_some());
}

#[tokio::test]
async fn test_remove_allow_from_rejects_non_numeric() {
    let (_dir, state_dir) = temp_state_dir();

    let result = remove_allow_from(&state_dir, "not-a-number").await;
    assert!(result.get("error").is_some());
}

// ── DM policy round-trips ───────────────────────────────────────────────────

#[tokio::test]
async fn test_update_dm_policy_drop() {
    let (_dir, state_dir) = temp_state_dir();

    let result = update_dm_policy(&state_dir, "drop").await;
    assert_eq!(result["ok"], true);

    let config = load_config_from_disk(&state_dir);
    assert_eq!(config.access.dm_policy, DmPolicy::Drop);
}

#[tokio::test]
async fn test_update_dm_policy_disabled() {
    let (_dir, state_dir) = temp_state_dir();

    let result = update_dm_policy(&state_dir, "disabled").await;
    assert_eq!(result["ok"], true);

    let config = load_config_from_disk(&state_dir);
    assert_eq!(config.access.dm_policy, DmPolicy::Disabled);
}

#[tokio::test]
async fn test_update_dm_policy_queue() {
    let (_dir, state_dir) = temp_state_dir();

    update_dm_policy(&state_dir, "drop").await;
    let result = update_dm_policy(&state_dir, "queue").await;
    assert_eq!(result["ok"], true);

    let config = load_config_from_disk(&state_dir);
    assert_eq!(config.access.dm_policy, DmPolicy::Queue);
}

#[tokio::test]
async fn test_update_dm_policy_invalid() {
    let (_dir, state_dir) = temp_state_dir();

    let result = update_dm_policy(&state_dir, "yolo").await;
    assert!(result.get("error").is_some());
}

// ── get_access_config ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_get_access_config_reflects_mutations() {
    let (_dir, state_dir) = temp_state_dir();

    update_dm_policy(&state_dir, "drop").await;
    add_allow_from(&state_dir, "12345").await;

    let result = get_access_config(&state_dir).await;
    assert_eq!(result["dm_policy"], "drop");
    let allow_from = result["allow_from"].as_array().unwrap();
    assert!(allow_from.iter().any(|v| v.as_str() == Some("12345")));
}

// ── Empty state dir ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_operations_on_empty_state_dir() {
    let (_dir, state_dir) = temp_state_dir();

    let list = list_config_channels(&state_dir).await;
    assert!(list["channels"].as_array().unwrap().is_empty());

    let access = get_access_config(&state_dir).await;
    assert_eq!(access["dm_policy"], "queue");
}
