use chrono::{DateTime, Timelike, Utc};
use chrono_tz::Tz;

// ── Compact batch timestamp ─────────────────────────────────────────────────

/// Format an RFC3339 timestamp as compact `HH:MM` (or `HH:MM:SS` when seconds
/// are non-zero), converted to `tz` (or UTC when `None`).
///
/// Used by both `batch.rs` and `coalesce.rs` for wire-format timestamps.
pub(crate) fn format_compact(raw: &str, tz: Option<Tz>) -> Result<String, chrono::ParseError> {
    let dt = raw.parse::<DateTime<chrono::FixedOffset>>()?;

    let (h, m, s) = match tz {
        Some(tz) => {
            let local = dt.with_timezone(&tz);
            (local.hour(), local.minute(), local.second())
        }
        None => {
            let utc = dt.with_timezone(&Utc);
            (utc.hour(), utc.minute(), utc.second())
        }
    };

    if s == 0 {
        Ok(format!("{h:02}:{m:02}"))
    } else {
        Ok(format!("{h:02}:{m:02}:{s:02}"))
    }
}

// ── LocalTimestamp newtype ───────────────────────────────────────────────────

/// An RFC3339 timestamp string that has already been converted to the
/// configured local timezone (or left as UTC when no timezone is set).
///
/// Construct via [`crate::config::LoadedConfig::localize_utc`] or
/// [`crate::config::LoadedConfig::localize_rfc3339`].
/// Serializes as a plain string, so `json!({"timestamp": local_ts})` just works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTimestamp(pub(crate) String);

impl LocalTimestamp {
    /// Returns the inner RFC3339 string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LocalTimestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for LocalTimestamp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl From<LocalTimestamp> for String {
    fn from(lt: LocalTimestamp) -> Self {
        lt.0
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::config::{Config, LoadedConfig};

    #[test]
    fn test_localize_utc_with_timezone() {
        let raw = Config {
            timezone: Some("America/Los_Angeles".to_string()),
            ..Default::default()
        };
        let cfg = LoadedConfig::from_raw(raw);
        assert!(cfg.tz.is_some(), "valid IANA timezone must parse");

        let utc = chrono::DateTime::parse_from_rfc3339("2026-01-15T20:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let ts = cfg.localize_utc(&utc);
        let formatted = ts.as_str();
        assert!(
            formatted.contains("-08:00"),
            "PST offset should be -08:00, got: {formatted}"
        );
        assert!(
            formatted.starts_with("2026-01-15T12:00:00"),
            "20:00 UTC should be 12:00 PST, got: {formatted}"
        );
    }

    #[test]
    fn test_localize_utc_without_timezone_is_utc() {
        let raw = Config::default();
        let cfg = LoadedConfig::from_raw(raw);
        assert!(cfg.tz.is_none(), "default config should have no timezone");

        let utc = chrono::DateTime::parse_from_rfc3339("2026-01-15T20:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let ts = cfg.localize_utc(&utc);
        let formatted = ts.as_str();
        assert!(
            formatted.contains("+00:00"),
            "UTC offset should be +00:00, got: {formatted}"
        );
    }

    #[test]
    fn test_localize_rfc3339_converts_to_local() {
        let raw = Config {
            timezone: Some("Europe/London".to_string()),
            ..Default::default()
        };
        let cfg = LoadedConfig::from_raw(raw);

        // Summer time (BST = UTC+1)
        let ts = cfg.localize_rfc3339("2026-07-15T12:00:00+00:00");
        let localized = ts.as_str();
        assert!(
            localized.contains("+01:00"),
            "BST offset should be +01:00, got: {localized}"
        );
        assert!(
            localized.starts_with("2026-07-15T13:00:00"),
            "12:00 UTC should be 13:00 BST, got: {localized}"
        );
    }

    #[test]
    fn test_localize_rfc3339_passthrough_without_timezone() {
        let cfg = LoadedConfig::from_raw(Config::default());
        let input = "2026-01-15T20:00:00+00:00";
        let ts = cfg.localize_rfc3339(input);
        assert_eq!(
            ts.as_str(),
            input,
            "no timezone configured means passthrough"
        );
    }

    #[test]
    fn test_localize_rfc3339_invalid_input_passthrough() {
        let raw = Config {
            timezone: Some("America/New_York".to_string()),
            ..Default::default()
        };
        let cfg = LoadedConfig::from_raw(raw);
        let ts = cfg.localize_rfc3339("not-a-timestamp");
        assert_eq!(
            ts.as_str(),
            "not-a-timestamp",
            "invalid input should pass through unchanged"
        );
    }

    #[test]
    fn test_invalid_timezone_falls_back_to_none() {
        let raw = Config {
            timezone: Some("Not/A/Timezone".to_string()),
            ..Default::default()
        };
        let cfg = LoadedConfig::from_raw(raw);
        assert!(
            cfg.tz.is_none(),
            "invalid IANA timezone should fall back to None"
        );
    }

    #[test]
    fn test_local_timestamp_serializes_as_string() {
        let raw = Config {
            timezone: Some("America/Los_Angeles".to_string()),
            ..Default::default()
        };
        let cfg = LoadedConfig::from_raw(raw);
        let utc = chrono::DateTime::parse_from_rfc3339("2026-01-15T20:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let ts = cfg.localize_utc(&utc);
        let json_val = serde_json::json!({"timestamp": ts});
        assert!(
            json_val["timestamp"].is_string(),
            "LocalTimestamp must serialize as a JSON string"
        );
        assert!(
            json_val["timestamp"].as_str().unwrap().contains("-08:00"),
            "serialized value should contain the local offset"
        );
    }

    #[test]
    fn test_local_timestamp_display() {
        let raw = Config {
            timezone: Some("Europe/London".to_string()),
            ..Default::default()
        };
        let cfg = LoadedConfig::from_raw(raw);
        let ts = cfg.localize_rfc3339("2026-07-15T12:00:00+00:00");
        let displayed = format!("{ts}");
        assert!(
            displayed.contains("+01:00"),
            "Display should show localized time"
        );
    }

    #[test]
    fn test_dst_transition_localize_utc() {
        // Test a timestamp right at a DST boundary: US spring-forward 2026-03-08 02:00 EST -> 03:00 EDT
        let raw = Config {
            timezone: Some("America/New_York".to_string()),
            ..Default::default()
        };
        let cfg = LoadedConfig::from_raw(raw);

        // 2026-03-08 06:30 UTC = 01:30 EST (before spring-forward)
        let before = chrono::DateTime::parse_from_rfc3339("2026-03-08T06:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let ts_before = cfg.localize_utc(&before);
        let formatted_before = ts_before.as_str();
        assert!(
            formatted_before.contains("-05:00"),
            "before DST should be EST (-05:00), got: {formatted_before}"
        );

        // 2026-03-08 08:00 UTC = 04:00 EDT (after spring-forward)
        let after = chrono::DateTime::parse_from_rfc3339("2026-03-08T08:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let ts_after = cfg.localize_utc(&after);
        let formatted_after = ts_after.as_str();
        assert!(
            formatted_after.contains("-04:00"),
            "after DST should be EDT (-04:00), got: {formatted_after}"
        );
    }
}
