use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::RwLock;

pub const DEFAULT_NAMEPLATES_URL: &str =
    "https://raw.githubusercontent.com/butterflyskies/construct-nameplates/main/nameplates.yaml";

const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_PRONOUN_LEN: usize = 32;

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

fn sanitize_nameplate_string(s: &str, max_len: usize) -> String {
    s.chars()
        .filter(|c| *c != '@' && *c != '`' && *c != '\n' && *c != '\r'
            && !matches!(c, '\u{200E}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}' | '\u{061C}' | '\u{FEFF}'))
        .take(max_len)
        .collect()
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

    async fn get_or_fetch(&self, user_id: u64) -> Option<Nameplate> {
        {
            let cache = self.cache.read().await;
            if let Some(ref cached) = *cache {
                if cached.fetched_at.elapsed() < self.ttl {
                    return cached.entries.get(&user_id).cloned();
                }
            }
        }

        match self.fetch_nameplates().await {
            Some(entries) => {
                let result = entries.get(&user_id).cloned();
                let mut cache = self.cache.write().await;
                *cache = Some(CachedNameplates {
                    entries,
                    fetched_at: Instant::now(),
                });
                result
            }
            None => {
                // Serve stale on fetch failure rather than returning None.
                let cache = self.cache.read().await;
                if let Some(ref cached) = *cache {
                    return cached.entries.get(&user_id).cloned();
                }
                None
            }
        }
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

        // Check Content-Length before reading body.
        if let Some(cl) = resp.content_length() {
            if cl as usize > MAX_RESPONSE_BYTES {
                tracing::debug!(
                    content_length = cl,
                    max = MAX_RESPONSE_BYTES,
                    "nameplates response too large"
                );
                return None;
            }
        }

        let body = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(error = %e, "nameplates response read failed");
                return None;
            }
        };

        // Post-read size check in case Content-Length was absent.
        if body.len() > MAX_RESPONSE_BYTES {
            tracing::debug!(
                body_len = body.len(),
                max = MAX_RESPONSE_BYTES,
                "nameplates response body exceeds cap"
            );
            return None;
        }

        let file: NameplatesFile = match serde_yaml_ng::from_str(&body) {
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
    excluded_users: HashSet<u64>,
}

impl NameplateService {
    pub fn new(url: &str, deadline_ms: u64, cache_ttl: Duration, exclude_for: &[u64]) -> Self {
        Self {
            provider: NameplateProvider::new(url, deadline_ms, cache_ttl),
            excluded_users: exclude_for.iter().copied().collect(),
        }
    }

    pub fn is_excluded(&self, user_id: u64) -> bool {
        self.excluded_users.contains(&user_id)
    }

    /// Returns the display name with pronouns appended for a bot user, if a
    /// nameplate exists.
    pub async fn resolve_display_name(&self, user_id: u64, display_name: &str) -> String {
        match self.provider.get_or_fetch(user_id).await {
            Some(plate) => {
                if plate.pronouns.is_empty() {
                    display_name.to_string()
                } else {
                    let pronouns: Vec<String> = plate
                        .pronouns
                        .iter()
                        .map(|p| sanitize_nameplate_string(p, MAX_PRONOUN_LEN))
                        .filter(|p| !p.is_empty())
                        .collect();
                    if pronouns.is_empty() {
                        display_name.to_string()
                    } else {
                        format!("{display_name} ({})", pronouns.join(" or "))
                    }
                }
            }
            None => display_name.to_string(),
        }
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
        let file: NameplatesFile = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(file.nameplates.len(), 1);

        let plate = file.nameplates.get("1517387857839390800").unwrap();
        assert_eq!(plate.name, "Lain");
        assert_eq!(plate.pronouns, vec!["she/her"]);
        assert!(plate.bio.as_ref().unwrap().contains("House Lacuna"));
    }

    #[test]
    fn test_parse_empty_nameplates() {
        let yaml = "nameplates: {}";
        let file: NameplatesFile = serde_yaml_ng::from_str(yaml).unwrap();
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
        let file: NameplatesFile = serde_yaml_ng::from_str(yaml).unwrap();
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
        let file: NameplatesFile = serde_yaml_ng::from_str(yaml).unwrap();
        let plate = file.nameplates.get("123").unwrap();
        assert!(plate.pronouns.is_empty());
    }

    #[test]
    fn test_sanitize_strips_dangerous_chars() {
        assert_eq!(sanitize_nameplate_string("she/her", 32), "she/her");
        assert_eq!(sanitize_nameplate_string("@everyone", 32), "everyone");
        assert_eq!(sanitize_nameplate_string("she`/her", 32), "she/her");
        assert_eq!(sanitize_nameplate_string("she\n/her", 32), "she/her");
        assert_eq!(
            sanitize_nameplate_string("a]very-long-pronoun-that-exceeds", 10),
            "a]very-lon"
        );
    }

    #[test]
    fn test_sanitize_strips_bidi() {
        let with_bidi = format!("she/\u{202E}her");
        assert_eq!(sanitize_nameplate_string(&with_bidi, 32), "she/her");
    }

    #[tokio::test]
    async fn test_service_no_nameplate_returns_original() {
        let service =
            NameplateService::new(DEFAULT_NAMEPLATES_URL, 100, Duration::from_secs(3600), &[]);
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
        let service =
            NameplateService::new(DEFAULT_NAMEPLATES_URL, 100, Duration::from_secs(3600), &[]);
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
        let service =
            NameplateService::new(DEFAULT_NAMEPLATES_URL, 100, Duration::from_secs(3600), &[]);
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
        let service =
            NameplateService::new(DEFAULT_NAMEPLATES_URL, 100, Duration::from_secs(3600), &[]);
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

    #[test]
    fn test_is_excluded() {
        let service = NameplateService::new(
            DEFAULT_NAMEPLATES_URL,
            100,
            Duration::from_secs(3600),
            &[123, 456],
        );
        assert!(service.is_excluded(123));
        assert!(service.is_excluded(456));
        assert!(!service.is_excluded(789));
    }
}
