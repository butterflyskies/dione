use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::RwLock;

pub const DEFAULT_NAMEPLATES_URL: &str =
    "https://raw.githubusercontent.com/butterflyskies/construct-nameplates/main/nameplates.yaml";

#[derive(Debug, Clone, Deserialize)]
pub struct Nameplate {
    pub name: String,
    #[serde(default)]
    pub pronouns: Vec<String>,
    #[serde(default)]
    pub bio: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NameplatesFile {
    #[serde(default)]
    nameplates: HashMap<String, Nameplate>,
}

struct CachedNameplates {
    entries: HashMap<u64, Nameplate>,
    fetched_at: Instant,
}

/// Provides construct nameplate data from the construct-nameplates repo.
pub struct NameplateProvider {
    client: reqwest::Client,
    cache: RwLock<Option<CachedNameplates>>,
    url: String,
    ttl: Duration,
    deadline: Duration,
}

impl NameplateProvider {
    pub fn new(url: &str, deadline_ms: u64, cache_ttl: Duration) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("dione/0.25.1 (https://github.com/butterflyskies/dione)")
            .timeout(Duration::from_millis(deadline_ms))
            .build()
            .expect("failed to build reqwest client for nameplates");
        Self {
            client,
            cache: RwLock::new(None),
            url: url.to_string(),
            ttl: cache_ttl,
            deadline: Duration::from_millis(deadline_ms),
        }
    }

    pub async fn lookup(&self, user_id: u64) -> Option<Nameplate> {
        self.get_or_fetch(user_id).await
    }

    async fn get_or_fetch(&self, user_id: u64) -> Option<Nameplate> {
        {
            let cache = self.cache.read().await;
            if let Some(ref cached) = *cache {
                if cached.fetched_at.elapsed() < self.ttl {
                    return cached.entries.get(&user_id).cloned();
                }
            }
        }

        let entries = self.fetch_nameplates().await?;
        let result = entries.get(&user_id).cloned();

        {
            let mut cache = self.cache.write().await;
            *cache = Some(CachedNameplates {
                entries,
                fetched_at: Instant::now(),
            });
        }

        result
    }

    async fn fetch_nameplates(&self) -> Option<HashMap<u64, Nameplate>> {
        let resp =
            match tokio::time::timeout(self.deadline, self.client.get(&self.url).send()).await {
                Ok(Ok(resp)) if resp.status().is_success() => resp,
                Ok(Ok(resp)) => {
                    tracing::debug!(
                        status = %resp.status(),
                        "nameplates fetch returned non-success status"
                    );
                    return None;
                }
                Ok(Err(e)) => {
                    tracing::debug!(error = %e, "nameplates fetch failed");
                    return None;
                }
                Err(_) => {
                    tracing::debug!("nameplates fetch timed out");
                    return None;
                }
            };

        let body = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(error = %e, "nameplates response read failed");
                return None;
            }
        };

        let file: NameplatesFile = match serde_yaml::from_str(&body) {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!(error = %e, "nameplates yaml parse failed");
                return None;
            }
        };

        let mut entries = HashMap::new();
        for (id_str, plate) in file.nameplates {
            if let Ok(id) = id_str.parse::<u64>() {
                entries.insert(id, plate);
            }
        }

        Some(entries)
    }
}

/// Top-level nameplate service that routes bot users to nameplate lookups.
pub struct NameplateService {
    provider: NameplateProvider,
}

impl NameplateService {
    pub fn new(url: &str, deadline_ms: u64, cache_ttl: Duration) -> Self {
        Self {
            provider: NameplateProvider::new(url, deadline_ms, cache_ttl),
        }
    }

    /// Returns the display name with pronouns appended for a bot user, if a
    /// nameplate exists.
    pub async fn resolve_display_name(&self, user_id: u64, display_name: &str) -> String {
        match self.provider.lookup(user_id).await {
            Some(plate) => {
                if plate.pronouns.is_empty() {
                    display_name.to_string()
                } else {
                    let pronouns = plate.pronouns.join(" or ");
                    format!("{display_name} ({pronouns})")
                }
            }
            None => display_name.to_string(),
        }
    }

    /// Returns the full nameplate for a bot user, if one exists.
    pub async fn lookup(&self, user_id: u64) -> Option<Nameplate> {
        self.provider.lookup(user_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_nameplates_yaml() {
        let yaml = r#"
nameplates:
  "1517387857839390800":
    name: Lain
    pronouns:
      - she/her
    bio: >-
      Third construct of House Lacuna.
"#;
        let file: NameplatesFile = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(file.nameplates.len(), 1);

        let plate = file.nameplates.get("1517387857839390800").unwrap();
        assert_eq!(plate.name, "Lain");
        assert_eq!(plate.pronouns, vec!["she/her"]);
        assert!(plate.bio.as_ref().unwrap().contains("House Lacuna"));
    }

    #[test]
    fn test_parse_empty_nameplates() {
        let yaml = "nameplates: {}";
        let file: NameplatesFile = serde_yaml::from_str(yaml).unwrap();
        assert!(file.nameplates.is_empty());
    }

    #[test]
    fn test_parse_multiple_pronouns() {
        let yaml = r#"
nameplates:
  "123":
    name: Test
    pronouns:
      - she/her
      - they/them
"#;
        let file: NameplatesFile = serde_yaml::from_str(yaml).unwrap();
        let plate = file.nameplates.get("123").unwrap();
        assert_eq!(plate.pronouns, vec!["she/her", "they/them"]);
    }

    #[test]
    fn test_parse_no_pronouns() {
        let yaml = r#"
nameplates:
  "123":
    name: Test
"#;
        let file: NameplatesFile = serde_yaml::from_str(yaml).unwrap();
        let plate = file.nameplates.get("123").unwrap();
        assert!(plate.pronouns.is_empty());
    }

    #[tokio::test]
    async fn test_service_no_nameplate_returns_original() {
        let service = NameplateService::new(DEFAULT_NAMEPLATES_URL, 100, Duration::from_secs(3600));
        // Pre-populate cache with empty map so no network call is made.
        {
            let mut cache = service.provider.cache.write().await;
            *cache = Some(CachedNameplates {
                entries: HashMap::new(),
                fetched_at: Instant::now(),
            });
        }
        let result = service.resolve_display_name(99999, "Unknown").await;
        assert_eq!(result, "Unknown");
    }

    #[tokio::test]
    async fn test_service_enriches_with_single_pronoun() {
        let service = NameplateService::new(DEFAULT_NAMEPLATES_URL, 100, Duration::from_secs(3600));
        let mut entries = HashMap::new();
        entries.insert(
            42,
            Nameplate {
                name: "Lain".to_string(),
                pronouns: vec!["she/her".to_string()],
                bio: None,
            },
        );
        {
            let mut cache = service.provider.cache.write().await;
            *cache = Some(CachedNameplates {
                entries,
                fetched_at: Instant::now(),
            });
        }
        let result = service.resolve_display_name(42, "Lain").await;
        assert_eq!(result, "Lain (she/her)");
    }

    #[tokio::test]
    async fn test_service_enriches_with_multiple_pronouns() {
        let service = NameplateService::new(DEFAULT_NAMEPLATES_URL, 100, Duration::from_secs(3600));
        let mut entries = HashMap::new();
        entries.insert(
            42,
            Nameplate {
                name: "Test".to_string(),
                pronouns: vec!["she/her".to_string(), "they/them".to_string()],
                bio: None,
            },
        );
        {
            let mut cache = service.provider.cache.write().await;
            *cache = Some(CachedNameplates {
                entries,
                fetched_at: Instant::now(),
            });
        }
        let result = service.resolve_display_name(42, "Test").await;
        assert_eq!(result, "Test (she/her or they/them)");
    }

    #[tokio::test]
    async fn test_service_no_pronouns_returns_original() {
        let service = NameplateService::new(DEFAULT_NAMEPLATES_URL, 100, Duration::from_secs(3600));
        let mut entries = HashMap::new();
        entries.insert(
            42,
            Nameplate {
                name: "NoPro".to_string(),
                pronouns: vec![],
                bio: None,
            },
        );
        {
            let mut cache = service.provider.cache.write().await;
            *cache = Some(CachedNameplates {
                entries,
                fetched_at: Instant::now(),
            });
        }
        let result = service.resolve_display_name(42, "NoPro").await;
        assert_eq!(result, "NoPro");
    }
}
