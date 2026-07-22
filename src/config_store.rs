use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{Value, json};
use toml_edit::{Array, DocumentMut, Item, Table, value};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

// ── DiscordId newtype ────────────────────────────────────────────────────────

/// A validated Discord snowflake ID (non-zero u64).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscordId(u64);

impl DiscordId {
    pub fn parse(s: &str) -> Result<Self, String> {
        let id = s
            .parse::<u64>()
            .map_err(|_| format!("invalid Discord ID: {s}"))?;
        // Snowflakes are nonzero — this is what the doc comment above
        // promises, and serenity's Id wrappers panic on zero.
        if id == 0 {
            return Err(format!("invalid Discord ID: {s}"));
        }
        Ok(Self(id))
    }
}

impl std::fmt::Display for DiscordId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub struct ConfigStore {
    doc: DocumentMut,
    config_path: Utf8PathBuf,
    tmp_path: Utf8PathBuf,
}

impl ConfigStore {
    pub async fn load(state_dir: &Utf8Path) -> Result<Self, BoxError> {
        let config_path = crate::config::config_path(state_dir);
        let contents = match tokio::fs::read_to_string(&config_path).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        let tmp_path = Utf8PathBuf::from(format!("{}.tmp", config_path));
        Ok(Self {
            doc: contents.parse()?,
            tmp_path,
            config_path,
        })
    }

    pub async fn save(&self) -> Result<(), BoxError> {
        let serialized = self.doc.to_string();

        // Parse and build the LoadedConfig *before* touching the disk so that
        // a parse failure leaves both the cache and the on-disk file unchanged
        // (consistent state). toml_edit ↔ toml round-trip should always
        // succeed, but we verify eagerly rather than racing the rename.
        let raw: crate::config::Config = toml::from_str(&serialized)?;
        raw.validate()?;
        let loaded = crate::config::LoadedConfig::from_raw(raw);

        tokio::fs::write(&self.tmp_path, &serialized).await?;
        if let Err(e) = tokio::fs::rename(&self.tmp_path, &self.config_path).await {
            let _ = tokio::fs::remove_file(&self.tmp_path).await;
            return Err(e.into());
        }

        // Update the in-memory ArcSwap cache so load_config() callers see
        // the new config immediately without a redundant disk re-read.
        crate::config::store_loaded_config(&loaded);

        Ok(())
    }

    // ── Read-only config queries ─────────────────────────────────────────────

    /// Return a JSON list of all configured channels (reads from ArcSwap cache).
    pub fn list_channels(state_dir: &Utf8Path) -> Value {
        let config = crate::config::load_config(state_dir);
        let channels: Vec<Value> = config
            .raw
            .channels
            .iter()
            .map(|ch| {
                json!({
                    "id": ch.id,
                    "require_mention": ch.require_mention,
                    "allow_from": ch.allow_from,
                })
            })
            .collect();
        json!({ "channels": channels })
    }

    /// Return a JSON object with the current access config (reads from ArcSwap cache).
    pub fn get_access(state_dir: &Utf8Path) -> Value {
        let config = crate::config::load_config(state_dir);
        json!({
            "dm_policy": match config.raw.access.dm_policy {
                crate::config::DmPolicy::Queue => "queue",
                crate::config::DmPolicy::Drop => "drop",
                crate::config::DmPolicy::Disabled => "disabled",
            },
            "allow_from": config.raw.access.allow_from,
            "admins": config.raw.access.admins,
            "include_pronouns": config.raw.access.include_pronouns,
        })
    }

    // ── Channel operations ───────────────────────────────────────────────────

    pub fn find_channel(&self, id: &str) -> Option<usize> {
        self.doc
            .get("channels")
            .and_then(Item::as_array_of_tables)
            .and_then(|aot| {
                aot.iter()
                    .position(|t| t.get("id").and_then(Item::as_str) == Some(id))
            })
    }

    pub fn ensure_channels(&mut self) {
        self.doc
            .entry("channels")
            .or_insert_with(|| Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    }

    pub fn channels_mut(&mut self) -> Result<&mut toml_edit::ArrayOfTables, BoxError> {
        self.doc
            .get_mut("channels")
            .and_then(Item::as_array_of_tables_mut)
            .ok_or_else(|| "no channels in config".into())
    }

    pub fn add_channel_entry(
        &mut self,
        id: &str,
        require_mention: bool,
        allow_from: Vec<String>,
    ) -> Result<(), BoxError> {
        if self.find_channel(id).is_some() {
            return Err(format!("channel {id} already exists in config").into());
        }
        let mut entry = Table::new();
        entry.insert("id", value(id));
        entry.insert("require_mention", value(require_mention));
        let af_arr: Array = allow_from.into_iter().collect();
        entry.insert("allow_from", Item::Value(toml_edit::Value::Array(af_arr)));
        self.ensure_channels();
        self.channels_mut()?.push(entry);
        Ok(())
    }

    pub fn remove_channel_entry(&mut self, id: &str) -> Result<(), BoxError> {
        let idx = self
            .find_channel(id)
            .ok_or_else(|| format!("channel {id} not found in config"))?;
        self.channels_mut()?.remove(idx);
        Ok(())
    }

    pub fn update_channel_entry(
        &mut self,
        id: &str,
        require_mention: Option<bool>,
        allow_from: Option<Vec<String>>,
    ) -> Result<(), BoxError> {
        let entry = self
            .channels_mut()
            .ok()
            .and_then(|aot| {
                aot.iter_mut()
                    .find(|t| t.get("id").and_then(Item::as_str) == Some(id))
            })
            .ok_or_else(|| format!("channel {id} not found in config"))?;

        if let Some(rm) = require_mention {
            entry.insert("require_mention", value(rm));
        }
        if let Some(af) = allow_from {
            let af_arr: Array = af.into_iter().collect();
            entry.insert("allow_from", Item::Value(toml_edit::Value::Array(af_arr)));
        }
        Ok(())
    }

    // ── Access operations ────────────────────────────────────────────────────

    pub fn ensure_access(&mut self) {
        if !self.doc.contains_key("access") {
            self.doc["access"] = Item::Table(Table::new());
        }
        let access = &mut self.doc["access"];
        if access.get("allow_from").is_none() {
            access["allow_from"] = Item::Value(toml_edit::Value::Array(Array::new()));
        }
    }

    pub fn allow_from_mut(&mut self) -> Result<&mut Array, BoxError> {
        self.doc
            .get_mut("access")
            .and_then(|a| a.get_mut("allow_from"))
            .and_then(Item::as_value_mut)
            .and_then(toml_edit::Value::as_array_mut)
            .ok_or_else(|| "allow_from is not an array".into())
    }

    pub fn set_dm_policy(&mut self, policy: &str) {
        self.ensure_access();
        self.doc["access"]["dm_policy"] = value(policy);
    }

    pub fn add_to_allow_from(&mut self, user_id: &str) -> Result<(), BoxError> {
        self.ensure_access();
        let allow_from = self.allow_from_mut()?;
        if allow_from.iter().any(|v| v.as_str() == Some(user_id)) {
            return Err(format!("user_id {user_id} already in allow_from").into());
        }
        allow_from.push(user_id);
        Ok(())
    }

    pub fn ensure_in_allow_from(&mut self, user_id: &str) -> Result<(), BoxError> {
        self.ensure_access();
        let allow_from = self.allow_from_mut()?;
        if !allow_from.iter().any(|v| v.as_str() == Some(user_id)) {
            allow_from.push(user_id);
        }
        Ok(())
    }

    pub fn remove_from_allow_from(&mut self, user_id: &str) -> Result<(), BoxError> {
        let access = self
            .doc
            .get_mut("access")
            .ok_or_else(|| format!("user_id {user_id} not found in allow_from"))?;
        let allow_from = access["allow_from"]
            .as_array_mut()
            .ok_or_else(|| format!("user_id {user_id} not found in allow_from"))?;

        let before = allow_from.len();
        allow_from.retain(|v| v.as_str() != Some(user_id));
        if allow_from.len() == before {
            return Err(format!("user_id {user_id} not found in allow_from").into());
        }
        if let Some(include_pronouns) = access
            .get_mut("include_pronouns")
            .and_then(Item::as_array_mut)
        {
            include_pronouns.retain(|value| value.as_str() != Some(user_id));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discord_id_rejects_zero() {
        // Snowflakes are nonzero; serenity's Id wrappers panic on 0, so the
        // newtype must refuse it at the parse boundary.
        assert!(DiscordId::parse("0").is_err());
    }

    #[test]
    fn discord_id_accepts_valid_snowflake() {
        let id = DiscordId::parse("1508300070599000225").expect("valid snowflake");
        assert_eq!(id.to_string(), "1508300070599000225");
    }

    #[test]
    fn discord_id_rejects_non_numeric() {
        assert!(DiscordId::parse("not-a-number").is_err());
        assert!(DiscordId::parse("").is_err());
        assert!(DiscordId::parse("-1").is_err());
    }

    #[test]
    fn removing_allowed_user_also_removes_pronoun_opt_in() {
        let doc = r#"
[access]
allow_from = ["42", "43"]
include_pronouns = ["42", "43"]
"#
        .parse()
        .expect("config document");
        let mut store = ConfigStore {
            doc,
            config_path: Utf8PathBuf::from("/tmp/config.toml"),
            tmp_path: Utf8PathBuf::from("/tmp/config.toml.tmp"),
        };

        store.remove_from_allow_from("42").expect("remove user");

        let access = store.doc["access"].as_table().expect("access table");
        let allowed = access["allow_from"].as_array().expect("allow_from");
        let included = access["include_pronouns"]
            .as_array()
            .expect("include_pronouns");
        assert_eq!(
            allowed
                .iter()
                .filter_map(|value| value.as_str())
                .collect::<Vec<_>>(),
            ["43"]
        );
        assert_eq!(
            included
                .iter()
                .filter_map(|value| value.as_str())
                .collect::<Vec<_>>(),
            ["43"]
        );
    }
}
