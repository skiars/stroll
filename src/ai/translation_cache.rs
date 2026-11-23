use std::{
    collections::HashMap,
    fs::OpenOptions,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use rusqlite::{Connection, TransactionBehavior, params, params_from_iter};

pub(super) type CacheKey = [u8; 32];

const MAX_ENTRIES: i64 = 16_384;
const MAX_BYTES: i64 = 64 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct TranslationCache {
    path: Arc<PathBuf>,
    connection: Arc<Mutex<Option<Connection>>>,
}

impl TranslationCache {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            path: Arc::new(path),
            connection: Arc::default(),
        }
    }

    pub(super) fn path(&self) -> &Arc<PathBuf> {
        &self.path
    }

    pub(super) fn load(&self, keys: &[CacheKey]) -> Result<HashMap<CacheKey, String>> {
        self.database(|db| {
            let placeholders = vec!["?"; keys.len()].join(", ");
            let mut query = db.prepare_cached(&format!(
                "SELECT key, translation FROM translations WHERE key IN ({placeholders})"
            ))?;
            let results =
                query.query_map(params_from_iter(keys), |row| Ok((row.get(0)?, row.get(1)?)))?;
            Ok(results.collect::<rusqlite::Result<_>>()?)
        })
    }

    pub(super) fn store(&self, entries: &[(CacheKey, String)]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        self.database(|db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            {
                let mut insert = tx.prepare_cached(
                    "INSERT OR REPLACE INTO translations (key, translation) VALUES (?1, ?2)",
                )?;
                for (key, translation) in entries {
                    insert.execute(params![key, translation])?;
                }
            }
            prune(&tx, MAX_ENTRIES, MAX_BYTES)?;
            tx.commit()?;
            Ok(())
        })
    }

    fn database<T>(&self, operation: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut connection = self.connection.lock().unwrap();
        if connection.is_none() {
            *connection = Some(open(&self.path)?);
        }
        operation(connection.as_mut().unwrap())
    }
}

fn open(path: &Path) -> Result<Connection> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)?;
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_millis(250))?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS translations (
            id INTEGER PRIMARY KEY,
            key BLOB NOT NULL UNIQUE,
            translation TEXT NOT NULL,
            bytes INTEGER GENERATED ALWAYS AS (length(CAST(translation AS BLOB))) STORED
        );
        CREATE INDEX IF NOT EXISTS translations_usage ON translations (bytes);",
    )?;
    Ok(db)
}

fn prune(db: &Connection, max_entries: i64, max_bytes: i64) -> Result<()> {
    let (entries, bytes): (i64, i64) = db.query_row(
        "SELECT count(*), coalesce(sum(bytes), 0) FROM translations",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if entries > max_entries || bytes > max_bytes {
        db.execute(
            "DELETE FROM translations WHERE id IN (
                SELECT id FROM (
                    SELECT id, row_number() OVER newest AS entries, sum(bytes) OVER newest AS bytes
                    FROM translations WINDOW newest AS (ORDER BY id DESC)
                ) WHERE entries > ?1 OR bytes > ?2
            )",
            params![max_entries, max_bytes],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_oldest_entries_to_fit_entry_and_byte_limits() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let cache = TranslationCache::new(directory.path().join("translations.sqlite3"));
        let oldest = [1; 32];
        let middle = [2; 32];
        let newest = [3; 32];
        cache.store(&[
            (oldest, "a".into()),
            (middle, "文".into()),
            (newest, "字".into()),
        ])?;
        cache.database(|db| prune(db, 2, 6))?;
        let keys = [oldest, middle, newest];
        assert_eq!(cache.load(&keys)?.len(), 2);
        cache.database(|db| prune(db, 2, 5))?;
        let remaining = cache.load(&keys)?;
        assert_eq!(remaining, HashMap::from([(newest, "字".into())]));
        Ok(())
    }
}
