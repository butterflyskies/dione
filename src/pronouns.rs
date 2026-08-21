use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;

/// Documented PronounDB v2 English pronoun sets.
/// Source: <https://pronoundb.org/wiki/api-docs>
fn pronoundb_set_to_display(set: &str) -> Option<&'static str> {
    match set {
        "he" => Some("he/him"),
        "it" => Some("it/its"),
        "she" => Some("she/her"),
        "they" => Some("they/them"),
        "any" => Some("any pronouns"),
        "other" => Some("other pronouns"),
        "ask" => Some("ask me my pronouns"),
        "avoid" => Some("avoid pronouns, use my name"),
        _ => None,
    }
}

/// Converts a PronounDB v2 `sets.en` array into a display string.
/// Multiple sets are joined with " or " (e.g. "she/her or they/them").
fn sets_to_display(sets: &[String]) -> Option<String> {
    let parts: Vec<&str> = sets
        .iter()
        .filter_map(|s| pronoundb_set_to_display(s))
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(parts.join(" or "))
}

struct CacheEntry {
    pronouns: Option<String>,
    fetched_at: Instant,
}

/// Thread-safe pronoun cache with TTL-based expiry.
pub struct PronounCache {
    entries: RwLock<HashMap<u64, CacheEntry>>,
    ttl: Duration,
}

impl PronounCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    pub async fn get(&self, user_id: u64) -> Option<Option<String>> {
        let entries = self.entries.read().await;
        let entry = entries.get(&user_id)?;
        if entry.fetched_at.elapsed() < self.ttl {
            Some(entry.pronouns.clone())
        } else {
            None
        }
    }

    pub async fn insert(&self, user_id: u64, pronouns: Option<String>) {
        let mut entries = self.entries.write().await;
        entries.insert(
            user_id,
            CacheEntry {
                pronouns,
                fetched_at: Instant::now(),
            },
        );
    }
}

/// PronounDB v2 API adapter.
pub struct PronounProvider {
    client: reqwest::Client,
    cache: PronounCache,
    deadline: Duration,
}

impl PronounProvider {
    pub fn new(deadline_ms: u64, cache_ttl: Duration) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("dione/0.24.0 (https://github.com/butterflyskies/dione)")
            .timeout(Duration::from_millis(deadline_ms))
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            cache: PronounCache::new(cache_ttl),
            deadline: Duration::from_millis(deadline_ms),
        }
    }

    /// Look up pronouns for a single Discord user ID.
    /// Returns `None` if not found, not configured, provider error, or timeout.
    pub async fn lookup(&self, user_id: u64) -> Option<String> {
        if let Some(cached) = self.cache.get(user_id).await {
            return cached;
        }

        let result = self.fetch_from_api(user_id).await;
        self.cache.insert(user_id, result.clone()).await;
        result
    }

    async fn fetch_from_api(&self, user_id: u64) -> Option<String> {
        let url = format!(
            "https://pronoundb.org/api/v2/lookup?platform=discord&ids={}",
            user_id
        );

        let resp = match tokio::time::timeout(self.deadline, self.client.get(&url).send()).await {
            Ok(Ok(resp)) if resp.status().is_success() => resp,
            Ok(Ok(resp)) => {
                tracing::debug!(
                    user_id,
                    status = %resp.status(),
                    "pronoundb lookup returned non-success status"
                );
                return None;
            }
            Ok(Err(e)) => {
                tracing::debug!(user_id, error = %e, "pronoundb lookup failed");
                return None;
            }
            Err(_) => {
                tracing::debug!(user_id, "pronoundb lookup timed out");
                return None;
            }
        };

        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(user_id, error = %e, "pronoundb response parse failed");
                return None;
            }
        };

        let user_id_str = user_id.to_string();
        let sets = body.get(&user_id_str)?.get("sets")?.get("en")?.as_array()?;

        let string_sets: Vec<String> = sets
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        sets_to_display(&string_sets)
    }
}

/// Top-level pronoun service that combines config + provider.
pub struct PronounService {
    provider: PronounProvider,
    excluded_users: std::collections::HashSet<u64>,
}

impl PronounService {
    pub fn new(
        excluded_users: std::collections::HashSet<u64>,
        deadline_ms: u64,
        cache_ttl: Duration,
    ) -> Self {
        Self {
            provider: PronounProvider::new(deadline_ms, cache_ttl),
            excluded_users,
        }
    }

    /// Returns the display name with pronouns appended, if available.
    /// Queries PronounDB for all users except those in the exclude list.
    pub async fn resolve_display_name(&self, user_id: u64, display_name: &str) -> String {
        if self.excluded_users.contains(&user_id) {
            return display_name.to_string();
        }

        match self.provider.lookup(user_id).await {
            Some(pronouns) => format!("{display_name} ({pronouns})"),
            None => display_name.to_string(),
        }
    }

    pub fn is_excluded(&self, user_id: u64) -> bool {
        self.excluded_users.contains(&user_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pronoundb_set_mapping() {
        assert_eq!(pronoundb_set_to_display("he"), Some("he/him"));
        assert_eq!(pronoundb_set_to_display("she"), Some("she/her"));
        assert_eq!(pronoundb_set_to_display("they"), Some("they/them"));
        assert_eq!(pronoundb_set_to_display("it"), Some("it/its"));
        assert_eq!(pronoundb_set_to_display("any"), Some("any pronouns"));
        assert_eq!(pronoundb_set_to_display("other"), Some("other pronouns"));
        assert_eq!(pronoundb_set_to_display("ask"), Some("ask me my pronouns"));
        assert_eq!(
            pronoundb_set_to_display("avoid"),
            Some("avoid pronouns, use my name")
        );
        assert_eq!(pronoundb_set_to_display("unknown_set"), None);
    }

    #[test]
    fn test_sets_to_display_single() {
        let sets = vec!["she".to_string()];
        assert_eq!(sets_to_display(&sets), Some("she/her".to_string()));
    }

    #[test]
    fn test_sets_to_display_multiple() {
        let sets = vec!["she".to_string(), "they".to_string()];
        assert_eq!(
            sets_to_display(&sets),
            Some("she/her or they/them".to_string())
        );
    }

    #[test]
    fn test_sets_to_display_empty() {
        let sets: Vec<String> = vec![];
        assert_eq!(sets_to_display(&sets), None);
    }

    #[test]
    fn test_sets_to_display_unknown_only() {
        let sets = vec!["xyzzy".to_string()];
        assert_eq!(sets_to_display(&sets), None);
    }

    #[test]
    fn test_sets_to_display_mixed_known_unknown() {
        let sets = vec!["they".to_string(), "xyzzy".to_string()];
        assert_eq!(sets_to_display(&sets), Some("they/them".to_string()));
    }

    #[tokio::test]
    async fn test_cache_miss_returns_none() {
        let cache = PronounCache::new(Duration::from_secs(300));
        assert!(cache.get(12345).await.is_none());
    }

    #[tokio::test]
    async fn test_cache_hit_returns_stored_value() {
        let cache = PronounCache::new(Duration::from_secs(300));
        cache.insert(12345, Some("she/her".to_string())).await;
        let result = cache.get(12345).await;
        assert_eq!(result, Some(Some("she/her".to_string())));
    }

    #[tokio::test]
    async fn test_cache_stores_none_for_missing_pronouns() {
        let cache = PronounCache::new(Duration::from_secs(300));
        cache.insert(12345, None).await;
        let result = cache.get(12345).await;
        assert_eq!(result, Some(None));
    }

    #[tokio::test]
    async fn test_service_skips_excluded_users() {
        let mut excluded = std::collections::HashSet::new();
        excluded.insert(12345u64);
        let service = PronounService::new(excluded, 500, Duration::from_secs(300));
        let result = service.resolve_display_name(12345, "TestUser").await;
        assert_eq!(result, "TestUser");
    }

    #[tokio::test]
    async fn test_service_non_excluded_but_provider_fails() {
        let service = PronounService::new(
            std::collections::HashSet::new(),
            100,
            Duration::from_secs(300),
        );
        let result = service.resolve_display_name(12345, "TestUser").await;
        assert_eq!(result, "TestUser");
    }
}
