use camino::Utf8PathBuf;
use serde_json::{Value, json};
use tempfile::TempDir;

use dione::config::{DmPolicy, load_config};
use dione::config_store::{ConfigStore, DiscordId};

// ── Test helpers ─────────────────────────────────────────────────────────────

fn temp_state_dir() -> (TempDir, Utf8PathBuf) {
    let dir = TempDir::new().unwrap();
    let state_dir = Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap();
    (dir, state_dir)
}

fn list_config_channels(state_dir: &Utf8PathBuf) -> Value {
    ConfigStore::list_channels(state_dir)
}

fn get_access_config(state_dir: &Utf8PathBuf) -> Value {
    ConfigStore::get_access(state_dir)
}

async fn add_channel(
    state_dir: &Utf8PathBuf,
    id: &str,
    require_mention: Option<bool>,
    allow_from: Option<Vec<String>>,
) -> Value {
    if DiscordId::parse(id).is_err() {
        return json!({ "error": format!("invalid channel id: {id}") });
    }
    let af = allow_from.unwrap_or_default();
    for af_id in &af {
        if DiscordId::parse(af_id).is_err() {
            return json!({ "error": format!("invalid allow_from user id: {af_id}") });
        }
    }
    match async {
        let mut editor = ConfigStore::load(state_dir).await?;
        editor.add_channel_entry(id, require_mention.unwrap_or(true), af)?;
        editor.save().await
    }
    .await
    {
        Ok(()) => json!({ "ok": true, "id": id }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn remove_channel(state_dir: &Utf8PathBuf, id: &str) -> Value {
    if DiscordId::parse(id).is_err() {
        return json!({ "error": format!("invalid channel id: {id}") });
    }
    match async {
        let mut editor = ConfigStore::load(state_dir).await?;
        editor.remove_channel_entry(id)?;
        editor.save().await
    }
    .await
    {
        Ok(()) => json!({ "ok": true, "id": id }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn update_channel(
    state_dir: &Utf8PathBuf,
    id: &str,
    require_mention: Option<bool>,
    allow_from: Option<Vec<String>>,
) -> Value {
    if DiscordId::parse(id).is_err() {
        return json!({ "error": format!("invalid channel id: {id}") });
    }
    if require_mention.is_none() && allow_from.is_none() {
        return json!({ "error": "at least one of require_mention or allow_from must be provided" });
    }
    if let Some(ref af) = allow_from {
        for af_id in af {
            if DiscordId::parse(af_id).is_err() {
                return json!({ "error": format!("invalid allow_from user id: {af_id}") });
            }
        }
    }
    match async {
        let mut editor = ConfigStore::load(state_dir).await?;
        editor.update_channel_entry(id, require_mention, allow_from)?;
        editor.save().await
    }
    .await
    {
        Ok(()) => json!({ "ok": true, "id": id }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn update_dm_policy(state_dir: &Utf8PathBuf, policy: &str) -> Value {
    if !matches!(policy, "drop" | "queue" | "disabled") {
        return json!({ "error": format!("invalid dm_policy: {policy}; must be one of: drop, queue, disabled") });
    }
    match async {
        let mut editor = ConfigStore::load(state_dir).await?;
        editor.set_dm_policy(policy);
        editor.save().await
    }
    .await
    {
        Ok(()) => json!({ "ok": true, "dm_policy": policy }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn add_allow_from(state_dir: &Utf8PathBuf, user_id: &str) -> Value {
    if DiscordId::parse(user_id).is_err() {
        return json!({ "error": format!("invalid user_id: {user_id}") });
    }
    match async {
        let mut editor = ConfigStore::load(state_dir).await?;
        editor.add_to_allow_from(user_id)?;
        editor.save().await
    }
    .await
    {
        Ok(()) => json!({ "ok": true, "user_id": user_id }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn remove_allow_from(state_dir: &Utf8PathBuf, user_id: &str) -> Value {
    if DiscordId::parse(user_id).is_err() {
        return json!({ "error": format!("invalid user_id: {user_id}") });
    }
    match async {
        let mut editor = ConfigStore::load(state_dir).await?;
        editor.remove_from_allow_from(user_id)?;
        editor.save().await
    }
    .await
    {
        Ok(()) => json!({ "ok": true, "user_id": user_id }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── Channel round-trips ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_add_channel_roundtrips_through_load_config() {
    let (_dir, state_dir) = temp_state_dir();

    let result = add_channel(&state_dir, "12345", Some(false), Some(vec!["111".into()])).await;
    assert_eq!(result["ok"], true);

    let config = load_config(&state_dir);
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

    let config = load_config(&state_dir);
    assert!(config.raw.channels[0].require_mention);
    assert!(config.raw.channels[0].allow_from.is_empty());
}

#[tokio::test]
async fn test_add_channel_rejects_duplicate() {
    let (_dir, state_dir) = temp_state_dir();

    add_channel(&state_dir, "12345", None, None).await;
    let result = add_channel(&state_dir, "12345", None, None).await;
    assert!(result.get("error").is_some());

    let config = load_config(&state_dir);
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

    let config = load_config(&state_dir);
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

    let list = list_config_channels(&state_dir);
    let channels = list["channels"].as_array().unwrap();
    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0]["id"], "222");

    let config = load_config(&state_dir);
    assert_eq!(config.raw.channels.len(), 1);
}

#[tokio::test]
async fn test_update_channel_roundtrip() {
    let (_dir, state_dir) = temp_state_dir();

    add_channel(&state_dir, "12345", Some(true), None).await;
    let result = update_channel(&state_dir, "12345", Some(false), Some(vec!["999".into()])).await;
    assert_eq!(result["ok"], true);

    let config = load_config(&state_dir);
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
    let config = load_config(&state_dir);
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

    let config = load_config(&state_dir);
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

    let config = load_config(&state_dir);
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

    let config = load_config(&state_dir);
    assert_eq!(config.access.dm_policy, DmPolicy::Drop);
}

#[tokio::test]
async fn test_update_dm_policy_disabled() {
    let (_dir, state_dir) = temp_state_dir();

    let result = update_dm_policy(&state_dir, "disabled").await;
    assert_eq!(result["ok"], true);

    let config = load_config(&state_dir);
    assert_eq!(config.access.dm_policy, DmPolicy::Disabled);
}

#[tokio::test]
async fn test_update_dm_policy_queue() {
    let (_dir, state_dir) = temp_state_dir();

    update_dm_policy(&state_dir, "drop").await;
    let result = update_dm_policy(&state_dir, "queue").await;
    assert_eq!(result["ok"], true);

    let config = load_config(&state_dir);
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

    let result = get_access_config(&state_dir);
    assert_eq!(result["dm_policy"], "drop");
    let allow_from = result["allow_from"].as_array().unwrap();
    assert!(allow_from.iter().any(|v| v.as_str() == Some("12345")));
}

// ── Empty state dir ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_operations_on_empty_state_dir() {
    let (_dir, state_dir) = temp_state_dir();

    let list = list_config_channels(&state_dir);
    assert!(list["channels"].as_array().unwrap().is_empty());

    let access = get_access_config(&state_dir);
    assert_eq!(access["dm_policy"], "queue");
}
