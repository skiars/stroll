use std::{fs, time::Duration};

use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, TransactionBehavior};

use super::Store;

const SCHEMA_VERSION: i64 = 1;

impl Store {
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
        match version {
            0 => {
                tx.execute_batch(include_str!("state.sql"))?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            version => ensure!(
                version == SCHEMA_VERSION,
                "Unsupported application state version {version}"
            ),
        }
        tx.commit()?;
        Ok(db)
    }
}
