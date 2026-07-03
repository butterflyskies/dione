use chrono::{DateTime, FixedOffset, Timelike, Utc};
use chrono_tz::Tz;

// ── Compact batch timestamp ─────────────────────────────────────────────────

/// Format a [`Timestamp`] as compact `HH:MM` (or `HH:MM:SS` when seconds
/// are non-zero), converted to `tz` (or UTC when `None`).
///
/// Used by both `batch.rs` and `coalesce.rs` for wire-format timestamps.
pub(crate) fn format_compact(ts: &Timestamp, tz: Option<Tz>) -> String {
    let (h, m, s) = match tz {
        Some(tz) => {
            let local = ts.0.with_timezone(&tz);
            (local.hour(), local.minute(), local.second())
        }
        None => {
            let utc = ts.0.with_timezone(&Utc);
            (utc.hour(), utc.minute(), utc.second())
        }
    };

    if s == 0 {
        format!("{h:02}:{m:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

// ── Timestamp newtype ───────────────────────────────────────────────────────

/// A parsed timestamp with a fixed UTC offset.
///
/// Wraps a `chrono::DateTime<FixedOffset>` — parsing happens at the boundary
/// (when Discord events arrive) so downstream code works with a typed value
/// instead of re-parsing strings.
///
/// Construct via [`crate::config::LoadedConfig::localize_utc`],
/// [`crate::config::LoadedConfig::localize_rfc3339`], or [`Timestamp::parse`].
///
/// Serializes as an RFC 3339 string, so `json!({"timestamp": ts})` just works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timestamp(pub(crate) DateTime<FixedOffset>);

impl Timestamp {
    /// Parse an RFC 3339 string into a `Timestamp`.
    pub fn parse(s: &str) -> Result<Self, chrono::ParseError> {
        s.parse::<DateTime<FixedOffset>>().map(Timestamp)
    }

    /// Returns the inner `DateTime<FixedOffset>`.
    pub fn as_datetime(&self) -> &DateTime<FixedOffset> {
        &self.0
    }

    /// Returns the RFC 3339 string representation.
    pub fn to_rfc3339(&self) -> String {
        self.0.to_rfc3339()
    }
}

impl std::fmt::Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.to_rfc3339())
    }
}

impl serde::Serialize for Timestamp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_rfc3339())
    }
}

impl From<DateTime<FixedOffset>> for Timestamp {
    fn from(dt: DateTime<FixedOffset>) -> Self {
        Timestamp(dt)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
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
        let formatted = ts.to_rfc3339();
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
        let formatted = ts.to_rfc3339();
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
        let localized = ts.to_rfc3339();
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
            ts.to_rfc3339(),
            input,
            "no timezone configured means passthrough"
        );
    }

    #[test]
    fn test_localize_rfc3339_invalid_input_falls_back_to_utc_now() {
        let raw = Config {
            timezone: Some("America/New_York".to_string()),
            ..Default::default()
        };
        let cfg = LoadedConfig::from_raw(raw);
        let ts = cfg.localize_rfc3339("not-a-timestamp");
        // Invalid input falls back to current UTC — verify it has a reasonable
        // offset (New York is either -05:00 or -04:00).
        let formatted = ts.to_rfc3339();
        assert!(
            formatted.contains("-05:00") || formatted.contains("-04:00"),
            "invalid input should fall back to localized current time, got: {formatted}"
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
    fn test_timestamp_serializes_as_string() {
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
            "Timestamp must serialize as a JSON string"
        );
        assert!(
            json_val["timestamp"].as_str().unwrap().contains("-08:00"),
            "serialized value should contain the local offset"
        );
    }

    #[test]
    fn test_timestamp_display() {
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
        let formatted_before = ts_before.to_rfc3339();
        assert!(
            formatted_before.contains("-05:00"),
            "before DST should be EST (-05:00), got: {formatted_before}"
        );

        // 2026-03-08 08:00 UTC = 04:00 EDT (after spring-forward)
        let after = chrono::DateTime::parse_from_rfc3339("2026-03-08T08:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let ts_after = cfg.localize_utc(&after);
        let formatted_after = ts_after.to_rfc3339();
        assert!(
            formatted_after.contains("-04:00"),
            "after DST should be EDT (-04:00), got: {formatted_after}"
        );
    }

    #[test]
    fn test_timestamp_parse_valid() {
        let ts = Timestamp::parse("2026-06-19T15:30:00+00:00").expect("valid RFC3339");
        assert_eq!(ts.to_rfc3339(), "2026-06-19T15:30:00+00:00");
    }

    #[test]
    fn test_timestamp_parse_invalid() {
        assert!(Timestamp::parse("not-a-timestamp").is_err());
    }

    #[test]
    fn test_format_compact_utc() {
        let ts = Timestamp::parse("2026-06-19T15:30:00+00:00").unwrap();
        assert_eq!(format_compact(&ts, None), "15:30");
    }

    #[test]
    fn test_format_compact_with_seconds() {
        let ts = Timestamp::parse("2026-06-19T15:30:45+00:00").unwrap();
        assert_eq!(format_compact(&ts, None), "15:30:45");
    }

    #[test]
    fn test_format_compact_with_tz() {
        let ts = Timestamp::parse("2026-06-19T15:30:00+00:00").unwrap();
        let tz: Tz = "America/Los_Angeles".parse().unwrap();
        // June = PDT = UTC-7
        assert_eq!(format_compact(&ts, Some(tz)), "08:30");
    }
}
