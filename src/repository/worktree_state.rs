use std::{
    fs,
    io::{ErrorKind, Write as _},
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Result, anyhow};
use rust_i18n::t;
use serde::{Deserialize, Serialize};

use super::{
    Repository, Store, atomic_write, read_json,
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
    pub(crate) legacy_root: Option<PathBuf>,
}

impl WorktreeState {
    pub(crate) fn storage_key(&self) -> &[u8] {
        self.legacy_root
            .as_deref()
            .map_or(self.state_id.as_bytes(), |root| {
                root.as_os_str().as_encoded_bytes()
            })
    }

    pub(crate) fn inherit(&mut self, archived: Self) {
        self.state_id = archived.state_id;
        self.legacy_root = archived.legacy_root;
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

impl Store {
    fn worktree_state_dir(&self) -> PathBuf {
        self.root.join("worktree-state")
    }

    fn read_worktree_state(&self, id: &str) -> Result<Option<WorktreeState>> {
        if !valid_id(id) {
            return Ok(None);
        }
        read_json(&self.worktree_state_dir().join(format!("{id}.json")))
    }

    pub(crate) fn write_worktree_state(&self, state: &WorktreeState) -> Result<()> {
        atomic_write(
            &self.worktree_state_dir().join(format!("{}.json", state.id)),
            &serde_json::to_vec(state)?,
        )
    }

    pub(crate) fn update_worktree_branch(&self, state: &mut WorktreeState) -> Result<()> {
        let branch = branch(&state.admin);
        if branch != state.branch {
            let mut updated = state.clone();
            updated.branch = branch;
            self.write_worktree_state(&updated)?;
            *state = updated;
        }
        Ok(())
    }

    fn worktree_records(&self) -> Result<Vec<(SystemTime, WorktreeState)>> {
        let mut records = Vec::new();
        match fs::read_dir(self.worktree_state_dir()) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    if entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "json")
                        && let Ok(Some(state)) = read_json::<WorktreeState>(&entry.path())
                    {
                        records.push((entry.metadata()?.modified()?, state));
                    }
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(records)
    }

    pub(crate) fn bind_worktree(&self, repo: &mut Repository) -> Result<Option<WorktreeState>> {
        let Some(admin) = linked_admin(&repo.root).filter(|admin| admin != &repo.common_dir) else {
            // A newly initialized repository can occupy a linked worktree's former path.
            if self.session_path(repo).try_exists()? || self.tabs_path(repo).try_exists()? {
                let claimed = self
                    .worktree_records()?
                    .iter()
                    .any(|(_, state)| state.legacy_root.as_ref() == Some(&repo.root));
                if claimed {
                    let mut key = b"local-repository\0".to_vec();
                    key.extend(repo.root.as_os_str().as_encoded_bytes());
                    repo.local_storage_key = Some(key);
                }
            }
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

        let records = self.worktree_records()?;
        let legacy_exists =
            self.session_path(repo).try_exists()? || self.tabs_path(repo).try_exists()?;
        let legacy_root = (legacy_exists
            && !records.iter().any(|(_, state)| {
                state.root == repo.root || state.legacy_root.as_ref() == Some(&repo.root)
            }))
        .then(|| repo.root.clone());

        // A surviving marker may belong to a worktree that can still be repaired.
        let archived = records
            .iter()
            .filter(|(_, state)| {
                branch.is_some()
                    && state.branch == branch
                    && state.common_dir == repo.common_dir
                    && matches!(marker(&state.admin), Ok(id) if id.as_ref() != Some(&state.id))
                    && !records.iter().any(|(_, other)| {
                        other.storage_key() == state.storage_key()
                            && marker(&other.admin).ok().flatten().as_ref() == Some(&other.id)
                    })
            })
            .max_by(|(left_time, left), (right_time, right)| {
                left_time
                    .cmp(right_time)
                    .then_with(|| left.id.cmp(&right.id))
            })
            .map(|(_, state)| state.clone());
        repo.worktree_state = Some(WorktreeState {
            state_id: id.clone(),
            id,
            root: repo.root.clone(),
            common_dir: repo.common_dir.clone(),
            admin,
            branch,
            legacy_root,
        });
        Ok(archived)
    }

    pub(crate) fn resolve_saved_worktree(&self, root: &Path) -> Result<Option<PathBuf>> {
        let Some(state) = self.workspace_state()? else {
            return Ok(None);
        };
        let saved = if state.location.root == root && state.common_dir.is_some() {
            state.common_dir.map(|dir| (dir, state.worktree_id))
        } else {
            state.recents.and_then(|recents| {
                recents
                    .into_iter()
                    .find(|recent| recent.root == root)
                    .map(|recent| (recent.common_dir, recent.worktree_id))
            })
        };
        let Some((common, id)) = saved else {
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
