use std::{
    ffi::OsString,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Result, ensure};
use rust_i18n::t;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{local_worktrees, read, worktree_commit_can_be_removed};
use crate::{paths, repository::files::rename_no_replace};

const PREVIEW_LIMIT: usize = 12;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum WorktreeRemovalAction {
    DeleteDirectory,
    RemoveRecord,
    MoveDirectory {
        destination: PathBuf,
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct WorktreeRemovalPreview {
    pub path: PathBuf,
    pub reference: String,
    pub head: Option<String>,
    pub detached_unreferenced: bool,
    pub action: WorktreeRemovalAction,
    pub entries: Vec<(String, String)>,
    pub entry_count: usize,
    fingerprint: [u8; 32],
}

pub(super) fn worktree_file_status(path: &Path) -> Result<Vec<u8>> {
    read(
        path,
        [
            "status",
            "--porcelain=v1",
            "-z",
            "--ignore-submodules=none",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    )
}

fn status_entries(status: &[u8]) -> (Vec<(String, String)>, usize) {
    let mut records = status.split(|byte| *byte == 0);
    let mut entries = Vec::new();
    let mut count = 0;
    while let Some(record) = records.next().filter(|record| record.len() >= 3) {
        count += 1;
        let (code, path) = (&record[..2], &record[3..]);
        // A rename or copy is followed by its original path in porcelain v1 -z.
        let source = code
            .iter()
            .any(|byte| matches!(byte, b'R' | b'C'))
            .then(|| records.next())
            .flatten();
        if entries.len() < PREVIEW_LIMIT {
            let path = String::from_utf8_lossy(path);
            let name = match source {
                Some(source) => format!("{} → {path}", String::from_utf8_lossy(source)),
                None => path.into_owned(),
            };
            entries.push((String::from_utf8_lossy(code).into_owned(), name));
        }
    }
    (entries, count)
}

pub(crate) fn preview_worktree_removal(
    root: &Path,
    path: &Path,
    reference: &str,
) -> Result<WorktreeRemovalPreview> {
    let trees = local_worktrees(root)?;
    let tree = trees
        .iter()
        .find(|tree| tree.root == path && tree.reference == reference)
        .ok_or_else(|| anyhow::anyhow!(t!("git.management_changed")))?;
    ensure!(
        !tree.main && !tree.locked && path != root,
        t!("git.management_changed")
    );
    let path_key = paths::simplified(path);
    ensure!(
        !trees.iter().any(|other| {
            let other = paths::simplified(&other.root);
            other != path_key && other.starts_with(&path_key)
        }),
        t!("git.worktree_nested")
    );

    let (action, entries, entry_count, fingerprint) = if tree.missing {
        (WorktreeRemovalAction::RemoveRecord, Vec::new(), 0, [0; 32])
    } else if let Some(reason) = &tree.prunable {
        // Avoid moving a symlink or a directory reached through a replaced ancestor.
        ensure!(
            path.symlink_metadata()?.is_dir()
                && paths::simplified(&path.canonicalize()?) == path_key,
            t!("git.worktree_status_unknown")
        );
        let mut names = fs::read_dir(path)?
            .map(|entry| {
                let entry = entry?;
                Ok((entry.file_name(), entry.file_type()?.is_dir()))
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort_unstable();
        let mut digest = Sha256::new();
        for (name, is_directory) in &names {
            digest.update(name.as_encoded_bytes());
            digest.update([0, u8::from(*is_directory)]);
        }
        let entries = names
            .iter()
            .take(PREVIEW_LIMIT)
            .map(|(name, is_directory)| {
                let mut name = name.to_string_lossy().into_owned();
                if *is_directory {
                    name.push('/');
                }
                (String::new(), name)
            })
            .collect();
        let mut suffix = 1;
        let destination = loop {
            let mut candidate: OsString = path.as_os_str().to_owned();
            if suffix == 1 {
                candidate.push(".stroll-saved");
            } else {
                candidate.push(format!(".stroll-saved-{suffix}"));
            }
            let candidate = PathBuf::from(candidate);
            match candidate.symlink_metadata() {
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    if !trees
                        .iter()
                        .any(|tree| paths::simplified(&tree.root) == paths::simplified(&candidate))
                    {
                        break candidate;
                    }
                }
                Err(error) => return Err(error.into()),
                Ok(_) => {}
            }
            suffix += 1;
        };
        (
            WorktreeRemovalAction::MoveDirectory {
                destination,
                reason: reason.clone(),
            },
            entries,
            names.len(),
            digest.finalize().into(),
        )
    } else {
        ensure!(tree.available, t!("git.worktree_status_unknown"));
        let status = worktree_file_status(path)?;
        let (entries, count) = status_entries(&status);
        (
            WorktreeRemovalAction::DeleteDirectory,
            entries,
            count,
            Sha256::digest(status).into(),
        )
    };
    Ok(WorktreeRemovalPreview {
        path: tree.root.clone(),
        reference: tree.reference.clone(),
        head: tree.head.clone(),
        detached_unreferenced: !worktree_commit_can_be_removed(root, tree)?,
        action,
        entries,
        entry_count,
        fingerprint,
    })
}

pub(super) fn remove_worktree(root: &Path, path: &Path, force: bool) -> Result<()> {
    let mut args = vec!["worktree".as_ref(), "remove".as_ref()];
    if force {
        args.push("--force".as_ref());
    }
    args.extend(["--".as_ref(), path.as_os_str()]);
    read(root, args)?;
    Ok(())
}

pub(super) fn apply_worktree_removal(root: &Path, preview: &WorktreeRemovalPreview) -> Result<()> {
    let path = &preview.path;
    let current = preview_worktree_removal(root, path, &preview.reference)
        .map_err(|_| anyhow::anyhow!(t!("git.worktree_removal_changed")))?;
    ensure!(&current == preview, t!("git.worktree_removal_changed"));

    match &preview.action {
        WorktreeRemovalAction::DeleteDirectory => {
            remove_worktree(root, path, preview.entry_count > 0)
        }
        WorktreeRemovalAction::RemoveRecord => remove_worktree(root, path, false),
        WorktreeRemovalAction::MoveDirectory { destination, .. } => {
            rename_no_replace(path, destination)?;
            let remove_record = || {
                ensure!(
                    local_worktrees(root)?.iter().any(|tree| {
                        tree.root == *path
                            && tree.reference == preview.reference
                            && tree.head == preview.head
                            && tree.missing
                            && !tree.main
                            && !tree.locked
                    }),
                    t!("git.worktree_removal_changed")
                );
                remove_worktree(root, path, false)
            };
            remove_record().map_err(|error| {
                anyhow::anyhow!(t!(
                    "git.worktree_moved_cleanup_failed",
                    destination = paths::display(destination),
                    error = error
                ))
            })
        }
    }
}
