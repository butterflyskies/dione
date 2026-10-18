#[cfg(unix)]
use super::SecretEnvironmentError;
use super::{StartupErrorKind, drain_shutdown, prepare_teams, validate_teams_control};
use crate::harness::{
    AttentionPolicy, CodexSessionOptions, HarnessSelection, HarnessSession, HarnessSessionError,
    HarnessSessionErrorKind,
};
use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use std::{collections::BTreeMap, env::VarError, ffi::OsString, net::SocketAddr, time::Duration};
#[cfg(unix)]
use std::{error::Error, os::unix::ffi::OsStringExt};
use tokio::{
    sync::{mpsc, oneshot},
    time::timeout,
};

#[tokio::test]
async fn shutdown_join_failure_waits_for_all_later_owned_drains() {
    assert_shutdown_drains(true).await;
}

#[tokio::test]
async fn shutdown_success_waits_for_all_owned_drains() {
    assert_shutdown_drains(false).await;
}

async fn assert_shutdown_drains(fail: bool) {
    let teams = tokio::spawn(async move {
        assert!(!fail, "injected Teams join failure");
    });
    let (stage_tx, mut stage_rx) = mpsc::channel(4);
    let (resident_release, resident_gate) = oneshot::channel();
    let (resident_done_tx, resident_done_rx) = oneshot::channel();
    let resident_stage = stage_tx.clone();
    let resident = tokio::spawn(async move {
        resident_stage.send("resident").await.unwrap();
        resident_gate.await.unwrap();
        resident_done_tx.send(()).unwrap();
    });
    let (mcp_release, mcp_gate) = oneshot::channel();
    let mcp_stage = stage_tx.clone();
    let mcp = tokio::spawn(async move {
        resident_done_rx.await.unwrap();
        mcp_stage.send("mcp").await.unwrap();
        mcp_gate.await.unwrap();
    });
    let (replies_release, replies_gate) = oneshot::channel();
    let replies_stage = stage_tx.clone();
    let replies = async move {
        replies_stage.send("replies").await.unwrap();
        replies_gate.await.unwrap();
    };
    let (session_release, session_gate) = oneshot::channel();
    let session = async move {
        stage_tx.send("session").await.unwrap();
        session_gate.await.unwrap();
        if fail {
            Err(HarnessSessionError {
                kind: HarnessSessionErrorKind::TeamsUnavailable,
            })
        } else {
            Ok(())
        }
    };
    let mut drain = tokio::spawn(drain_shutdown(
        Some(teams),
        Some(resident),
        mcp,
        replies,
        session,
    ));
    for (expected, release) in [
        ("resident", resident_release),
        ("mcp", mcp_release),
        ("replies", replies_release),
        ("session", session_release),
    ] {
        let stage = timeout(Duration::from_secs(5), async {
            tokio::select! {
                biased;
                result = &mut drain => panic!("shutdown returned before {expected} drained: {result:?}"),
                stage = stage_rx.recv() => stage,
            }
        })
        .await
        .unwrap();
        assert_eq!(stage, Some(expected));
        assert!(!drain.is_finished());
        release.send(()).unwrap();
    }
    let result = timeout(Duration::from_secs(5), drain)
        .await
        .unwrap()
        .unwrap();
    if fail {
        match result.unwrap_err().kind {
            StartupErrorKind::TeamsDrain(source) => assert!(source.is_panic()),
            other => panic!("first typed shutdown failure was lost: {other:?}"),
        }
    } else {
        result.unwrap();
    }
}

fn teams_config() -> crate::config::TeamsConfig {
    crate::config::TeamsConfig {
        allowed_conversations: ["conversation/with space".parse().unwrap()].into(),
    }
}

fn environment(values: &BTreeMap<&str, &str>, key: &str) -> Result<String, VarError> {
    values
        .get(key)
        .map(|value| (*value).to_owned())
        .ok_or(VarError::NotPresent)
}

#[tokio::test]
async fn startup_teams_absent_and_non_unicode_app_id_skip_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let state = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
    let session = HarnessSession::claude();
    for error in [
        VarError::NotPresent,
        VarError::NotUnicode(OsString::from("opaque")),
    ] {
        let prepared = prepare_teams(
            &session,
            &state,
            &crate::config::TeamsConfig::default(),
            |key| {
                assert_eq!(key, "TEAMS_APP_ID");
                Err(error.clone())
            },
        )
        .await
        .unwrap();
        assert!(prepared.is_none());
    }
    assert_eq!(std::fs::read_dir(&state).unwrap().count(), 0);
}

#[tokio::test]
async fn startup_teams_requires_explicit_nonempty_conversation_scope() {
    let dir = tempfile::tempdir().unwrap();
    let state = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
    let session = HarnessSession::claude();
    for raw in ["", "[teams]\nallowed_conversations = []"] {
        let config: crate::config::Config = toml::from_str(raw).unwrap();
        let error = prepare_teams(&session, &state, &config.teams, |key| {
            assert_eq!(key, "TEAMS_APP_ID");
            Ok("app".to_owned())
        })
        .await
        .err()
        .unwrap();
        assert!(matches!(
            error.kind,
            StartupErrorKind::TeamsConversationsRequired
        ));
    }
    assert_eq!(std::fs::read_dir(&state).unwrap().count(), 0);
}

#[test]
fn teams_conversation_config_preserves_native_ids_and_rejects_invalid_ids() {
    let config: crate::config::Config =
        toml::from_str("[teams]\nallowed_conversations = ['19:room/with space@thread.v2']")
            .unwrap();
    assert_eq!(
        config
            .teams
            .allowed_conversations
            .iter()
            .next()
            .unwrap()
            .as_str(),
        "19:room/with space@thread.v2"
    );
    for invalid in [
        "[teams]\nallowed_conversations = ['']",
        "[teams]\nallowed_conversations = [42]",
    ] {
        assert!(toml::from_str::<crate::config::Config>(invalid).is_err());
    }
}

#[tokio::test]
async fn startup_teams_capability_precedes_required_environment() {
    let dir = tempfile::tempdir().unwrap();
    let state = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
    let session = HarnessSession::claude();
    let error = prepare_teams(&session, &state, &teams_config(), |key| {
        assert_eq!(key, "TEAMS_APP_ID");
        Ok("app".to_owned())
    })
    .await
    .err()
    .unwrap();
    assert!(matches!(error.kind, StartupErrorKind::Harness(_)));
    assert_eq!(
        error.to_string(),
        "resident Teams ingress requires a prepared Codex or OMP session"
    );
}

#[tokio::test]
async fn startup_teams_required_environment_and_listen_precedence() {
    let dir = tempfile::tempdir().unwrap();
    let state = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
    let session = HarnessSession::codex(
        &state,
        CodexSessionOptions::new(
            None,
            None,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        ),
    )
    .await
    .unwrap();
    let mut values = BTreeMap::from([("TEAMS_APP_ID", "app")]);
    for (key, value, message) in [
        (
            "TEAMS_TENANT_ID",
            "tenant",
            "TEAMS_TENANT_ID is required when TEAMS_APP_ID is set",
        ),
        (
            "TEAMS_CLIENT_SECRET",
            "secret",
            "TEAMS_CLIENT_SECRET is required when TEAMS_APP_ID is set",
        ),
        (
            "TEAMS_ALLOWED_SERVICE_HOST",
            "smba.trafficmanager.net",
            "TEAMS_ALLOWED_SERVICE_HOST is required when TEAMS_APP_ID is set",
        ),
    ] {
        let error = prepare_teams(&session, &state, &teams_config(), |key| {
            environment(&values, key)
        })
        .await
        .err()
        .unwrap();
        assert_eq!(error.to_string(), message);
        values.insert(key, value);
    }
    values.insert("TEAMS_LISTEN_ADDR", "invalid");
    let error = prepare_teams(&session, &state, &teams_config(), |key| {
        environment(&values, key)
    })
    .await
    .err()
    .unwrap();
    assert!(matches!(error.kind, StartupErrorKind::TeamsListen(_)));
    assert_eq!(error.to_string(), "TEAMS_LISTEN_ADDR is invalid");
    values.remove("TEAMS_LISTEN_ADDR");
    let prepared = prepare_teams(&session, &state, &teams_config(), |key| {
        environment(&values, key)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        prepared.listen,
        "127.0.0.1:3978".parse::<SocketAddr>().unwrap()
    );
    prepared.bridge.drain_replies().await;
    drop(prepared);
    let prepared = prepare_teams(&session, &state, &teams_config(), |key| {
        if key == "TEAMS_LISTEN_ADDR" {
            Err(VarError::NotUnicode(OsString::from("opaque")))
        } else {
            environment(&values, key)
        }
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        prepared.listen,
        "127.0.0.1:3978".parse::<SocketAddr>().unwrap()
    );
    drop(prepared);
    std::fs::write(state.join("teams-reply-authorities.json"), b"invalid").unwrap();
    let error = prepare_teams(&session, &state, &teams_config(), |key| {
        environment(&values, key)
    })
    .await
    .err()
    .unwrap();
    assert!(matches!(error.kind, StartupErrorKind::TeamsRestore(_)));
    assert_eq!(error.to_string(), "failed to restore Teams reply authority");
}

#[tokio::test]
async fn startup_selection_policy_matches_prepared_connections() {
    let dir = tempfile::tempdir().unwrap();
    let state = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
    for (selection, expected) in [
        (HarnessSelection::ClaudeCode, AttentionPolicy::Direct),
        (HarnessSelection::OhMyPi, AttentionPolicy::Direct),
        (HarnessSelection::Codex, AttentionPolicy::SafeTurn),
    ] {
        assert_eq!(selection.attention_policy(), expected);
        let session = selection
            .prepare(
                &state,
                CodexSessionOptions::new(
                    None,
                    None,
                    crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                        crate::core::ProviderId::new("discord").unwrap(),
                        crate::core::ModuleSlotId::new("primary").unwrap(),
                    )),
                ),
                false,
            )
            .await
            .unwrap();
        assert_eq!(session.connection().attention_policy(), expected);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn startup_teams_secret_errors_never_retain_secret_values() {
    let dir = tempfile::tempdir().unwrap();
    let state = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
    let session = HarnessSession::codex(
        &state,
        CodexSessionOptions::new(
            None,
            None,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        ),
    )
    .await
    .unwrap();
    let canary = "secret-canary-7feca479";
    let mut malformed = canary.as_bytes().to_vec();
    malformed.push(0xff);
    for (cause, expected) in [
        (
            VarError::NotUnicode(OsString::from_vec(malformed)),
            SecretEnvironmentError::NonUnicode,
        ),
        (VarError::NotPresent, SecretEnvironmentError::Missing),
    ] {
        let error = prepare_teams(&session, &state, &teams_config(), |key| match key {
            "TEAMS_APP_ID" => Ok("app".to_owned()),
            "TEAMS_TENANT_ID" => Ok("tenant".to_owned()),
            "TEAMS_CLIENT_SECRET" => Err(cause.clone()),
            _ => panic!("secret failure must precede later environment reads"),
        })
        .await
        .err()
        .unwrap();
        let StartupErrorKind::TeamsSecret(_) = &error.kind else {
            panic!("expected a typed Teams secret failure");
        };
        assert_eq!(
            error.to_string(),
            "TEAMS_CLIENT_SECRET is required when TEAMS_APP_ID is set"
        );
        let mut source: Option<&dyn Error> = Some(&error);
        while let Some(current) = source {
            assert!(!current.to_string().contains(canary));
            assert!(!format!("{current:?}").contains(canary));
            source = current.source();
        }
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<SecretEnvironmentError>(),
            Some(&expected)
        );
    }
}

#[test]
fn oh_my_pi_cli_selection_preserves_existing_default() {
    assert_eq!(
        super::Cli::try_parse_from(["dione", "--mode", "oh-my-pi"])
            .unwrap()
            .mode,
        HarnessSelection::OhMyPi
    );
    assert_eq!(
        super::Cli::try_parse_from(["dione"]).unwrap().mode,
        HarnessSelection::ClaudeCode
    );
}
#[test]
fn teams_omp_requires_control_but_direct_omp_and_codex_do_not() {
    let error = validate_teams_control(HarnessSelection::OhMyPi, true, None).unwrap_err();
    assert!(matches!(error.kind, StartupErrorKind::OmpControlRequired));
    assert!(validate_teams_control(HarnessSelection::OhMyPi, false, None).is_ok());
    assert!(validate_teams_control(HarnessSelection::Codex, true, None).is_ok());
    assert!(
        validate_teams_control(
            HarnessSelection::OhMyPi,
            true,
            Some(Utf8Path::new("/private/control.sock"))
        )
        .is_ok()
    );
}
