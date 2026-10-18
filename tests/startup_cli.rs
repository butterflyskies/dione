//! Executable startup contracts with isolated state and no external credentials.
#![cfg(unix)]

use camino::Utf8PathBuf;
use dione::{attention::store::AttentionStore, codex::CodexEventQueue};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::ErrorKind,
    os::unix::net::UnixListener,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct CliFixture {
    root: TempDir,
    state: Utf8PathBuf,
}

impl CliFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = Utf8PathBuf::from_path_buf(root.path().join("state")).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(root.path().join("home")).unwrap();
        fs::write(
            state.join("config.toml"),
            "[access]\nadmin_only_mutations = false\n",
        )
        .unwrap();
        Self { root, state }
    }

    fn run(&self, arguments: &[&str], input: &[u8]) -> Output {
        let stdin = self.root.path().join("stdin");
        let stdout = self.root.path().join("stdout");
        let stderr = self.root.path().join("stderr");
        fs::write(&stdin, input).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_dione"))
            .env_clear()
            .env("HOME", self.root.path().join("home"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("xdg-state"))
            .env("DIONE_STATE_DIR", &self.state)
            .env("NO_COLOR", "1")
            .env("RUST_BACKTRACE", "0")
            .current_dir(self.root.path())
            .args(["--config", self.state.join("config.toml").as_str()])
            .args(arguments)
            .stdin(Stdio::from(File::open(stdin).unwrap()))
            .stdout(Stdio::from(File::create(&stdout).unwrap()))
            .stderr(Stdio::from(File::create(&stderr).unwrap()))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                child.wait().unwrap();
                panic!(
                    "CLI exceeded ten seconds: {}",
                    fs::read_to_string(stderr).unwrap()
                );
            }
            thread::sleep(Duration::from_millis(10));
        };
        Output {
            status,
            stdout: fs::read(stdout).unwrap(),
            stderr: fs::read(stderr).unwrap(),
        }
    }

    fn attention(&self, mode: &str, input: &[u8]) -> Output {
        self.run(&["--mode", mode, "--attention-command", "-"], input)
    }
}

fn successful_json(output: Output) -> Value {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn rejected(output: Output, message: &str) {
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(message),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn startup_cli_offline_status_preserves_harness_policy_without_opening_inbox() {
    for (mode, supported) in [("claude-code", false), ("codex", true), ("oh-my-pi", false)] {
        let fixture = CliFixture::new();
        let status = successful_json(fixture.attention(mode, br#"{"operation":"status"}"#));
        assert_eq!(status["safe_delivery_supported"], json!(supported));
        assert_eq!(status["settings"]["mode"], json!("off"));
        assert_eq!(status["settings"]["recipient"], json!("default"));
        assert_eq!(status["store"]["records"], json!(0));
        assert_eq!(status["store_error"], Value::Null);
        assert!(!fixture.state.join("codex-inbox.json").exists());
        assert!(!fixture.state.join("codex-inbox.lock").exists());
    }
}

#[test]
fn startup_cli_offline_codex_ignores_occupied_inbox_and_app_server() {
    let fixture = CliFixture::new();
    let queue = CodexEventQueue::load(
        &fixture.state,
        dione::codex::LegacyDiscordScope::new(dione::core::ModuleInstanceId::new(
            dione::core::ProviderId::new("discord").unwrap(),
            dione::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let socket = fixture.root.path().join("app.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let before = fs::read(fixture.state.join("codex-inbox.json")).ok();
    let status = successful_json(fixture.run(
        &[
            "--mode",
            "codex",
            "--codex-app-server-socket",
            socket.to_str().unwrap(),
            "--attention-command",
            "-",
        ],
        br#"{"operation":"status"}"#,
    ));
    assert_eq!(status["safe_delivery_supported"], json!(true));
    assert_eq!(status["store"]["records"], json!(0));
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
    assert_eq!(
        fs::read(fixture.state.join("codex-inbox.json")).ok(),
        before
    );
    drop(queue);
}

#[test]
fn startup_cli_offline_configure_persists_without_token() {
    let fixture = CliFixture::new();
    let configured = successful_json(fixture.attention(
        "codex",
        br#"{"operation":"configure","settings":{"mode":"log","notices":"off"}}"#,
    ));
    assert_eq!(configured["durable"], json!(true));
    assert_eq!(configured["warning"], Value::Null);
    let status = successful_json(fixture.attention("codex", br#"{"operation":"status"}"#));
    assert_eq!(status["settings"]["mode"], json!("log"));
    assert_eq!(status["settings"]["notices"], json!("off"));
    assert!(!fixture.state.join("codex-inbox.lock").exists());
}

#[test]
fn startup_cli_rejects_invalid_oversized_and_source_validating_commands() {
    let fixture = CliFixture::new();
    rejected(
        fixture.attention("codex", b"{"),
        "invalid attention command",
    );
    rejected(
        fixture.attention("codex", &vec![b' '; 1_048_577]),
        "attention command exceeds one MiB",
    );
    rejected(
        fixture.attention("codex", br#"{"operation":"review","requested":1,"seed":1}"#),
        "source validation requires a Discord bot token",
    );
    assert_eq!(
        successful_json(fixture.attention("codex", br#"{"operation":"status"}"#))["store"]["records"],
        json!(0)
    );
}

#[test]
fn startup_cli_refuses_attention_store_owned_by_another_process() {
    let fixture = CliFixture::new();
    let store = AttentionStore::open(fixture.state.join("attention/records.json")).unwrap();
    rejected(
        fixture.attention("codex", br#"{"operation":"status"}"#),
        "another process owns attention store",
    );
    drop(store);
    assert_eq!(
        successful_json(fixture.attention("codex", br#"{"operation":"status"}"#))["store"]["records"],
        json!(0)
    );
}

#[test]
fn startup_cli_rejects_unknown_mode_and_malformed_thread_before_startup() {
    let fixture = CliFixture::new();
    for (arguments, message) in [
        (
            vec!["--mode", "unknown"],
            "invalid value 'unknown' for '--mode <MODE>'",
        ),
        (
            vec!["--codex-thread-id", "bad/thread"],
            "invalid Codex thread id",
        ),
    ] {
        let output = fixture.run(&arguments, b"");
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(
        successful_json(fixture.attention("claude-code", br#"{"operation":"status"}"#))["safe_delivery_supported"],
        json!(false)
    );
}
#[test]
fn startup_cli_invalid_log_filter_is_a_reported_error() {
    let fixture = CliFixture::new();
    let output = fixture.run(&["--log-level", "["], b"");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid log filter"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert_eq!(
        successful_json(fixture.attention("codex", br#"{"operation":"status"}"#))["safe_delivery_supported"],
        json!(true)
    );
}
