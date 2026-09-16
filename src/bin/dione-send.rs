//! `dione-send`: one-shot, outbound-only Discord send with an explicit
//! identity preflight (#426). See `docs/oneshot-send.md` and
//! [`dione::oneshot`] for the contract.
//!
//! stdout carries exactly one JSON object; every log line goes to stderr.

use camino::Utf8PathBuf;
use clap::{Parser, error::ErrorKind};
use dione::oneshot::{Outcome, Reason, Refusal, SendRequest, TokenSource};
use std::io::{Read as _, Write as _};

#[derive(Debug, Parser)]
#[command(
    name = "dione-send",
    about = "One-shot outbound-only Discord send with identity preflight",
    long_about = "Loads the config read-only, verifies the bot identity, gates the target \
                  against [[channels]], chunks, judges, and sends. Prints exactly one JSON \
                  object on stdout. Exit 0 sent or dry-run clean; 1 usage/config/IO; \
                  2 preflight refusal; 3 Discord REST failure."
)]
struct Cli {
    /// Target channel ID (must be listed in [[channels]]).
    #[arg(long)]
    channel: u64,
    /// Required bot user ID; the token must authenticate as exactly this user.
    #[arg(long)]
    expect_identity: u64,
    /// Message text, or `-` to read it from stdin.
    #[arg(long)]
    message: String,
    /// Config file path. Defaults to `$DIONE_STATE_DIR/config.toml`.
    #[arg(long)]
    config: Option<Utf8PathBuf>,
    /// Where the token comes from: `config` or `env:<VAR>`. Ambient
    /// DISCORD_BOT_TOKEN is never consulted unless named here.
    #[arg(long, default_value = "config")]
    token_source: String,
    /// Stop after preflight; print `preflight_ok` and send nothing.
    #[arg(long)]
    dry_run: bool,
    /// Accept a message that chunks into more than one Discord message.
    #[arg(long)]
    allow_multi_chunk: bool,
    /// Caller-stable idempotency key (max 25 chars), sent with
    /// `enforce_nonce` as defense-in-depth for a prompt manual replay.
    /// It does not make an ambiguous send failure retryable.
    #[arg(long)]
    nonce: Option<String>,
    /// Test seam: replace the Discord API base URL with a local mock.
    /// Compiled only with the `oneshot-test-seam` cargo feature; production
    /// bytes have no such flag.
    #[cfg(feature = "oneshot-test-seam")]
    #[arg(long, hide = true)]
    discord_api_base: Option<String>,
}

fn emit(outcome: &Outcome) -> i32 {
    let mut stdout = std::io::stdout().lock();
    // One object, one line, one newline.
    let _ = writeln!(stdout, "{}", outcome.to_json());
    let _ = stdout.flush();
    outcome.exit_code()
}

/// Usage and stdin failures still honor "exactly one JSON object": the
/// sanitized message goes to both stderr (human) and the `usage` detail.
fn usage_error(message: &str) -> i32 {
    let message = message.trim();
    eprintln!("dione-send: {message}");
    emit(&Outcome::Refused(Refusal::new(Reason::Usage, message)))
}

#[tokio::main]
async fn main() {
    std::process::exit(real_main().await);
}

/// Crates whose instrumentation renders request bodies (serenity's
/// `Http::request` / `Request::build` spans carry the POST body at trace),
/// so they are clamped to `warn` no matter what `RUST_LOG` says.
const THIRD_PARTY_CLAMP: &str =
    "serenity=warn,reqwest=warn,hyper=warn,hyper_util=warn,h2=warn,rustls=warn,tokio=warn";

/// `RUST_LOG` governs only Dione's own targets, and only as a plain level
/// (`error|warn|info|debug|trace`); target directives are ignored so a
/// third-party crate can never be raised above `warn` in this binary.
fn log_filter(rust_log: Option<&str>) -> tracing_subscriber::EnvFilter {
    let level = rust_log
        .and_then(|value| {
            value
                .trim()
                .parse::<tracing::level_filters::LevelFilter>()
                .ok()
        })
        .unwrap_or(tracing::level_filters::LevelFilter::WARN);
    tracing_subscriber::EnvFilter::new(format!(
        "warn,dione={level},dione_send={level},{THIRD_PARTY_CLAMP}"
    ))
}

async fn real_main() -> i32 {
    // Logs never touch stdout, and third-party targets are clamped.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(log_filter(std::env::var("RUST_LOG").ok().as_deref()))
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init();

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            // --help / --version are the one non-JSON stdout exception.
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                let _ = error.print();
                return 0;
            }
            // Never on argv: the token. Clap's text may echo other argv.
            let rendered = error.render().to_string();
            eprintln!("{rendered}");
            // The message block before clap's "Usage:" line, one line.
            let detail = rendered
                .lines()
                .take_while(|line| !line.starts_with("Usage:"))
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            return emit(&Outcome::Refused(Refusal::new(
                Reason::Usage,
                detail.trim_start_matches("error: ").trim(),
            )));
        }
    };

    let token_source = match TokenSource::parse(&cli.token_source) {
        Ok(source) => source,
        Err(message) => return usage_error(&message),
    };

    let message = if cli.message == "-" {
        let mut text = String::new();
        if let Err(error) = std::io::stdin().lock().read_to_string(&mut text) {
            return usage_error(&format!("failed to read --message from stdin: {error}"));
        }
        text
    } else {
        cli.message
    };

    let config_path = match cli.config {
        Some(path) => path,
        None => dione::config::config_path(&dione::config::state_dir()),
    };
    // Redacted on purpose, on BOTH streams: a raw TOML error echoes the
    // offending source line, which may be the token. Only the redacted
    // kind/path/line:col ever leaves the process.
    let config = match dione::oneshot::load_config_for_send(&config_path) {
        Ok(config) => config,
        Err(refusal) => {
            eprintln!("dione-send: {}", refusal.detail);
            return emit(&Outcome::Refused(refusal));
        }
    };

    let request = SendRequest {
        channel_id: cli.channel,
        expect_identity: cli.expect_identity,
        message,
        token_source,
        allow_multi_chunk: cli.allow_multi_chunk,
        dry_run: cli.dry_run,
        nonce: cli.nonce,
    };
    #[cfg(feature = "oneshot-test-seam")]
    let outcome = match cli.discord_api_base.as_deref() {
        Some(api_base) => dione::oneshot::run_with_api_base(&config, &request, api_base).await,
        None => dione::oneshot::run(&config, &request).await,
    };
    #[cfg(not(feature = "oneshot-test-seam"))]
    let outcome = dione::oneshot::run(&config, &request).await;
    emit(&outcome)
}

#[cfg(test)]
mod tests {
    use super::log_filter;
    use tracing_subscriber::layer::SubscriberExt as _;

    struct Noop;
    impl tracing::Callsite for Noop {
        fn set_interest(&self, _: tracing::subscriber::Interest) {}
        fn metadata(&self) -> &tracing::Metadata<'_> {
            unreachable!()
        }
    }
    static NOOP: Noop = Noop;

    /// Would an event at `target`/`level` pass the filter built from
    /// `rust_log`? Evaluated through a real subscriber so the answer is
    /// the one the binary would give.
    fn enabled(rust_log: &str, target: &str, level: tracing::Level) -> bool {
        let subscriber = tracing_subscriber::registry().with(log_filter(Some(rust_log)));
        let meta = tracing::Metadata::new(
            "probe",
            target,
            level,
            None,
            None,
            None,
            tracing::field::FieldSet::new(&[], tracing::callsite::Identifier(&NOOP)),
            tracing::metadata::Kind::EVENT,
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::dispatcher::get_default(|dispatch| dispatch.enabled(&meta))
        })
    }

    #[test]
    fn third_party_targets_stay_clamped_at_trace() {
        assert!(enabled("trace", "dione::oneshot", tracing::Level::TRACE));
        assert!(enabled("trace", "dione_send", tracing::Level::TRACE));
        for target in [
            "serenity::http::client",
            "serenity::http::request",
            "reqwest::connect",
            "hyper::proto",
            "hyper_util::client",
            "h2::codec",
            "rustls::client",
            "tokio::task",
        ] {
            assert!(!enabled("trace", target, tracing::Level::INFO), "{target}");
            assert!(!enabled("trace", target, tracing::Level::DEBUG), "{target}");
            assert!(enabled("trace", target, tracing::Level::WARN), "{target}");
        }
    }

    #[test]
    fn target_directives_in_rust_log_are_ignored() {
        let rust_log = "serenity=trace,dione=trace";
        assert!(!enabled(
            rust_log,
            "serenity::http::client",
            tracing::Level::DEBUG
        ));
        assert!(!enabled(rust_log, "dione::oneshot", tracing::Level::INFO));
        assert!(!enabled("", "dione::oneshot", tracing::Level::INFO));
        assert!(enabled("info", "dione::oneshot", tracing::Level::INFO));
    }
}
