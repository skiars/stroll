use std::path::Path;

use anyhow::{Context, Result, ensure};
use rust_i18n::t;
use serde::{Deserialize, Serialize};

use super::{
    HistoryTip, Operation, Owner, ResetMode, apply_reset, check_published_range, check_tip, head,
    object_id, range_count, read, ready, with_saved_changes,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum RewriteKind {
    Squash,
    Rebase,
    Amend,
    CherryPick,
}

impl RewriteKind {
    fn key(self) -> &'static str {
        match self {
            Self::Squash => "squash",
            Self::Rebase => "rebase",
            Self::Amend => "amend",
            Self::CherryPick => "cherry-pick",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RewriteRecord {
    pub kind: RewriteKind,
    pub(super) before: String,
    after: HistoryTip,
}

fn reference(branch: &str, after: &str) -> String {
    format!("refs/worktree/stroll/undo/{after}/{branch}")
}

pub(super) fn record_success(root: &Path, before: &HistoryTip, kind: RewriteKind) -> Result<()> {
    let after = head(root)?.context(t!("git.history_edit_stale"))?;
    if after == before.oid {
        return Ok(());
    }
    check_tip(
        root,
        &HistoryTip {
            branch: before.branch.clone(),
            oid: after.clone(),
        },
    )?;
    // Pin the pre-operation commit and attach one reflog message to the operation's result.
    read(
        root,
        [
            "update-ref",
            "--create-reflog",
            "-m",
            &format!("stroll: {} {}", kind.key(), before.branch),
            &reference(&before.branch, &after),
            &before.oid,
        ],
    )?;
    Ok(())
}

pub(super) fn record_for(root: &Path, branch: &str, after: &str) -> Option<RewriteRecord> {
    if !object_id(after) || !branch.starts_with("refs/heads/") {
        return None;
    }
    let reference = reference(branch, after);
    let output = read(
        root,
        [
            "reflog",
            "show",
            "-1",
            "--format=%H%x00%gs",
            &reference,
            "--",
        ],
    )
    .ok()?;
    let (before, message) = std::str::from_utf8(&output)
        .ok()?
        .trim_end()
        .split_once('\0')?;
    let kind = [
        RewriteKind::Squash,
        RewriteKind::Rebase,
        RewriteKind::Amend,
        RewriteKind::CherryPick,
    ]
    .into_iter()
    .find(|kind| message == format!("stroll: {} {branch}", kind.key()))?;
    object_id(before).then(|| RewriteRecord {
        kind,
        before: before.to_owned(),
        after: HistoryTip {
            branch: branch.to_owned(),
            oid: after.to_owned(),
        },
    })
}

pub(super) fn undo(root: &Path, record: RewriteRecord, confirmed: bool) -> Result<()> {
    let state = ready(root, &record.after)?;
    ensure!(
        record_for(root, &record.after.branch, &record.after.oid).as_ref() == Some(&record),
        t!("git.history_edit_stale")
    );
    if !confirmed {
        let removed = range_count(root, &record.after.oid, Some(&record.before), false)?;
        check_published_range(
            root,
            &record.after.oid,
            Some(&record.before),
            removed,
            false,
        )?;
    }
    let touched = read(
        root,
        [
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            &record.after.oid,
            &record.before,
            "--",
        ],
    )?;
    if touched.is_empty() || record.kind == RewriteKind::Amend {
        // Amend's patch becomes staged again with the retained index.
        apply_reset(root, &record.after, &record.before, ResetMode::Soft)?;
    } else if record.kind == RewriteKind::CherryPick {
        // A keep reset preserves unrelated worktree edits but can unstage changes.
        ensure!(!state.ready, t!("git.history_edit_cherry_pick_undo_staged"));
        super::backup::check_ignored_paths(root, &touched)?;
        apply_reset(root, &record.after, &record.before, ResetMode::Keep)?;
    } else {
        let owner = Owner {
            tip: record.after.clone(),
            operation: Operation::UndoRewrite,
            rewrite: None,
        };
        with_saved_changes(root, &owner, &touched, |_| {
            apply_reset(root, &record.after, &record.before, ResetMode::Keep)
        })?;
    }
    read(
        root,
        [
            "update-ref",
            "-d",
            &reference(&record.after.branch, &record.after.oid),
            &record.before,
        ],
    )?;
    Ok(())
}
