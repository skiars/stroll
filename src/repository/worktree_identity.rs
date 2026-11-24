use std::{
    fs,
    io::{ErrorKind, Write as _},
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use super::selection::{Worktree, common_dir, local_worktrees, path_from_git};

const MARKER: &str = "stroll-worktree-id";

pub(super) fn linked_admin(root: &Path) -> Option<PathBuf> {
    let bytes = fs::read(root.join(".git")).ok()?;
    let bytes = bytes.strip_prefix(b"gitdir: ")?;
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    root.join(path_from_git(bytes).ok()?).canonicalize().ok()
}

pub(super) fn branch(admin: &Path) -> Option<String> {
    let head = fs::read_to_string(admin.join("HEAD")).ok()?;
    Some(head.trim_end().strip_prefix("ref: refs/heads/")?.to_owned())
}

fn valid_id(id: &str) -> bool {
    id.strip_prefix("stroll-").is_some_and(|suffix| {
        suffix.len() == 24 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

pub(super) fn marker(admin: &Path) -> Result<Option<String>> {
    match fs::read_to_string(admin.join(MARKER)) {
        Ok(id) if valid_id(&id) => Ok(Some(id)),
        Ok(_) => Err(anyhow!("Invalid worktree identity in {}", admin.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn ensure_marker(admin: &Path) -> Result<String> {
    if let Some(id) = marker(admin)? {
        return Ok(id);
    }
    let mut file = tempfile::Builder::new()
        .prefix("stroll-")
        .rand_bytes(24)
        .tempfile_in(admin)?;
    let id = file
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|id| valid_id(id))
        .ok_or_else(|| anyhow!("Could not generate a worktree identity"))?
        .to_owned();
    file.write_all(id.as_bytes())?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(admin.join(MARKER)) {
        Ok(_) => Ok(id),
        Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {
            marker(admin)?.ok_or_else(|| error.into())
        }
        Err(error) => Err(error.into()),
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct WorktreeIdentity {
    pub id: String,
    pub admin: PathBuf,
    pub branch: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct LocatedWorktree {
    pub root: PathBuf,
    pub branch: Option<String>,
    pub same_worktree: bool,
}

pub(crate) fn identity(root: &Path, common: &Path) -> Result<Option<WorktreeIdentity>> {
    let Some(admin) = linked_admin(root).filter(|admin| admin != common) else {
        return Ok(None);
    };
    let id = ensure_marker(&admin)?;
    let branch = branch(&admin);
    Ok(Some(WorktreeIdentity { id, admin, branch }))
}

pub(crate) fn has_marker(common: &Path, admin: &Path, id: &str) -> Result<bool> {
    anyhow::ensure!(
        admin.parent() == Some(common.join("worktrees").as_path()) && valid_id(id),
        "Invalid saved worktree marker"
    );
    Ok(marker(admin)?.as_deref() == Some(id))
}

pub(super) fn find_main(trees: Vec<Worktree>, common: &Path) -> Option<PathBuf> {
    trees
        .into_iter()
        .find(|tree| {
            tree.main && tree.available && common_dir(&tree.root).is_ok_and(|dir| dir == common)
        })
        .map(|tree| tree.root)
}

pub(crate) fn locate(root: &Path, common: &Path, id: &str) -> Result<Option<LocatedWorktree>> {
    anyhow::ensure!(
        root.is_absolute() && common.is_absolute() && valid_id(id),
        "Invalid saved worktree"
    );
    let matching_admin = |path: &Path| {
        let admin = linked_admin(path)?;
        (admin.parent() == Some(common.join("worktrees").as_path())
            && marker(&admin).ok().flatten().as_deref() == Some(id))
        .then_some(admin)
    };
    let located = |root, admin: PathBuf| LocatedWorktree {
        root,
        branch: branch(&admin),
        same_worktree: true,
    };
    if let Some(admin) = matching_admin(root) {
        return Ok(Some(located(root.to_owned(), admin)));
    }
    if !common.is_dir() {
        return Ok(None);
    }
    let trees = local_worktrees(common)?;
    if let Some((path, admin)) = trees
        .iter()
        .filter(|tree| tree.available)
        .find_map(|tree| matching_admin(&tree.root).map(|admin| (&tree.root, admin)))
    {
        return Ok(Some(located(path.clone(), admin)));
    }
    Ok(find_main(trees, common).map(|root| LocatedWorktree {
        root,
        branch: None,
        same_worktree: false,
    }))
}
