mod backup;
mod rewrite;

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rust_i18n::t;
use serde::{Deserialize, Serialize};

use super::{Backend, capture_repository, command, commit_state, head, object_id, read, text};
use super::{operations::OperationLock, repo::git_dir};
use backup::BackupPhase;
pub(crate) use rewrite::{RewriteKind, RewriteRecord};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HistoryTip {
    pub branch: String,
    pub oid: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HistoryAction {
    Pick,
    Reword,
    Squash,
    Fixup,
    Drop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryEntry {
    pub oid: String,
    pub subject: String,
    pub message: String,
    pub action: HistoryAction,
}

#[derive(Clone)]
pub(crate) struct HistoryPlan {
    pub tip: HistoryTip,
    pub base: Option<String>,
    oldest: String,
    pub entries: Vec<HistoryEntry>,
    pub rewrite: Option<RewriteKind>,
}

impl HistoryPlan {
    pub fn valid(&self) -> bool {
        let mut picked = false;
        for entry in &self.entries {
            match entry.action {
                HistoryAction::Drop => {}
                HistoryAction::Pick | HistoryAction::Reword => picked = true,
                HistoryAction::Squash | HistoryAction::Fixup if !picked => return false,
                HistoryAction::Squash | HistoryAction::Fixup => {}
            }
        }
        picked || self.base.is_some()
    }
}

pub(crate) fn valid_message(message: &str) -> bool {
    !message.trim().is_empty() && !message.contains('\0')
}

#[derive(Clone)]
pub(crate) enum HistoryEdit {
    Rebase(HistoryPlan),
    Amend(HistoryTip, super::CommitState, String),
    UndoRewrite(RewriteRecord),
    Undo(HistoryTip),
    Reset(ResetPlan, ResetMode),
    Revert(HistoryTip, String),
    Resume(HistoryProgress, HistoryResolution),
}

#[derive(Debug)]
pub(crate) struct PublishedHistory;

impl std::fmt::Display for PublishedHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&t!("git.history_edit_published"))
    }
}

impl std::error::Error for PublishedHistory {}

impl HistoryEdit {
    fn recording_tip(&self) -> Option<&HistoryTip> {
        match self {
            Self::Rebase(plan) if plan.rewrite.is_some() => Some(&plan.tip),
            Self::Amend(tip, ..) => Some(tip),
            Self::Resume(
                progress,
                HistoryResolution::Continue | HistoryResolution::RestoreSaved,
            ) if progress.owner.rewrite.is_some() => Some(&progress.owner.tip),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ResetMode {
    #[default]
    Soft,
    Mixed,
    Keep,
    Hard,
}

#[derive(Clone)]
pub(crate) struct ResetPlan {
    tip: HistoryTip,
    pub target: String,
    pub removed: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Operation {
    Rebase,
    Revert(String),
    UndoRewrite,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Owner {
    tip: HistoryTip,
    operation: Operation,
    #[serde(default)]
    rewrite: Option<RewriteKind>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryProgress {
    owner: Owner,
    pub commit: Option<String>,
    pub conflicts: Vec<String>,
    pub saved: Option<SavedChanges>,
}

impl HistoryProgress {
    pub fn is_rebase(&self) -> bool {
        self.owner.operation == Operation::Rebase
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SavedChanges {
    pub reference: String,
    pub can_restore: bool,
}

#[derive(Clone)]
pub(crate) enum HistoryResolution {
    Continue,
    Abort,
    Stage(String),
    RestoreSaved,
    KeepBackup,
}

impl Backend {
    pub(crate) fn supports_history_edit(&self) -> bool {
        self.ssh.is_none()
    }

    pub(crate) fn history_plan(&self, tip: HistoryTip, targets: &[String]) -> Result<HistoryPlan> {
        ensure!(self.supports_history_edit(), t!("git.history_edit_local"));
        prepare(&self.root, tip, targets)
    }

    pub(crate) fn reset_plan(&self, tip: HistoryTip, target: &str) -> Result<ResetPlan> {
        ensure!(self.supports_history_edit(), t!("git.history_edit_local"));
        prepare_reset(&self.root, tip, target)
    }

    pub(crate) fn amend_message(
        &self,
        tip: &HistoryTip,
        expected: &super::CommitState,
    ) -> Result<String> {
        ensure!(self.supports_history_edit(), t!("git.history_edit_local"));
        check_amend(&self.root, tip, expected)?;
        let message = read(
            &self.root,
            ["show", "-s", "--no-notes", "--format=%B", &tip.oid, "--"],
        )?;
        Ok(text(&message).trim_end_matches('\n').to_owned())
    }

    pub(crate) fn rewrite_record(
        &self,
        branch: Option<&str>,
        oid: Option<&str>,
    ) -> Option<RewriteRecord> {
        if !self.supports_history_edit() {
            return None;
        }
        rewrite::record_for(&self.root, branch?, oid?)
    }

    pub(crate) fn recover_history_backup(&self) -> Result<bool> {
        if !self.supports_history_edit() {
            return Ok(false);
        }
        let result = recover_backup(&self.root);
        if !matches!(result, Ok(false)) {
            self.invalidate();
        }
        result
    }

    pub(crate) fn history_progress(&self) -> Option<HistoryProgress> {
        self.supports_history_edit()
            .then(|| progress(&self.root))
            .flatten()
    }

    pub(crate) fn edit_history(
        &self,
        edit: HistoryEdit,
    ) -> Result<(Option<String>, Option<RewriteRecord>)> {
        self.edit_history_with_confirmation(edit, false)
    }

    pub(crate) fn edit_history_with_confirmation(
        &self,
        edit: HistoryEdit,
        confirmed: bool,
    ) -> Result<(Option<String>, Option<RewriteRecord>)> {
        ensure!(self.supports_history_edit(), t!("git.history_edit_local"));
        let recording = edit.recording_tip().cloned();
        let result = execute_history_edit(&self.root, edit, confirmed);
        self.invalidate();
        result?;
        let head = head(&self.root)?;
        let record = recording.and_then(|tip| {
            let after = head.as_deref().filter(|head| *head != tip.oid)?;
            self.rewrite_record(Some(&tip.branch), Some(after))
        });
        Ok((head, record))
    }
}

fn check_tip(root: &Path, tip: &HistoryTip) -> Result<()> {
    ensure!(
        object_id(&tip.oid) && tip.branch.starts_with("refs/heads/"),
        t!("git.history_edit_local")
    );
    let checkout = fs::read_to_string(git_dir(root).join("HEAD"))?;
    ensure!(
        checkout.trim_end().strip_prefix("ref: ") == Some(tip.branch.as_str())
            && head(root)?.as_ref() == Some(&tip.oid),
        t!("git.history_edit_stale")
    );
    Ok(())
}

fn ready(root: &Path, tip: &HistoryTip) -> Result<()> {
    check_tip(root, tip)?;
    ensure!(!commit_state(root)?.blocked, t!("git.history_edit_blocked"));
    ensure!(
        backup::load(root)?.is_none(),
        t!("git.history_backup_pending")
    );
    Ok(())
}

fn prepare_reset(root: &Path, tip: HistoryTip, target: &str) -> Result<ResetPlan> {
    ready(root, &tip)?;
    ensure!(
        object_id(target) && target != tip.oid,
        t!("git.reset_ancestor")
    );
    let mainline = read(root, ["rev-list", "--first-parent", &tip.oid, "--"])?;
    ensure!(
        mainline
            .split(|&byte| byte == b'\n')
            .any(|oid| oid == target.as_bytes()),
        t!("git.reset_ancestor")
    );
    let removed = range_count(root, &tip.oid, Some(target), false)?;
    Ok(ResetPlan {
        tip,
        target: target.to_owned(),
        removed,
    })
}

fn range_count(root: &Path, tip: &str, base: Option<&str>, private_only: bool) -> Result<usize> {
    let excluded = base.map(|oid| format!("^{oid}"));
    let remotes = private_only
        .then_some(["--not", "--remotes"])
        .into_iter()
        .flatten();
    let count = read(
        root,
        ["rev-list", "--count", tip]
            .into_iter()
            .chain(excluded.as_deref())
            .chain(remotes)
            .chain(["--"]),
    )?;
    Ok(std::str::from_utf8(&count)?.trim().parse()?)
}

fn check_published_range(
    root: &Path,
    tip: &str,
    base: Option<&str>,
    count: usize,
    confirmed: bool,
) -> Result<()> {
    if !confirmed && range_count(root, tip, base, true)? != count {
        return Err(PublishedHistory.into());
    }
    Ok(())
}

fn reset(root: &Path, plan: ResetPlan, mode: ResetMode, confirmed: bool) -> Result<()> {
    let current = prepare_reset(root, plan.tip, &plan.target)?;
    check_published_range(
        root,
        &current.tip.oid,
        Some(&current.target),
        current.removed,
        confirmed,
    )?;
    apply_reset(root, &current.tip, &current.target, mode)
}

fn apply_reset(root: &Path, tip: &HistoryTip, target: &str, mode: ResetMode) -> Result<()> {
    check_tip(root, tip)?;
    if mode == ResetMode::Soft {
        // Compare-and-swap the tip and record the original tip in the same transaction.
        use std::io::{Seek as _, Write as _};
        let mut input = tempfile::tempfile()?;
        writeln!(input, "update HEAD {target} {}", tip.oid)?;
        writeln!(input, "update ORIG_HEAD {}", tip.oid)?;
        input.rewind()?;
        let mut git = command(
            root,
            ["update-ref", "-m", "stroll: reset --soft", "--stdin"],
        );
        git.stdin(input);
        capture_repository(git)?;
    } else {
        let flag = match mode {
            ResetMode::Mixed => "--mixed",
            ResetMode::Keep => "--keep",
            ResetMode::Hard => "--hard",
            ResetMode::Soft => unreachable!(),
        };
        read(
            root,
            ["reset", flag, "--no-recurse-submodules", target, "--"],
        )?;
    }
    Ok(())
}

fn take_entry(root: &Path, tip: &HistoryTip, oldest: &str) -> Result<HistoryPlan> {
    let oldest = super::commit(root, oldest)?;
    let base = oldest.parents.first().cloned();
    let excluded = base.as_ref().map(|oid| format!("^{oid}"));
    let range = || std::iter::once(tip.oid.as_str()).chain(excluded.as_deref());
    let output = read(
        root,
        [
            "log",
            "--reverse",
            "--first-parent",
            "--no-notes",
            "--encoding=UTF-8",
            "-z",
            "--format=%H%x00%P%x00%s%x00%B",
        ]
        .into_iter()
        .chain(range())
        .chain(["--"]),
    )?;
    let fields = output
        .strip_suffix(&[0])
        .unwrap_or(&output)
        .split(|byte| *byte == 0)
        .collect::<Vec<_>>();
    let (rows, remainder) = fields.as_chunks::<4>();
    ensure!(remainder.is_empty(), t!("git.history_edit_linear"));
    let mut parent = base.as_deref();
    for [oid, parents, _, _] in rows {
        ensure!(
            std::str::from_utf8(parents)? == parent.unwrap_or_default(),
            t!("git.history_edit_linear")
        );
        parent = Some(std::str::from_utf8(oid)?);
    }
    ensure!(
        rows.first()
            .is_some_and(|row| row[0] == oldest.oid.as_bytes())
            && parent == Some(tip.oid.as_str()),
        t!("git.history_edit_linear")
    );
    let entries = rows
        .iter()
        .map(|[oid, _, subject, message]| HistoryEntry {
            oid: text(oid),
            subject: text(subject),
            message: text(message).trim_end_matches('\n').to_owned(),
            action: HistoryAction::Pick,
        })
        .collect();
    Ok(HistoryPlan {
        tip: tip.clone(),
        oldest: oldest.oid,
        base,
        entries,
        rewrite: None,
    })
}

fn prepare(root: &Path, tip: HistoryTip, targets: &[String]) -> Result<HistoryPlan> {
    ready(root, &tip)?;
    ensure!(
        !targets.is_empty() && targets.iter().all(|oid| object_id(oid)),
        t!("git.invalid_commit_id")
    );
    // The selection may be filtered or ordered by commit date in the history pane.
    let selected = targets.iter().map(String::as_str).collect::<HashSet<_>>();
    let oldest = if let [oid] = targets {
        oid.clone()
    } else {
        let history = read(root, ["rev-list", "--first-parent", &tip.oid, "--"])?;
        let history = std::str::from_utf8(&history)?;
        let mut found = history.lines().filter(|oid| selected.contains(oid));
        let first = found.next().context(t!("git.history_edit_linear"))?;
        found.last().unwrap_or(first).to_owned()
    };
    let plan = take_entry(root, &tip, &oldest)?;
    ensure!(
        plan.entries
            .iter()
            .filter(|entry| selected.contains(entry.oid.as_str()))
            .count()
            == selected.len(),
        t!("git.history_edit_linear")
    );
    Ok(plan)
}

fn directory(root: &Path) -> PathBuf {
    git_dir(root).join("stroll/history-edit")
}

fn owner_at(path: &Path) -> Option<Owner> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn active_commit(root: &Path, owner: &Owner) -> Option<Option<String>> {
    let git = git_dir(root);
    let matches_file = |path, value: &str| {
        fs::read_to_string(git.join(path)).is_ok_and(|text| text.trim_end() == value)
    };
    match &owner.operation {
        Operation::Rebase => {
            let attached = owner_at(&git.join("rebase-merge/git-rebase-todo.stroll"));
            if attached.as_ref() != Some(owner)
                || !matches_file("rebase-merge/orig-head", &owner.tip.oid)
                || !matches_file("rebase-merge/head-name", &owner.tip.branch)
            {
                return None;
            }
            Some(
                fs::read_to_string(git.join("REBASE_HEAD"))
                    .ok()
                    .map(|oid| oid.trim_end().to_owned()),
            )
        }
        Operation::UndoRewrite => None,
        Operation::Revert(oid) => (matches_file("REVERT_HEAD", oid)
            && check_tip(root, &owner.tip).is_ok())
        .then(|| Some(oid.clone())),
    }
}

fn progress(root: &Path) -> Option<HistoryProgress> {
    let backup = backup::load(root).ok().flatten();
    let owner = backup
        .as_ref()
        .and_then(|backup| backup.owner.clone())
        .or_else(|| owner_at(&directory(root).join("owner")))?;
    let active = active_commit(root, &owner);
    let saved = if active.is_none() {
        let backup = backup?;
        Some(SavedChanges {
            reference: backup.reference,
            can_restore: backup.phase == BackupPhase::Ready,
        })
    } else {
        None
    };
    let conflicts = super::worktree_status(root)
        .ok()?
        .into_iter()
        .filter_map(|(path, state)| state.conflict.then_some(path))
        .collect();
    Some(HistoryProgress {
        owner,
        commit: active.flatten(),
        conflicts,
        saved,
    })
}

fn rewrite_paths<'a>(root: &Path, commits: impl Iterator<Item = &'a str>) -> Result<Vec<u8>> {
    use std::io::{Seek as _, Write as _};
    let mut input = tempfile::tempfile()?;
    for commit in commits {
        writeln!(input, "{commit}")?;
    }
    input.rewind()?;
    let mut git = command(
        root,
        [
            "diff-tree",
            "--stdin",
            "--root",
            "-r",
            "--no-commit-id",
            "--no-renames",
            "--name-only",
            "-z",
        ],
    );
    git.stdin(input);
    capture_repository(git)
}

fn settle(root: &Path, owner: &Owner, result: Result<()>) -> Result<()> {
    if active_commit(root, owner).is_some() {
        return result;
    }
    let recorded = match owner.rewrite.filter(|_| result.is_ok()) {
        Some(kind) => rewrite::record_success(root, &owner.tip, kind),
        None => Ok(()),
    };
    let restore = (|| {
        if let Some(backup) = backup::load(root)? {
            ensure!(
                backup.restore(root, owner)?,
                t!("git.history_backup_pending")
            );
        }
        clear_operation(root)
    })();
    restore.and(result).and(recorded)
}

fn clear_operation(root: &Path) -> Result<()> {
    match fs::remove_dir_all(directory(root)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    backup::forget(root)
}

fn recover_backup(root: &Path) -> Result<bool> {
    if !backup::load(root)?.is_some_and(|backup| backup.phase == BackupPhase::Ready) {
        return Ok(false);
    }
    let _lock = OperationLock::acquire(root)?;
    let Some(backup) = backup::load(root)? else {
        return Ok(false);
    };
    let Some(owner) = backup
        .owner
        .clone()
        .or_else(|| owner_at(&directory(root).join("owner")))
    else {
        return Ok(false);
    };
    // Git can finish outside Stroll. Restore under the same lock as a live
    // operation. The Attempted phase prevents re-applying after a partial failure.
    let restored = backup.restore(root, &owner)?;
    if restored {
        clear_operation(root)?;
    }
    Ok(restored)
}

fn with_saved_changes(
    root: &Path,
    owner: &Owner,
    touched: &[u8],
    run: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let directory = store(root, owner)?;
    let result = backup::save(root, owner, touched)
        .and_then(|()| check_tip(root, &owner.tip))
        .and_then(|()| run(&directory));
    settle(root, owner, result)
}

fn store(root: &Path, owner: &Owner) -> Result<PathBuf> {
    let directory = directory(root);
    // A completed or externally aborted operation may have left this app-owned directory.
    if directory.exists() {
        ensure!(
            owner_at(&directory.join("owner")).is_some(),
            t!("git.invalid_file_path")
        );
        fs::remove_dir_all(&directory)?;
    }
    fs::create_dir(&directory)?;
    if let Err(error) = fs::write(directory.join("owner"), serde_json::to_vec(owner)?) {
        _ = fs::remove_dir_all(&directory);
        return Err(error.into());
    }
    Ok(directory)
}

fn write_todo(directory: &Path, entries: &[HistoryEntry]) -> Result<()> {
    use std::fmt::Write as _;

    let mut todo = String::new();
    let mut message: Option<String> = None;
    let mut edit_message = false;
    let mut message_index = 0;
    let mut flush = |todo: &mut String, message: Option<String>, edit_message| -> Result<()> {
        if let Some(mut message) = message.filter(|_| edit_message) {
            if !message.ends_with('\n') && !message.is_empty() {
                message.push('\n');
            }
            let name = format!("message-{message_index}");
            fs::write(directory.join(&name), message)?;
            writeln!(
                todo,
                "exec git commit --amend --allow-empty --cleanup=verbatim -F \"$(git rev-parse --git-path stroll/history-edit)/{name}\""
            )?;
            message_index += 1;
        }
        Ok(())
    };
    for entry in entries {
        let action = match entry.action {
            HistoryAction::Pick | HistoryAction::Reword => {
                flush(&mut todo, message.take(), edit_message)?;
                message = Some(entry.message.clone());
                edit_message = entry.action == HistoryAction::Reword;
                "pick"
            }
            HistoryAction::Squash => {
                let message = message
                    .as_mut()
                    .context(t!("git.history_edit_plan_invalid"))?;
                if !message.is_empty() && !entry.message.is_empty() {
                    message.push_str("\n\n");
                }
                message.push_str(&entry.message);
                edit_message = true;
                "fixup"
            }
            HistoryAction::Fixup => "fixup",
            HistoryAction::Drop => "drop",
        };
        writeln!(todo, "{action} {}", entry.oid)?;
    }
    flush(&mut todo, message, edit_message)?;
    fs::write(directory.join("todo"), todo)?;
    Ok(())
}

fn rebase(root: &Path, plan: HistoryPlan, confirmed: bool) -> Result<()> {
    ready(root, &plan.tip)?;
    ensure!(plan.valid(), t!("git.history_edit_plan_invalid"));
    ensure!(
        plan.entries
            .iter()
            .all(|row| row.action != HistoryAction::Reword || valid_message(&row.message)),
        t!("git.commit_message_required")
    );
    let actual = take_entry(root, &plan.tip, &plan.oldest)?;
    let entries: HashMap<_, _> = actual
        .entries
        .iter()
        .map(|entry| (&entry.oid, entry))
        .collect();
    let mut seen = HashSet::new();
    ensure!(
        actual.base == plan.base
            && actual.entries.len() == plan.entries.len()
            && plan.entries.iter().all(|entry| {
                seen.insert(&entry.oid)
                    && entries.get(&entry.oid).is_some_and(|original| {
                        entry.action == HistoryAction::Reword || entry.message == original.message
                    })
            }),
        t!("git.history_edit_stale")
    );
    check_published_range(
        root,
        &plan.tip.oid,
        plan.base.as_deref(),
        actual.entries.len(),
        confirmed,
    )?;
    let owner = Owner {
        tip: plan.tip.clone(),
        operation: Operation::Rebase,
        rewrite: plan.rewrite,
    };
    let touched = rewrite_paths(root, plan.entries.iter().map(|row| row.oid.as_str()))?;
    with_saved_changes(root, &owner, &touched, |directory| {
        write_todo(directory, &plan.entries)?;
        let mut git = command(
            root,
            [
                "-c",
                "rebase.rescheduleFailedExec=true",
                "rebase",
                "--interactive",
                "--no-autosquash",
                "--no-autostash",
                "--no-update-refs",
                "--no-rebase-merges",
                "--no-fork-point",
                "--reapply-cherry-picks",
                "--keep-empty",
                "--force-rebase",
            ]
            .into_iter()
            .chain([plan.base.as_deref().unwrap_or("--root")]),
        );
        // Git appends the todo path as an argument. The worktree's Git directory also survives a restart.
        git.env("GIT_SEQUENCE_EDITOR", r#"sh -c 'p="$(git rev-parse --git-path stroll/history-edit)" && cp "$p/owner" "$1.stroll" && cat "$p/todo" > "$1"' stroll"#)
            .env("GIT_EDITOR", ":");
        capture_repository(git)?;
        Ok(())
    })
}

fn check_amend(root: &Path, tip: &HistoryTip, expected: &super::CommitState) -> Result<()> {
    ready(root, tip)?;
    let current = commit_state(root)?;
    ensure!(&current == expected, t!("git.worktree_changed"));

    Ok(())
}

fn amend(
    root: &Path,
    tip: HistoryTip,
    expected: super::CommitState,
    message: &str,
    confirmed: bool,
) -> Result<()> {
    use std::io::{Seek as _, Write as _};
    ensure!(valid_message(message), t!("git.commit_message_required"));
    let mut input = tempfile::tempfile()?;
    input.write_all(message.as_bytes())?;
    input.rewind()?;
    check_amend(root, &tip, &expected)?;
    if !confirmed
        && !read(
            root,
            ["for-each-ref", "--contains", &tip.oid, "refs/remotes"],
        )?
        .is_empty()
    {
        return Err(PublishedHistory.into());
    }
    let mut git = command(root, ["commit", "--amend", "--file=-"]);
    git.stdin(input);
    capture_repository(git)?;
    rewrite::record_success(root, &tip, RewriteKind::Amend)
}

#[cfg(test)]
fn edit_history(root: &Path, edit: HistoryEdit) -> Result<()> {
    execute_history_edit(root, edit, false)
}

fn execute_history_edit(root: &Path, edit: HistoryEdit, confirmed: bool) -> Result<()> {
    let _lock = OperationLock::acquire(root)?;
    match edit {
        HistoryEdit::Rebase(plan) => rebase(root, plan, confirmed),
        HistoryEdit::Amend(tip, expected, message) => {
            amend(root, tip, expected, &message, confirmed)
        }
        HistoryEdit::UndoRewrite(record) => rewrite::undo(root, record, confirmed),
        HistoryEdit::Undo(tip) => {
            let plan = prepare(root, tip.clone(), std::slice::from_ref(&tip.oid))?;
            let parent = plan.base.context(t!("git.history_edit_no_parent"))?;
            check_published_range(root, &tip.oid, Some(&parent), 1, confirmed)?;
            apply_reset(root, &tip, &parent, ResetMode::Soft)
        }
        HistoryEdit::Reset(plan, mode) => reset(root, plan, mode, confirmed),
        HistoryEdit::Revert(tip, oid) => {
            ready(root, &tip)?;
            let entry = super::commit(root, &oid)?;
            ensure!(
                entry.parents.len() <= 1
                    && command(root, ["merge-base", "--is-ancestor", &oid, &tip.oid])
                        .status()?
                        .success(),
                t!("git.history_edit_linear")
            );
            let owner = Owner {
                tip,
                operation: Operation::Revert(oid.clone()),
                rewrite: None,
            };
            let touched = rewrite_paths(root, std::iter::once(oid.as_str()))?;
            with_saved_changes(root, &owner, &touched, |_| {
                capture_repository(command(root, ["revert", "--no-edit", &oid])).map(|_| ())
            })
        }
        HistoryEdit::Resume(expected, action) => {
            let current = progress(root).context(t!("git.history_edit_stale"))?;
            ensure!(
                current.owner == expected.owner
                    && current.commit == expected.commit
                    && current.saved == expected.saved,
                t!("git.history_edit_stale")
            );
            if let HistoryResolution::Stage(path) = action {
                ensure!(
                    current.conflicts.contains(&path),
                    t!("git.worktree_changed")
                );
                read(root, ["add", "-A", "--", &path])?;
                return Ok(());
            }
            if let Some(saved) = &current.saved {
                match action {
                    HistoryResolution::RestoreSaved if saved.can_restore => {
                        return settle(root, &current.owner, Ok(()));
                    }
                    HistoryResolution::KeepBackup => {
                        return clear_operation(root);
                    }
                    _ => anyhow::bail!(t!("git.history_edit_stale")),
                }
            }
            ensure!(
                matches!(
                    action,
                    HistoryResolution::Continue | HistoryResolution::Abort
                ),
                t!("git.history_edit_stale")
            );
            let subcommand = if current.is_rebase() {
                "rebase"
            } else {
                "revert"
            };
            let flag = if matches!(action, HistoryResolution::Abort) {
                "--abort"
            } else {
                // Include unmerged paths that cannot be represented as UI strings.
                ensure!(
                    read(root, ["ls-files", "--unmerged", "-z"])?.is_empty(),
                    t!("git.history_edit_resolve_first")
                );
                "--continue"
            };
            let mut git = command(root, [subcommand, flag]);
            git.env("GIT_EDITOR", ":");
            let result = capture_repository(git).map(|_| ());
            settle(root, &current.owner, result)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let temp = tempfile::tempdir()?;
            let root = temp.path().join("test repository");
            fs::create_dir(&root)?;
            read(&root, ["init", "-q", "-b", "main"])?;
            read(&root, ["config", "user.name", "History Test"])?;
            read(&root, ["config", "user.email", "history@example.invalid"])?;
            read(&root, ["config", "commit.gpgsign", "false"])?;
            read(&root, ["config", "core.autocrlf", "false"])?;
            Ok(Self { _temp: temp, root })
        }

        fn change(&self, path: &str, contents: &str, message: &str) -> Result<String> {
            fs::write(self.root.join(path), contents)?;
            self.git(["add", "--", path])?;
            self.git(["commit", "-qm", message])?;
            self.git(["rev-parse", "HEAD"])
        }

        fn git<const N: usize>(&self, args: [&str; N]) -> Result<String> {
            Ok(text(&read(&self.root, args)?).trim_end().to_owned())
        }

        fn tip(&self) -> Result<HistoryTip> {
            Ok(HistoryTip {
                branch: self.git(["symbolic-ref", "HEAD"])?,
                oid: self.git(["rev-parse", "HEAD"])?,
            })
        }

        fn plan(&self, start: &str) -> Result<HistoryPlan> {
            prepare(&self.root, self.tip()?, &[start.to_owned()])
        }
    }

    #[test]
    fn reorders_squashes_rewords_and_drops_only_the_current_branch() -> Result<()> {
        let repo = Fixture::new()?;
        let base = repo.change("base", "base\n", "Base")?;
        let first = repo.change("one", "one\n", "First")?;
        repo.change("two", "two\n", "Second\n\nDetailed message")?;
        repo.change("three", "three\n", "Third")?;
        let original = repo.change("four", "four\n", "Fourth")?;
        repo.git(["branch", "other", &original])?;
        repo.git(["tag", "original", &original])?;
        repo.git(["update-ref", "refs/remotes/origin/main", &base])?;
        repo.git(["config", "rebase.updateRefs", "true"])?;
        let mut plan = repo.plan(&first)?;
        plan.entries[0].action = HistoryAction::Reword;
        plan.entries[0].message = "New message\n\n# This line is part of the message".into();
        plan.entries[1].action = HistoryAction::Squash;
        plan.entries[2].action = HistoryAction::Drop;
        plan.entries.swap(2, 3);
        edit_history(&repo.root, HistoryEdit::Rebase(plan))?;
        assert_eq!(
            repo.git(["log", "-3", "--format=%s"])?,
            "Fourth\nNew message\nBase"
        );
        assert_eq!(
            repo.git(["show", "-s", "--format=%B", "HEAD^"])?,
            "New message\n\n# This line is part of the message\n\nSecond\n\nDetailed message"
        );
        assert_eq!(repo.git(["rev-parse", "other"])?, original);
        assert_eq!(repo.git(["rev-parse", "original"])?, original);
        assert!(!repo.root.join("three").exists());
        assert!(!directory(&repo.root).exists());
        Ok(())
    }

    #[test]
    fn undo_keeps_the_patch_staged_and_revert_keeps_published_history() -> Result<()> {
        let repo = Fixture::new()?;
        let base = repo.change("base", "base\n", "Base")?;
        let published = repo.change("one", "one\n", "Published")?;
        repo.git(["update-ref", "refs/remotes/origin/feature", &published])?;
        let private = repo.change("two", "two\n", "Private")?;
        let tip = repo.tip()?;
        assert!(repo.plan(&base).is_ok());
        let published_plan = repo.plan(&published)?;
        assert!(
            edit_history(&repo.root, HistoryEdit::Rebase(published_plan))
                .unwrap_err()
                .is::<PublishedHistory>()
        );
        let plan = repo.plan(&private)?;
        edit_history(&repo.root, HistoryEdit::Undo(tip))?;
        assert_eq!(repo.tip()?.oid, published);
        assert_eq!(repo.git(["diff", "--cached", "--name-only"])?, "two");
        assert_eq!(repo.git(["diff", "--name-only"])?, "");
        assert!(edit_history(&repo.root, HistoryEdit::Rebase(plan)).is_err());
        assert!(edit_history(&repo.root, HistoryEdit::Undo(repo.tip()?)).is_err());
        repo.git(["commit", "-qm", "Recommitted"])?;
        let before_revert = repo.tip()?;
        edit_history(
            &repo.root,
            HistoryEdit::Revert(before_revert.clone(), published.clone()),
        )?;
        assert_eq!(repo.git(["rev-parse", "HEAD^"])?, before_revert.oid);
        assert_eq!(repo.git(["rev-parse", "origin/feature"])?, published);
        assert!(!repo.root.join("one").exists());
        Ok(())
    }

    #[test]
    fn a_conflicted_plan_can_be_aborted_or_resumed_in_a_linked_worktree() -> Result<()> {
        let mut repo = Fixture::new()?;
        let shared = repo.change("shared", "base\n", "Base")?;
        let primary = repo.root.clone();
        let linked = repo._temp.path().join("linked worktree");
        let mut add = command(&repo.root, ["worktree", "add", "-qb", "feature"]);
        add.arg(&linked);
        capture_repository(add)?;
        repo.root = linked;
        let first = repo.change("one", "one\n", "First")?;
        repo.change("shared", "intermediate\n", "Intermediate")?;
        let last = repo.change("shared", "end\n", "Last")?;
        let mut plan = repo.plan(&first)?;
        plan.rewrite = Some(RewriteKind::Rebase);
        plan.entries[1].action = HistoryAction::Drop;
        plan.entries[2].action = HistoryAction::Squash;
        fs::write(repo.root.join("local-before"), "untracked before rebase\n")?;
        for should_abort in [true, false] {
            assert!(edit_history(&repo.root, HistoryEdit::Rebase(plan.clone())).is_err());
            let backend = Backend::new(repo.root.clone(), None);
            let paused = backend
                .history_progress()
                .context("missing paused rebase")?;
            let backup = backup::load(&repo.root)?.unwrap();
            assert!(
                command(
                    &primary,
                    ["show-ref", "--verify", "--quiet", &backup.reference]
                )
                .status()?
                .code()
                    == Some(1)
            );
            assert!(object_id(&repo.git([
                "rev-parse",
                "--verify",
                &backup.reference
            ])?));

            assert_eq!(paused.commit.as_deref(), Some(last.as_str()));
            assert_eq!(paused.conflicts, ["shared"]);
            assert!(
                backend
                    .edit_history(HistoryEdit::Resume(
                        paused.clone(),
                        HistoryResolution::Continue,
                    ))
                    .is_err()
            );
            if should_abort {
                let (_, record) =
                    backend.edit_history(HistoryEdit::Resume(paused, HistoryResolution::Abort))?;
                assert!(record.is_none());
                assert_eq!(repo.tip()?.oid, last);
                assert_eq!(fs::read_to_string(repo.root.join("shared"))?, "end\n");
            } else {
                fs::write(repo.root.join("shared"), "resolved\n")?;
                backend.edit_history(HistoryEdit::Resume(
                    paused.clone(),
                    HistoryResolution::Stage("shared".into()),
                ))?;
                assert!(backend.history_progress().unwrap().conflicts.is_empty());
                let (_, record) = backend
                    .edit_history(HistoryEdit::Resume(paused, HistoryResolution::Continue))?;
                assert_eq!(record.unwrap().kind, RewriteKind::Rebase);
                assert_eq!(
                    repo.git(["log", "-2", "--format=%B"])?,
                    "First\n\nLast\n\nBase"
                );
                assert_eq!(fs::read_to_string(repo.root.join("shared"))?, "resolved\n");
            }
            assert_eq!(
                fs::read_to_string(repo.root.join("local-before"))?,
                "untracked before rebase\n"
            );
            assert!(backend.history_progress().is_none());
        }
        assert_eq!(head(&primary)?, Some(shared));
        Ok(())
    }
    #[test]
    fn dirty_reword_restores_all_layers_without_changing_user_stashes() -> Result<()> {
        let repo = Fixture::new()?;
        repo.change("tracked", "original\n", "Base")?;
        let tip = repo.change("other", "tip\n", "Old message")?;
        fs::write(repo.root.join("user-stash"), "private\n")?;
        repo.git(["stash", "push", "--include-untracked", "-m", "user stash"])?;
        let user_stashes = repo.git(["stash", "list", "--format=%H"])?;
        fs::write(repo.root.join("tracked"), "staged\n")?;
        repo.git(["add", "tracked"])?;
        fs::write(repo.root.join("tracked"), "unstaged\n")?;
        fs::create_dir(repo.root.join("new dir"))?;
        fs::write(repo.root.join("new dir/file"), b"untracked\x00data")?;
        let mut plan = repo.plan(&tip)?;
        plan.entries[0].action = HistoryAction::Reword;
        plan.entries[0].message = "New message".into();
        edit_history(&repo.root, HistoryEdit::Rebase(plan))?;
        assert_eq!(repo.git(["log", "-1", "--format=%s"])?, "New message");
        assert_eq!(repo.git(["show", ":tracked"])?, "staged");
        assert_eq!(fs::read_to_string(repo.root.join("tracked"))?, "unstaged\n");
        assert_eq!(
            fs::read(repo.root.join("new dir/file"))?,
            b"untracked\x00data"
        );
        assert_eq!(repo.git(["stash", "list", "--format=%H"])?, user_stashes);
        assert!(
            repo.git(["for-each-ref", "refs/worktree/stroll/history-backups"])?
                .is_empty()
        );
        assert!(progress(&repo.root).is_none());
        Ok(())
    }

    #[test]
    fn dirty_reword_saves_an_ignored_file_at_a_staged_deletion() -> Result<()> {
        let repo = Fixture::new()?;
        repo.change("local", "original\n", "Base")?;
        let tip = repo.change("other", "tip\n", "Old message")?;
        fs::write(repo.root.join(".git/info/exclude"), "local\n")?;
        repo.git(["rm", "--cached", "local"])?;
        fs::write(repo.root.join("local"), "ignored user contents\n")?;
        let mut plan = repo.plan(&tip)?;
        plan.entries[0].action = HistoryAction::Reword;
        plan.entries[0].message = "New message".into();
        let result = edit_history(&repo.root, HistoryEdit::Rebase(plan));
        if result.is_err() {
            let saved = progress(&repo.root).unwrap().saved.unwrap();
            assert_eq!(
                repo.git(["show", &format!("{}^3:local", saved.reference)])?,
                "ignored user contents"
            );
        } else {
            assert_eq!(
                fs::read_to_string(repo.root.join("local"))?,
                "ignored user contents\n"
            );
            assert!(!repo.git(["diff", "--cached", "--name-only"])?.is_empty());
        }
        Ok(())
    }

    #[test]
    fn dirty_changes_survive_a_failed_restore_and_an_interrupted_rebase() -> Result<()> {
        let repo = Fixture::new()?;
        repo.change("tracked", "base\n", "Base")?;
        let first = repo.change("tracked", "one\n", "First")?;
        let second = repo.change("tracked", "two\n", "Second")?;
        fs::write(repo.root.join("tracked"), "draft\n")?;
        fs::write(repo.root.join("local"), "local\n")?;
        let mut plan = repo.plan(&first)?;
        plan.entries[0].action = HistoryAction::Drop;
        assert!(edit_history(&repo.root, HistoryEdit::Rebase(plan)).is_err());
        let paused = progress(&repo.root).context("expected rebase conflict")?;
        assert!(paused.is_rebase() && paused.saved.is_none());
        assert_eq!(paused.commit.as_deref(), Some(second.as_str()));
        assert!(!repo.root.join("local").exists());
        edit_history(
            &repo.root,
            HistoryEdit::Resume(paused, HistoryResolution::Abort),
        )?;
        assert_eq!(repo.tip()?.oid, second);
        assert_eq!(fs::read_to_string(repo.root.join("tracked"))?, "draft\n");
        assert_eq!(fs::read_to_string(repo.root.join("local"))?, "local\n");
        assert!(progress(&repo.root).is_none());

        let mut plan = repo.plan(&second)?;
        plan.entries[0].action = HistoryAction::Drop;
        assert!(edit_history(&repo.root, HistoryEdit::Rebase(plan)).is_err());
        assert_eq!(repo.tip()?.oid, first);
        let failed = progress(&repo.root).context("expected saved changes")?;
        let saved = failed.saved.as_ref().unwrap();
        assert!(!saved.can_restore);
        assert!(!recover_backup(&repo.root)?);
        let original = format!("{}:tracked", saved.reference);
        let untracked = format!("{}^3:local", saved.reference);
        assert_eq!(repo.git(["show", &original])?, "draft");
        assert_eq!(repo.git(["show", &untracked])?, "local");
        assert!(
            edit_history(
                &repo.root,
                HistoryEdit::Resume(failed, HistoryResolution::RestoreSaved,)
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn recovery_restores_all_layers_after_rewrite_state_is_gone() -> Result<()> {
        let repo = Fixture::new()?;
        repo.change("tracked", "base\n", "Base")?;
        let tip = repo.tip()?;
        fs::write(repo.root.join("tracked"), "staged\n")?;
        repo.git(["add", "tracked"])?;
        fs::write(repo.root.join("tracked"), "worktree\n")?;
        fs::create_dir(repo.root.join("local"))?;
        fs::write(repo.root.join("local/.gitignore"), "build/\n")?;
        fs::write(repo.root.join("local/draft"), "draft\n")?;
        fs::create_dir(repo.root.join("local/build"))?;
        fs::write(repo.root.join("local/build/cache"), "cache\n")?;
        let owner = Owner {
            tip,
            operation: Operation::Rebase,
            rewrite: Some(RewriteKind::Squash),
        };
        {
            let _lock = OperationLock::acquire(&repo.root)?;
            store(&repo.root, &owner)?;
            backup::save(&repo.root, &owner, b"tracked\0")?;
        }
        let saved = backup::load(&repo.root)?.unwrap().reference;
        // The completed Git operation can outlive its working files.
        repo.git(["commit", "--amend", "-qm", "Squashed"])?;
        let rewritten = repo.tip()?;
        fs::remove_dir_all(directory(&repo.root))?;
        assert!(progress(&repo.root).unwrap().saved.unwrap().can_restore);
        repo.git(["switch", "--detach", "-q"])?;
        assert!(!recover_backup(&repo.root)?);
        assert!(!repo.root.join("local/draft").exists());
        repo.git(["switch", "-q", "main"])?;

        assert!(recover_backup(&repo.root)?);
        assert_eq!(repo.tip()?, rewritten);
        assert_eq!(repo.git(["show", ":tracked"])?, "staged");
        assert_eq!(fs::read_to_string(repo.root.join("tracked"))?, "worktree\n");
        assert_eq!(
            fs::read_to_string(repo.root.join("local/draft"))?,
            "draft\n"
        );
        assert_eq!(
            fs::read_to_string(repo.root.join("local/build/cache"))?,
            "cache\n"
        );
        assert!(repo.git(["for-each-ref", &saved])?.is_empty());
        assert!(!recover_backup(&repo.root)?);
        assert!(progress(&repo.root).is_none());
        Ok(())
    }

    #[test]
    fn reset_modes_follow_git_and_recheck_the_branch_and_remote_refs() -> Result<()> {
        for mode in [
            ResetMode::Soft,
            ResetMode::Mixed,
            ResetMode::Keep,
            ResetMode::Hard,
        ] {
            let repo = Fixture::new()?;
            let base = repo.change("local", "base\n", "Base")?;
            let tip = repo.change("committed", "committed\n", "Tip")?;
            fs::write(repo.root.join("local"), "staged\n")?;
            repo.git(["add", "local"])?;
            fs::write(repo.root.join("local"), "unstaged\n")?;
            fs::write(repo.root.join("untracked"), "untracked\n")?;
            let plan = prepare_reset(&repo.root, repo.tip()?, &base)?;
            assert_eq!(plan.removed, 1);
            repo.git(["update-ref", "refs/remotes/origin/main", &tip])?;
            assert!(edit_history(&repo.root, HistoryEdit::Reset(plan.clone(), mode)).is_err());
            assert_eq!(repo.tip()?.oid, tip);
            execute_history_edit(&repo.root, HistoryEdit::Reset(plan.clone(), mode), true)?;
            assert_eq!(repo.git(["rev-parse", "origin/main"])?, tip);
            assert_eq!(repo.tip()?.oid, base);
            assert_eq!(repo.git(["rev-parse", "ORIG_HEAD"])?, tip);
            assert!(edit_history(&repo.root, HistoryEdit::Reset(plan, mode)).is_err());
            assert_eq!(
                repo.git(["show", ":local"])?,
                if mode == ResetMode::Soft {
                    "staged"
                } else {
                    "base"
                }
            );
            assert_eq!(
                fs::read_to_string(repo.root.join("local"))?,
                if mode == ResetMode::Hard {
                    "base\n"
                } else {
                    "unstaged\n"
                }
            );
            assert_eq!(
                repo.root.join("committed").exists(),
                matches!(mode, ResetMode::Soft | ResetMode::Mixed)
            );
            assert_eq!(
                fs::read_to_string(repo.root.join("untracked"))?,
                "untracked\n"
            );
        }
        let repo = Fixture::new()?;
        let base = repo.change("tracked", "base\n", "Base")?;
        let tip = repo.change("tracked", "tip\n", "Tip")?;
        fs::write(repo.root.join("tracked"), "draft\n")?;
        let plan = prepare_reset(&repo.root, repo.tip()?, &base)?;
        assert!(edit_history(&repo.root, HistoryEdit::Reset(plan, ResetMode::Keep)).is_err());
        assert_eq!(repo.tip()?.oid, tip);
        assert_eq!(fs::read_to_string(repo.root.join("tracked"))?, "draft\n");
        Ok(())
    }
    #[test]
    fn amend_records_one_undo_and_returns_its_patch_to_the_index() -> Result<()> {
        let repo = Fixture::new()?;
        let original = repo.change("file", "original\n", "Original")?;
        let tip = repo.tip()?;
        fs::write(repo.root.join("file"), "staged\n")?;
        repo.git(["add", "file"])?;
        fs::write(repo.root.join("file"), "unstaged\n")?;
        fs::write(repo.root.join("loose"), "loose\n")?;
        let expected = commit_state(&repo.root)?;
        let backend = Backend::new(repo.root.clone(), None);
        assert_eq!(backend.amend_message(&tip, &expected)?, "Original");
        let (amended, record) =
            backend.edit_history(HistoryEdit::Amend(tip, expected, "Amended".into()))?;
        let amended = amended.unwrap();
        let record = record.unwrap();
        assert_eq!(record.kind, RewriteKind::Amend);
        assert_eq!(repo.git(["show", "HEAD:file"])?, "staged");
        assert_eq!(fs::read_to_string(repo.root.join("file"))?, "unstaged\n");
        assert!(repo.git(["diff", "--cached", "--name-only"])?.is_empty());
        assert_eq!(
            backend.rewrite_record(Some("refs/heads/main"), Some(&amended)),
            Some(record.clone())
        );
        backend.edit_history(HistoryEdit::UndoRewrite(record.clone()))?;
        assert_eq!(repo.tip()?.oid, original);
        assert_eq!(repo.git(["show", ":file"])?, "staged");
        assert_eq!(fs::read_to_string(repo.root.join("file"))?, "unstaged\n");
        assert_eq!(fs::read_to_string(repo.root.join("loose"))?, "loose\n");
        assert!(
            backend
                .rewrite_record(Some("refs/heads/main"), Some(&amended))
                .is_none()
        );
        assert!(
            backend
                .edit_history(HistoryEdit::UndoRewrite(record))
                .is_err()
        );
        let new =
            super::super::create_commit(&repo.root, &commit_state(&repo.root)?, "New commit")?;
        assert!(
            backend
                .rewrite_record(Some("refs/heads/main"), Some(&new))
                .is_none()
        );

        let mut plan = repo.plan(&new)?;
        plan.entries[0].action = HistoryAction::Reword;
        plan.entries[0].message = "Only a new message".into();
        assert!(backend.edit_history(HistoryEdit::Rebase(plan))?.1.is_none());
        assert!(
            backend
                .rewrite_record(Some("refs/heads/main"), Some(&repo.tip()?.oid))
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn squash_and_rebase_keep_disjoint_nested_repositories_and_support_undo() -> Result<()> {
        for kind in [RewriteKind::Squash, RewriteKind::Rebase] {
            let repo = Fixture::new()?;
            repo.change("tracked", "base\n", "Base")?;
            let first = repo.change("first", "first\n", "First")?;
            let original = repo.change("last", "last\n", "Last")?;
            let nested = repo.root.join("nested");
            fs::create_dir(&nested)?;
            read(&nested, ["init", "-q"])?;
            fs::write(nested.join("local"), "nested work\n")?;
            fs::write(repo.root.join("tracked"), "staged\n")?;
            repo.git(["add", "tracked"])?;
            fs::write(repo.root.join("tracked"), "working\n")?;
            fs::write(repo.root.join("loose"), "loose\n")?;

            let mut plan = repo.plan(&first)?;
            plan.rewrite = Some(kind);
            if kind == RewriteKind::Squash {
                plan.entries[1].action = HistoryAction::Squash;
            } else {
                plan.entries[0].action = HistoryAction::Drop;
            }
            let backend = Backend::new(repo.root.clone(), None);
            let (after, record) = backend.edit_history(HistoryEdit::Rebase(plan))?;
            let record = record.unwrap();
            assert_eq!(record.kind, kind);
            let after = after.unwrap();
            assert_eq!(
                repo.root.join("first").exists(),
                kind == RewriteKind::Squash
            );
            repo.git(["update-ref", "refs/remotes/origin/main", &after])?;
            assert!(
                backend
                    .edit_history(HistoryEdit::UndoRewrite(record.clone()))
                    .is_err()
            );
            assert_eq!(repo.tip()?.oid, after);
            backend.edit_history_with_confirmation(HistoryEdit::UndoRewrite(record), true)?;
            assert_eq!(repo.git(["rev-parse", "origin/main"])?, after);
            assert_eq!(repo.tip()?.oid, original);
            assert!(repo.root.join("first").exists());
            assert_eq!(repo.git(["show", ":tracked"])?, "staged");
            assert_eq!(fs::read_to_string(repo.root.join("tracked"))?, "working\n");
            assert_eq!(fs::read_to_string(repo.root.join("loose"))?, "loose\n");
            assert_eq!(fs::read_to_string(nested.join("local"))?, "nested work\n");
            assert!(nested.join(".git").is_dir());
        }
        Ok(())
    }

    #[test]
    fn rewrite_rejects_a_nested_repository_that_intersects_the_replay() -> Result<()> {
        let repo = Fixture::new()?;
        fs::create_dir(repo.root.join("nested"))?;
        repo.change("nested/file", "tracked\n", "Base")?;
        repo.git(["rm", "nested/file"])?;
        repo.git(["commit", "-qm", "Remove nested path"])?;
        let start = repo.tip()?.oid;
        let original = repo.change("last", "last\n", "Last")?;
        let nested = repo.root.join("nested");
        fs::create_dir_all(&nested)?;
        read(&nested, ["init", "-q"])?;
        fs::write(nested.join("file"), "independent work\n")?;
        let mut plan = repo.plan(&start)?;
        plan.entries[1].action = HistoryAction::Squash;
        let result = edit_history(&repo.root, HistoryEdit::Rebase(plan));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nested"));
        assert_eq!(repo.tip()?.oid, original);
        assert_eq!(
            fs::read_to_string(nested.join("file"))?,
            "independent work\n"
        );
        assert!(progress(&repo.root).is_none());
        Ok(())
    }
}
