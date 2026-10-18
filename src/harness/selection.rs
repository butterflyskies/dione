//! Construction selection and resource-free harness metadata.
use super::{AttentionPolicy, CodexSessionOptions, HarnessSession, HarnessSessionError};
use crate::mcp::server::StdioExitPolicy;
use camino::Utf8Path;
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub(crate) enum HarnessSelection {
    /// Emit Claude Code channel notifications on MCP stdout.
    #[default]
    ClaudeCode,
    /// Persist events for explicit pull through MCP tools.
    Codex,
    /// Emit versioned Dione events for the Oh My Pi extension.
    OhMyPi,
}
impl HarnessSelection {
    pub(crate) const fn stdio_exit_policy(self, resident_control: bool) -> StdioExitPolicy {
        match (self, resident_control) {
            (Self::Codex, true) => StdioExitPolicy::DetachUntilCancellation,
            _ => StdioExitPolicy::ShutdownProcess,
        }
    }
    pub(crate) const fn attention_policy(self) -> AttentionPolicy {
        match self {
            Self::ClaudeCode => super::claude::ATTENTION_POLICY,
            Self::Codex => super::codex::ATTENTION_POLICY,
            Self::OhMyPi => super::oh_my_pi::ATTENTION_POLICY,
        }
    }
    pub(crate) async fn prepare(
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
}
