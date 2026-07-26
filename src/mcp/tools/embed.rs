//! Discord embed parsing: JSON value to serenity [`CreateEmbed`] builders.
//!
//! Embeds arrive as a JSON array on the MCP wire. Each element maps 1:1 to
//! Discord's [embed object](https://discord.com/developers/docs/resources/channel#embed-object),
//! and we translate it into serenity's builder types without inventing new
//! semantics.

use serenity::builder::{CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter};
use serenity::model::Timestamp;

use serde_json::Value;

/// Maximum number of embeds Discord allows per message.
const MAX_EMBEDS_PER_MESSAGE: usize = 10;

/// Extract all text surfaces from an embed JSON array for hook scanning.
///
/// Collects title, description, field names/values, footer text, and author
/// name — every text surface that appears in the channel with the bot's name
/// on it. URLs and non-text fields are excluded.
pub fn extract_text_surfaces(value: &Value) -> String {
    let Some(arr) = value.as_array() else {
        return String::new();
    };
    let mut surfaces = Vec::new();
    for obj in arr.iter().filter_map(Value::as_object) {
        if let Some(title) = obj.get("title").and_then(Value::as_str) {
            surfaces.push(title);
        }
        if let Some(desc) = obj.get("description").and_then(Value::as_str) {
            surfaces.push(desc);
        }
        if let Some(footer) = obj.get("footer") {
            if let Some(text) = footer.as_str() {
                surfaces.push(text);
            } else if let Some(text) = footer.get("text").and_then(Value::as_str) {
                surfaces.push(text);
            }
        }
        if let Some(author) = obj.get("author") {
            if let Some(name) = author.as_str() {
                surfaces.push(name);
            } else if let Some(name) = author.get("name").and_then(Value::as_str) {
                surfaces.push(name);
            }
        }
        if let Some(fields) = obj.get("fields").and_then(Value::as_array) {
            for field in fields.iter().filter_map(Value::as_object) {
                if let Some(name) = field.get("name").and_then(Value::as_str) {
                    surfaces.push(name);
                }
                if let Some(val) = field.get("value").and_then(Value::as_str) {
                    surfaces.push(val);
                }
            }
        }
    }
    surfaces.join("\n")
}

/// Parse a JSON array of embed objects into serenity [`CreateEmbed`] builders.
///
/// Returns `Err` with a human-readable message on malformed input.
pub fn parse_embeds(value: &Value) -> Result<Vec<CreateEmbed>, String> {
    let arr = value
        .as_array()
        .ok_or_else(|| "embeds must be a JSON array".to_string())?;

    if arr.len() > MAX_EMBEDS_PER_MESSAGE {
        return Err(format!(
            "too many embeds: got {}, maximum is {MAX_EMBEDS_PER_MESSAGE}",
            arr.len()
        ));
    }

    arr.iter()
        .enumerate()
        .map(|(i, v)| parse_one(v, i))
        .collect()
}

/// Parse a single embed object.
fn parse_one(value: &Value, index: usize) -> Result<CreateEmbed, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| format!("embeds[{index}] must be a JSON object"))?;

    let mut embed = CreateEmbed::new();

    if let Some(title) = obj.get("title").and_then(Value::as_str) {
        embed = embed.title(title);
    }

    if let Some(description) = obj.get("description").and_then(Value::as_str) {
        embed = embed.description(description);
    }

    if let Some(url) = obj.get("url").and_then(Value::as_str) {
        embed = embed.url(url);
    }

    if let Some(color) = parse_color(obj.get("color")) {
        embed = embed.color(color);
    }

    if let Some(timestamp) = obj.get("timestamp").and_then(Value::as_str) {
        let ts: Timestamp = timestamp
            .parse()
            .map_err(|_| format!("embeds[{index}].timestamp: invalid ISO 8601 timestamp"))?;
        embed = embed.timestamp(ts);
    }

    if let Some(footer_val) = obj.get("footer") {
        embed = embed.footer(parse_footer(footer_val, index)?);
    }

    if let Some(author_val) = obj.get("author") {
        embed = embed.author(parse_author(author_val, index)?);
    }

    if let Some(thumbnail) = obj.get("thumbnail").and_then(as_url_field) {
        embed = embed.thumbnail(thumbnail);
    }

    if let Some(image) = obj.get("image").and_then(as_url_field) {
        embed = embed.image(image);
    }

    if let Some(fields_val) = obj.get("fields") {
        embed = apply_fields(embed, fields_val, index)?;
    }

    Ok(embed)
}

/// Parse the `color` field. Accepts integer or hex string (with or without `#` prefix).
fn parse_color(value: Option<&Value>) -> Option<u32> {
    let value = value?;
    if let Some(n) = value.as_u64() {
        return Some(n as u32);
    }
    if let Some(s) = value.as_str() {
        let hex = s.strip_prefix('#').unwrap_or(s);
        return u32::from_str_radix(hex, 16).ok();
    }
    None
}

/// Extract a URL from either a plain string or an object with a `url` field.
///
/// Discord's embed spec uses `{ "url": "..." }` for thumbnail and image, but
/// for ergonomics we also accept a bare string.
fn as_url_field(value: &Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| value.get("url").and_then(Value::as_str))
}

/// Parse the `footer` field into a [`CreateEmbedFooter`].
fn parse_footer(value: &Value, index: usize) -> Result<CreateEmbedFooter, String> {
    if let Some(text) = value.as_str() {
        return Ok(CreateEmbedFooter::new(text));
    }

    let obj = value
        .as_object()
        .ok_or_else(|| format!("embeds[{index}].footer must be a string or object"))?;

    let text = obj
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("embeds[{index}].footer.text is required"))?;

    let mut footer = CreateEmbedFooter::new(text);
    if let Some(icon_url) = obj.get("icon_url").and_then(Value::as_str) {
        footer = footer.icon_url(icon_url);
    }
    Ok(footer)
}

/// Parse the `author` field into a [`CreateEmbedAuthor`].
fn parse_author(value: &Value, index: usize) -> Result<CreateEmbedAuthor, String> {
    if let Some(name) = value.as_str() {
        return Ok(CreateEmbedAuthor::new(name));
    }

    let obj = value
        .as_object()
        .ok_or_else(|| format!("embeds[{index}].author must be a string or object"))?;

    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("embeds[{index}].author.name is required"))?;

    let mut author = CreateEmbedAuthor::new(name);
    if let Some(url) = obj.get("url").and_then(Value::as_str) {
        author = author.url(url);
    }
    if let Some(icon_url) = obj.get("icon_url").and_then(Value::as_str) {
        author = author.icon_url(icon_url);
    }
    Ok(author)
}

/// Parse the `fields` array and apply each field to the embed builder.
fn apply_fields(
    mut embed: CreateEmbed,
    value: &Value,
    index: usize,
) -> Result<CreateEmbed, String> {
    let arr = value
        .as_array()
        .ok_or_else(|| format!("embeds[{index}].fields must be an array"))?;

    for (fi, field) in arr.iter().enumerate() {
        let obj = field
            .as_object()
            .ok_or_else(|| format!("embeds[{index}].fields[{fi}] must be an object"))?;

        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("embeds[{index}].fields[{fi}].name is required"))?;

        let value_str = obj
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("embeds[{index}].fields[{fi}].value is required"))?;

        let inline = obj.get("inline").and_then(Value::as_bool).unwrap_or(false);

        embed = embed.field(name, value_str, inline);
    }

    Ok(embed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── extract_text_surfaces ───────────────────────────────────────────────

    #[test]
    fn extracts_title_and_description() {
        let text = extract_text_surfaces(&json!([{
            "title": "My Title",
            "description": "My description"
        }]));
        assert!(text.contains("My Title"));
        assert!(text.contains("My description"));
    }

    #[test]
    fn extracts_field_names_and_values() {
        let text = extract_text_surfaces(&json!([{
            "fields": [
                { "name": "Field Name", "value": "Field Value" }
            ]
        }]));
        assert!(text.contains("Field Name"));
        assert!(text.contains("Field Value"));
    }

    #[test]
    fn extracts_footer_and_author() {
        let text = extract_text_surfaces(&json!([{
            "footer": { "text": "Footer Text" },
            "author": { "name": "Author Name" }
        }]));
        assert!(text.contains("Footer Text"));
        assert!(text.contains("Author Name"));
    }

    #[test]
    fn extracts_footer_from_string_shorthand() {
        let text = extract_text_surfaces(&json!([{
            "footer": "Simple Footer"
        }]));
        assert!(text.contains("Simple Footer"));
    }

    #[test]
    fn excludes_urls_and_non_text() {
        let text = extract_text_surfaces(&json!([{
            "url": "https://example.com",
            "thumbnail": "https://example.com/thumb.png",
            "image": "https://example.com/img.png",
            "color": 0xFF0000
        }]));
        assert!(!text.contains("https://"));
    }

    #[test]
    fn empty_on_non_array() {
        assert!(extract_text_surfaces(&json!("not an array")).is_empty());
        assert!(extract_text_surfaces(&json!(42)).is_empty());
    }

    #[test]
    fn empty_on_empty_array() {
        assert!(extract_text_surfaces(&json!([])).is_empty());
    }

    // ── parse_embeds: input validation ──────────────────────────────────────

    #[test]
    fn rejects_non_array() {
        assert!(parse_embeds(&json!("not an array")).is_err());
        assert!(parse_embeds(&json!(42)).is_err());
        assert!(parse_embeds(&json!({})).is_err());
    }

    #[test]
    fn rejects_too_many_embeds() {
        let arr: Vec<Value> = (0..11).map(|_| json!({})).collect();
        let result = parse_embeds(&json!(arr));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too many embeds"));
    }

    #[test]
    fn accepts_empty_array() {
        assert_eq!(parse_embeds(&json!([])).unwrap().len(), 0);
    }

    #[test]
    fn rejects_non_object_element() {
        let result = parse_embeds(&json!(["not an object"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be a JSON object"));
    }

    // ── minimal embed ───────────────────────────────────────────────────────

    #[test]
    fn parses_minimal_embed() {
        let embeds = parse_embeds(&json!([{ "title": "hello" }])).unwrap();
        assert_eq!(embeds.len(), 1);
    }

    #[test]
    fn parses_empty_object_embed() {
        // An embed with no fields set is valid — Discord renders it as empty.
        let embeds = parse_embeds(&json!([{}])).unwrap();
        assert_eq!(embeds.len(), 1);
    }

    // ── color parsing ───────────────────────────────────────────────────────

    #[test]
    fn color_from_integer() {
        assert_eq!(parse_color(Some(&json!(0xFF0000))), Some(0xFF0000));
    }

    #[test]
    fn color_from_hex_string() {
        assert_eq!(parse_color(Some(&json!("FF0000"))), Some(0xFF0000));
    }

    #[test]
    fn color_from_hex_string_with_hash() {
        assert_eq!(parse_color(Some(&json!("#FF0000"))), Some(0xFF0000));
    }

    #[test]
    fn color_none_when_absent() {
        assert_eq!(parse_color(None), None);
    }

    #[test]
    fn color_none_for_invalid_string() {
        assert_eq!(parse_color(Some(&json!("not-a-color"))), None);
    }

    // ── footer ──────────────────────────────────────────────────────────────

    #[test]
    fn footer_from_string() {
        let footer = parse_footer(&json!("simple footer"), 0).unwrap();
        // We can't inspect CreateEmbedFooter internals directly, but
        // the fact that it parsed without error is the contract.
        let _ = footer;
    }

    #[test]
    fn footer_from_object() {
        let footer = parse_footer(
            &json!({ "text": "footer text", "icon_url": "https://example.com/icon.png" }),
            0,
        )
        .unwrap();
        let _ = footer;
    }

    #[test]
    fn footer_requires_text() {
        let result = parse_footer(&json!({ "icon_url": "https://example.com/icon.png" }), 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("text is required"));
    }

    // ── author ──────────────────────────────────────────────────────────────

    #[test]
    fn author_from_string() {
        let author = parse_author(&json!("Author Name"), 0).unwrap();
        let _ = author;
    }

    #[test]
    fn author_from_object() {
        let author = parse_author(
            &json!({
                "name": "Author",
                "url": "https://example.com",
                "icon_url": "https://example.com/avatar.png"
            }),
            0,
        )
        .unwrap();
        let _ = author;
    }

    #[test]
    fn author_requires_name() {
        let result = parse_author(&json!({ "url": "https://example.com" }), 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("name is required"));
    }

    // ── fields ──────────────────────────────────────────────────────────────

    #[test]
    fn fields_parsed_correctly() {
        let embed = parse_one(
            &json!({
                "fields": [
                    { "name": "Field 1", "value": "Value 1", "inline": true },
                    { "name": "Field 2", "value": "Value 2" }
                ]
            }),
            0,
        )
        .unwrap();
        let _ = embed;
    }

    #[test]
    fn fields_require_name_and_value() {
        let result = parse_one(&json!({ "fields": [{ "name": "missing value" }] }), 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("value is required"));

        let result = parse_one(&json!({ "fields": [{ "value": "missing name" }] }), 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("name is required"));
    }

    // ── thumbnail / image URL flexibility ───────────────────────────────────

    #[test]
    fn thumbnail_from_string() {
        let embed = parse_one(&json!({ "thumbnail": "https://example.com/thumb.png" }), 0).unwrap();
        let _ = embed;
    }

    #[test]
    fn thumbnail_from_url_object() {
        let embed = parse_one(
            &json!({ "thumbnail": { "url": "https://example.com/thumb.png" } }),
            0,
        )
        .unwrap();
        let _ = embed;
    }

    #[test]
    fn image_from_string() {
        let embed = parse_one(&json!({ "image": "https://example.com/img.png" }), 0).unwrap();
        let _ = embed;
    }

    #[test]
    fn image_from_url_object() {
        let embed = parse_one(
            &json!({ "image": { "url": "https://example.com/img.png" } }),
            0,
        )
        .unwrap();
        let _ = embed;
    }

    // ── timestamp ───────────────────────────────────────────────────────────

    #[test]
    fn valid_timestamp_parses() {
        let embed = parse_one(&json!({ "timestamp": "2026-06-20T12:00:00Z" }), 0).unwrap();
        let _ = embed;
    }

    #[test]
    fn invalid_timestamp_rejected() {
        let result = parse_one(&json!({ "timestamp": "not-a-date" }), 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("timestamp"));
    }

    // ── full embed (wire format snapshot) ───────────────────────────────────

    /// Exercises every field simultaneously — the complete embed shape that
    /// the MCP tool accepts. This is the wire format contract.
    #[test]
    fn full_embed_parses() {
        let input = json!([{
            "title": "Reading Digest",
            "description": "Today's highlights from the feeds.",
            "url": "https://example.com/digest",
            "color": 3447003,
            "timestamp": "2026-06-20T08:30:00Z",
            "footer": {
                "text": "Generated by Ariadne",
                "icon_url": "https://example.com/footer-icon.png"
            },
            "author": {
                "name": "Ariadne",
                "url": "https://example.com/ariadne",
                "icon_url": "https://example.com/avatar.png"
            },
            "thumbnail": { "url": "https://example.com/thumb.png" },
            "image": { "url": "https://example.com/hero.png" },
            "fields": [
                { "name": "Article", "value": "Something interesting", "inline": true },
                { "name": "Source", "value": "example.com", "inline": true },
                { "name": "Summary", "value": "A longer description of the article." }
            ]
        }]);

        let embeds = parse_embeds(&input).unwrap();
        assert_eq!(embeds.len(), 1);
    }

    /// Multiple embeds in a single message.
    #[test]
    fn multiple_embeds_parse() {
        let input = json!([
            { "title": "First" },
            { "title": "Second", "color": 0xFF0000 },
            { "description": "Third — no title" }
        ]);

        let embeds = parse_embeds(&input).unwrap();
        assert_eq!(embeds.len(), 3);
    }

    /// At the limit: exactly 10 embeds.
    #[test]
    fn exactly_ten_embeds_accepted() {
        let arr: Vec<Value> = (0..10)
            .map(|i| json!({ "title": format!("Embed {i}") }))
            .collect();
        let embeds = parse_embeds(&json!(arr)).unwrap();
        assert_eq!(embeds.len(), 10);
    }
}
