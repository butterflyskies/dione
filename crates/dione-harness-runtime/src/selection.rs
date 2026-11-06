//! Construction selection and resource-free harness metadata.
use super::{HarnessSession, HarnessSessionError};
use crate::StdioExitPolicy;
use camino::Utf8Path;
use dione_codex::CodexSessionOptions;
use dione_harness::AttentionPolicy;
/// Resource-free selection of the concrete provider session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum HarnessSelection {
    /// Emit Claude Code channel notifications on MCP stdout.
    #[default]
    ClaudeCode,
    /// Persist events for explicit pull through MCP tools.
    Codex,
    /// Emit versioned Dione events for the Oh My Pi extension.
    OhMyPi,
}
impl HarnessSelection {
    /// Selects whether MCP EOF shuts down or waits for cancellation.
    pub const fn stdio_exit_policy(self, resident_control: bool) -> StdioExitPolicy {
        match (self, resident_control) {
            (Self::Codex, true) => StdioExitPolicy::DetachUntilCancellation,
            _ => StdioExitPolicy::ShutdownProcess,
        }
    }
    /// Returns the selected provider attention policy without opening resources.
    pub const fn attention_policy(self) -> AttentionPolicy {
        match self {
            Self::ClaudeCode => dione_claude::ATTENTION_POLICY,
            Self::Codex => dione_codex::ATTENTION_POLICY,
            Self::OhMyPi => AttentionPolicy::Direct,
        }
    }
    /// Prepares the selected provider without starting delivery.
    pub async fn prepare(
        self,
        state_dir: &Utf8Path,
        options: CodexSessionOptions,
        omp_resident: bool,
    ) -> Result<HarnessSession, HarnessSessionError> {
        match self {
            Self::ClaudeCode => Ok(HarnessSession::claude()),
            Self::Codex => HarnessSession::codex(state_dir, options).await,
            Self::OhMyPi if omp_resident => HarnessSession::oh_my_pi_resident(state_dir).await,
            Self::OhMyPi => Ok(HarnessSession::oh_my_pi()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_codex_resident_detaches_on_stdio_close() {
        for mode in [HarnessSelection::ClaudeCode, HarnessSelection::OhMyPi] {
            for control in [false, true] {
                assert_eq!(
                    mode.stdio_exit_policy(control),
                    StdioExitPolicy::ShutdownProcess
                );
            }
        }
        assert_eq!(
            HarnessSelection::Codex.stdio_exit_policy(false),
            StdioExitPolicy::ShutdownProcess
        );
        assert_eq!(
            HarnessSelection::Codex.stdio_exit_policy(true),
            StdioExitPolicy::DetachUntilCancellation
        );
    }
    #[tokio::test]
    async fn claude_composition_uses_actual_codex_catalog() {
        let session = HarnessSession::claude();
        let connection = session.connection();
        for name in dione_codex::TOOL_NAMES {
            let result = connection
                .protocol()
                .call(name, &serde_json::json!({}))
                .await;
            let error = result
                .expect("actual Codex tool must be reserved")
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("{name} is only available in Codex mode")
            );
        }
        assert!(
            connection
                .protocol()
                .call("unknown-app-tool", &serde_json::json!({}))
                .await
                .is_none()
        );
    }
}
