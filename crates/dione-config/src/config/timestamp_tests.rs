use super::{Config, LoadedConfig};

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
