use std::{fs, time::Duration};

use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, TransactionBehavior};

use super::Store;

const SCHEMA_VERSION: i64 = 5;

impl Store {
    #[cfg(feature = "desktop")]
    pub(crate) fn lsp_prompt_dismissed(&self, host: &str, server: &str) -> Result<bool> {
        self.with_state(|db| {
            Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM lsp_prompt_dismissals WHERE host = ?1 AND server = ?2 AND explicit = 1)",
                (host, server),
                |row| row.get(0),
            )?)
        })
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn dismiss_lsp_prompt(&self, host: &str, server: &str) -> Result<()> {
        self.with_state(|db| {
            db.execute(
                "INSERT INTO lsp_prompt_dismissals (host, server, explicit) VALUES (?1, ?2, 1)
                 ON CONFLICT (host, server) DO UPDATE SET explicit = 1",
                (host, server),
            )?;
            Ok(())
        })
    }

    pub(super) fn with_state<T>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T>,
    ) -> Result<T> {
        let mut connection = self.state.lock().unwrap();
        if connection.is_none() {
            *connection = Some(self.open_state()?);
        }
        operation(connection.as_mut().unwrap())
    }

    fn open_state(&self) -> Result<Connection> {
        let path = self.root.join("state.sqlite3");
        let mut options = fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        options.open(&path)?;
        let mut db = Connection::open(&path)
            .with_context(|| format!("Could not open application state at {}", path.display()))?;
        db.busy_timeout(Duration::from_secs(3))?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(
            (0..=SCHEMA_VERSION).contains(&version),
            "Unsupported application state version {version}"
        );
        if version == 0 {
            tx.execute_batch(include_str!("state.sql"))?;
        }
        if version < 2 {
            tx.execute_batch(
                "CREATE TABLE editor_preview_preferences (
                    format TEXT PRIMARY KEY NOT NULL,
                    value BLOB NOT NULL
                )",
            )?;
        }
        if version < 3 {
            tx.execute_batch(
                "CREATE TABLE lsp_prompt_dismissals (
                    host TEXT NOT NULL,
                    server TEXT NOT NULL,
                    PRIMARY KEY (host, server)
                )",
            )?;
        }
        if version < 4 {
            // Earlier versions recorded the notification close control as a dismissal.
            tx.execute_batch(
                "ALTER TABLE lsp_prompt_dismissals ADD COLUMN explicit INTEGER NOT NULL DEFAULT 0",
            )?;
        }
        if version < 5 {
            tx.execute_batch("ALTER TABLE documents ADD COLUMN format INTEGER NOT NULL DEFAULT 0")?;
        }
        if version < SCHEMA_VERSION {
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        tx.commit()?;
        Ok(db)
    }
}

#[cfg(all(test, feature = "desktop"))]
mod tests {
    use super::*;

    #[test]
    fn only_the_dismiss_action_suppresses_future_lsp_prompts() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let db = Connection::open(temporary.path().join("state.sqlite3"))?;
        db.execute_batch(include_str!("state.sql"))?;
        db.execute_batch(
            "CREATE TABLE lsp_prompt_dismissals (
                host TEXT NOT NULL,
                server TEXT NOT NULL,
                PRIMARY KEY (host, server)
            );
            INSERT INTO lsp_prompt_dismissals (host, server) VALUES ('local', 'tsc');
            PRAGMA user_version = 3;",
        )?;
        drop(db);
        let store = Store::new(temporary.path().to_owned())?;
        assert!(!store.lsp_prompt_dismissed("local", "tsc")?);
        store.dismiss_lsp_prompt("local", "tsc")?;
        drop(store);
        let reopened = Store::new(temporary.path().to_owned())?;
        assert!(reopened.lsp_prompt_dismissed("local", "tsc")?);
        assert!(!reopened.lsp_prompt_dismissed("another host", "tsc")?);
        assert!(!reopened.lsp_prompt_dismissed("local", "json")?);
        Ok(())
    }
}
