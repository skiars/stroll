use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use rusqlite::{Connection, OptionalExtension as _, params};
use rust_i18n::t;
use serde::{Deserialize, Serialize};

use super::{
    Repository, Store,
    selection::local_worktrees,
    worktree_identity::{
        self, LocatedWorktree, WorktreeIdentity, branch, find_main, linked_admin, marker,
    },
};
use crate::remote::{Request, SshSession, SshTarget};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct WorktreeState {
    pub(super) id: String,
    pub(crate) root: PathBuf,
    common_dir: PathBuf,
    admin: PathBuf,
    branch: Option<String>,
    state_id: String,
    #[serde(default)]
    remote_marker: Option<String>,
}

impl WorktreeState {
    pub(crate) fn storage_key(&self) -> &[u8] {
        self.state_id.as_bytes()
    }

    pub(crate) fn inherit(&mut self, archived: Self) {
        self.state_id = archived.state_id;
    }

    fn still_at_root(&self) -> bool {
        self.is_at(&self.root)
    }

    fn is_at(&self, root: &Path) -> bool {
        marker(&self.admin).ok().flatten().as_ref() == Some(&self.id)
            && linked_admin(root).as_deref() == Some(&self.admin)
    }

    fn marker_exists(&self, ssh: Option<&SshSession>) -> Result<bool> {
        if let (Some(ssh), Some(id)) = (ssh, &self.remote_marker) {
            return ssh.call(Request::WorktreeMarker(self.admin.clone(), id.clone()));
        }
        Ok(marker(&self.admin)?.as_ref() == Some(&self.id))
    }

    fn locate_remote(&self, ssh: &SshSession) -> Result<Option<LocatedWorktree>> {
        let Some(id) = &self.remote_marker else {
            return Ok(None);
        };
        let Some(root) = SshTarget::parse(&self.root)? else {
            return Ok(None);
        };
        let Some(common) = SshTarget::parse(&self.common_dir)? else {
            return Ok(None);
        };
        if root.authority() != ssh.target().authority() || common.authority() != root.authority() {
            return Ok(None);
        }
        ssh.call(Request::LocateWorktree {
            root: root.path,
            common: common.path,
            id: id.clone(),
        })
    }
}

pub(super) fn worktree_id(root: &Path) -> Option<String> {
    marker(&linked_admin(root)?).ok().flatten()
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

    fn update_worktree_branch(
        &self,
        state: &mut WorktreeState,
        branch: Option<String>,
    ) -> Result<()> {
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
        let identity = if let Some(ssh) = &repo.ssh {
            if !ssh.supports_worktree_identity() {
                return Ok(None);
            }
            ssh.call::<Option<WorktreeIdentity>>(Request::WorktreeIdentity)?
        } else {
            worktree_identity::identity(&repo.root, &repo.common_dir)?
        };
        let Some(WorktreeIdentity {
            id: marker,
            admin,
            branch,
        }) = identity
        else {
            return Ok(None);
        };
        let id = if let Some(ssh) = &repo.ssh {
            format!("{}\0{marker}", ssh.target().authority())
        } else {
            marker.clone()
        };
        let mut recorded = self.read_worktree_state(&id)?;
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
                    && matches!(state.marker_exists(repo.ssh.as_ref()), Ok(false))
                    && !records.iter().any(|other| {
                        other.storage_key() == state.storage_key()
                            && other
                                .marker_exists(repo.ssh.as_ref())
                                .unwrap_or(repo.ssh.is_some())
                    })
            })
            .cloned();
        // Keep existing path-keyed SSH drafts on first use. An already bound key
        // may belong to an older checkout that the user can restore explicitly.
        let state_id = repo
            .remote_identity
            .as_ref()
            .filter(|key| !records.iter().any(|state| &state.state_id == *key))
            .cloned()
            .unwrap_or_else(|| id.clone());
        repo.worktree_state = Some(WorktreeState {
            state_id,
            id,
            root: repo.root.clone(),
            common_dir: repo.common_dir.clone(),
            admin,
            branch,
            remote_marker: repo.ssh.as_ref().map(|_| marker),
        });
        Ok(archived)
    }

    pub(crate) fn check_worktree_location(
        &self,
        state: &mut WorktreeState,
        ssh: Option<&SshSession>,
    ) -> Result<Option<PathBuf>> {
        if let Some(ssh) = ssh {
            let Some(location) = state.locate_remote(ssh)? else {
                return Err(anyhow!(t!(
                    "repository.directory_not_found",
                    path = crate::paths::display(&state.root)
                )));
            };
            let root = ssh.target().locator(&location.root);
            if root != state.root {
                return Ok(Some(root));
            }
            self.update_worktree_branch(state, location.branch)?;
        } else if state.still_at_root() {
            self.update_worktree_branch(state, branch(&state.admin))?;
        } else {
            return state.replacement();
        }
        Ok(None)
    }

    pub(crate) fn resolve_saved_ssh_worktree(
        &self,
        root: &Path,
        ssh: &SshSession,
    ) -> Result<Option<PathBuf>> {
        if !ssh.supports_worktree_identity() {
            return Ok(None);
        }
        let Some((_, Some(id))) = self.saved_repository(root)? else {
            return Ok(None);
        };
        let Some(state) = self.read_worktree_state(&id)? else {
            return Ok(None);
        };
        state
            .locate_remote(ssh)?
            .map(|location| Some(location.root))
            .ok_or_else(|| {
                anyhow!(t!(
                    "repository.directory_not_found",
                    path = crate::paths::display(root)
                ))
            })
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
