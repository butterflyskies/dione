use chrono::{DateTime, FixedOffset, Timelike, Utc};
use chrono_tz::Tz;

// ── Compact batch timestamp ─────────────────────────────────────────────────

/// Format a [`Timestamp`] as compact `HH:MM` (or `HH:MM:SS` when seconds
/// are non-zero), converted to `tz` (or UTC when `None`).
///
/// Used by host presentation for compact wire timestamps.
pub fn format_compact(ts: &Timestamp, tz: Option<Tz>) -> String {
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
/// Construct with the existing `From<DateTime<FixedOffset>>` conversion
/// or parse text with [`Timestamp::parse`].
///
/// Serializes as an RFC 3339 string, so `json!({"timestamp": ts})` just works.
/// The tuple representation stays private; callers use parsing or the existing conversion.
///
/// ```compile_fail
/// use chrono::{DateTime, FixedOffset};
/// use dione_messages::timestamp::Timestamp;
///
/// fn forge(value: DateTime<FixedOffset>) -> Timestamp {
///     Timestamp(value)
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timestamp(DateTime<FixedOffset>);

impl Timestamp {
    /// Parse an RFC 3339 string into a `Timestamp`.
    pub fn parse(s: &str) -> Result<Self, chrono::ParseError> {
        s.parse::<DateTime<FixedOffset>>().map(Timestamp)
    }

    /// The current wall-clock time, in UTC.
    pub fn now() -> Self {
        Timestamp(Utc::now().fixed_offset())
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

impl<'de> serde::Deserialize<'de> for Timestamp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = Timestamp;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an RFC 3339 timestamp string")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Timestamp, E> {
                Timestamp::parse(v).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
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
