use std::{
    fs,
    io::{ErrorKind, Write as _},
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use rusqlite::{Connection, OptionalExtension as _, params};
use rust_i18n::t;
use serde::{Deserialize, Serialize};

use super::{
    Repository, Store,
    selection::{Worktree, common_dir, local_worktrees, path_from_git},
};

const MARKER: &str = "stroll-worktree-id";

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct WorktreeState {
    pub(super) id: String,
    pub(crate) root: PathBuf,
    common_dir: PathBuf,
    admin: PathBuf,
    branch: Option<String>,
    state_id: String,
}

impl WorktreeState {
    pub(crate) fn storage_key(&self) -> &[u8] {
        self.state_id.as_bytes()
    }

    pub(crate) fn inherit(&mut self, archived: Self) {
        self.state_id = archived.state_id;
    }

    pub(crate) fn still_at_root(&self) -> bool {
        self.is_at(&self.root)
    }

    fn is_at(&self, root: &Path) -> bool {
        marker(&self.admin).ok().flatten().as_ref() == Some(&self.id)
            && linked_admin(root).as_deref() == Some(&self.admin)
    }
}

fn linked_admin(root: &Path) -> Option<PathBuf> {
    let bytes = fs::read(root.join(".git")).ok()?;
    let bytes = bytes.strip_prefix(b"gitdir: ")?;
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    root.join(path_from_git(bytes).ok()?).canonicalize().ok()
}

fn branch(admin: &Path) -> Option<String> {
    let head = fs::read_to_string(admin.join("HEAD")).ok()?;
    Some(head.trim_end().strip_prefix("ref: refs/heads/")?.to_owned())
}

fn valid_id(id: &str) -> bool {
    id.strip_prefix("stroll-").is_some_and(|suffix| {
        suffix.len() == 24 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

fn marker(admin: &Path) -> Result<Option<String>> {
    match fs::read_to_string(admin.join(MARKER)) {
        Ok(id) if valid_id(&id) => Ok(Some(id)),
        Ok(_) => Err(anyhow!("Invalid worktree identity in {}", admin.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn worktree_id(root: &Path) -> Option<String> {
    marker(&linked_admin(root)?).ok().flatten()
}

fn ensure_marker(admin: &Path) -> Result<String> {
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

pub(super) fn write_worktree(db: &Connection, state: &WorktreeState) -> Result<()> {
    db.execute(
        "INSERT INTO worktrees (id, common_dir, updated, value)
         VALUES (?1, ?2, (SELECT coalesce(max(updated), 0) + 1 FROM worktrees WHERE common_dir = ?2), ?3)
         ON CONFLICT(id) DO UPDATE SET
            common_dir = excluded.common_dir, updated = excluded.updated, value = excluded.value",
        params![
            state.id,
            state.common_dir.as_os_str().as_encoded_bytes(),
            serde_json::to_vec(state)?,
        ],
    )?;
    Ok(())
}

impl Store {
    fn read_worktree_state(&self, id: &str) -> Result<Option<WorktreeState>> {
        self.with_state(|db| {
            let value: Option<Vec<u8>> = db
                .query_row("SELECT value FROM worktrees WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .optional()?;
            Ok(value.as_deref().map(serde_json::from_slice).transpose()?)
        })
    }

    pub(crate) fn update_worktree_branch(&self, state: &mut WorktreeState) -> Result<()> {
        let branch = branch(&state.admin);
        if branch != state.branch {
            let mut updated = state.clone();
            updated.branch = branch;
            self.with_state(|db| write_worktree(db, &updated))?;
            *state = updated;
        }
        Ok(())
    }

    fn worktree_records(&self, common_dir: &Path) -> Result<Vec<WorktreeState>> {
        self.with_state(|db| {
            let mut query = db.prepare_cached(
                "SELECT value FROM worktrees WHERE common_dir = ?1 ORDER BY updated DESC, id DESC",
            )?;
            query
                .query_map([common_dir.as_os_str().as_encoded_bytes()], |row| {
                    row.get::<_, Vec<u8>>(0)
                })?
                .map(|row| Ok(serde_json::from_slice(&row?)?))
                .collect()
        })
    }

    pub(crate) fn bind_worktree(&self, repo: &mut Repository) -> Result<Option<WorktreeState>> {
        let Some(admin) = linked_admin(&repo.root).filter(|admin| admin != &repo.common_dir) else {
            return Ok(None);
        };
        let id = ensure_marker(&admin)?;
        let mut recorded = self.read_worktree_state(&id)?;
        let branch = branch(&admin);
        if let Some(state) = &mut recorded {
            state.root.clone_from(&repo.root);
            state.common_dir.clone_from(&repo.common_dir);
            state.admin = admin;
            state.branch = branch;
            repo.worktree_state = recorded;
            return Ok(None);
        }

        let records = self.worktree_records(&repo.common_dir)?;
        // A surviving marker may belong to a worktree that can still be repaired.
        let archived = records
            .iter()
            .find(|state| {
                branch.is_some()
                    && state.branch == branch
                    && matches!(marker(&state.admin), Ok(id) if id.as_ref() != Some(&state.id))
                    && !records.iter().any(|other| {
                        other.storage_key() == state.storage_key()
                            && marker(&other.admin).ok().flatten().as_ref() == Some(&other.id)
                    })
            })
            .cloned();
        repo.worktree_state = Some(WorktreeState {
            state_id: id.clone(),
            id,
            root: repo.root.clone(),
            common_dir: repo.common_dir.clone(),
            admin,
            branch,
        });
        Ok(archived)
    }

    pub(crate) fn resolve_saved_worktree(&self, root: &Path) -> Result<Option<PathBuf>> {
        let Some((common, id)) = self.saved_repository(root)? else {
            return Ok(None);
        };
        if let Some(id) = id {
            if let Some(worktree) = self.read_worktree_state(&id)? {
                if worktree.still_at_root() {
                    return Ok(Some(worktree.root));
                }
                return worktree.replacement()?.map(Some).ok_or_else(|| {
                    anyhow!(
                        t!(
                            "repository.directory_not_found",
                            path = crate::paths::display(root)
                        )
                        .into_owned()
                    )
                });
            }
            return Ok(None);
        }
        if !matches!(root.try_exists(), Ok(false)) {
            return Ok(None);
        }
        main_worktree(&common)
    }
}

fn main_worktree(common: &Path) -> Result<Option<PathBuf>> {
    if !common.is_dir() {
        return Ok(None);
    }
    Ok(find_main(local_worktrees(common)?, common))
}

fn find_main(trees: Vec<Worktree>, common: &Path) -> Option<PathBuf> {
    trees
        .into_iter()
        .find(|tree| {
            tree.main && tree.available && common_dir(&tree.root).is_ok_and(|dir| dir == common)
        })
        .map(|tree| tree.root)
}

impl WorktreeState {
    pub(crate) fn replacement(&self) -> Result<Option<PathBuf>> {
        if !self.common_dir.is_dir() {
            return Ok(None);
        }
        let trees = local_worktrees(&self.common_dir)?;
        if let Some(tree) = trees
            .iter()
            .find(|tree| tree.available && self.is_at(&tree.root))
        {
            return Ok(Some(tree.root.clone()));
        }
        Ok(find_main(trees, &self.common_dir))
    }
}
