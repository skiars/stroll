use std::{
    env,
    fs::DirBuilder,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

pub(crate) mod http;

const MAX_CONTENT_BYTES: i64 = 1024 * 1024 * 1024;
const MAX_CONTENT_ENTRIES: i64 = 65_536;
const MAX_SOURCE_ENTRIES: i64 = 1_048_576;
const MAX_SOURCE_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy)]
pub(crate) struct SourceKey([u8; 32]);

impl SourceKey {
    pub(crate) fn new(namespace: &str, parts: &[&[u8]]) -> Self {
        let mut hash = Sha256::new();
        for part in std::iter::once(namespace.as_bytes()).chain(parts.iter().copied()) {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part);
        }
        Self(hash.finalize().into())
    }
}

#[derive(Clone)]
pub(crate) struct ResourceCache(Arc<CacheDatabase>);

struct CacheDatabase {
    root: PathBuf,
    connection: Mutex<Option<Connection>>,
}

pub(crate) struct CachedSource<V> {
    pub(crate) version: V,
    pub(crate) hash: [u8; 32],
}

impl ResourceCache {
    pub(crate) fn for_desktop() -> Option<Self> {
        static CACHE: OnceLock<Option<ResourceCache>> = OnceLock::new();
        CACHE
            .get_or_init(|| {
                env::var_os("STROLL_GPUI_DATA")
                    .map(PathBuf::from)
                    .or_else(|| dirs::cache_dir().map(|path| path.join("stroll")))
                    .map(|root| Self::new(root.join("resources-v2")))
            })
            .clone()
    }

    pub(crate) fn new(root: PathBuf) -> Self {
        Self(Arc::new(CacheDatabase {
            root,
            connection: Mutex::new(None),
        }))
    }

    pub(crate) fn source<V: DeserializeOwned>(&self, key: SourceKey) -> Option<CachedSource<V>> {
        let (hash, version): ([u8; 32], Vec<u8>) = self
            .database(|db| {
                let source = db.query_row(
                    "SELECT hash, version FROM sources WHERE key = ?1
                 AND EXISTS (SELECT 1 FROM content WHERE content.hash = sources.hash)",
                    [key.0],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let _ = db.execute(
                    "UPDATE sources SET used = ?2 WHERE key = ?1",
                    params![key.0, now()],
                );
                Ok(source)
            })
            .ok()?;
        Some(CachedSource {
            hash,
            version: serde_json::from_slice(&version).ok()?,
        })
    }

    pub(crate) fn read_content(&self, hash: &[u8; 32], limit: u64) -> Option<Vec<u8>> {
        let limit = i64::try_from(limit).ok()?;
        let bytes: Vec<u8> = self
            .database(|db| {
                Ok(db.query_row(
                    "SELECT data FROM blobs WHERE hash = ?1 AND length(data) <= ?2",
                    params![hash, limit],
                    |row| row.get(0),
                )?)
            })
            .ok()?;
        if &<[u8; 32]>::from(Sha256::digest(&bytes)) != hash {
            return None;
        }
        let _ = self.database(|db| {
            Ok(db.execute(
                "UPDATE content SET used = ?2 WHERE hash = ?1",
                params![hash, now()],
            )?)
        });
        Some(bytes)
    }

    pub(crate) fn store<V: Serialize>(
        &self,
        key: SourceKey,
        version: &V,
        bytes: &[u8],
    ) -> Result<()> {
        let hash: [u8; 32] = Sha256::digest(bytes).into();
        let version = serde_json::to_vec(version)?;
        ensure!(
            version.len() <= MAX_SOURCE_BYTES,
            "Resource cache index is too large"
        );
        ensure!(
            bytes.len() as u64 <= MAX_CONTENT_BYTES as u64,
            "Cached resource is too large"
        );
        self.database(|db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let used = now();
            tx.execute(
                "INSERT INTO content(hash, size, used) VALUES (?1, ?2, ?3)
                 ON CONFLICT(hash) DO UPDATE SET size = excluded.size, used = excluded.used",
                params![hash, bytes.len() as i64, used],
            )?;
            tx.execute(
                "INSERT INTO blobs(hash, data) VALUES (?1, ?2)
                 ON CONFLICT(hash) DO UPDATE SET data = excluded.data WHERE data <> excluded.data",
                params![hash, bytes],
            )?;
            tx.execute(
                "INSERT INTO sources(key, hash, version, used) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(key) DO UPDATE SET
                    hash = excluded.hash, version = excluded.version, used = excluded.used",
                params![key.0, hash, version, used],
            )?;
            let evicted = prune(
                &tx,
                MAX_CONTENT_BYTES,
                MAX_CONTENT_ENTRIES,
                MAX_SOURCE_ENTRIES,
            )?;
            tx.commit()?;
            if evicted {
                let _ = db.execute_batch("PRAGMA incremental_vacuum(256)");
            }
            Ok(())
        })
    }

    pub(crate) fn forget(&self, key: SourceKey) {
        let _ = self.database(|db| Ok(db.execute("DELETE FROM sources WHERE key = ?1", [key.0])?));
    }

    fn database<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut guard = self.0.connection.lock().unwrap();
        if guard.is_none() {
            let mut directory = DirBuilder::new();
            directory.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                directory.mode(0o700);
            }
            directory.create(&self.0.root)?;
            let mut db = Connection::open(self.0.root.join("cache.sqlite3"))?;
            db.busy_timeout(Duration::from_millis(500))?;
            db.execute_batch(
                "PRAGMA auto_vacuum = INCREMENTAL;
                 PRAGMA journal_mode = WAL;
                 PRAGMA foreign_keys = ON;",
            )?;
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(SCHEMA)?;
            tx.commit()?;
            *guard = Some(db);
        }
        f(guard.as_mut().unwrap())
    }

    #[cfg(test)]
    pub(crate) fn count_contents(&self) -> i64 {
        self.database(|db| Ok(db.query_row("SELECT contents FROM usage", [], |row| row.get(0))?))
            .unwrap()
    }

    #[cfg(test)]
    pub(crate) fn corrupt_content(&self, hash: &[u8; 32]) -> Result<()> {
        self.database(|db| {
            db.execute(
                "UPDATE blobs SET data = ?2 WHERE hash = ?1",
                params![hash, &b"corrupt"[..]],
            )?;
            Ok(())
        })
    }
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS content(hash BLOB PRIMARY KEY, size INTEGER NOT NULL, used INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS blobs(
        hash BLOB PRIMARY KEY REFERENCES content(hash) ON DELETE CASCADE, data BLOB NOT NULL
    );
    CREATE TABLE IF NOT EXISTS sources(
        key BLOB PRIMARY KEY, hash BLOB NOT NULL, version BLOB NOT NULL, used INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS content_lru ON content(used, hash);
    CREATE INDEX IF NOT EXISTS source_lru ON sources(used, key);

    CREATE TABLE IF NOT EXISTS usage(
        id INTEGER PRIMARY KEY CHECK(id = 1), contents INTEGER, bytes INTEGER, sources INTEGER
    );
    INSERT OR IGNORE INTO usage VALUES (1, 0, 0, 0);
    CREATE TRIGGER IF NOT EXISTS content_insert AFTER INSERT ON content BEGIN
        UPDATE usage SET contents = contents + 1, bytes = bytes + NEW.size WHERE id = 1;
    END;
    CREATE TRIGGER IF NOT EXISTS content_resize AFTER UPDATE OF size ON content
        WHEN NEW.size <> OLD.size BEGIN
        UPDATE usage SET bytes = bytes + NEW.size - OLD.size WHERE id = 1;
    END;
    CREATE TRIGGER IF NOT EXISTS content_delete AFTER DELETE ON content BEGIN
        UPDATE usage SET contents = contents - 1, bytes = bytes - OLD.size WHERE id = 1;
    END;
    CREATE TRIGGER IF NOT EXISTS source_insert AFTER INSERT ON sources BEGIN
        UPDATE usage SET sources = sources + 1 WHERE id = 1;
    END;
    CREATE TRIGGER IF NOT EXISTS source_delete AFTER DELETE ON sources BEGIN
        UPDATE usage SET sources = sources - 1 WHERE id = 1;
    END;";

fn prune(db: &Connection, max_bytes: i64, max_contents: i64, max_sources: i64) -> Result<bool> {
    let (mut count, mut bytes, sources): (i64, i64, i64) =
        db.query_row("SELECT contents, bytes, sources FROM usage", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    let excess_sources = (sources - max_sources).max(0);
    if excess_sources > 0 {
        db.execute(
            "DELETE FROM sources WHERE key IN (SELECT key FROM sources ORDER BY used, key LIMIT ?1)",
            [excess_sources],
        )?;
    }
    let mut evicted = excess_sources > 0;
    while bytes > max_bytes || count > max_contents {
        let size: i64 = db.query_row(
            "DELETE FROM content WHERE hash = (SELECT hash FROM content ORDER BY used, hash LIMIT 1)
             RETURNING size",
            [],
            |row| row.get(0),
        ).optional()?.context("Missing oldest cached content")?;
        bytes -= size;
        count -= 1;
        evicted = true;
    }
    Ok(evicted)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_independent_lru_budgets_and_preserves_shared_content() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let cache = ResourceCache::new(directory.path().to_owned());
        let local = SourceKey::new("local", &[b"image.png"]);
        let ssh = SourceKey::new("ssh", &[b"image.png"]);
        let other = SourceKey::new("http", &[b"image.png"]);
        cache.store(local, &1, b"shared")?;
        cache.store(ssh, &2, b"shared")?;
        cache.store(other, &3, b"other")?;
        let shared = cache.source::<u32>(local).unwrap().hash;
        let oldest = cache.source::<u32>(other).unwrap().hash;
        cache.database(|db| {
            db.execute("UPDATE content SET used = 0", [])?;
            db.execute("UPDATE sources SET used = 0 WHERE key = ?1", [ssh.0])?;
            Ok(())
        })?;
        assert_eq!(
            cache.read_content(&shared, 16).as_deref(),
            Some(b"shared".as_slice())
        );
        cache.database(|db| Ok(prune(db, 6, 2, 2)?))?;
        assert_eq!(cache.count_contents(), 1);
        assert!(cache.read_content(&oldest, 16).is_none());
        assert!(cache.source::<u32>(other).is_none());
        assert!(cache.source::<u32>(ssh).is_none());
        assert_eq!(cache.source::<u32>(local).unwrap().version, 1);
        cache.database(|db| Ok(prune(db, MAX_CONTENT_BYTES, 0, MAX_SOURCE_ENTRIES)?))?;
        assert_eq!(cache.count_contents(), 0);
        assert!(cache.read_content(&shared, 16).is_none());
        Ok(())
    }
}
