use std::{collections::BTreeMap, path::PathBuf};

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::{Serialize, de::DeserializeOwned};

use super::{Repository, Store};
use crate::{session::WorktreeSession, tabs::TabSession};

#[cfg(feature = "desktop")]
use {
    super::{DraftRelocation, WindowGeometry, WindowSlot},
    crate::session::{StandaloneSession, TerminalSession},
    rusqlite::{OptionalExtension as _, TransactionBehavior},
    std::path::Path,
};

macro_rules! session_fields {
    ($apply:ident) => {
        $apply!(
            view,
            tabs,
            source_history,
            document,
            open_documents,
            git,
            panels,
            editor_tree_expanded,
            docs_tree_expanded,
            terminal,
            commit_draft
        )
    };
}

impl Store {
    pub(super) fn read_worktree_session(&self, repo: &Repository) -> Result<WorktreeSession> {
        self.with_state(|db| {
            let tx = db.transaction()?;
            let session = read_worktree_session(&tx, repo.storage_key())?;
            #[cfg(feature = "desktop")]
            let session = WorktreeSession {
                worktree_lsp_trusted: tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM lsp_trust WHERE checkout = ?1)",
                    [repo.checkout_key()],
                    |row| row.get(0),
                )?,
                ..session
            };
            tx.commit()?;
            Ok(session)
        })
    }

    pub fn save_tabs(&self, repo: &Repository, tabs: &TabSession) -> Result<()> {
        self.with_state(|db| write_field(db, repo.storage_key(), "tabs", tabs))
    }

    #[cfg(all(test, feature = "desktop"))]
    pub(crate) fn save_session(&self, repo: &Repository, session: &WorktreeSession) -> Result<()> {
        let previous = self.read_worktree_session(repo)?;
        self.update_worktree_session(
            repo.storage_key(),
            repo.checkout_key(),
            &previous,
            session,
            [],
        )
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn update_worktree_session<'a>(
        &self,
        key: &[u8],
        checkout: &[u8],
        previous: &WorktreeSession,
        session: &WorktreeSession,
        relocations: impl IntoIterator<Item = &'a DraftRelocation>,
    ) -> Result<()> {
        self.with_state(|db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // A window publishes its local changes, leaving untouched parts at their latest values.
            macro_rules! write_fields {
                ($($field:ident),*) => {
                    $(if session.$field != previous.$field {
                        write_field(&tx, key, stringify!($field), &session.$field)?;
                    })*
                };
            }
            session_fields!(write_fields);
            write_positions(&tx, key, "file", &previous.files, &session.files)?;
            write_positions(
                &tx,
                key,
                "document",
                &previous.documents,
                &session.documents,
            )?;
            if session.worktree_lsp_trusted != previous.worktree_lsp_trusted {
                if session.worktree_lsp_trusted {
                    tx.execute("INSERT OR IGNORE INTO lsp_trust VALUES (?1)", [checkout])?;
                } else {
                    tx.execute("DELETE FROM lsp_trust WHERE checkout = ?1", [checkout])?;
                }
            }
            for relocation in relocations {
                relocation.apply(&tx)?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn save_terminal_session(
        &self,
        key: &[u8],
        terminal: &Option<TerminalSession>,
    ) -> Result<()> {
        self.with_state(|db| write_field(db, key, "terminal", terminal))
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn standalone_session(&self, path: &Path) -> Result<StandaloneSession> {
        self.with_state(|db| read_standalone(db, path))
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn update_standalone_session(
        &self,
        path: &Path,
        previous: &StandaloneSession,
        session: &StandaloneSession,
    ) -> Result<()> {
        if previous == session {
            return Ok(());
        }
        self.with_state(|db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut current = read_standalone(&tx, path)?;
            if session.view != previous.view {
                current.view = session.view;
            }
            if session.panels != previous.panels {
                current.panels = session.panels;
            }
            if session.file_position != previous.file_position {
                current.file_position = session.file_position;
            }
            if session.document_position != previous.document_position {
                current.document_position = session.document_position;
            }
            tx.execute(
                "INSERT INTO standalone_sessions VALUES (?1, ?2)
                 ON CONFLICT(path) DO UPDATE SET value = excluded.value",
                params![
                    path.as_os_str().as_encoded_bytes(),
                    serde_json::to_vec(&current)?
                ],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn window_geometry(&self, slot: WindowSlot) -> Result<Option<WindowGeometry>> {
        self.with_state(|db| {
            let value: Option<Vec<u8>> = db
                .query_row(
                    "SELECT value FROM window_geometry WHERE slot = ?1",
                    [slot.key()],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(value.as_deref().map(serde_json::from_slice).transpose()?)
        })
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn save_window_geometry(
        &self,
        slot: WindowSlot,
        geometry: WindowGeometry,
    ) -> Result<()> {
        self.with_state(|db| {
            db.execute(
                "INSERT INTO window_geometry VALUES (?1, ?2)
                 ON CONFLICT(slot) DO UPDATE SET value = excluded.value",
                params![slot.key(), serde_json::to_vec(&geometry)?],
            )?;
            Ok(())
        })
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn terminal_height(&self) -> Result<Option<f32>> {
        self.with_state(|db| {
            Ok(db
                .query_row(
                    "SELECT height FROM terminal_height WHERE id = 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn save_terminal_height(&self, height: f32) -> Result<()> {
        self.with_state(|db| {
            db.execute(
                "INSERT INTO terminal_height VALUES (1, ?1)
                 ON CONFLICT(id) DO UPDATE SET height = excluded.height",
                [height],
            )?;
            Ok(())
        })
    }
}

#[cfg(feature = "desktop")]
impl WindowSlot {
    fn key(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Editor => "editor",
            Self::Repository => "repository",
        }
    }
}

#[cfg(feature = "desktop")]
fn read_standalone(db: &Connection, path: &Path) -> Result<StandaloneSession> {
    let value: Option<Vec<u8>> = db
        .query_row(
            "SELECT value FROM standalone_sessions WHERE path = ?1",
            [path.as_os_str().as_encoded_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value
        .as_deref()
        .map(serde_json::from_slice)
        .transpose()?
        .unwrap_or_default())
}

fn read_worktree_session(db: &Connection, key: &[u8]) -> Result<WorktreeSession> {
    let mut session = WorktreeSession::default();
    let mut statement =
        db.prepare_cached("SELECT field, value FROM worktree_session_fields WHERE session = ?1")?;
    let mut rows = statement.query([key])?;
    while let Some(row) = rows.next()? {
        let field: String = row.get(0)?;
        let value: Vec<u8> = row.get(1)?;
        macro_rules! read_fields {
            ($($field:ident),*) => {
                match field.as_str() {
                    $(stringify!($field) => session.$field = serde_json::from_slice(&value)?,)*
                    _ => anyhow::bail!("Unknown worktree session field: {field}"),
                }
            };
        }
        session_fields!(read_fields);
    }
    session.files = read_positions(db, key, "file")?;
    session.documents = read_positions(db, key, "document")?;
    Ok(session)
}

fn write_field(db: &Connection, key: &[u8], field: &str, value: &impl Serialize) -> Result<()> {
    db.prepare_cached(
        "INSERT INTO worktree_session_fields VALUES (?1, ?2, ?3)
         ON CONFLICT(session, field) DO UPDATE SET value = excluded.value
         WHERE value != excluded.value",
    )?
    .execute(params![key, field, serde_json::to_vec(value)?])?;
    Ok(())
}

fn read_positions<T: DeserializeOwned>(
    db: &Connection,
    key: &[u8],
    kind: &str,
) -> Result<BTreeMap<PathBuf, T>> {
    let mut statement = db.prepare_cached(
        "SELECT path, value FROM session_positions WHERE session = ?1 AND kind = ?2",
    )?;
    let mut rows = statement.query(params![key, kind])?;
    let mut positions = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let path: Vec<u8> = row.get(0)?;
        let value: Vec<u8> = row.get(1)?;
        positions.insert(
            serde_json::from_slice(&path)?,
            serde_json::from_slice(&value)?,
        );
    }
    Ok(positions)
}

#[cfg(feature = "desktop")]
fn write_positions<T: PartialEq + Serialize>(
    db: &Connection,
    key: &[u8],
    kind: &str,
    previous: &BTreeMap<PathBuf, T>,
    positions: &BTreeMap<PathBuf, T>,
) -> Result<()> {
    if previous == positions {
        return Ok(());
    }
    let mut delete = db.prepare_cached(
        "DELETE FROM session_positions WHERE session = ?1 AND kind = ?2 AND path = ?3",
    )?;
    for path in previous
        .keys()
        .filter(|path| !positions.contains_key(*path))
    {
        delete.execute(params![key, kind, serde_json::to_vec(path)?])?;
    }
    let mut upsert = db.prepare_cached(
        "INSERT INTO session_positions VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session, kind, path) DO UPDATE SET value = excluded.value",
    )?;
    for (path, position) in positions
        .iter()
        .filter(|(path, value)| previous.get(*path) != Some(*value))
    {
        upsert.execute(params![
            key,
            kind,
            serde_json::to_vec(path)?,
            serde_json::to_vec(position)?
        ])?;
    }
    Ok(())
}

#[cfg(all(test, feature = "desktop"))]
mod tests {
    use std::fs;

    use super::*;
    use crate::session::{FilePosition, WorkspaceView};

    fn repository(root: &Path) -> Result<Repository> {
        fs::create_dir(root)?;
        super::super::git(root, &["init", "-q"])?;
        Repository::open(root)
    }

    fn position(line: u32) -> FilePosition {
        FilePosition {
            selection_start: (line, 0),
            selection_end: (line, 0),
            reversed: false,
            top: (line, 0),
            inset: 0.,
            horizontal: 0.,
        }
    }

    #[test]
    fn stale_sessions_update_only_changed_fields_and_paths() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let repo = repository(&fixture.path().join("repo"))?;
        let state = fixture.path().join("state");
        let first = Store::new(state.clone())?;
        let second = Store::new(state)?;
        let a = PathBuf::from("a.rs");
        let b = PathBuf::from("b.rs");
        let baseline = first.read_worktree_session(&repo)?;
        let mut tabs_and_position = baseline.clone();
        tabs_and_position.tabs.open(a.clone(), false);
        tabs_and_position.files.insert(a.clone(), position(10));

        first.update_worktree_session(
            repo.storage_key(),
            repo.checkout_key(),
            &baseline,
            &tabs_and_position,
            [],
        )?;
        let terminal = Some(TerminalSession {
            open: true,
            ..Default::default()
        });
        second.save_terminal_session(repo.storage_key(), &terminal)?;

        let mut view_and_position = baseline.clone();
        view_and_position.view = Some(WorkspaceView::Docs);
        view_and_position.files.insert(b.clone(), position(20));
        second.update_worktree_session(
            repo.storage_key(),
            repo.checkout_key(),
            &baseline,
            &view_and_position,
            [],
        )?;
        let result = first.read_worktree_session(&repo)?;
        assert_eq!(result.tabs.active(), Some(a.as_path()));
        assert_eq!(result.terminal, terminal);
        assert_eq!(result.view, Some(WorkspaceView::Docs));
        assert_eq!(result.files, [(a, position(10)), (b, position(20))].into());
        Ok(())
    }

    #[test]
    fn session_and_draft_relocation_commit_or_roll_back_together() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let repo = repository(&fixture.path().join("repo"))?;
        let store = Store::new(fixture.path().join("state"))?;
        let old = Path::new("old.rs");
        let renamed = Path::new("renamed.rs");
        let mut previous = WorktreeSession::default();
        previous.tabs.open(old.to_owned(), false);
        previous.files.insert(old.to_owned(), position(10));
        store.save_session(&repo, &previous)?;
        store.write_draft(&repo, old, [0; 32], "draft", true)?;

        let mut session = previous.clone();
        session.tabs = TabSession::default();
        session.tabs.open(renamed.to_owned(), false);
        session.files.remove(old);
        session.files.insert(renamed.to_owned(), position(10));
        let relocation = store.draft_relocation(&repo, old, renamed, None);
        let save = || {
            store.update_worktree_session(
                repo.storage_key(),
                repo.checkout_key(),
                &previous,
                &session,
                [&relocation],
            )
        };
        store.with_state(|db| {
            db.execute_batch(
                "CREATE TEMP TRIGGER reject_draft_removal BEFORE UPDATE OF draft ON documents
                 WHEN NEW.draft IS NULL BEGIN SELECT RAISE(ABORT, 'draft removal failed'); END;",
            )?;
            Ok(())
        })?;

        assert!(save().is_err());
        assert_eq!(store.read_worktree_session(&repo)?, previous);
        assert!(store.has_draft(&repo, old));
        assert!(!store.has_draft(&repo, renamed));

        store.with_state(|db| {
            db.execute_batch("DROP TRIGGER reject_draft_removal;")?;
            Ok(())
        })?;
        save()?;
        assert_eq!(store.read_worktree_session(&repo)?, session);
        assert!(!store.has_draft(&repo, old));
        assert_eq!(
            store.editor_draft(Some(&repo), renamed)?.unwrap().text,
            "draft"
        );
        Ok(())
    }
}
