use std::{sync::atomic::Ordering, time::Duration};

use anyhow::{Result, ensure};
use rust_i18n::t;

use super::{
    Cancellation, Snapshot,
    provider::{Failure, Provider, Thinking},
};
use crate::git::{Backend, CommitState, StagedCommitContext};

const PROMPT: &str = include_str!("prompts/commit_message.txt");

impl Snapshot {
    pub(crate) async fn commit_message(
        &self,
        git: Backend,
        expected: CommitState,
    ) -> Result<String> {
        let snapshot = self.clone();
        let cancellation = Cancellation::default();
        let cancelled = cancellation.flag();
        blocking::unblock(move || {
            let live = || !cancelled.load(Ordering::Relaxed) && snapshot.is_current();
            let context = git
                .staged_commit_context(expected.clone())?
                .ok_or_else(|| anyhow::anyhow!(t!("ai.no_commit_context")))?;
            let context = prompt_context(context)?;
            ensure!(live(), t!("ai.request_changed"));
            let provider = Provider::open(&snapshot.settings, &snapshot.service.credential_scope)
                .map_err(|failure| anyhow::anyhow!(failure.localized()))?;
            ensure!(live(), t!("ai.request_changed"));
            let message = provider
                .complete(
                    PROMPT,
                    &context,
                    1024,
                    Duration::from_secs(30),
                    64 * 1024,
                    Thinking::Default,
                )
                .and_then(|message| validate_message(&message))
                .map_err(|failure| anyhow::anyhow!(failure.localized()))?;
            ensure!(live(), t!("ai.request_changed"));
            ensure!(git.commit_state()? == expected, t!("git.worktree_changed"));
            Ok(message)
        })
        .await
    }
}

fn prompt_context(context: StagedCommitContext) -> Result<String> {
    use std::fmt::Write as _;
    let mut summary = String::from("Staged file changes (status, path, rename destination)\n");
    for file in context.files {
        writeln!(summary, "{}", serde_json::to_string(&file)?)?;
    }
    write!(
        summary,
        "\nOmitted file entries: {}. Patch truncated: {}.\n\nStaged patch\n{}",
        context.omitted, context.truncated, context.patch,
    )?;
    Ok(summary)
}

fn validate_message(raw: &str) -> Result<String, Failure> {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let message = normalized.trim();
    if message.is_empty()
        || message.starts_with("```")
        || message.len() > 8_000
        || message
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return Err(Failure::ResponseShape);
    }
    Ok(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_commit_message_and_rejects_invalid_drafts() {
        assert_eq!(
            validate_message("  fix(git): preserve the index\r\n\r\nKeep the staged version.\n")
                .unwrap(),
            "fix(git): preserve the index\n\nKeep the staged version."
        );
        for malformed in [
            "",
            "\0fix(git): fix this",
            "```\nfix: use the current index\n```",
        ] {
            assert_eq!(validate_message(malformed), Err(Failure::ResponseShape));
        }
    }
}
