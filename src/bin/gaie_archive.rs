use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use color_eyre::eyre::Result;
use dione::gaie::{Archive, ArchivePaths, CorpusId, DiscordArchiveClient, build_latest_state};

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
    let loaded = dione::config::reload_config(&state_dir).await.0;
    loaded
        .archive
        .validate(&loaded.channels)
        .map_err(color_eyre::eyre::Report::msg)?;
    let corpus = CorpusId::parse(loaded.archive.corpus_id.clone())?;
    let paths = ArchivePaths::new(loaded.archive.data_dir.clone(), &corpus)?;
    let now = now();
    match cli.command {
        Command::LatestState => {
            let archive = Archive::open(paths, corpus, &now)?;
            let read = archive.read_committed()?;
            report_quarantine(&read);
            let mut messages: Vec<_> = build_latest_state(&read.events)?.into_values().collect();
            messages.sort_by(|left, right| left.created_at.cmp(&right.created_at));
            println!("{}", serde_json::to_string_pretty(&messages)?);
        }
        Command::Reconcile => {
            let archive = Archive::open(paths, corpus, &now)?;
            let read = archive.read_committed()?;
            report_quarantine(&read);
            let messages = build_latest_state(&read.events)?;
            println!("Total messages in latest-state view: {}", messages.len());
            println!(
                "Total committed events: {}",
                read.events.len() + read.quarantined_events.len()
            );
            println!("Quarantined events: {}", read.quarantined_events.len());
        }
        Command::Backfill => backfill_parent(&loaded, paths, corpus, &now).await?,
    }
    Ok(())
}

fn report_quarantine(read: &dione::gaie::ReadResult) {
    for quarantined in &read.quarantined_events {
        eprintln!(
            "WARNING: quarantined GAIE event {} (message {}): {}",
            quarantined.event.event_id,
            quarantined.event.source.message_id,
            quarantined.origin_error
        );
    }
}

async fn backfill_parent(
    config: &dione::config::LoadedConfig,
    paths: ArchivePaths,
    corpus: CorpusId,
    created_at: &str,
) -> Result<()> {
    let token = dione::config::resolve_token(config)
        .ok_or_else(|| color_eyre::eyre::eyre!("no Discord bot token found"))?;
    let client = DiscordArchiveClient::new();
    let added = dione::gaie::run_backfill(
        &client,
        &token,
        dione::gaie::BackfillOptions::new(
            &config.archive.corpus_id,
            &config.archive.guild_id,
            &config.archive.channel_id,
            config.archive.allow_partial,
        )
        .with_attachment_limits(
            config.archive.max_attachment_bytes,
            config.archive.max_run_download_bytes,
        ),
        paths,
        corpus,
        created_at,
    )
    .await?;
    if config.archive.allow_partial {
        eprintln!("warning: allow_partial captured the parent stream only");
    }
    println!("{added} new messages");
    Ok(())
}

fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
