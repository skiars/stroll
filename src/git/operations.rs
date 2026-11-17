use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{BufReader, Read, Seek, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use rust_i18n::t;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    FileAction, FileEdit, RepoState, WorktreeStatus, capture, capture_repository, command,
    file_version, head, read, repo::git_dir, worktree_edit::validate_path,
};
use crate::{remote::Bytes, repository::atomic_write};

const HISTORY_LIMIT: usize = 50;
const HISTORY_BYTES: usize = 64 * 1024 * 1024;
const HISTORY_WRITE_HEADROOM: usize = 512;
const ASSUME_UNCHANGED: u32 = 0x8000;
const INTENT_TO_ADD: u32 = 0x2000_0000;
const SKIP_WORKTREE: u32 = 0x4000_0000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Checkout {
    reference: String,
    head: Option<String>,
}

fn checkout(root: &Path) -> Result<Checkout> {
    let reference = fs::read_to_string(git_dir(root).join("HEAD"))?;
    Ok(Checkout {
        reference: reference.trim_end().to_owned(),
        head: head(root)?,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CommitState {
    checkout: Checkout,
    staged: [u8; 32],
    pub ready: bool,
    pub blocked: bool,
}

pub(crate) fn commit_state(root: &Path) -> Result<CommitState> {
    let checkout = checkout(root)?;
    let entries = read(root, ["ls-files", "--stage", "-z"])?;
    let conflict = entries.split(|&b| b == 0).any(|record| {
        record
            .split(|&b| b == b'\t')
            .next()
            .is_some_and(|header| !header.is_empty() && !header.ends_with(b" 0"))
    });
    let dir = git_dir(root);
    let blocked = conflict
        || [
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "rebase-merge",
            "rebase-apply",
            "sequencer",
        ]
        .iter()
        .any(|name| dir.join(name).exists());
    let mut diff = command(
        root,
        [
            "diff",
            "--cached",
            "--quiet",
            "--no-ext-diff",
            "--ignore-submodules=none",
            "--",
        ],
    );
    let status = diff.output()?;
    ensure!(
        matches!(status.status.code(), Some(0 | 1)),
        "{}",
        String::from_utf8_lossy(&status.stderr).trim_end()
    );
    Ok(CommitState {
        checkout,
        staged: Sha256::digest(entries).into(),
        ready: status.status.code() == Some(1),
        blocked,
    })
}

#[derive(Serialize, Deserialize)]
pub(crate) struct StagedCommitContext {
    pub files: Vec<(String, String, Option<String>)>,
    pub omitted: usize,
    pub truncated: bool,
    pub patch: String,
}

/// Gather bounded, read-only context from a private snapshot of the Git index.
pub(crate) fn staged_commit_context(
    root: &Path,
    expected: &CommitState,
) -> Result<Option<StagedCommitContext>> {
    const SUMMARY_LIMIT: usize = 12 * 1024;
    const PATCH_LIMIT: usize = 48 * 1024;

    let current = commit_state(root)?;
    ensure!(current == *expected, t!("git.worktree_changed"));
    ensure!(!current.blocked, t!("git.commit_blocked"));
    ensure!(current.ready, t!("git.nothing_to_commit"));

    let index = index_path(root)?;
    let mut scratch = tempfile::NamedTempFile::new()?;
    std::io::copy(&mut File::open(index)?, scratch.as_file_mut())?;
    let index = scratch.path();
    let mut list = command(root, ["ls-files", "--stage", "-z"]);
    list.env("GIT_INDEX_FILE", index);
    let observed: [u8; 32] = Sha256::digest(capture_repository(list)?).into();
    ensure!(observed == expected.staged, t!("git.worktree_changed"));

    // The explicit base keeps HEAD changes from affecting either half of the context.
    let empty;
    let base = if let Some(head) = &expected.checkout.head {
        head.as_str()
    } else {
        empty = super::empty_tree(root)?;
        &empty
    };
    let args = [
        "diff",
        "--cached",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--find-renames",
        "--ignore-submodules=dirty",
        "--submodule=short",
    ];
    let mut summary_command = command(
        root,
        args.into_iter().chain(["--name-status", "-z", base, "--"]),
    );
    summary_command.env("GIT_INDEX_FILE", index);
    let raw = capture_repository(summary_command)?;
    let mut records = raw
        .strip_suffix(&[0])
        .unwrap_or(&raw)
        .split(|byte| *byte == 0);
    if raw.is_empty() {
        return Ok(None);
    }
    let mut paths = BTreeSet::new();
    let mut files = Vec::new();
    let mut summary_size = 0;
    let mut omitted = 0;
    while let Some(status) = records.next() {
        let status = std::str::from_utf8(status)?;
        let old = records.next().context(t!("git.read_context_failed"))?;
        let new = if status.starts_with(['R', 'C']) {
            Some(records.next().context(t!("git.read_context_failed"))?)
        } else {
            None
        };
        let allowed = |path| {
            std::str::from_utf8(path)
                .ok()
                .filter(|path| crate::ai::context::allowed_path(path))
        };
        let Some(old) = allowed(old) else {
            omitted += 1;
            continue;
        };
        let new = new.map(allowed);
        if new == Some(None) {
            omitted += 1;
            continue;
        }
        let new = new.flatten();
        let entry_size = serde_json::to_vec(&(status, old, new))?.len() + 1;
        if summary_size + entry_size > SUMMARY_LIMIT {
            omitted += 1;
            continue;
        }
        summary_size += entry_size;
        files.push((status.to_owned(), old.to_owned(), new.map(str::to_owned)));
        paths.insert(old);
        paths.extend(new);
    }
    if paths.is_empty() {
        return Ok(None);
    }
    let mut patch = command(
        root,
        args.into_iter().chain(["--patch", base, "--"]).chain(paths),
    );
    patch.env("GIT_INDEX_FILE", index);
    let (mut patch, truncated) = capture(patch, PATCH_LIMIT, false)?;
    if truncated {
        patch.truncate(
            patch
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |pos| pos + 1),
        );
    }
    ensure!(commit_state(root)? == *expected, t!("git.worktree_changed"));
    Ok(Some(StagedCommitContext {
        files,
        omitted,
        truncated,
        patch: String::from_utf8_lossy(&patch).into_owned(),
    }))
}

fn index_path(root: &Path) -> Result<PathBuf> {
    let path = read(root, ["rev-parse", "--git-path", "index"])?;
    let path = root.join(std::str::from_utf8(&path)?.trim_end_matches(['\r', '\n']));
    // Git also uses this directory for its scratch index and lock file.
    Ok(crate::paths::simplified(&path))
}

pub(crate) fn create_commit(root: &Path, expected: &CommitState, message: &str) -> Result<String> {
    ensure!(
        !message.trim().is_empty() && !message.contains('\0'),
        t!("git.commit_message_required")
    );
    let mut input = tempfile::tempfile()?;
    input.write_all(message.as_bytes())?;
    input.rewind()?;
    let _lock = OperationLock::acquire(root)?;
    let current = commit_state(root)?;
    ensure!(current == *expected, t!("git.worktree_changed"));
    ensure!(!current.blocked, t!("git.commit_blocked"));
    ensure!(current.ready, t!("git.nothing_to_commit"));
    let mut git = command(root, ["commit", "--file=-"]);
    git.stdin(input);
    capture_repository(git)?;
    head(root)?.context(t!("git.worktree_changed"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum UndoAvailability {
    Available,
    Undone,
    Unavailable,
    Interrupted,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Operation {
    pub id: u64,
    pub seconds: u64,
    pub action: FileAction,
    pub paths: Vec<String>,
    pub availability: UndoAvailability,
    #[serde(default)]
    pub redoable: bool,
    #[serde(default)]
    pub obsolete: bool,
    #[serde(default)]
    pub undo_order: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    Prepared,
    Done,
    Undoing,
    Undone,
    Redoing,
}

#[derive(Serialize, Deserialize)]
struct Record {
    id: u64,
    seconds: u64,
    action: FileAction,
    checkout: Checkout,
    phase: Phase,
    #[serde(default, skip_serializing_if = "is_zero")]
    undo_order: u64,
    change: Change,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Serialize, Deserialize)]
enum Change {
    Index {
        entries: BTreeMap<String, IndexChange>,
        blobs: BTreeMap<String, Bytes>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        redo_blobs: BTreeMap<String, Bytes>,
    },
    Disk {
        path: String,
        before: Option<DiskEntry>,
        after: Option<DiskVersion>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        redo: Option<DiskEntry>,
    },
}

impl Record {
    fn paths(&self) -> Vec<String> {
        match &self.change {
            Change::Index { entries, .. } => entries.keys().cloned().collect(),
            Change::Disk { path, .. } => vec![path.clone()],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct IndexEntry {
    mode: String,
    oid: String,
    stage: u8,
    #[serde(default)]
    flags: u32,
}

#[derive(Serialize, Deserialize)]
struct IndexChange {
    before: Vec<IndexEntry>,
    after: Vec<IndexEntry>,
}

impl IndexChange {
    fn target(&self, undo: bool) -> &[IndexEntry] {
        if undo { &self.before } else { &self.after }
    }
}

#[derive(Serialize, Deserialize)]
enum DiskEntry {
    File {
        mode: u32,
        bytes: Bytes,
    },
    #[cfg(unix)]
    Symlink(Bytes),
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
enum DiskVersion {
    Missing,
    File {
        mode: u32,
        digest: [u8; 32],
    },
    #[cfg(unix)]
    Symlink([u8; 32]),
}

impl DiskVersion {
    fn before(entry: &Option<DiskEntry>) -> Self {
        match entry {
            None => Self::Missing,
            Some(DiskEntry::File { mode, bytes }) => Self::File {
                mode: *mode,
                digest: Sha256::digest(&bytes.0).into(),
            },
            #[cfg(unix)]
            Some(DiskEntry::Symlink(bytes)) => Self::Symlink(Sha256::digest(&bytes.0).into()),
        }
    }
}

struct OperationLock {
    directory: PathBuf,
    file: File,
}

impl OperationLock {
    fn acquire(root: &Path) -> Result<Self> {
        let directory = git_dir(root).join("stroll");
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        match builder.create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    fs::symlink_metadata(&directory)?.is_dir(),
                    t!("git.invalid_file_path")
                );
            }
            Err(error) => return Err(error.into()),
        }
        let path = directory.join("operations.lock");
        if let Ok(meta) = fs::symlink_metadata(&path) {
            ensure!(meta.is_file(), t!("git.invalid_file_path"));
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.try_lock().context(t!("git.git_busy"))?;
        Ok(Self { directory, file })
    }
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        _ = self.file.unlock();
    }
}

struct Journal {
    path: PathBuf,
    records: Vec<Record>,
    _lock: OperationLock,
}

impl Journal {
    fn open(root: &Path) -> Result<Self> {
        let lock = OperationLock::acquire(root)?;
        let path = lock.directory.join("edit-operations.json");
        let records = match fs::symlink_metadata(&path) {
            Ok(meta) => {
                ensure!(
                    meta.is_file() && meta.len() <= HISTORY_BYTES as u64,
                    t!("git.operation_history_invalid")
                );
                serde_json::from_reader(BufReader::new(File::open(&path)?))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        let mut journal = Self {
            path,
            records,
            _lock: lock,
        };
        journal.reconcile(root)?;
        Ok(journal)
    }

    fn save(&self) -> Result<()> {
        let bytes = serde_json::to_vec(&self.records)?;
        ensure!(
            bytes.len() <= HISTORY_BYTES,
            t!("git.operation_history_limit")
        );
        atomic_write(&self.path, &bytes)
    }

    fn prepare(&mut self, checkout: Checkout, action: FileAction, change: Change) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let id = (now.as_millis() as u64).max(self.records.last().map_or(0, |last| last.id + 1));
        self.records.push(Record {
            id,
            seconds: now.as_secs(),
            action,
            checkout,
            phase: Phase::Prepared,
            undo_order: 0,
            change,
        });
        let excess = self.records.len().saturating_sub(HISTORY_LIMIT);
        self.records.drain(..excess);
        // Leave room to record the post-restore digest and update operation states.
        let limit = HISTORY_BYTES - HISTORY_WRITE_HEADROOM;
        let bytes = serde_json::to_vec(&self.records)?;
        if bytes.len() <= limit {
            return atomic_write(&self.path, &bytes);
        }
        let mut size = bytes.len();
        let mut discard = 0;
        for record in &self.records[..self.records.len() - 1] {
            if size <= limit {
                break;
            }
            size -= serde_json::to_vec(record)?.len() + 1;
            discard += 1;
        }
        ensure!(size <= limit, t!("git.operation_history_limit"));
        self.records.drain(..discard);
        self.save()
    }

    fn complete(&mut self) -> Result<Operation> {
        let record = self.records.last_mut().unwrap();
        record.phase = Phase::Done;
        let operation = Operation {
            id: record.id,
            seconds: record.seconds,
            action: record.action,
            paths: record.paths(),
            availability: UndoAvailability::Available,
            redoable: false,
            obsolete: false,
            undo_order: 0,
        };
        self.save()?;
        Ok(operation)
    }

    fn reconcile(&mut self, root: &Path) -> Result<()> {
        if !self.records.iter().any(|record| {
            matches!(
                record.phase,
                Phase::Prepared | Phase::Undoing | Phase::Redoing
            )
        }) {
            return Ok(());
        }
        let current = checkout(root)?;
        let mut changed = false;
        let mut discard = BTreeSet::new();
        for record in &mut self.records {
            if !matches!(
                record.phase,
                Phase::Prepared | Phase::Undoing | Phase::Redoing
            ) || !same_context(record, &current)
            {
                continue;
            }
            let (before, after) = match &record.change {
                Change::Index { entries, .. } => {
                    let index = index_entries(root, None)?;
                    (
                        matches_index(&index, entries, false),
                        matches_index(&index, entries, true),
                    )
                }
                Change::Disk {
                    path,
                    before,
                    after,
                    ..
                } => {
                    let Ok(version) = disk_version(root, path) else {
                        continue;
                    };
                    (
                        version == DiskVersion::before(before),
                        after.as_ref() == Some(&version),
                    )
                }
            };
            if record.phase == Phase::Prepared && before && !after {
                discard.insert(record.id);
            } else if matches!(record.phase, Phase::Undoing | Phase::Redoing) && before {
                record.phase = Phase::Undone;
                changed = true;
            } else if after {
                record.phase = Phase::Done;
                changed = true;
            }
        }
        if !discard.is_empty() {
            self.records.retain(|record| !discard.contains(&record.id));
            changed = true;
        }
        if changed {
            self.save()?;
        }
        Ok(())
    }
}

fn same_context(record: &Record, checkout: &Checkout) -> bool {
    record.checkout.reference == checkout.reference
        && (record.action == FileAction::Restore || record.checkout.head == checkout.head)
}

fn availability(records: &[Record], position: usize, current: &Checkout) -> UndoAvailability {
    let record = &records[position];
    match record.phase {
        Phase::Prepared | Phase::Undoing | Phase::Redoing => UndoAvailability::Interrupted,
        Phase::Undone => UndoAvailability::Undone,
        Phase::Done => {
            let disk = record.action == FileAction::Restore;
            let paths = record.paths();
            let covered = records[position + 1..].iter().any(|next| {
                next.phase != Phase::Undone
                    && (next.action == FileAction::Restore) == disk
                    && same_context(next, current)
                    && next.paths().iter().any(|path| paths.contains(path))
            });
            if covered || !same_context(record, current) {
                UndoAvailability::Unavailable
            } else {
                UndoAvailability::Available
            }
        }
    }
}

fn redoable(records: &[Record], position: usize, current: &Checkout) -> bool {
    let record = &records[position];
    if record.phase != Phase::Undone || !same_context(record, current) {
        return false;
    }
    if let Change::Disk { after, redo, .. } = &record.change
        && (after.is_none() || redo.is_none() && after.as_ref() != Some(&DiskVersion::Missing))
    {
        return false;
    }
    let disk = record.action == FileAction::Restore;
    let paths = record.paths();
    !records.iter().enumerate().any(|(index, other)| {
        index != position
            && (other.action == FileAction::Restore) == disk
            && same_context(other, current)
            && (index < position) == (other.phase == Phase::Undone)
            && other.paths().iter().any(|path| paths.contains(path))
    })
}

pub(crate) fn operation_history(root: &Path) -> Result<Vec<Operation>> {
    let journal = Journal::open(root)?;
    let current = checkout(root)?;
    Ok(journal
        .records
        .iter()
        .enumerate()
        .rev()
        .map(|(position, record)| Operation {
            id: record.id,
            seconds: record.seconds,
            action: record.action,
            paths: record.paths(),
            availability: availability(&journal.records, position, &current),
            redoable: redoable(&journal.records, position, &current),
            obsolete: record.action != FileAction::Restore && record.checkout != current,
            undo_order: record.undo_order,
        })
        .collect())
}

/// Read the other side of a text restore whose editor transaction was not recorded while verifying the current worktree.
pub(crate) fn restore_history_text(
    root: &Path,
    id: u64,
    undo: bool,
) -> Result<Option<(Bytes, [u8; 32])>> {
    let journal = Journal::open(root)?;
    let Some(record) = journal.records.iter().find(|record| record.id == id) else {
        return Ok(None);
    };
    let Change::Disk {
        path,
        before,
        after,
        redo,
    } = &record.change
    else {
        return Ok(None);
    };
    let baseline = DiskVersion::before(before);
    let (target, expected) = if undo {
        (before, after.as_ref())
    } else {
        (redo, Some(&baseline))
    };
    let (Some(DiskEntry::File { bytes, .. }), Some(expected @ DiskVersion::File { digest, .. })) =
        (target, expected)
    else {
        return Ok(None);
    };
    if bytes.0.len() as u64 > crate::repository::MAX_FILE_BYTES
        || std::str::from_utf8(&bytes.0).is_err()
        || bytes.0.contains(&0)
    {
        return Ok(None);
    }
    ensure!(
        disk_version(root, path)? == *expected,
        t!("git.undo_conflict")
    );
    Ok(Some((Bytes(bytes.0.clone()), *digest)))
}

type IndexEntries = BTreeMap<Vec<u8>, Vec<IndexEntry>>;

fn index_entries(root: &Path, index: Option<&Path>) -> Result<IndexEntries> {
    let mut git = command(root, ["ls-files", "--stage", "--debug", "-z"]);
    if let Some(index) = index {
        git.env("GIT_INDEX_FILE", index);
    }
    let raw = capture_repository(git)?;
    let mut entries = IndexEntries::new();
    let mut remaining = raw.as_slice();
    while !remaining.is_empty() {
        let nul = remaining
            .iter()
            .position(|&byte| byte == 0)
            .context(t!("git.operation_history_invalid"))?;
        let record = &remaining[..nul];
        remaining = &remaining[nul + 1..];
        // NUL terminates the entry name. Git then emits five lines of cache metadata.
        let mut debug = remaining.splitn(6, |&byte| byte == b'\n');
        let flags = debug
            .nth(4)
            .and_then(|line| line.split(|&byte| byte == b'\t').last())
            .and_then(|field| field.strip_prefix(b"flags: "))
            .context(t!("git.operation_history_invalid"))?;
        let flags = u32::from_str_radix(std::str::from_utf8(flags)?, 16)?
            & (ASSUME_UNCHANGED | INTENT_TO_ADD | SKIP_WORKTREE);
        remaining = debug.next().context(t!("git.operation_history_invalid"))?;
        let (header, path) = record.split_at(
            record
                .iter()
                .position(|&byte| byte == b'\t')
                .context(t!("git.operation_history_invalid"))?,
        );
        let header = std::str::from_utf8(header)?;
        let mut fields = header.split_ascii_whitespace();
        let mode = fields
            .next()
            .context(t!("git.operation_history_invalid"))?
            .to_owned();
        let oid = fields
            .next()
            .context(t!("git.operation_history_invalid"))?
            .to_owned();
        let stage = fields
            .next()
            .context(t!("git.operation_history_invalid"))?
            .parse()?;
        entries
            .entry(path[1..].to_vec())
            .or_default()
            .push(IndexEntry {
                mode,
                oid,
                stage,
                flags,
            });
    }
    Ok(entries)
}

fn matches_index(
    index: &IndexEntries,
    changes: &BTreeMap<String, IndexChange>,
    after: bool,
) -> bool {
    changes.iter().all(|(path, change)| {
        let actual = index.get(path.as_bytes()).map_or(&[][..], Vec::as_slice);
        actual == if after { &change.after } else { &change.before }
    })
}

struct IndexTransaction {
    index: PathBuf,
    lock: PathBuf,
    file: Option<File>,
    temp: tempfile::TempDir,
}

impl IndexTransaction {
    fn start(root: &Path) -> Result<Self> {
        let index = index_path(root)?;
        let parent = index.parent().context(t!("git.invalid_file_path"))?;
        let temp = tempfile::tempdir_in(parent)?;
        let mut lock = index.as_os_str().to_os_string();
        lock.push(".lock");
        let lock = PathBuf::from(lock);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .context(t!("git.git_busy"))?;
        let transaction = Self {
            index,
            lock,
            file: Some(file),
            temp,
        };
        match fs::copy(&transaction.index, transaction.scratch()) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(transaction)
    }

    fn scratch(&self) -> PathBuf {
        self.temp.path().join("index")
    }

    fn publish(mut self) -> Result<()> {
        let mut index = File::open(self.scratch())?;
        let file = self.file.as_mut().unwrap();
        file.set_permissions(index.metadata()?.permissions())?;
        std::io::copy(&mut index, file)?;
        file.sync_all()?;
        drop(self.file.take());
        if let Err(error) = fs::rename(&self.lock, &self.index) {
            _ = fs::remove_file(&self.lock);
            return Err(error.into());
        }
        Ok(())
    }
}

impl Drop for IndexTransaction {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            drop(file);
            _ = fs::remove_file(&self.lock);
        }
    }
}

fn indexed_operation(
    root: &Path,
    action: FileAction,
    mutate: impl FnOnce(&Path) -> Result<()>,
) -> Result<Option<Operation>> {
    let mut journal = Journal::open(root)?;
    let transaction = IndexTransaction::start(root)?;
    let expected = checkout(root)?;
    let before = index_entries(root, None)?;
    let scratch = transaction.scratch();
    mutate(&scratch)?;
    let after = index_entries(root, Some(&scratch))?;
    let mut entries = BTreeMap::new();
    let paths = before.keys().chain(after.keys()).collect::<BTreeSet<_>>();
    for path in paths {
        if before.get(path) != after.get(path) {
            let name = std::str::from_utf8(path)?;
            validate_path(name)?;
            entries.insert(
                name.to_owned(),
                IndexChange {
                    before: before.get(path).cloned().unwrap_or_default(),
                    after: after.get(path).cloned().unwrap_or_default(),
                },
            );
        }
    }
    if entries.is_empty() {
        return Ok(None);
    }
    let blobs = backup_index_blobs(root, &expected, &entries, true)?;
    ensure!(checkout(root)? == expected, t!("git.worktree_changed"));
    journal.prepare(
        expected,
        action,
        Change::Index {
            entries,
            blobs,
            redo_blobs: BTreeMap::new(),
        },
    )?;
    transaction.publish()?;
    journal.complete().map(Some)
}

fn backup_index_blobs(
    root: &Path,
    checkout: &Checkout,
    entries: &BTreeMap<String, IndexChange>,
    before: bool,
) -> Result<BTreeMap<String, Bytes>> {
    // Objects in the expected HEAD remain reachable for as long as this record can be undone.
    let base = checkout
        .head
        .as_deref()
        .map(|head| {
            read(
                root,
                ["ls-tree", "-r", "-z", head, "--"]
                    .into_iter()
                    .chain(entries.keys().map(String::as_str)),
            )
        })
        .transpose()?
        .unwrap_or_default();
    let reachable = base
        .split(|&b| b == 0)
        .filter_map(|record| {
            let header = record.split(|&b| b == b'\t').next()?;
            header.split(|&b| b == b' ').nth(2)
        })
        .collect::<BTreeSet<_>>();
    let mut remaining = HISTORY_BYTES;
    let mut blobs = BTreeMap::new();
    for entry in entries.values().flat_map(|change| change.target(before)) {
        if entry.mode == "160000"
            || reachable.contains(entry.oid.as_bytes())
            || blobs.contains_key(&entry.oid)
        {
            continue;
        }
        let (bytes, truncated) = super::capture(
            command(root, ["cat-file", "blob", &entry.oid]),
            remaining,
            false,
        )?;
        ensure!(!truncated, t!("git.operation_history_limit"));
        remaining -= bytes.len();
        blobs.insert(entry.oid.clone(), Bytes(bytes));
    }
    Ok(blobs)
}

pub(crate) fn stage_operation(
    root: &Path,
    expected: &WorktreeStatus,
    paths: &[String],
    staged: bool,
) -> Result<Option<Operation>> {
    for path in paths {
        validate_path(path)?;
    }
    let action = if staged {
        FileAction::Stage
    } else {
        FileAction::Unstage
    };
    indexed_operation(root, action, |index| {
        super::changes::stage_in_index(root, expected, paths, staged, Some(index))
    })
}

pub(crate) fn edit_operation(root: &Path, edit: &FileEdit) -> Result<Option<Operation>> {
    if edit.action != FileAction::Restore {
        return indexed_operation(root, edit.action, |index| {
            super::worktree_edit::edit_file_in_index(&RepoState::oneshot(root), edit, Some(index))
        });
    }
    let mut journal = Journal::open(root)?;
    let expected = checkout(root)?;
    let state = RepoState::oneshot(root);
    ensure!(
        file_version(&state, &edit.path)? == edit.version,
        t!("git.worktree_changed")
    );
    let before = backup_disk(root, &edit.path)?;
    ensure!(
        disk_version(root, &edit.path)? == DiskVersion::before(&before),
        t!("git.worktree_changed")
    );
    let after = edit
        .version
        .status
        .as_ref()
        .filter(|status| status.untracked)
        .map(|_| DiskVersion::Missing);
    journal.prepare(
        expected,
        edit.action,
        Change::Disk {
            path: edit.path.clone(),
            before,
            after,
            redo: None,
        },
    )?;
    super::edit_file(&RepoState::oneshot(root), edit)?;
    let Change::Disk { after, .. } = &mut journal.records.last_mut().unwrap().change else {
        unreachable!()
    };
    let version = disk_version(root, &edit.path)?;
    ensure!(
        after.as_ref().is_none_or(|expected| *expected == version),
        t!("git.worktree_changed")
    );
    *after = Some(version);
    journal.complete().map(Some)
}

fn checked_disk_path(root: &Path, path: &str, create: bool) -> Result<PathBuf> {
    validate_path(path)?;
    let mut current = root.canonicalize()?;
    let mut components = Path::new(path).components().peekable();
    while let Some(component) = components.next() {
        current.push(component);
        if components.peek().is_some() {
            match fs::symlink_metadata(&current) {
                Ok(meta) => ensure!(
                    meta.is_dir() && !meta.is_symlink(),
                    t!("git.symlink_directory")
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if create {
                        fs::create_dir(&current)?;
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(current)
}

fn disk_metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn file_mode(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        meta.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        u32::from(meta.permissions().readonly())
    }
}

fn backup_disk(root: &Path, path: &str) -> Result<Option<DiskEntry>> {
    let path = checked_disk_path(root, path, false)?;
    let Some(meta) = disk_metadata(&path)? else {
        return Ok(None);
    };
    #[cfg(unix)]
    if meta.is_symlink() {
        use std::os::unix::ffi::OsStringExt as _;
        return Ok(Some(DiskEntry::Symlink(Bytes(
            fs::read_link(&path)?.into_os_string().into_vec(),
        ))));
    }
    ensure!(meta.is_file() && !meta.is_symlink(), t!("git.file_type"));
    let mut bytes = Vec::new();
    File::open(&path)?
        .take(HISTORY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= HISTORY_BYTES,
        t!("git.operation_history_limit")
    );
    Ok(Some(DiskEntry::File {
        mode: file_mode(&meta),
        bytes: Bytes(bytes),
    }))
}

fn disk_version(root: &Path, path: &str) -> Result<DiskVersion> {
    let path = checked_disk_path(root, path, false)?;
    let Some(meta) = disk_metadata(&path)? else {
        return Ok(DiskVersion::Missing);
    };
    #[cfg(unix)]
    if meta.is_symlink() {
        let target = fs::read_link(&path)?;
        return Ok(DiskVersion::Symlink(
            Sha256::digest(target.as_os_str().as_encoded_bytes()).into(),
        ));
    }
    ensure!(meta.is_file() && !meta.is_symlink(), t!("git.file_type"));
    let mut digest = Sha256::new();
    let mut file = File::open(path)?;
    let mut chunk = [0; 16 * 1024];
    loop {
        let count = file.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        digest.update(&chunk[..count]);
    }
    Ok(DiskVersion::File {
        mode: file_mode(&meta),
        digest: digest.finalize().into(),
    })
}

fn write_disk(
    root: &Path,
    path: &str,
    entry: &Option<DiskEntry>,
    expected: &DiskVersion,
) -> Result<()> {
    ensure!(
        &disk_version(root, path)? == expected,
        t!("git.undo_conflict")
    );
    let target = checked_disk_path(root, path, entry.is_some())?;
    let parent = target.parent().context(t!("git.invalid_file_path"))?;
    match entry {
        None => fs::remove_file(target)?,
        Some(DiskEntry::File { mode, bytes }) => {
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            file.write_all(&bytes.0)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                file.as_file()
                    .set_permissions(fs::Permissions::from_mode(*mode))?;
            }
            #[cfg(not(unix))]
            {
                let mut permissions = file.as_file().metadata()?.permissions();
                permissions.set_readonly(*mode != 0);
                file.as_file().set_permissions(permissions)?;
            }
            file.as_file().sync_all()?;
            ensure!(
                &disk_version(root, path)? == expected,
                t!("git.undo_conflict")
            );
            if *expected == DiskVersion::Missing {
                file.persist_noclobber(target)?;
            } else {
                file.persist(target)?;
            }
        }
        #[cfg(unix)]
        Some(DiskEntry::Symlink(bytes)) => {
            use std::os::unix::{ffi::OsStrExt as _, fs::symlink};
            let link = std::ffi::OsStr::from_bytes(&bytes.0);
            if *expected == DiskVersion::Missing {
                symlink(link, target)?;
            } else {
                let temp = tempfile::tempdir_in(parent)?;
                let file = temp.path().join("link");
                symlink(link, &file)?;
                ensure!(
                    &disk_version(root, path)? == expected,
                    t!("git.undo_conflict")
                );
                fs::rename(file, target)?;
            }
        }
    }
    Ok(())
}

fn git_input(root: &Path, args: &[&str], input: &[u8]) -> Result<Command> {
    let mut data = tempfile::tempfile()?;
    data.write_all(input)?;
    data.rewind()?;
    let mut git = command(root, args);
    git.stdin(data);
    Ok(git)
}

fn replay_index(
    root: &Path,
    transaction: IndexTransaction,
    entries: &BTreeMap<String, IndexChange>,
    blobs: &BTreeMap<String, Bytes>,
    undo: bool,
) -> Result<()> {
    ensure!(
        matches_index(&index_entries(root, None)?, entries, undo),
        t!("git.undo_conflict")
    );
    for (oid, bytes) in blobs {
        let output = capture_repository(git_input(
            root,
            &["hash-object", "-w", "--stdin"],
            &bytes.0,
        )?)?;
        ensure!(
            std::str::from_utf8(&output)?.trim_end() == oid,
            t!("git.operation_history_invalid")
        );
    }
    let mut input = Vec::new();
    let zeros = "0".repeat(
        entries
            .values()
            .flat_map(|entry| entry.after.iter().chain(&entry.before))
            .next()
            .context(t!("git.operation_history_invalid"))?
            .oid
            .len(),
    );
    for path in entries.keys() {
        validate_path(path)?;
        write!(input, "0 {zeros}\t{path}\0")?;
    }
    // Remove all paths before replacing them, including file/directory swaps.
    for (path, change) in entries {
        for entry in change.target(undo) {
            ensure!(
                matches!(
                    entry.mode.as_str(),
                    "100644" | "100755" | "120000" | "160000" | "040000"
                ) && entry.oid.len() == zeros.len()
                    && entry.oid.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && entry.stage <= 3,
                t!("git.operation_history_invalid")
            );
            if entry.flags & INTENT_TO_ADD == 0 {
                write!(
                    input,
                    "{} {} {}\t{path}\0",
                    entry.mode, entry.oid, entry.stage
                )?;
            }
        }
    }
    let scratch = transaction.scratch();
    let mut git = git_input(root, &["update-index", "-z", "--index-info"], &input)?;
    git.env("GIT_INDEX_FILE", &scratch);
    capture_repository(git)?;
    restore_index_flags(root, &transaction, entries, undo)?;
    let updated = index_entries(root, Some(&scratch))?;
    ensure!(
        matches_index(&updated, entries, !undo),
        t!("git.undo_conflict")
    );
    // A file/directory replacement can cause Git to remove other index paths.
    let current = index_entries(root, None)?;
    ensure!(
        current.keys().chain(updated.keys()).all(|path| {
            current.get(path) == updated.get(path)
                || std::str::from_utf8(path).is_ok_and(|path| entries.contains_key(path))
        }),
        t!("git.undo_conflict")
    );
    transaction.publish()
}

fn restore_index_flags(
    root: &Path,
    transaction: &IndexTransaction,
    entries: &BTreeMap<String, IndexChange>,
    undo: bool,
) -> Result<()> {
    let paths = |flag| {
        entries.iter().filter_map(move |(path, change)| {
            let entry = change
                .target(undo)
                .iter()
                .find(|entry| entry.flags & flag != 0)?;
            Some((path, entry))
        })
    };
    let scratch = transaction.scratch();
    let intents = paths(INTENT_TO_ADD).collect::<Vec<_>>();
    if !intents.is_empty() {
        let worktree = transaction.temp.path().join("worktree");
        let mut input = Vec::new();
        for (path, entry) in intents {
            let file = worktree.join(path);
            fs::create_dir_all(file.parent().context(t!("git.invalid_file_path"))?)?;
            #[cfg(unix)]
            if entry.mode == "120000" {
                std::os::unix::fs::symlink("placeholder", &file)?;
            }
            if !file.is_symlink() {
                let file = File::create(file)?;
                #[cfg(unix)]
                if entry.mode == "100755" {
                    use std::os::unix::fs::PermissionsExt as _;
                    file.set_permissions(fs::Permissions::from_mode(0o755))?;
                }
                let _ = (file, entry);
            }
            write!(input, "{path}\0")?;
        }
        // Recreate intents against placeholder files so undo never depends on the working copies.
        let mut git = git_input(
            &worktree,
            &[
                "add",
                "--intent-to-add",
                "--force",
                "--sparse",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
            &input,
        )?;
        git.env(
            "GIT_DIR",
            crate::paths::simplified(&git_dir(root).canonicalize()?),
        )
        .env("GIT_WORK_TREE", &worktree)
        .env("GIT_INDEX_FILE", &scratch);
        capture_repository(git)?;
    }
    for (flag, option) in [
        (ASSUME_UNCHANGED, "--assume-unchanged"),
        (SKIP_WORKTREE, "--skip-worktree"),
    ] {
        let mut input = Vec::new();
        for (path, _) in paths(flag) {
            write!(input, "{path}\0")?;
        }
        if !input.is_empty() {
            let mut git = git_input(root, &["update-index", option, "-z", "--stdin"], &input)?;
            git.env("GIT_INDEX_FILE", &scratch);
            capture_repository(git)?;
        }
    }
    Ok(())
}

pub(crate) fn undo_operation(root: &Path, id: u64) -> Result<()> {
    replay_operation(root, id, true)
}

pub(crate) fn replay_operation(root: &Path, id: u64, undo: bool) -> Result<()> {
    let mut journal = Journal::open(root)?;
    let position = journal
        .records
        .iter()
        .position(|record| record.id == id)
        .context(t!("git.undo_conflict"))?;
    let current = checkout(root)?;
    let available = if undo {
        availability(&journal.records, position, &current) == UndoAvailability::Available
    } else {
        redoable(&journal.records, position, &current)
    };
    ensure!(available, t!("git.undo_conflict"));
    let undo_order = journal
        .records
        .iter()
        .map(|record| record.undo_order)
        .max()
        .unwrap_or_default()
        + 1;
    let record = &mut journal.records[position];
    let transaction = match &mut record.change {
        Change::Index {
            entries,
            redo_blobs,
            ..
        } => {
            let transaction = IndexTransaction::start(root)?;
            ensure!(
                matches_index(&index_entries(root, None)?, entries, undo),
                t!("git.undo_conflict")
            );
            if undo {
                *redo_blobs =
                    backup_index_blobs(root, &current, entries, false).unwrap_or_default();
            }
            Some(transaction)
        }
        Change::Disk {
            path,
            before,
            after,
            redo,
        } => {
            let actual = disk_version(root, path)?;
            if undo {
                ensure!(after.as_ref() == Some(&actual), t!("git.undo_conflict"));
                *redo = backup_disk(root, path)
                    .ok()
                    .filter(|entry| DiskVersion::before(entry) == actual)
                    .flatten();
            } else {
                ensure!(
                    DiskVersion::before(before) == actual,
                    t!("git.undo_conflict")
                );
                ensure!(
                    after.as_ref() == Some(&DiskVersion::before(redo)),
                    t!("git.undo_conflict")
                );
            }
            None
        }
    };
    ensure!(checkout(root)? == current, t!("git.undo_conflict"));
    record.phase = if undo { Phase::Undoing } else { Phase::Redoing };
    if undo {
        record.undo_order = undo_order;
    }
    if undo && serde_json::to_vec(&journal.records)?.len() > HISTORY_BYTES - HISTORY_WRITE_HEADROOM
    {
        // An undo must still be possible when its inverse would outgrow the history.
        match &mut journal.records[position].change {
            Change::Index { redo_blobs, .. } => redo_blobs.clear(),
            Change::Disk { redo, .. } => *redo = None,
        }
    }
    journal.save()?;
    let result = match &journal.records[position].change {
        Change::Index {
            entries,
            blobs,
            redo_blobs,
        } => replay_index(
            root,
            transaction.unwrap(),
            entries,
            if undo { blobs } else { redo_blobs },
            undo,
        ),
        Change::Disk {
            path,
            before,
            after,
            redo,
        } => {
            if undo {
                write_disk(root, path, before, after.as_ref().unwrap())
            } else {
                write_disk(root, path, redo, &DiskVersion::before(before))
            }
        }
    };
    if result.is_ok() {
        journal.records[position].phase = if undo { Phase::Undone } else { Phase::Done };
        journal.save()?;
    } else {
        journal.reconcile(root)?;
    }
    result
}
