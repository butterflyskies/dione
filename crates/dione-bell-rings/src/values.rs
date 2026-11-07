use serde::{Deserialize, Deserializer, de::Error as _};
use thiserror::Error;

/// Validated recall limits. Construction rejects invalid values before evaluation.
///
/// ```
/// use dione_bell_rings::RecallOptions;
/// let options = RecallOptions::new(0.3, 3, 300)?;
/// assert_eq!(options.max_bells(), 3);
/// # Ok::<(), dione_bell_rings::RecallOptionsError>(())
/// ```
///
/// Limits cannot be replaced with unchecked scalar values.
///
/// ```compile_fail
/// use dione_bell_rings::RecallOptions;
/// let _ = RecallOptions { max_semantic_distance: f64::NAN, max_bells: 0, deadline_ms: 0 };
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct RecallOptions {
    max_semantic_distance: f64,
    max_bells: usize,
    deadline_ms: u64,
}

impl RecallOptions {
    /// Distance must be finite in `0..=2`, count in `1..=100`, deadline in `1..=2000` ms.
    pub fn new(
        max_semantic_distance: f64,
        max_bells: usize,
        deadline_ms: u64,
    ) -> Result<Self, RecallOptionsError> {
        let kind = if !max_semantic_distance.is_finite()
            || !(0.0..=2.0).contains(&max_semantic_distance)
        {
            Some(RecallOptionsErrorKind::MaxSemanticDistance)
        } else if !(1..=100).contains(&max_bells) {
            Some(RecallOptionsErrorKind::MaxBells)
        } else if !(1..=2000).contains(&deadline_ms) {
            Some(RecallOptionsErrorKind::Deadline)
        } else {
            None
        };
        if let Some(kind) = kind {
            return Err(RecallOptionsError { kind });
        }
        Ok(Self {
            max_semantic_distance,
            max_bells,
            deadline_ms,
        })
    }
    /// Returns the largest admitted semantic distance.
    pub fn max_semantic_distance(&self) -> f64 {
        self.max_semantic_distance
    }
    /// Returns the request and retained-result limit.
    pub fn max_bells(&self) -> usize {
        self.max_bells
    }
    /// Returns the timeout for each provider recall in milliseconds.
    pub fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }
}

impl Default for RecallOptions {
    fn default() -> Self {
        Self {
            max_semantic_distance: 0.3,
            max_bells: 3,
            deadline_ms: 300,
        }
    }
}

/// Invalid recall limits, classified by the first invalid setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("{kind}")]
pub struct RecallOptionsError {
    kind: RecallOptionsErrorKind,
}
impl RecallOptionsError {
    /// Returns the structured validation failure.
    pub fn kind(&self) -> RecallOptionsErrorKind {
        self.kind
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
/// The first invalid recall option.
pub enum RecallOptionsErrorKind {
    /// The semantic distance is nonfinite or outside zero through two.
    #[error("max_semantic_distance must be finite and between 0 and 2")]
    MaxSemanticDistance,
    /// The candidate limit is outside one through one hundred.
    #[error("max_bells must be between 1 and 100")]
    MaxBells,
    /// The recall timeout is outside one through two thousand milliseconds.
    #[error("deadline_ms must be between 1 and 2000")]
    Deadline,
}

/// A provider endpoint or scope failed validation. Display preserves config diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("{kind}")]
pub struct ProviderValueError {
    kind: ProviderValueErrorKind,
}
impl ProviderValueError {
    /// Returns the structured validation failure.
    pub fn kind(&self) -> ProviderValueErrorKind {
        self.kind
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
/// The provider value that failed validation.
pub enum ProviderValueErrorKind {
    /// The endpoint cannot be parsed as a URL.
    #[error("bell_rings provider url must be a valid HTTP(S) URL")]
    InvalidUrl,
    /// The endpoint uses an unsupported scheme or embedded credentials.
    #[error("bell_rings provider url must be HTTP(S) without embedded credentials")]
    UrlSchemeOrCredentials,
    /// The scope is empty or the reserved cross-scope value.
    #[error("bell_rings provider scope must be explicit and cannot be `all`")]
    InvalidScope,
}

/// One memory-mcp endpoint and one explicit allowed scope.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BellProviderConfig {
    /// Streamable HTTP MCP endpoint.
    pub url: BellProviderUrl,
    /// Explicit recall scope. `all` and empty scopes are rejected while parsing.
    pub scope: BellScope,
    /// Optional provenance alias. Defaults to the scope value.
    #[serde(default)]
    pub alias: Option<String>,
}

impl BellProviderConfig {
    /// Returns the supplied alias or the configured scope.
    pub fn alias(&self) -> &str {
        self.alias.as_deref().unwrap_or(self.scope.as_str())
    }
}

/// A validated HTTP(S) memory-mcp endpoint without embedded credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BellProviderUrl(String);

impl BellProviderUrl {
    /// Parses and validates the supplied value.
    pub fn parse(value: impl Into<String>) -> Result<Self, ProviderValueError> {
        let value = value.into();
        let parsed = value
            .parse::<reqwest::Url>()
            .map_err(|_| ProviderValueError {
                kind: ProviderValueErrorKind::InvalidUrl,
            })?;
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(ProviderValueError {
                kind: ProviderValueErrorKind::UrlSchemeOrCredentials,
            });
        }
        Ok(Self(parsed.to_string()))
    }

    // Diagnostics retain the provider location, never query credentials or fragments.
    pub(crate) fn diagnostic_endpoint(&self) -> &str {
        self.0.split(['?', '#']).next().unwrap_or(&self.0)
    }

    /// Borrows the validated value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for BellProviderUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// A validated memory scope which cannot represent the cross-scope `all` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BellScope(String);

impl BellScope {
    /// Parses and validates the supplied value.
    pub fn parse(value: impl Into<String>) -> Result<Self, ProviderValueError> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("all") {
            Err(ProviderValueError {
                kind: ProviderValueErrorKind::InvalidScope,
            })
        } else {
            Ok(Self(trimmed.to_owned()))
        }
    }

    /// Borrows the validated value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for BellScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_options_accept_boundaries_and_default() {
        for (distance, count, deadline) in [(0.0, 1, 1), (2.0, 100, 2000), (0.3, 3, 300)] {
            let options = RecallOptions::new(distance, count, deadline).unwrap();
            assert_eq!(options.max_semantic_distance(), distance);
            assert_eq!(options.max_bells(), count);
            assert_eq!(options.deadline_ms(), deadline);
        }
        assert_eq!(
            RecallOptions::default(),
            RecallOptions::new(0.3, 3, 300).unwrap()
        );
    }

    #[test]
    fn recall_options_reject_invalid_programmatic_values() {
        for distance in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.1, 2.1] {
            let error = RecallOptions::new(distance, 3, 300).unwrap_err();
            assert_eq!(error.kind(), RecallOptionsErrorKind::MaxSemanticDistance);
            assert_eq!(
                error.to_string(),
                "max_semantic_distance must be finite and between 0 and 2"
            );
        }
        for count in [0, 101] {
            let error = RecallOptions::new(0.3, count, 300).unwrap_err();
            assert_eq!(error.kind(), RecallOptionsErrorKind::MaxBells);
            assert_eq!(error.to_string(), "max_bells must be between 1 and 100");
        }
        for deadline in [0, 2001] {
            let error = RecallOptions::new(0.3, 3, deadline).unwrap_err();
            assert_eq!(error.kind(), RecallOptionsErrorKind::Deadline);
            assert_eq!(error.to_string(), "deadline_ms must be between 1 and 2000");
        }
        assert_eq!(
            RecallOptions::new(f64::NAN, 0, 0).unwrap_err().kind(),
            RecallOptionsErrorKind::MaxSemanticDistance
        );
        assert_eq!(
            RecallOptions::new(0.3, 0, 0).unwrap_err().kind(),
            RecallOptionsErrorKind::MaxBells
        );
    }

    #[test]
    fn provider_url_constructor_and_deserialize_share_validation() {
        for valid in ["http://memory.example/mcp", "https://MEMORY.example/mcp"] {
            let parsed = BellProviderUrl::parse(valid).unwrap();
            let decoded: BellProviderUrl =
                serde_json::from_value(serde_json::json!(valid)).unwrap();
            assert_eq!(parsed, decoded);
            assert!(parsed.as_str().starts_with("http"));
        }
        for (invalid, kind, message) in [
            (
                "not a url",
                ProviderValueErrorKind::InvalidUrl,
                "bell_rings provider url must be a valid HTTP(S) URL",
            ),
            (
                "file:///tmp/memory",
                ProviderValueErrorKind::UrlSchemeOrCredentials,
                "bell_rings provider url must be HTTP(S) without embedded credentials",
            ),
            (
                "https://user:pass@memory/mcp",
                ProviderValueErrorKind::UrlSchemeOrCredentials,
                "bell_rings provider url must be HTTP(S) without embedded credentials",
            ),
            (
                "https://user@memory/mcp",
                ProviderValueErrorKind::UrlSchemeOrCredentials,
                "bell_rings provider url must be HTTP(S) without embedded credentials",
            ),
        ] {
            let error = BellProviderUrl::parse(invalid).unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(error.to_string(), message);
            assert!(serde_json::from_value::<BellProviderUrl>(serde_json::json!(invalid)).is_err());
        }
        assert_eq!(
            BellProviderUrl::parse("https://MEMORY.example:443/mcp")
                .unwrap()
                .as_str(),
            "https://memory.example/mcp"
        );
    }

    #[test]
    fn scope_constructor_and_deserialize_share_validation() {
        for valid in ["syne", " global ", " All-project "] {
            let parsed = BellScope::parse(valid).unwrap();
            let decoded: BellScope = serde_json::from_value(serde_json::json!(valid)).unwrap();
            assert_eq!(parsed, decoded);
            assert_eq!(parsed.as_str(), valid.trim());
        }
        for invalid in ["", "   ", "all", " All ", "ALL"] {
            let error = BellScope::parse(invalid).unwrap_err();
            assert_eq!(error.kind(), ProviderValueErrorKind::InvalidScope);
            assert_eq!(
                error.to_string(),
                "bell_rings provider scope must be explicit and cannot be `all`"
            );
            assert!(serde_json::from_value::<BellScope>(serde_json::json!(invalid)).is_err());
        }
    }

    #[test]
    fn provider_config_deserialize_retains_alias_default_and_rejects_unknown_fields() {
        let provider: BellProviderConfig = serde_json::from_value(
            serde_json::json!({"url": "http://memory/mcp", "scope": "syne"}),
        )
        .unwrap();
        assert_eq!(provider.alias(), "syne");
        assert!(
            serde_json::from_value::<BellProviderConfig>(
                serde_json::json!({"url": "http://memory/mcp", "scope": "syne", "unknown": true})
            )
            .is_err()
        );
    }
}
