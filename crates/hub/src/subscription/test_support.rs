//! Test helpers for the subscription subsystem; the login model's own are
//! in `hub_core::logins::test_support`, re-exported here.

pub(crate) use crate::hub_core::logins::test_support::*;

/// Running a test body with a CLI's program pointed at a stand-in.
pub(crate) trait Scoped {
    /// Run `run` with this CLI's program at `program`.
    async fn scoped<T>(
        self,
        program: std::path::PathBuf,
        run: impl std::future::Future<Output = T>,
    ) -> T;
}

impl Scoped for crate::hub_core::subscriptions::CliKind {
    async fn scoped<T>(
        self,
        program: std::path::PathBuf,
        run: impl std::future::Future<Output = T>,
    ) -> T {
        match self {
            Self::ClaudeCode => crate::claude_code::tests::CLAUDE.scope(program, run).await,
            Self::Codex => {
                crate::coding_engine::codex::tests::CODEX
                    .scope(program, run)
                    .await
            }
        }
    }
}
