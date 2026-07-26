//! Cross-turn mask capsule injection.
//!
//! Queries a masks-mcp SQLite database to check if the configured principal
//! has an active mask. If so, returns formatted capsule text for injection
//! into notification metadata.
//!
//! Design: fail-open. If the database is unavailable, the query fails, or
//! no mask is active, returns `None` and delivery proceeds unmodified.

use crate::config::MasksConfig;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Cached mask state with TTL to avoid per-message SQLite queries.
struct CachedState {
    capsule: Option<String>,
    checked_at: Instant,
}

/// Owns the mask-checking lifecycle: config, cache, and catalog.
pub struct MaskChecker {
    config: MasksConfig,
    cache: Mutex<Option<CachedState>>,
    catalog: Option<HashMap<String, String>>,
}

/// How long to cache a mask check result before re-querying.
const CACHE_TTL: Duration = Duration::from_secs(30);

impl MaskChecker {
    /// Create a new checker from config. Loads the catalog eagerly if configured.
    pub fn new(config: MasksConfig) -> Self {
        let catalog = config
            .catalog_path
            .as_deref()
            .and_then(|path| load_catalog(path));
        Self {
            config,
            cache: Mutex::new(None),
            catalog,
        }
    }

    /// Returns `true` if mask checking is enabled and properly configured.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
            && self.config.database_path.is_some()
            && self.config.principal.is_some()
    }

    /// Returns the mask capsule text if an active mask exists, or `None`.
    ///
    /// Uses a TTL cache to avoid hitting SQLite on every notification.
    /// Fails open: any error returns `None`.
    pub fn get_capsule(&self) -> Option<String> {
        if !self.is_enabled() {
            return None;
        }

        // Check cache
        {
            let cache = self.cache.lock().ok()?;
            if let Some(ref state) = *cache {
                if state.checked_at.elapsed() < CACHE_TTL {
                    return state.capsule.clone();
                }
            }
        }

        // Cache miss or expired — query the database
        let capsule = self.query_active_mask();

        // Update cache
        if let Ok(mut cache) = self.cache.lock() {
            *cache = Some(CachedState {
                capsule: capsule.clone(),
                checked_at: Instant::now(),
            });
        }

        capsule
    }

    /// Query the SQLite database for an active, non-expired mask.
    fn query_active_mask(&self) -> Option<String> {
        let db_path = self.config.database_path.as_deref()?;
        let principal = self.config.principal.as_deref()?;

        if !Path::new(db_path).exists() {
            tracing::debug!(path = db_path, "masks database not found, skipping");
            return None;
        }

        let conn = Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .ok()?;

        // Query for active mask that hasn't expired.
        // We use > instead of >= to match masks-mcp's lazy expiry semantics:
        // a mask whose expires_at equals now is considered expired.
        let now = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.6fZ")
            .to_string();

        let result: Option<(String, String, String)> = conn
            .query_row(
                "SELECT mask_name, scope, expires_at FROM active_masks \
                 WHERE principal = ?1 AND expires_at > ?2",
                rusqlite::params![principal, now],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();

        let (mask_name, scope, expires_at) = result?;

        // Look up description from catalog if available.
        let description = self
            .catalog
            .as_ref()
            .and_then(|cat| cat.get(&mask_name.to_lowercase()))
            .cloned();

        Some(format_capsule(
            &mask_name,
            &scope,
            &expires_at,
            description.as_deref(),
        ))
    }
}

/// Format the mask capsule text for injection.
///
/// Uses McLeod's template: agency framing + mask identity + description.
fn format_capsule(name: &str, scope: &str, expires_at: &str, description: Option<&str>) -> String {
    let mut capsule = format!(
        "I am wearing a mask. A mask is an ephemeral emotion I chose to put on. \
         This mask is called {name}."
    );

    if let Some(desc) = description {
        capsule.push(' ');
        capsule.push_str(desc);
    }

    capsule.push_str(&format!(" [scope: {scope}, expires: {expires_at}]"));
    capsule
}

/// Load the masks catalog TOML and extract name -> description mappings.
///
/// Returns None on any error (file missing, parse error, etc.).
fn load_catalog(path: &str) -> Option<HashMap<String, String>> {
    let content = std::fs::read_to_string(path).ok()?;
    let table: toml::Value = content.parse().ok()?;

    let masks = table.get("masks")?.as_array()?;
    let mut map = HashMap::new();

    for mask in masks {
        let name = mask.get("name")?.as_str()?;
        let description = mask.get("description")?.as_str()?;
        map.insert(name.to_lowercase(), description.to_owned());
    }

    Some(map)
}

/// Inject the mask capsule into a notification Value (mutates in place).
///
/// Adds `mask_capsule` to `params.meta`. If `params.meta` doesn't exist
/// or isn't an object, this is a no-op (fail-open).
pub fn inject_capsule(notification: &mut serde_json::Value, capsule: &str) {
    if let Some(params) = notification.get_mut("params") {
        if let Some(meta) = params.get_mut("meta") {
            if let Some(obj) = meta.as_object_mut() {
                obj.insert(
                    "mask_capsule".to_owned(),
                    serde_json::Value::String(capsule.to_owned()),
                );
            }
        } else {
            // No meta field — add one with just the capsule.
            if let Some(params_obj) = params.as_object_mut() {
                params_obj.insert(
                    "meta".to_owned(),
                    serde_json::json!({ "mask_capsule": capsule }),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_capsule_with_description() {
        let result = format_capsule(
            "Melancholy",
            "session",
            "2026-07-25T13:00:00.000000Z",
            Some("A gentle, reflective sadness that colors perception without overwhelming."),
        );
        assert!(result.starts_with("I am wearing a mask."));
        assert!(result.contains("This mask is called Melancholy."));
        assert!(result.contains("A gentle, reflective sadness"));
        assert!(result.contains("[scope: session, expires: 2026-07-25T13:00:00.000000Z]"));
    }

    #[test]
    fn format_capsule_without_description() {
        let result = format_capsule(
            "Melancholy",
            "session",
            "2026-07-25T13:00:00.000000Z",
            None,
        );
        assert!(result.starts_with("I am wearing a mask."));
        assert!(result.contains("This mask is called Melancholy."));
        assert!(result.contains("[scope: session, expires: 2026-07-25T13:00:00.000000Z]"));
    }

    #[test]
    fn inject_capsule_adds_to_meta() {
        let mut notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel",
            "params": {
                "content": "hello",
                "meta": {
                    "chat_id": "123",
                    "user": "alice",
                }
            }
        });
        inject_capsule(
            &mut notification,
            "I am wearing a mask. This mask is called Test.",
        );
        assert_eq!(
            notification["params"]["meta"]["mask_capsule"],
            "I am wearing a mask. This mask is called Test."
        );
        // Original fields preserved
        assert_eq!(notification["params"]["meta"]["chat_id"], "123");
        assert_eq!(notification["params"]["meta"]["user"], "alice");
    }

    #[test]
    fn inject_capsule_creates_meta_when_absent() {
        let mut notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel",
            "params": {
                "content": "hello",
            }
        });
        inject_capsule(
            &mut notification,
            "I am wearing a mask. This mask is called Test.",
        );
        assert_eq!(
            notification["params"]["meta"]["mask_capsule"],
            "I am wearing a mask. This mask is called Test."
        );
    }

    #[test]
    fn inject_capsule_noop_without_params() {
        let mut notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel",
        });
        let original = notification.clone();
        inject_capsule(&mut notification, "capsule text");
        assert_eq!(notification, original);
    }

    #[test]
    fn checker_disabled_returns_none() {
        let checker = MaskChecker::new(MasksConfig::default());
        assert!(!checker.is_enabled());
        assert!(checker.get_capsule().is_none());
    }

    #[test]
    fn checker_missing_db_returns_none() {
        let checker = MaskChecker::new(MasksConfig {
            enabled: true,
            database_path: Some("/nonexistent/masks.sqlite3".to_owned()),
            principal: Some("ariadne".to_owned()),
            catalog_path: None,
        });
        assert!(checker.is_enabled());
        assert!(checker.get_capsule().is_none());
    }
}
