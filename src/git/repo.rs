//! Repository facts and caches shared within an operation.
//!
//! On Windows, a Git subprocess can take around 60 ms because of MSYS2 startup and real-time scanning.
//! Querying index and HEAD entries separately for each path used to take 30–40 processes per patch.
//! Cache facts that depend only on the index and HEAD in [`Facts`], and share them along with one
//! status result through [`RepoState`] for each operation. A persistent `git cat-file --batch` process
//! serves blob reads.
//!
//! Worktree file contents are never cached. Status is shared within a single [`RepoState`]
//! and refreshed from Git for each operation to keep the panel current.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

use anyhow::Result;

use super::{METADATA_LIMIT, WorktreeStatus, capture, command, empty_tree, head, read};
use crate::git::changes::{read_blob, worktree_status};

type FileStamp = Option<(u64, Option<SystemTime>)>;

fn file_stamp(path: &Path) -> FileStamp {
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()))
}

/// A regular repository uses `<root>/.git`. Worktrees and submodules use a `gitdir:` file pointing elsewhere.
pub(super) fn git_dir(root: &Path) -> PathBuf {
    let dot = root.join(".git");
    let Ok(text) = std::fs::read_to_string(&dot) else {
        return dot;
    };
    let Some(target) = text.strip_prefix("gitdir:").map(str::trim) else {
        return dot;
    };
    if target.is_empty() {
        return dot;
    }
    let target = Path::new(target);
    if target.is_absolute() {
        target.to_owned()
    } else {
        root.join(target)
    }
}

/// Fingerprint the index, HEAD, and ref updates using filesystem metadata without spawning Git.
///
/// Detect repository changes such as commit, checkout, reset, or add. Worktree edits are outside
/// this fingerprint, so checking whether a diff is current also requires checking file contents.
#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct Stamp(Vec<FileStamp>);

impl Stamp {
    fn of(git_dir: &Path) -> Self {
        let mut stamps = Vec::with_capacity(5);
        stamps.push(file_stamp(&git_dir.join("index")));
        stamps.push(file_stamp(&git_dir.join("HEAD")));
        stamps.push(file_stamp(&git_dir.join("packed-refs")));
        // Ref updates write the reflog, including commits that leave the index intact.
        stamps.push(file_stamp(&git_dir.join("logs/HEAD")));
        if let Ok(text) = std::fs::read_to_string(git_dir.join("HEAD"))
            && let Some(reference) = text.strip_prefix("ref:").map(str::trim)
            && !reference.is_empty()
        {
            stamps.push(file_stamp(&git_dir.join(reference)));
        }
        Self(stamps)
    }
}

/// Index and HEAD facts reusable across operations.
pub(super) struct Facts {
    stamp: Stamp,
    head: OnceLock<Option<String>>,
    index: OnceLock<Arc<IndexSnapshot>>,
    empty: OnceLock<String>,
    #[cfg(windows)]
    canonical: OnceLock<Option<PathBuf>>,
}

impl Facts {
    fn new(stamp: Stamp) -> Self {
        Self {
            stamp,
            head: OnceLock::new(),
            index: OnceLock::new(),
            empty: OnceLock::new(),
            #[cfg(windows)]
            canonical: OnceLock::new(),
        }
    }
}

/// Repository-wide `git ls-files --stage -z` snapshot, with raw record bytes indexed by path.
///
/// If output exceeds 32 MB, fall back to per-path queries so large repositories still work.
pub(super) struct IndexSnapshot {
    records: Option<HashMap<String, Vec<u8>>>,
    root: PathBuf,
}

impl IndexSnapshot {
    fn fetch(root: &Path) -> Self {
        let records = match capture(
            command(root, ["ls-files", "--stage", "-z"]),
            METADATA_LIMIT,
            false,
        ) {
            Ok((bytes, false)) => Some(Self::parse(&bytes)),
            _ => None,
        };
        Self {
            records,
            root: root.to_owned(),
        }
    }

    fn parse(bytes: &[u8]) -> HashMap<String, Vec<u8>> {
        let mut records = HashMap::new();
        for record in bytes
            .split(|&byte| byte == 0)
            .filter(|part| !part.is_empty())
        {
            let Some(tab) = record.iter().position(|&byte| byte == b'\t') else {
                continue;
            };
            let Ok(path) = std::str::from_utf8(&record[tab + 1..]) else {
                continue;
            };
            // Keep all index stages for conflicted paths.
            records
                .entry(path.to_owned())
                .or_insert_with(Vec::new)
                .extend_from_slice(record);
        }
        records
    }

    /// Equivalent to `git ls-files --stage -z -- <path>` with the trailing NUL removed.
    pub(super) fn record(&self, path: &str) -> Result<Vec<u8>> {
        match &self.records {
            Some(records) => Ok(records.get(path).cloned().unwrap_or_default()),
            None => read(&self.root, ["ls-files", "--stage", "-z", "--", path]),
        }
    }
}

/// Repository view shared within an operation.
pub(crate) struct RepoState {
    root: PathBuf,
    facts: Arc<Facts>,
    status: OnceLock<Arc<WorktreeStatus>>,
    blobs: Option<Arc<BlobReader>>,
}

impl RepoState {
    fn new(root: &Path, facts: Arc<Facts>, blobs: Option<Arc<BlobReader>>) -> Self {
        Self {
            root: root.to_owned(),
            facts,
            status: OnceLock::new(),
            blobs,
        }
    }

    /// State with no shared cache for one-shot calls and tests.
    pub(crate) fn oneshot(root: &Path) -> Self {
        let git_dir = git_dir(root);
        let facts = Arc::new(Facts::new(Stamp::of(&git_dir)));
        Self::new(root, facts, None)
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn head(&self) -> Result<Option<String>> {
        if let Some(head) = self.facts.head.get() {
            return Ok(head.clone());
        }
        let value = head(&self.root)?;
        Ok(self.facts.head.get_or_init(|| value).clone())
    }

    pub(super) fn index(&self) -> Result<&IndexSnapshot> {
        if let Some(index) = self.facts.index.get() {
            return Ok(index);
        }
        let value = Arc::new(IndexSnapshot::fetch(&self.root));
        Ok(self.facts.index.get_or_init(|| value))
    }

    pub(crate) fn status(&self) -> Result<&Arc<WorktreeStatus>> {
        if let Some(status) = self.status.get() {
            return Ok(status);
        }
        let value = Arc::new(worktree_status(&self.root)?);
        Ok(self.status.get_or_init(|| value))
    }

    pub(super) fn empty_tree(&self) -> Result<&str> {
        if let Some(empty) = self.facts.empty.get() {
            return Ok(empty);
        }
        let value = empty_tree(&self.root)?;
        Ok(self.facts.empty.get_or_init(|| value))
    }

    /// Read a blob, falling back to a one-shot process if persistent `cat-file --batch` is unavailable.
    pub(super) fn blob(&self, spec: &str, limit: usize) -> Result<Option<(Vec<u8>, bool)>> {
        if let Some(blobs) = &self.blobs
            && let Some(value) = blobs.read(spec, limit)
        {
            return Ok(Some(value));
        }
        read_blob(&self.root, spec, limit)
    }

    pub(super) fn read_disk(&self, path: &str, limit: usize) -> Result<Option<(Vec<u8>, bool)>> {
        super::changes::read_disk_at(self, path, limit)
    }

    /// Check whether the index, HEAD, and refs are unchanged since this state was created.
    pub(super) fn unchanged(&self) -> bool {
        let git_dir = git_dir(&self.root);
        Stamp::of(&git_dir) == self.facts.stamp
    }

    #[cfg(windows)]
    pub(super) fn canonical_root(&self) -> Result<Option<&Path>> {
        use std::io::ErrorKind;
        if let Some(canonical) = self.facts.canonical.get() {
            return Ok(canonical.as_deref());
        }
        let canonical = match self.root.canonicalize() {
            Ok(root) => Some(root),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        Ok(self.facts.canonical.get_or_init(|| canonical).as_deref())
    }
}

/// A persistent `git cat-file --batch` process serves multiple reads, replacing the two one-shot
/// calls (`cat-file -t` and `cat-file blob`) per blob.
struct BlobReader {
    root: PathBuf,
    child: Mutex<Option<BlobChild>>,
}

struct BlobChild {
    process: Child,
    input: BufWriter<ChildStdin>,
    output: BufReader<ChildStdout>,
}

/// Objects larger than this limit are left for the caller to read with a one-shot command.
const BLOB_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

impl BlobReader {
    fn open(root: &Path) -> Result<Self> {
        let mut git = command(root, ["cat-file", "--batch"]);
        let mut process = git.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        let input = BufWriter::new(process.stdin.take().expect("piped stdin"));
        let output = BufReader::new(process.stdout.take().expect("piped stdout"));
        Ok(Self {
            root: root.to_owned(),
            child: Mutex::new(Some(BlobChild {
                process,
                input,
                output,
            })),
        })
    }

    /// `None` means batch reading is unavailable for this request and the caller should use a one-shot read.
    fn read(&self, spec: &str, limit: usize) -> Option<(Vec<u8>, bool)> {
        // Newlines in paths would break batch message framing, so use a one-shot read.
        if spec.contains('\n') || spec.contains('\0') {
            return None;
        }
        let mut child = self.child.lock().unwrap();
        let Some(active) = child.as_mut() else {
            return None;
        };
        match active.read(spec, limit) {
            Ok(value) => value.map(|(bytes, truncated)| (bytes, truncated)),
            Err(()) => {
                if let Some(mut dead) = child.take() {
                    let _ = dead.process.kill();
                    let _ = dead.process.wait();
                }
                // Retry with a fresh process once, then let the caller fall back to a one-shot read.
                match BlobReader::open(&self.root) {
                    Ok(fresh) => {
                        let mut fresh = fresh
                            .child
                            .into_inner()
                            .unwrap()
                            .expect("Expected a new child process");
                        let value = fresh.read(spec, limit).ok().flatten();
                        *child = Some(fresh);
                        value
                    }
                    Err(_) => None,
                }
            }
        }
    }
}

impl BlobChild {
    /// Return `Ok(None)` when the object is missing or is not a blob.
    fn read(&mut self, spec: &str, limit: usize) -> Result<Option<(Vec<u8>, bool)>, ()> {
        self.input.write_all(spec.as_bytes()).map_err(|_| ())?;
        self.input.write_all(b"\n").map_err(|_| ())?;
        self.input.flush().map_err(|_| ())?;
        let mut header = Vec::new();
        let read = self.output.read_until(b'\n', &mut header).map_err(|_| ())?;
        if read == 0 {
            return Err(());
        }
        if header.last() != Some(&b'\n') {
            return Err(());
        }
        header.pop();
        let header = String::from_utf8(header).map_err(|_| ())?;
        let mut fields = header.split(' ');
        let (Some(_oid), Some(kind), Some(size)) = (fields.next(), fields.next(), fields.next())
        else {
            // `<spec> missing` or `<spec> ambiguous`.
            return Ok(None);
        };
        let size = size.parse::<usize>().map_err(|_| ())?;
        if size > BLOB_MEMORY_LIMIT {
            return Err(());
        }
        let mut bytes = Vec::with_capacity(size.min(limit) + 1);
        self.output
            .by_ref()
            .take(size.min(limit) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| ())?;
        let oversized = size > limit;
        let consume = if oversized { size - limit } else { 0 } + 1;
        let copied = std::io::copy(
            &mut self.output.by_ref().take(consume as u64),
            &mut std::io::sink(),
        )
        .map_err(|_| ())?;
        if copied as usize != consume {
            return Err(());
        }
        if kind != "blob" {
            return Ok(None);
        }
        if oversized {
            bytes.truncate(limit);
        }
        Ok(Some((bytes, oversized)))
    }
}

/// One session per repository root shares repository facts and owns the persistent blob reader.
pub(crate) struct GitSession {
    root: PathBuf,
    git_dir: OnceLock<PathBuf>,
    facts: Mutex<Option<Arc<Facts>>>,
    blobs: Mutex<Option<Arc<BlobReader>>>,
    blobs_off: AtomicBool,
}

static SESSIONS: OnceLock<Mutex<HashMap<PathBuf, Arc<GitSession>>>> = OnceLock::new();

/// Reuse sessions by worktree root. Start a blob reader only when needed.
pub(crate) fn session_for(root: &Path) -> Arc<GitSession> {
    let sessions = SESSIONS.get_or_init(Default::default);
    let mut sessions = sessions.lock().unwrap();
    if let Some(session) = sessions.get(root) {
        return session.clone();
    }
    let session = Arc::new(GitSession::new(root));
    sessions.insert(root.to_owned(), session.clone());
    session
}

impl GitSession {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            git_dir: OnceLock::new(),
            facts: Mutex::new(None),
            blobs: Mutex::new(None),
            blobs_off: AtomicBool::new(false),
        }
    }

    fn state(&self) -> Result<RepoState> {
        let git_dir = self.git_dir.get_or_init(|| git_dir(&self.root)).clone();
        let stamp = Stamp::of(&git_dir);
        let mut cached = self.facts.lock().unwrap();
        let facts = match cached.as_ref() {
            Some(facts) if facts.stamp == stamp => facts.clone(),
            _ => {
                let facts = Arc::new(Facts::new(stamp));
                *cached = Some(facts.clone());
                facts
            }
        };
        drop(cached);
        Ok(RepoState::new(&self.root, facts, self.blob_reader()))
    }

    fn blob_reader(&self) -> Option<Arc<BlobReader>> {
        if self.blobs_off.load(Ordering::Relaxed) {
            return None;
        }
        let mut blobs = self.blobs.lock().unwrap();
        if let Some(reader) = blobs.as_ref() {
            return Some(reader.clone());
        }
        match BlobReader::open(&self.root) {
            Ok(reader) => {
                let reader = Arc::new(reader);
                *blobs = Some(reader.clone());
                Some(reader)
            }
            Err(_) => {
                self.blobs_off.store(true, Ordering::Relaxed);
                None
            }
        }
    }

    pub(crate) fn call<T>(&self, local: impl FnOnce(&RepoState) -> Result<T>) -> Result<T> {
        let state = self.state()?;
        local(&state)
    }

    /// For callers that combine multiple requests, such as validation in `edit_file`.
    pub(crate) fn open_state(&self) -> Result<RepoState> {
        self.state()
    }

    /// Invalidate cached facts after this process changes the index or HEAD through stage, unstage, or apply.
    pub(crate) fn invalidate(&self) {
        *self.facts.lock().unwrap() = None;
    }
}
