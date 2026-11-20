use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs,
    io::{Seek, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rust_i18n::t;
use serde::{Deserialize, Serialize};

use super::{Owner, capture_repository, command, directory, read, text};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum BackupPhase {
    Captured,
    Ready,
    Attempted,
    Restored,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Backup {
    pub reference: String,
    pub phase: BackupPhase,
    oid: String,
    #[serde(default)]
    pub owner: Option<Owner>,
}

pub(super) fn load(root: &Path) -> Result<Option<Backup>> {
    match contents(root) {
        Ok(bytes) => {
            let backup: Backup = serde_json::from_slice(&bytes)?;
            ensure!(
                super::object_id(&backup.oid)
                    && backup
                        .reference
                        .starts_with("refs/worktree/stroll/history-backups/"),
                t!("git.invalid_file_path")
            );
            Ok((backup.phase != BackupPhase::Restored).then_some(backup))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn journal(root: &Path) -> PathBuf {
    super::git_dir(root).join("stroll/history-backup")
}

fn contents(root: &Path) -> std::io::Result<Vec<u8>> {
    match fs::read(journal(root)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::read(directory(root).join("backup"))
        }
        result => result,
    }
}

pub(super) fn forget(root: &Path) -> Result<()> {
    match fs::remove_file(journal(root)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

impl Backup {
    fn write(&self, root: &Path) -> Result<()> {
        crate::repository::atomic_write(&journal(root), &serde_json::to_vec(self)?)
    }

    fn can_restore(&self, root: &Path, owner: &Owner) -> Result<bool> {
        if self.phase != BackupPhase::Ready
            || super::active_commit(root, owner).is_some()
            || !fs::read_to_string(super::git_dir(root).join("HEAD"))
                .is_ok_and(|head| head.trim_end().strip_prefix("ref: ") == Some(&owner.tip.branch))
        {
            return Ok(false);
        }
        Ok(!super::commit_state(root)?.blocked && status(root, "--untracked-files=no")?.is_empty())
    }

    pub(super) fn restore(mut self, root: &Path, owner: &Owner) -> Result<bool> {
        if !self.can_restore(root, owner)? {
            return Ok(false);
        }
        ensure!(
            rev(root, &self.reference)? == self.oid
                && rev(root, &format!("{}^1", self.oid))? == owner.tip.oid,
            t!("git.history_edit_stale")
        );
        self.owner = Some(owner.clone());
        // A failed apply may already have modified worktree files and the index.
        // Preserve the ref, and never automatically apply the same snapshot twice.
        self.phase = BackupPhase::Attempted;
        self.write(root)?;
        read(root, ["stash", "apply", "--quiet", "--index", &self.oid])?;
        self.phase = BackupPhase::Restored;
        self.write(root)?;
        read(root, ["update-ref", "-d", &self.reference, &self.oid])?;
        Ok(true)
    }
}

fn status(root: &Path, untracked: &str) -> Result<Vec<u8>> {
    read(
        root,
        [
            "status",
            "--porcelain=v1",
            "-z",
            untracked,
            "--ignore-submodules=none",
        ],
    )
}

fn rev(root: &Path, revision: &str) -> Result<String> {
    Ok(text(&read(root, ["rev-parse", "--verify", revision])?)
        .trim_end()
        .to_owned())
}

fn tracked_snapshot(root: &Path) -> Result<Option<String>> {
    let oid = text(&read(root, ["stash", "create"])?)
        .trim_end()
        .to_owned();
    Ok((!oid.is_empty()).then_some(oid))
}

fn file_name(bytes: &[u8]) -> Result<OsString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        Ok(OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    {
        Ok(std::str::from_utf8(bytes)?.into())
    }
}

fn git_paths(bytes: &[u8]) -> impl Iterator<Item = Result<PathBuf>> {
    bytes
        .split(|&byte| byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            let path = PathBuf::from(file_name(path)?);
            ensure!(
                path.components()
                    .all(|part| matches!(part, std::path::Component::Normal(_))),
                t!("git.invalid_file_path")
            );
            Ok(path)
        })
}

// Find a file or symlink obstructing a deleted index path without following it.
fn first_obstruction(root: &Path, path: &Path) -> Result<PathBuf> {
    let mut partial = PathBuf::new();
    for part in path.components() {
        partial.push(part);
        if partial == path {
            break;
        }
        match fs::symlink_metadata(root.join(&partial)) {
            Ok(meta) if !meta.is_dir() => return Ok(partial),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path.to_owned())
}

fn intersects(left: &Path, right: &Path) -> bool {
    fn inside(path: &Path, parent: &Path) -> bool {
        if cfg!(any(target_os = "windows", target_os = "macos")) {
            Path::new(&path.to_string_lossy().to_lowercase())
                .starts_with(parent.to_string_lossy().to_lowercase())
        } else {
            path.starts_with(parent)
        }
    }
    inside(left, right) || inside(right, left)
}

fn untracked_paths(root: &Path, touched: &[u8]) -> Result<(BTreeSet<PathBuf>, BTreeSet<PathBuf>)> {
    let listed = read(root, ["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut files = BTreeSet::new();
    let mut nested = BTreeSet::new();
    for path in git_paths(&listed) {
        let path = path?;
        let meta = fs::symlink_metadata(root.join(&path))?;
        if meta.is_dir() && fs::symlink_metadata(root.join(&path).join(".git")).is_ok() {
            nested.insert(path);
        } else {
            ensure!(
                meta.is_file() || meta.is_symlink(),
                t!("git.history_backup_unsupported", path = path.display())
            );
            files.insert(path);
        }
    }
    let deleted = read(
        root,
        [
            "diff",
            "--cached",
            "--no-renames",
            "--name-only",
            "--diff-filter=D",
            "-z",
            "--",
        ],
    )?;
    if !nested.is_empty() {
        for path in git_paths(touched).chain(git_paths(&deleted)) {
            let path = path?;
            if let Some(directory) = nested.iter().find(|dir| intersects(dir, &path)) {
                anyhow::bail!(t!("git.history_nested_overlap", path = directory.display()));
            }
        }
    }
    // Restoring HEAD can overwrite an ignored file left behind by a staged deletion.
    for path in git_paths(&deleted) {
        let path = first_obstruction(root, &path?)?;
        match fs::symlink_metadata(root.join(&path)) {
            Ok(meta) if meta.is_file() || meta.is_symlink() => {
                files.insert(path);
            }
            Ok(meta) if meta.is_dir() => {
                let ignored = read(
                    root,
                    [
                        "ls-files",
                        "--others",
                        "--ignored",
                        "--exclude-standard",
                        "--directory",
                        "-z",
                        "--",
                    ]
                    .into_iter()
                    .map(OsStr::new)
                    .chain([path.as_os_str()]),
                )?;
                ensure!(
                    ignored.is_empty(),
                    t!("git.history_backup_unsupported", path = path.display())
                );
            }
            Ok(_) => anyhow::bail!(t!("git.history_backup_unsupported", path = path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    for path in &files {
        let meta = fs::symlink_metadata(root.join(path))?;
        ensure!(
            meta.is_file() || meta.is_symlink(),
            t!("git.history_backup_unsupported", path = path.display())
        );
    }
    Ok((files, nested))
}

fn untracked_tree(
    root: &Path,
    scratch: &Path,
    paths: &BTreeSet<PathBuf>,
) -> Result<Option<String>> {
    if paths.is_empty() {
        return Ok(None);
    }
    let mut args = tempfile::tempfile()?;
    for path in paths {
        #[cfg(unix)]
        let bytes = {
            use std::os::unix::ffi::OsStrExt as _;
            path.as_os_str().as_bytes()
        };
        #[cfg(not(unix))]
        let bytes = path
            .to_str()
            .context(t!("git.invalid_file_path"))?
            .as_bytes();
        args.write_all(bytes)?;
        args.write_all(&[0])?;
    }
    args.rewind()?;
    let mut add = command(
        root,
        [
            "add",
            "--force",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ],
    );
    add.env("GIT_INDEX_FILE", scratch).stdin(args);
    capture_repository(add)?;
    let mut write = command(root, ["write-tree"]);
    write.env("GIT_INDEX_FILE", scratch);
    Ok(Some(
        text(&capture_repository(write)?).trim_end().to_owned(),
    ))
}

fn commit_tree(root: &Path, tree: &str, parents: &[&str], message: &str) -> Result<String> {
    let args = ["commit-tree", tree, "-m", message]
        .into_iter()
        .chain(parents.iter().flat_map(|parent| ["-p", *parent]));
    Ok(text(&read(root, args)?).trim_end().to_owned())
}

fn tracked_trees(root: &Path, snapshot: Option<&str>, head: &str) -> Result<(String, String)> {
    if let Some(oid) = snapshot {
        Ok((
            rev(root, &format!("{oid}^{{tree}}"))?,
            rev(root, &format!("{oid}^2^{{tree}}"))?,
        ))
    } else {
        let tree = rev(root, &format!("{head}^{{tree}}"))?;
        Ok((tree.clone(), tree))
    }
}

pub(super) fn check_ignored_paths(root: &Path, touched: &[u8]) -> Result<BTreeSet<PathBuf>> {
    let ignored = read(
        root,
        [
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    )?;
    let ignored = git_paths(&ignored).collect::<Result<BTreeSet<_>>>()?;
    for path in git_paths(touched) {
        let path = path?;
        if let Some(obstruction) = ignored.iter().find(|ignored| intersects(ignored, &path)) {
            anyhow::bail!(t!(
                "git.history_edit_ignored_overlap",
                path = obstruction.display()
            ));
        }
    }
    Ok(ignored)
}

pub(super) fn save(root: &Path, owner: &Owner, touched: &[u8]) -> Result<()> {
    let initial = status(root, "--untracked-files=all")?;
    if initial.is_empty() {
        return Ok(());
    }

    let scratch = tempfile::Builder::new()
        .prefix("backup-")
        .tempdir_in(directory(root))?;
    let (paths, nested) = untracked_paths(root, touched)?;
    let untracked = untracked_tree(root, &scratch.path().join("untracked"), &paths)?;
    let tracked = tracked_snapshot(root)?;
    if paths.is_empty()
        && tracked.is_none()
        && !nested.is_empty()
        && status(root, "--untracked-files=no")?.is_empty()
    {
        return Ok(());
    }
    // Restoring a committed .gitignore can expose ignored build output.
    let ignored = check_ignored_paths(root, touched)?;
    let (worktree, index) = tracked_trees(root, tracked.as_deref(), &owner.tip.oid)?;
    let oid = if let Some(tree) = &untracked {
        let index = commit_tree(root, &index, &[&owner.tip.oid], "Stroll saved index")?;
        let untracked = commit_tree(root, tree, &[], "Stroll saved untracked files")?;
        commit_tree(
            root,
            &worktree,
            &[&owner.tip.oid, &index, &untracked],
            "Stroll saved changes",
        )?
    } else {
        tracked.context(t!("git.history_backup_unsupported", path = root.display()))?
    };

    // Verify the snapshot before changing disk contents. A worktree-specific ref
    // keeps it reachable across restarts and leaves the shared stash stack alone.
    super::check_tip(root, &owner.tip)?;
    ensure!(
        status(root, "--untracked-files=all")? == initial
            && tracked_trees(root, tracked_snapshot(root)?.as_deref(), &owner.tip.oid)?
                == (worktree, index)
            && untracked_tree(root, &scratch.path().join("verify"), &paths)? == untracked,
        t!("git.worktree_changed")
    );
    let name = scratch
        .path()
        .file_name()
        .and_then(OsStr::to_str)
        .context(t!("git.invalid_file_path"))?;
    let reference = format!("refs/worktree/stroll/history-backups/{name}");
    read(
        root,
        ["update-ref", &reference, &oid, &"0".repeat(oid.len())],
    )?;
    let mut backup = Backup {
        reference,
        phase: BackupPhase::Captured,
        oid,
        owner: Some(owner.clone()),
    };
    if let Err(error) = backup.write(root) {
        _ = read(root, ["update-ref", "-d", &backup.reference, &backup.oid]);
        return Err(error);
    }
    super::check_tip(root, &owner.tip)?;

    for path in paths {
        ensure!(
            first_obstruction(root, &path)? == path,
            t!("git.worktree_changed")
        );
        fs::remove_file(root.join(path))?;
    }
    read(
        root,
        [
            "restore",
            "--source=HEAD",
            "--staged",
            "--worktree",
            "--",
            ".",
        ],
    )?;
    let remaining = read(root, ["ls-files", "--others", "--exclude-standard", "-z"])?;
    let remaining = git_paths(&remaining).collect::<Result<BTreeSet<_>>>()?;
    ensure!(
        status(root, "--untracked-files=no")?.is_empty()
            && nested.is_subset(&remaining)
            && remaining
                .difference(&nested)
                .all(|path| { path.ancestors().any(|ancestor| ignored.contains(ancestor)) }),
        t!("git.history_backup_pending")
    );
    backup.phase = BackupPhase::Ready;
    backup.write(root)?;
    Ok(())
}
