#[cfg(feature = "desktop")]
use super::PathBuf;
use super::{Draft, Path, Repository, Result, Store, digest};
use rusqlite::{Connection, OptionalExtension as _, params};
use sha2::{Digest as _, Sha256};

#[cfg(feature = "desktop")]
pub(crate) struct SavedEditHistory {
    pub key: String,
    pub baseline: [u8; 32],
    pub buffer: [u8; 32],
    pub draft: Option<String>,
    pub history: Vec<u8>,
    pub undo_group: Option<u64>,
    pub redo_group: Option<u64>,
}

impl Store {
    pub(crate) fn edit_history_key(&self, repo: Option<&Repository>, path: &Path) -> String {
        if path.is_absolute() || repo.is_none() {
            return hex::encode(digest(path.as_os_str().as_encoded_bytes()));
        }
        let repo = repo.unwrap();
        if repo.ssh.is_none() && repo.storage_key() == repo.root.as_os_str().as_encoded_bytes() {
            return hex::encode(digest(repo.root.join(path).as_os_str().as_encoded_bytes()));
        }
        let remote = crate::remote::RepoPath::from_local(path).ok();
        let path = remote.as_ref().map_or(path, |path| path.as_path());
        let mut hash = Sha256::new();
        hash.update(repo.storage_key());
        hash.update([0]);
        hash.update(path.as_os_str().as_encoded_bytes());
        hex::encode(hash.finalize())
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn draft_relocation(
        &self,
        repo: &Repository,
        from: &Path,
        to: &Path,
        current: Option<([u8; 32], Option<String>)>,
    ) -> DraftRelocation {
        DraftRelocation {
            from: self.edit_history_key(Some(repo), from),
            to: self.edit_history_key(Some(repo), to),
            current,
        }
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn read_editor_history(
        &self,
        repo: Option<&Repository>,
        path: &Path,
    ) -> Result<Option<SavedEditHistory>> {
        self.with_state(|db| {
            let key = self.edit_history_key(repo, path);
            Ok(db
                .query_row(
                    "SELECT baseline, buffer, draft, history, undo_group, redo_group FROM documents WHERE key = ?1 AND history IS NOT NULL",
                    [&key],
                    |row| {
                        Ok(SavedEditHistory {
                            key: key.clone(),
                            baseline: row.get(0)?,
                            buffer: row.get(1)?,
                            draft: row.get(2)?,
                            history: row.get(3)?,
                            undo_group: row.get::<_, Option<i64>>(4)?.map(|id| id as u64),
                            redo_group: row.get::<_, Option<i64>>(5)?.map(|id| id as u64),
                        })
                    },
                )
                .optional()?)
        })
    }

    pub(super) fn editor_draft(
        &self,
        repo: Option<&Repository>,
        path: &Path,
    ) -> Result<Option<Draft>> {
        self.with_state(|db| read_draft(db, &self.edit_history_key(repo, path)))
    }

    pub(crate) fn update_editor_draft(
        &self,
        repo: Option<&Repository>,
        path: &Path,
        baseline: Option<[u8; 32]>,
        draft: Option<&str>,
    ) -> Result<()> {
        self.with_state(|db| {
            let key = self.edit_history_key(repo, path);
            update_draft(db, &key, baseline, draft)
        })
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn write_editor_histories(
        &self,
        files: impl IntoIterator<Item = SavedEditHistory>,
        restore: Option<(&Repository, u64, &Path)>,
    ) -> Result<()> {
        self.with_state(|db| {
            let tx = db.transaction()?;
            {
                let mut update = tx.prepare_cached(
                    "INSERT INTO documents (key, baseline, buffer, draft, history, undo_group, redo_group) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                    ON CONFLICT(key) DO UPDATE SET baseline = excluded.baseline, buffer = excluded.buffer,
                        draft = excluded.draft, history = excluded.history,
                        undo_group = excluded.undo_group, redo_group = excluded.redo_group",
                )?;
                for file in files {
                    update.execute(params![
                        file.key,
                        file.baseline,
                        file.buffer,
                        file.draft,
                        file.history,
                        file.undo_group.map(|id| id as i64),
                        file.redo_group.map(|id| id as i64),
                    ])?;
                }
            }
            if let Some((repo, id, path)) = restore {
                tx.execute(
                    "INSERT OR IGNORE INTO recorded_restores (worktree, id, document, path) VALUES (?1, ?2, ?3, ?4)",
                    params![repo.storage_key(), id as i64, self.edit_history_key(Some(repo), path),
                            serde_json::to_vec(path)?],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn register_edit_group(&self, paths: &[PathBuf]) -> Result<u64> {
        self.with_state(|db| {
            db.execute(
                "INSERT INTO edit_groups (paths) VALUES (?1)",
                [serde_json::to_vec(paths)?],
            )?;
            Ok(db.last_insert_rowid() as u64)
        })
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn edit_group_paths(&self, id: u64) -> Result<Option<Vec<PathBuf>>> {
        self.with_state(|db| {
            let paths: Option<Vec<u8>> = db
                .query_row(
                    "SELECT paths FROM edit_groups WHERE id = ?1",
                    [id as i64],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(paths.as_deref().map(serde_json::from_slice).transpose()?)
        })
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn text_restore_tips(
        &self,
        repo: &Repository,
        undo: bool,
    ) -> Result<(std::collections::HashSet<u64>, Vec<(u64, PathBuf)>)> {
        self.with_state(|db| {
            let mut statement = db.prepare_cached(
                "SELECT records.id, records.path, documents.undo_group, documents.redo_group
                 FROM recorded_restores records JOIN documents ON records.document = documents.key
                 WHERE records.worktree = ?1 ORDER BY records.id DESC",
            )?;
            let mut rows = statement.query([repo.storage_key()])?;
            let mut recorded = std::collections::HashSet::new();
            let mut tips = Vec::new();
            while let Some(row) = rows.next()? {
                let id = row.get::<_, i64>(0)? as u64;
                let group = row.get::<_, Option<i64>>(if undo { 2 } else { 3 })?;
                recorded.insert(id);
                if group.is_some_and(|group| group as u64 == id | (1_u64 << 63)) {
                    let path: Vec<u8> = row.get(1)?;
                    tips.push((id, serde_json::from_slice(&path)?));
                }
            }
            Ok((recorded, tips))
        })
    }
}

fn read_draft(db: &Connection, key: &str) -> Result<Option<Draft>> {
    Ok(db
        .query_row(
            "SELECT baseline, draft FROM documents WHERE key = ?1 AND draft IS NOT NULL",
            [key],
            |row| {
                Ok(Draft {
                    baseline: row.get(0)?,
                    text: row.get(1)?,
                })
            },
        )
        .optional()?)
}

fn update_draft(
    db: &Connection,
    key: &str,
    baseline: Option<[u8; 32]>,
    draft: Option<&str>,
) -> Result<()> {
    if let Some(text) = draft {
        let baseline = baseline.expect("a draft requires a disk baseline");
        db.execute(
            "INSERT INTO documents (key, baseline, buffer, draft) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(key) DO UPDATE SET baseline = excluded.baseline, draft = excluded.draft",
            params![key, baseline, digest(text.as_bytes()), text],
        )?;
    } else {
        db.execute(
            "UPDATE documents SET baseline = COALESCE(?2, baseline), draft = NULL WHERE key = ?1",
            params![key, baseline],
        )?;
    }
    Ok(())
}

#[cfg(feature = "desktop")]
pub(crate) struct DraftRelocation {
    from: String,
    to: String,
    current: Option<([u8; 32], Option<String>)>,
}

#[cfg(feature = "desktop")]
impl DraftRelocation {
    pub(super) fn apply(&self, db: &Connection) -> Result<()> {
        if let Some((baseline, text)) = &self.current {
            update_draft(db, &self.to, Some(*baseline), text.as_deref())?;
        } else if let Some(draft) = read_draft(db, &self.from)? {
            update_draft(db, &self.to, Some(draft.baseline), Some(&draft.text))?;
        }
        update_draft(db, &self.from, None, None)
    }
}
