use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use color_eyre::eyre::{Result, bail};
use dione::gaie::{
    Archive, ArchivePaths, Checkpoint, CorpusId, DiscordArchiveClient, MessageContext,
    build_latest_state, message_batch,
};
use std::collections::HashSet;

#[derive(Debug, Parser)]
#[command(about = "Explicit one-shot GAIE archive operations")]
struct Cli {
    #[arg(long)]
    config: Option<Utf8PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Backfill the configured parent channel through lossless Discord HTTP.
    Backfill,
    /// Print a summary of committed archive state.
    Reconcile,
    /// Emit the committed latest-state view as JSON.
    LatestState,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    if let Some(path) = cli.config {
        dione::config::set_config_path(path);
    }
    let state_dir = dione::config::state_dir();
    let loaded = dione::config::reload_config(&state_dir).0;
    loaded
        .archive
        .validate(&loaded.channels)
        .map_err(color_eyre::eyre::Report::msg)?;
    let corpus = CorpusId::parse(loaded.archive.corpus_id.clone())?;
    let paths = ArchivePaths::new(loaded.archive.data_dir.clone(), &corpus)?;
    let now = now();
    let mut archive = Archive::open(paths, corpus, &now)?;
    match cli.command {
        Command::LatestState => {
            let mut messages: Vec<_> = build_latest_state(&archive.read_committed()?.events)
                .into_values()
                .collect();
            messages.sort_by(|left, right| left.created_at.cmp(&right.created_at));
            println!("{}", serde_json::to_string_pretty(&messages)?);
        }
        Command::Reconcile => {
            let events = archive.read_committed()?.events;
            let messages = build_latest_state(&events);
            println!("Total messages in latest-state view: {}", messages.len());
            println!("Total committed events: {}", events.len());
        }
        Command::Backfill => backfill_parent(&loaded, &mut archive).await?,
    }
    Ok(())
}

async fn backfill_parent(
    config: &dione::config::LoadedConfig,
    archive: &mut Archive,
) -> Result<()> {
    if !config.archive.allow_partial {
        bail!(
            "owned-thread enumeration is not implemented in atom 1; set archive.allow_partial = true to archive only the parent"
        );
    }
    let token = dione::config::resolve_token(config)
        .ok_or_else(|| color_eyre::eyre::eyre!("no Discord bot token found"))?;
    let client = DiscordArchiveClient::new();
    let checkpoint = archive.load_checkpoint()?;
    if checkpoint
        .as_ref()
        .is_some_and(|checkpoint| checkpoint.channel_id != config.archive.channel_id)
    {
        bail!("archive checkpoint belongs to another channel");
    }
    let mut cursor = checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.after_message_id.clone());
    let incremental = cursor.is_some();
    let mut messages = Vec::new();
    let mut cursors = HashSet::new();
    loop {
        let (before, after) = if incremental {
            (None, cursor.as_deref())
        } else {
            (cursor.as_deref(), None)
        };
        let page = client
            .raw_message_page(
                &token,
                &config.archive.channel_id,
                &config.archive.channel_id,
                before,
                after,
            )
            .await?;
        if page.is_empty() {
            break;
        }
        let (minimum, maximum) = page_bounds(&page)?;
        let next = if incremental { maximum } else { minimum };
        cursor = Some(advance_cursor(cursor.as_deref(), &mut cursors, next)?);
        let short = page.len() < 100;
        messages.extend(page);
        if short {
            break;
        }
    }
    messages.sort_by_key(|message| {
        message
            .get("id")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| id.parse::<u64>().ok())
            .unwrap_or(0)
    });
    let committed = archive.read_committed()?;
    let mut seen: HashSet<String> = committed
        .events
        .iter()
        .filter(|event| matches!(event.event_kind, dione::gaie::EventKind::MessageCreate))
        .map(|event| event.source.message_id.clone())
        .collect();
    let mut sequence = archive.last_sequence() + 1;
    let mut added = 0;
    for message in messages {
        let Some(message_id) = message
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        if seen.contains(&message_id) {
            archive.save_checkpoint(&Checkpoint {
                corpus_id: config.archive.corpus_id.clone(),
                channel_id: config.archive.channel_id.clone(),
                after_message_id: message_id,
                updated_at: now(),
            })?;
            continue;
        }
        let observed = now();
        let mut attachment_hashes = std::collections::HashMap::new();
        for attachment in message
            .get("attachments")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            let attachment_id = attachment
                .get("id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| color_eyre::eyre::eyre!("Discord attachment is missing id"))?;
            let url = attachment
                .get("url")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| color_eyre::eyre::eyre!("Discord attachment is missing url"))?;
            let digest = client
                .download_attachment(url, archive.attachments_dir())
                .await?;
            attachment_hashes.insert(attachment_id.to_owned(), digest);
        }
        let batch = message_batch(
            &message,
            MessageContext {
                corpus_id: &config.archive.corpus_id,
                guild_id: &config.archive.guild_id,
                channel_id: &config.archive.channel_id,
                thread_id: None,
                thread_parent_channel_id: None,
                observed_at: &observed,
            },
            sequence,
            &attachment_hashes,
        )?;
        archive.append_batch(&batch, &message_id, &observed)?;
        archive.save_checkpoint(&Checkpoint {
            corpus_id: config.archive.corpus_id.clone(),
            channel_id: config.archive.channel_id.clone(),
            after_message_id: message_id.clone(),
            updated_at: observed,
        })?;
        sequence += batch.len() as u64;
        seen.insert(message_id);
        added += 1;
    }
    eprintln!(
        "warning: atom 1 backfilled the allowlisted parent only; owned-thread enumeration is not yet implemented"
    );
    println!("{added} new messages");
    Ok(())
}

fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn page_bounds(page: &[serde_json::Value]) -> Result<(String, String)> {
    let mut ids = page.iter().map(|message| -> Result<(u64, String)> {
        let id = message
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| color_eyre::eyre::eyre!("Discord message page contains a missing id"))?;
        let numeric = id
            .parse::<u64>()
            .map_err(|_| color_eyre::eyre::eyre!("Discord message page contains a malformed id"))?;
        Ok((numeric, id.to_owned()))
    });
    let Some(first) = ids.next() else {
        bail!("cannot derive bounds from an empty page")
    };
    let first = first?;
    let mut minimum = first.clone();
    let mut maximum = first;
    for current in ids {
        let current = current?;
        if current.0 < minimum.0 {
            minimum = current.clone();
        }
        if current.0 > maximum.0 {
            maximum = current;
        }
    }
    Ok((minimum.1, maximum.1))
}

fn advance_cursor(
    current: Option<&str>,
    seen: &mut HashSet<String>,
    next: String,
) -> Result<String> {
    if current == Some(next.as_str()) || !seen.insert(next.clone()) {
        bail!("Discord archive pagination cursor did not advance");
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_bounds_rejects_malformed_ids_and_orders_snowflakes() {
        let page = [
            serde_json::json!({"id":"20"}),
            serde_json::json!({"id":"10"}),
        ];
        assert_eq!(
            page_bounds(&page).unwrap(),
            ("10".to_owned(), "20".to_owned())
        );
        assert!(page_bounds(&[serde_json::json!({"id":"not-a-snowflake"})]).is_err());
        assert!(page_bounds(&[serde_json::json!({})]).is_err());
    }

    #[test]
    fn repeated_pagination_cursor_fails_closed() {
        let mut seen = HashSet::new();
        assert_eq!(
            advance_cursor(None, &mut seen, "10".to_owned()).unwrap(),
            "10"
        );
        assert!(advance_cursor(Some("10"), &mut seen, "10".to_owned()).is_err());
        assert!(advance_cursor(Some("9"), &mut seen, "10".to_owned()).is_err());
    }
}
