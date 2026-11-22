use rust_i18n::t;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    path::{Component, Path},
    sync::Arc,
};

use anyhow::{Context, Result, bail, ensure};
#[cfg(feature = "desktop")]
use gpui_component::diff::{DiffFile, DiffParseOptions};

use super::{Commit, RepoState, capture, command, head, object_id, read, text};
use crate::{
    remote::Bytes,
    repository::{MAX_ASSET_BYTES, files::EntryVersion, resolve_file, resources::ResourceFile},
};

const PATCH_LIMIT: usize = 4 * 1024 * 1024;
const SNAPSHOT_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Mode {
    #[default]
    All,
    Staged,
    Unstaged,
}

#[cfg(feature = "desktop")]
impl Mode {
    pub fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::All => t!("git.all_changes"),
            Self::Staged => t!("git.staged"),
            Self::Unstaged => t!("git.unstaged"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Comparison {
    Commit {
        commit: Box<Commit>,
        parent: Option<String>,
    },
    Worktree {
        mode: Mode,
        base: Option<String>,
    },
    Versions {
        base: Option<String>,
        target: Option<String>,
    },
}

impl Comparison {
    pub(crate) fn commit_target(&self) -> Option<&str> {
        match self {
            Self::Commit { commit, .. } => Some(&commit.oid),
            Self::Versions { target, .. } => target.as_deref(),
            Self::Worktree { .. } => None,
        }
    }

    pub(crate) fn worktree_mode(&self) -> Option<Mode> {
        match self {
            Self::Worktree { mode, .. } => Some(*mode),
            Self::Versions { target: None, .. } => Some(Mode::All),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct WorktreeFile {
    pub old_path: Option<String>,
    pub staged: bool,
    pub unstaged: bool,
    pub untracked: bool,
    pub conflict: bool,
    pub(super) staged_rename: bool,
    pub(super) unstaged_rename: bool,
    pub code: char,
}

pub(crate) type WorktreeStatus = BTreeMap<String, WorktreeFile>;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct ChangedFile {
    pub path: String,
    pub old_path: Option<String>,
    pub code: char,
    pub old_mode: String,
    pub new_mode: String,
    pub counts: Option<(usize, usize)>,
    pub binary: bool,
    pub worktree: Option<WorktreeFile>,
}

impl ChangedFile {
    pub(super) fn new(path: String, code: char) -> Self {
        Self {
            path,
            code,
            old_path: None,
            old_mode: String::new(),
            new_mode: String::new(),
            counts: None,
            binary: false,
            worktree: None,
        }
    }

    pub fn submodule(&self) -> bool {
        self.old_mode == "160000" || self.new_mode == "160000"
    }

    pub fn conflict(&self) -> bool {
        self.code == 'U'
    }

    pub(crate) fn untracked(&self) -> bool {
        self.worktree.as_ref().is_some_and(|file| file.untracked)
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Changes {
    pub comparison: Comparison,
    pub base: String,
    pub files: Vec<ChangedFile>,
    /// Worktree status snapshot shared with the repository-wide status in `RepoState`.
    pub status: Option<Arc<WorktreeStatus>>,
}

fn fields(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|&byte| byte == 0)
        .filter(|part| !part.is_empty())
}

pub(super) fn worktree_status(root: &Path) -> Result<WorktreeStatus> {
    let output = read(
        root,
        [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
            "--find-renames",
        ],
    )?;
    let mut records = fields(&output);
    let mut files = BTreeMap::new();
    while let Some(record) = records.next() {
        let [index, disk, b' ', path @ ..] = record else {
            bail!(t!("git.invalid_worktree_status"))
        };
        let old_path = if matches!(index, b'R' | b'C') || matches!(disk, b'R' | b'C') {
            let old = records.next().context(t!("git.missing_rename_source"))?;
            let Ok(old) = std::str::from_utf8(old) else {
                continue;
            };
            Some(old.to_owned())
        } else {
            None
        };
        let Ok(path) = std::str::from_utf8(path) else {
            continue;
        };
        let untracked = *index == b'?';
        if untracked && path.ends_with('/') {
            continue;
        }
        let conflict = *index == b'U'
            || *disk == b'U'
            || matches!((*index, *disk), (b'A', b'A') | (b'D', b'D'));
        let code = match (*index, *disk) {
            _ if conflict => 'U',
            (b'R', b' ') => 'R',
            (_, b' ') => char::from(*index),
            (_, disk) => char::from(disk),
        };
        let state: &mut WorktreeFile = files.entry(path.to_owned()).or_default();
        state.staged |= !untracked && *index != b' ';
        state.unstaged |= !untracked && *disk != b' ';
        state.untracked |= untracked;
        state.conflict |= conflict;
        state.staged_rename |= *index == b'R';
        state.unstaged_rename |= *disk == b'R';
        if !untracked || state.code == '\0' {
            state.code = code;
            state.old_path = old_path;
        }
    }
    Ok(files)
}

fn parse_changes(output: &[u8]) -> Result<Vec<ChangedFile>> {
    let mut fields = output.split(|&byte| byte == 0).peekable();
    let mut files = BTreeMap::new();
    while let Some(header) = fields.next_if(|field| field.starts_with(b":")) {
        let header = text(&header[1..]);
        let fields_meta = header.split_ascii_whitespace().collect::<Vec<_>>();
        let [old_mode, new_mode, _, _, status] = fields_meta.as_slice() else {
            bail!(t!("git.invalid_changes"));
        };
        let code = status
            .chars()
            .next()
            .context(t!("git.missing_file_status"))?;
        let first = fields.next().context(t!("git.missing_file_path"))?;
        let (old_path, path) = if matches!(code, 'R' | 'C') {
            let path = fields.next().context(t!("git.missing_rename_target"))?;
            let Ok(old) = std::str::from_utf8(first) else {
                continue;
            };
            (Some(old.to_owned()), path)
        } else {
            (None, first)
        };
        let Ok(path) = std::str::from_utf8(path) else {
            continue;
        };
        let mut file = ChangedFile::new(path.to_owned(), code);
        file.old_path = old_path;
        file.old_mode = (*old_mode).to_owned();
        file.new_mode = (*new_mode).to_owned();
        if file.conflict() || !files.get(path).is_some_and(ChangedFile::conflict) {
            files.insert(path.to_owned(), file);
        }
    }
    for (path, stat) in parse_stats(fields)? {
        if let Some(file) = files.get_mut(&path) {
            file.binary = stat.binary;
            file.counts = stat.counts;
        }
    }
    Ok(files.into_values().collect())
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct ChangeStat {
    pub binary: bool,
    pub counts: Option<(usize, usize)>,
}

pub(crate) type ChangeStats = BTreeMap<String, ChangeStat>;

fn parse_stats<'a>(mut fields: impl Iterator<Item = &'a [u8]>) -> Result<ChangeStats> {
    let mut stats = BTreeMap::new();
    while let Some(record) = fields.next().filter(|part| !part.is_empty()) {
        let mut parts = record.splitn(3, |&byte| byte == b'\t');
        let (Some(add), Some(del), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            bail!(t!("git.invalid_numstat"));
        };
        let path = if path.is_empty() {
            fields.next().context(t!("git.missing_old_numstat_path"))?;
            fields.next().context(t!("git.missing_new_numstat_path"))?
        } else {
            path
        };
        if let Ok(path) = std::str::from_utf8(path) {
            stats.insert(
                path.to_owned(),
                ChangeStat {
                    binary: add == b"-",
                    counts: text(add).parse().ok().zip(text(del).parse().ok()),
                },
            );
        }
    }
    Ok(stats)
}

pub(super) const DIFF_ARGS: &[&str] = &[
    "diff",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-relative",
    "--ignore-submodules=none",
    "--submodule=short",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--output-indicator-new=+",
    "--output-indicator-old=-",
    "--output-indicator-context= ",
    "--find-renames",
];

impl Changes {
    fn revisions(&self) -> Vec<&str> {
        match &self.comparison {
            Comparison::Commit { commit, .. } => vec![self.base.as_str(), &commit.oid],
            Comparison::Worktree {
                mode: Mode::Staged, ..
            } => vec!["--cached", &self.base],
            Comparison::Worktree {
                mode: Mode::Unstaged,
                ..
            } => Vec::new(),
            Comparison::Worktree { .. } => vec![&self.base],
            Comparison::Versions { target, .. } => std::iter::once(self.base.as_str())
                .chain(target.as_deref())
                .collect(),
        }
    }
}

pub(crate) fn changes(state: &RepoState, comparison: Comparison) -> Result<Changes> {
    read_changes(state, comparison, true)
}

pub(crate) fn review_files(state: &RepoState, comparison: Comparison) -> Result<Changes> {
    read_changes(state, comparison, false)
}

fn comparison_base(state: &RepoState, comparison: &Comparison) -> Result<String> {
    let base = match comparison {
        Comparison::Commit { commit, parent } => {
            ensure!(
                object_id(&commit.oid)
                    && parent
                        .as_ref()
                        .is_none_or(|id| { object_id(id) && commit.parents.contains(id) }),
                t!("git.invalid_commit_base")
            );
            parent.clone()
        }
        Comparison::Worktree { base, .. } => {
            ensure!(
                base.as_ref().is_none_or(|id| object_id(id)),
                t!("git.invalid_branch_base")
            );
            match base {
                Some(base) => Some(base.clone()),
                None => state.head()?,
            }
        }
        Comparison::Versions { base, target } => {
            ensure!(
                base.iter().chain(target).all(|id| object_id(id)),
                t!("git.invalid_commit_id")
            );
            base.clone()
        }
    };
    match base {
        Some(base) => Ok(base),
        None => Ok(state.empty_tree()?.to_owned()),
    }
}

pub(crate) fn review_stats(
    state: &RepoState,
    comparison: Comparison,
    expected_base: &str,
) -> Result<ChangeStats> {
    let base = comparison_base(state, &comparison)?;
    ensure!(base == expected_base, t!("git.worktree_changed"));
    let detail = Changes {
        comparison,
        base,
        files: Vec::new(),
        status: None,
    };
    let output = read(
        state.root(),
        DIFF_ARGS
            .iter()
            .copied()
            .chain(["--numstat", "-z"])
            .chain(detail.revisions())
            .chain(["--"]),
    )?;
    parse_stats(output.split(|&byte| byte == 0))
}

fn read_changes(state: &RepoState, comparison: Comparison, stats: bool) -> Result<Changes> {
    let base = comparison_base(state, &comparison)?;
    let mode = comparison.worktree_mode();
    let status = mode
        .is_some()
        .then(|| state.status().map(Arc::clone))
        .transpose()?;
    let default_worktree = matches!(comparison, Comparison::Worktree { base: None, .. });
    let custom_worktree = matches!(comparison, Comparison::Versions { target: None, .. });
    let mut detail = Changes {
        comparison,
        base,
        files: Vec::new(),
        status,
    };
    let output = read(
        state.root(),
        DIFF_ARGS
            .iter()
            .copied()
            .chain(["--raw", "--no-abbrev", "-z"])
            .chain(stats.then_some("--numstat"))
            .chain(detail.revisions())
            .chain(["--"]),
    )?;
    detail.files = parse_changes(&output)?;
    if let (Some(mode), Some(status)) = (mode, &detail.status) {
        let mut files = detail
            .files
            .into_iter()
            .map(|file| (file.path.clone(), file))
            .collect::<BTreeMap<_, _>>();
        for (path, entry) in status.iter() {
            let relevant = match mode {
                Mode::All => true,
                Mode::Staged => entry.staged,
                Mode::Unstaged => entry.unstaged || entry.untracked,
            };
            if !relevant {
                continue;
            }
            let file = if let Some(file) = files.get_mut(path) {
                file
            } else if mode == Mode::All && default_worktree
                || entry.conflict
                || entry.untracked && mode != Mode::Staged
            {
                let mut file = ChangedFile::new(path.clone(), entry.code);
                file.old_path = entry.old_path.clone();
                files.entry(path.clone()).or_insert(file)
            } else {
                continue;
            };
            file.worktree = Some(entry.clone());
            if entry.conflict {
                file.code = 'U';
                file.counts = None;
            } else if entry.untracked && mode != Mode::Staged {
                if custom_worktree
                    && file.code == 'D'
                    && let Some(disk_mode) = untracked_mode(state.root(), path)
                {
                    file.new_mode = disk_mode.into();
                    if file.old_mode == file.new_mode
                        && let Ok(Some((new, false))) = state.read_disk(path, SNAPSHOT_LIMIT)
                        && let Ok(Some((old, false))) =
                            state.blob(&format!("{}:{path}", detail.base), new.len())
                        && old == new
                    {
                        files.remove(path);
                        continue;
                    }
                }
                file.code = if file.old_mode.is_empty() || file.old_mode == "000000" {
                    '?'
                } else {
                    'M'
                };
                file.counts = None;
                file.binary = false;
            }
        }
        detail.files = files.into_values().collect();
    }
    // Directories precede leaf files at each level, as in the file index.
    detail.files.sort_by(|a, b| path_order(&a.path, &b.path));
    Ok(detail)
}

fn untracked_mode(root: &Path, path: &str) -> Option<&'static str> {
    let meta = fs::symlink_metadata(root.join(path)).ok()?;
    if meta.is_symlink() {
        return Some("120000");
    }
    if !meta.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o100 != 0 {
            return Some("100755");
        }
    }
    Some("100644")
}

fn path_order(a: &str, b: &str) -> std::cmp::Ordering {
    let mut a = a.split('/').peekable();
    let mut b = b.split('/').peekable();
    while let (Some(left), Some(right)) = (a.next(), b.next()) {
        let order = (a.peek().is_none(), left).cmp(&(b.peek().is_none(), right));
        if order.is_ne() {
            return order;
        }
    }
    std::cmp::Ordering::Equal
}

pub(crate) fn stage(
    root: &Path,
    expected: &WorktreeStatus,
    paths: &[String],
    staged: bool,
) -> Result<()> {
    stage_in_index(root, expected, paths, staged, None)
}

pub(super) fn stage_in_index(
    root: &Path,
    expected: &WorktreeStatus,
    paths: &[String],
    staged: bool,
    index: Option<&Path>,
) -> Result<()> {
    ensure!(!paths.is_empty(), t!("git.select_files"));
    let current = worktree_status(root)?;
    let mut actual = BTreeSet::new();
    for path in paths {
        let state = current
            .get(path)
            .filter(|state| Some(*state) == expected.get(path))
            .context(t!("git.worktree_changed"))?;
        ensure!(
            !state.conflict
                && if staged {
                    state.unstaged || state.untracked
                } else {
                    state.staged
                },
            t!("git.cannot_stage")
        );
        actual.insert(path.as_str());
        if staged && state.unstaged_rename || !staged && state.staged_rename {
            actual.extend(state.old_path.as_deref());
        }
    }

    let mut input = tempfile::tempfile()?;
    use std::io::{Seek as _, Write as _};
    for path in actual {
        input.write_all(path.as_bytes())?;
        input.write_all(&[0])?;
    }
    input.rewind()?;
    let args = if staged {
        vec!["add", "-A"]
    } else if head(root)?.is_some() {
        vec!["restore", "--staged", "--source=HEAD"]
    } else {
        vec!["rm", "--cached", "-f", "--ignore-unmatch"]
    };
    let mut git = command(
        root,
        args.into_iter()
            .chain(["--pathspec-from-file=-", "--pathspec-file-nul"]),
    );
    if let Some(index) = index {
        git.env("GIT_INDEX_FILE", index);
    }
    git.stdin(input);
    capture(git, super::METADATA_LIMIT, false)?;
    Ok(())
}

#[cfg(feature = "desktop")]
pub(crate) struct Patch {
    pub document: DiffFile,
    pub truncated: bool,
    pub counts: Option<(usize, usize)>,
}

#[derive(Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Side {
    Old,
    New,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) enum Snapshot {
    Text { text: String, truncated: bool },
    Binary,
    Missing,
    Submodule,
}

pub(super) fn read_disk_at(
    state: &RepoState,
    path: &str,
    limit: usize,
) -> Result<Option<(Vec<u8>, bool)>> {
    let relative = Path::new(path);
    ensure!(
        relative
            .components()
            .all(|part| matches!(part, Component::Normal(_))),
        t!("git.invalid_file_path")
    );
    // On Windows, normalize ordinary roots and extended-length paths returned by canonicalize.
    // Cache the normalized roots in the state to avoid two extra system calls per file.
    #[cfg(windows)]
    let root = match state.canonical_root()? {
        Some(root) => root,
        None => return Ok(None),
    };
    #[cfg(not(windows))]
    let root = state.root();
    let path = root.join(relative);
    let parent = path.parent().context(t!("git.invalid_file_path"))?;
    match parent.canonicalize() {
        Ok(canonical) => ensure!(canonical == parent, t!("git.symlink_directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if meta.is_symlink() {
        let target = fs::read_link(path)?;
        return Ok(Some((
            target.as_os_str().as_encoded_bytes().to_vec(),
            false,
        )));
    }
    ensure!(meta.is_file(), t!("git.file_type"));
    let mut bytes = Vec::new();
    File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    Ok(Some((bytes, truncated)))
}

pub(super) fn read_blob(root: &Path, spec: &str, limit: usize) -> Result<Option<(Vec<u8>, bool)>> {
    let kind = command(root, ["cat-file", "-t", spec]).output()?;
    if !kind.status.success() || kind.stdout != b"blob\n" {
        return Ok(None);
    }
    Ok(Some(capture(
        command(root, ["cat-file", "blob", spec]),
        limit,
        false,
    )?))
}

enum FileSide<'a> {
    Blob(String),
    Worktree(&'a str),
}

fn file_side<'a>(detail: &Changes, file: &'a ChangedFile, side: Side) -> FileSide<'a> {
    let path = match side {
        Side::Old => file.old_path.as_deref().unwrap_or(&file.path),
        Side::New => &file.path,
    };
    let spec = match (&detail.comparison, side) {
        (comparison, Side::New) if let Some(target) = comparison.commit_target() => {
            format!("{target}:{path}")
        }
        (
            Comparison::Worktree {
                mode: Mode::Staged, ..
            },
            Side::New,
        )
        | (
            Comparison::Worktree {
                mode: Mode::Unstaged,
                ..
            },
            Side::Old,
        ) => format!(":{path}"),
        (_, Side::New) => return FileSide::Worktree(path),
        (_, Side::Old) => format!("{}:{path}", detail.base),
    };
    FileSide::Blob(spec)
}

fn read_side(
    state: &RepoState,
    detail: &Changes,
    file: &ChangedFile,
    side: Side,
    limit: usize,
) -> Result<Option<(Vec<u8>, bool)>> {
    match file_side(detail, file, side) {
        FileSide::Blob(spec) => state.blob(&spec, limit),
        FileSide::Worktree(path) => state.read_disk(path, limit),
    }
}

#[derive(Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ImageDiffVersion {
    Blob(String),
    Worktree(EntryVersion),
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) enum ImageDiffResource {
    Version(ImageDiffVersion),
    Content(Bytes),
    Uncached,
}

pub(crate) fn image_diff_resource(
    state: &RepoState,
    detail: &Changes,
    file: &ChangedFile,
    side: Side,
    expected: Option<&ImageDiffVersion>,
) -> Result<Option<ImageDiffResource>> {
    if file.submodule() || file.conflict() {
        return Ok(None);
    }
    let changed = || t!("workspace.tree_entry_changed").into_owned();
    match file_side(detail, file, side) {
        FileSide::Blob(spec) => {
            // Replacement refs can change the bytes returned for an otherwise immutable OID.
            let replace_base =
                std::env::var_os("GIT_REPLACE_REF_BASE").unwrap_or_else(|| "refs/replace".into());
            let replacements = command(
                state.root(),
                [
                    std::ffi::OsStr::new("for-each-ref"),
                    "--count=1".as_ref(),
                    replace_base.as_os_str(),
                ],
            )
            .output()?;
            if !replacements.status.success() || !replacements.stdout.is_empty() {
                return Ok(Some(ImageDiffResource::Uncached));
            }
            let resolved = command(
                state.root(),
                [
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    &spec,
                ],
            )
            .output()?;
            let oid = std::str::from_utf8(&resolved.stdout)?.trim();
            if !resolved.status.success() || !object_id(oid) {
                return Ok(None);
            }
            let version = ImageDiffVersion::Blob(oid.to_owned());
            let Some(expected) = expected else {
                return Ok(Some(ImageDiffResource::Version(version)));
            };
            ensure!(expected == &version, changed());
            // Use the exact object even if a replacement ref appears during this read.
            let Some((bytes, truncated)) =
                read_blob_exact(state.root(), oid, MAX_ASSET_BYTES as usize)?
            else {
                return Ok(None);
            };
            ensure!(!truncated, t!("storage.image_limit"));
            Ok(Some(ImageDiffResource::Content(Bytes(bytes))))
        }
        FileSide::Worktree(path) => {
            let target = match resolve_file(state.root(), Path::new(path)) {
                Ok(target) => target,
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            let resource = ResourceFile::open(&target, true)?;
            let Some(version) = resource.version().cloned().map(ImageDiffVersion::Worktree) else {
                return Ok(Some(ImageDiffResource::Uncached));
            };
            let Some(expected) = expected else {
                return Ok(Some(ImageDiffResource::Version(version)));
            };
            ensure!(expected == &version, changed());
            let (bytes, read_version) = resource.read()?;
            ensure!(
                read_version.map(ImageDiffVersion::Worktree).as_ref() == Some(expected),
                changed()
            );
            Ok(Some(ImageDiffResource::Content(Bytes(bytes))))
        }
    }
}

fn read_blob_exact(root: &Path, oid: &str, limit: usize) -> Result<Option<(Vec<u8>, bool)>> {
    let kind = command(root, ["--no-replace-objects", "cat-file", "-t", oid]).output()?;
    if !kind.status.success() || kind.stdout != b"blob\n" {
        return Ok(None);
    }
    Ok(Some(capture(
        command(root, ["--no-replace-objects", "cat-file", "blob", oid]),
        limit,
        false,
    )?))
}

pub(crate) fn image_diff_side(
    state: &RepoState,
    detail: &Changes,
    file: &ChangedFile,
    side: Side,
) -> Result<Option<Bytes>> {
    if file.submodule() || file.conflict() {
        return Ok(None);
    }
    let Some((bytes, truncated)) = read_side(state, detail, file, side, MAX_ASSET_BYTES as usize)?
    else {
        return Ok(None);
    };
    ensure!(!truncated, t!("storage.image_limit"));
    Ok(Some(Bytes(bytes)))
}

pub(crate) fn snapshot(
    state: &RepoState,
    detail: &Changes,
    file: &ChangedFile,
    side: Side,
) -> Result<Snapshot> {
    if file.submodule() {
        return Ok(Snapshot::Submodule);
    }
    let Some((mut bytes, truncated)) = read_side(state, detail, file, side, SNAPSHOT_LIMIT)? else {
        return Ok(Snapshot::Missing);
    };
    if bytes.contains(&0) {
        return Ok(Snapshot::Binary);
    }
    if truncated
        && let Err(error) = std::str::from_utf8(&bytes)
        && error.error_len().is_none()
    {
        bytes.truncate(error.valid_up_to());
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(Snapshot::Binary);
    };
    Ok(Snapshot::Text { text, truncated })
}

fn quote(path: &str) -> String {
    let mut value = String::from("\"");
    for character in path.chars() {
        match character {
            '\\' | '"' => {
                value.push('\\');
                value.push(character);
            }
            '\n' => value.push_str("\\n"),
            '\r' => value.push_str("\\r"),
            '\t' => value.push_str("\\t"),
            c if c.is_ascii_control() => value.push_str(&format!("\\{:03o}", c as u8)),
            c => value.push(c),
        }
    }
    value.push('"');
    value
}

fn patch_prefix(file: &ChangedFile) -> String {
    let old = file.old_path.as_deref().unwrap_or(&file.path);
    format!(
        "diff --git {} {}\n",
        quote(&format!("a/{old}")),
        quote(&format!("b/{}", file.path))
    )
}

#[cfg(feature = "desktop")]
fn placeholder_text(file: &ChangedFile, message: &str) -> String {
    let mut patch = patch_prefix(file);
    if !file.new_mode.is_empty() && !file.old_mode.is_empty() && file.old_mode != file.new_mode {
        patch.push_str(&t!(
            "git.mode_changed",
            old = file.old_mode,
            new = file.new_mode
        ));
        patch.push('\n');
    }
    if file.binary {
        patch.push_str("Binary files differ\n");
    } else if !message.is_empty() {
        patch.push_str(message);
        patch.push('\n');
    }
    patch
}

#[cfg(feature = "desktop")]
pub(crate) fn placeholder(file: &ChangedFile, message: &str) -> Result<DiffFile> {
    parse_patch(&placeholder_text(file, message), false).and_then(|files| {
        files
            .into_iter()
            .next()
            .context(t!("git.build_diff_failed"))
    })
}

#[cfg(feature = "desktop")]
pub(crate) fn worktree_buffer_diff(
    path: &str,
    base: Option<&str>,
    text: &str,
    context: usize,
) -> Result<DiffFile> {
    if base == Some(text) {
        return Ok(DiffFile::unchanged(path.to_owned(), text));
    }
    let old = base.map_or_else(|| "/dev/null".to_owned(), |_| quote(&format!("a/{path}")));
    let new = quote(&format!("b/{path}"));
    let mut patch = format!("--- {old}\n+++ {new}\n");
    let diff = similar::TextDiff::from_lines(base.unwrap_or_default(), text);
    patch.push_str(&diff.unified_diff().context_radius(context).to_string());
    parse_patch(&patch, false)?
        .into_iter()
        .next()
        .context(t!("git.build_diff_failed"))
}

#[cfg(feature = "desktop")]
fn parse_patch(patch: &str, truncated: bool) -> Result<Vec<DiffFile>> {
    DiffFile::parse_with(patch, DiffParseOptions::new().truncated_hunks(truncated))
        .map_err(|error| anyhow::anyhow!(t!("git.invalid_diff", error = error)))
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct RawPatch {
    pub(super) text: String,
    pub(super) truncated: bool,
    pub(super) count_lines: bool,
}

#[cfg(feature = "desktop")]
impl RawPatch {
    pub(crate) fn content_fingerprint(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        Sha256::digest(self.text.as_bytes()).into()
    }
}

pub(crate) fn raw_patch(
    state: &RepoState,
    detail: &Changes,
    file: &ChangedFile,
    context: usize,
    whitespace: bool,
) -> Result<RawPatch> {
    let context = format!("--unified={context}");
    let whitespace = whitespace.then_some("--ignore-all-space");
    let flags = [context.as_str(), "--no-indent-heuristic"]
        .into_iter()
        .chain(whitespace);
    let disk_diff = file.untracked()
        && matches!(
            detail.comparison.worktree_mode(),
            Some(Mode::All | Mode::Unstaged)
        );
    let (mut bytes, truncated) = if disk_diff || file.code == 'T' {
        compare_file_sides(state, detail, file, flags)?
    } else {
        let args = DIFF_ARGS
            .iter()
            .copied()
            .chain(flags)
            .chain(["--patch"])
            .chain(detail.revisions())
            .chain(["--"])
            .chain(file.old_path.as_deref())
            .chain([file.path.as_str()]);
        capture(command(state.root(), args), PATCH_LIMIT, false)?
    };
    if truncated {
        bytes.truncate(complete_lines(&bytes).len());
    }
    Ok(RawPatch {
        text: text(&bytes),
        truncated,
        count_lines: disk_diff && whitespace.is_none(),
    })
}

#[cfg(all(test, feature = "desktop"))]
pub(crate) fn patch(
    root: &Path,
    detail: &Changes,
    file: &ChangedFile,
    context: usize,
    whitespace: bool,
) -> Result<Patch> {
    render_patch(
        file,
        raw_patch(&RepoState::oneshot(root), detail, file, context, whitespace)?,
    )
}

#[cfg(feature = "desktop")]
pub(crate) fn render_patch(file: &ChangedFile, patch: RawPatch) -> Result<Patch> {
    let RawPatch {
        text,
        truncated,
        count_lines,
    } = patch;
    let files = if text.starts_with("* Unmerged path ") {
        Vec::new()
    } else {
        parse_patch(&text, truncated)?
    };
    let document = if let Some(document) = files
        .into_iter()
        .find(|document| document.path().as_str() == file.path)
    {
        document
    } else {
        placeholder(file, "")?
    };
    let counts = if count_lines && !truncated && !document.is_binary() {
        Some((document.additions(), document.deletions()))
    } else {
        None
    };
    Ok(Patch {
        document,
        truncated,
        counts,
    })
}

fn compare_file_sides<'a>(
    state: &RepoState,
    detail: &Changes,
    file: &ChangedFile,
    flags: impl Iterator<Item = &'a str>,
) -> Result<(Vec<u8>, bool)> {
    let old = if !file.untracked()
        || matches!(
            detail.comparison,
            Comparison::Worktree { base: Some(_), .. } | Comparison::Versions { .. }
        )
        || file.worktree.as_ref().is_some_and(|state| state.staged)
    {
        read_side(state, detail, file, Side::Old, SNAPSHOT_LIMIT)?
    } else {
        None
    };
    let new = read_side(state, detail, file, Side::New, SNAPSHOT_LIMIT)?
        .context(t!("git.file_removed"))?;
    let (mut new, new_truncated) = new;
    let (old_bytes, old_truncated) = old.as_ref().map_or((&[][..], false), |(bytes, truncated)| {
        (bytes.as_slice(), *truncated)
    });
    let old_bytes = if old_truncated {
        complete_lines(old_bytes)
    } else {
        old_bytes
    };
    if new_truncated {
        new.truncate(complete_lines(&new).len());
    }
    let dir = tempfile::tempdir()?;
    let before = dir.path().join("before");
    let after = dir.path().join("after");
    fs::write(&before, old_bytes)?;
    fs::write(&after, new)?;
    let mut git = command(
        state.root(),
        DIFF_ARGS
            .iter()
            .copied()
            .chain(flags)
            .chain(["--no-index", "--"]),
    );
    git.args([&before, &after]);
    let (output, truncated) = capture(git, PATCH_LIMIT, true)?;
    let mut patch = patch_prefix(file).into_bytes();
    if output
        .split(|&byte| byte == b'\n')
        .any(|line| line.starts_with(b"Binary files "))
    {
        patch.extend_from_slice(b"Binary files differ\n");
    } else if let Some(start) = output.windows(4).position(|bytes| bytes == b"\n@@ ") {
        let old_path = if old.is_some() {
            quote(&format!(
                "a/{}",
                file.old_path.as_deref().unwrap_or(&file.path)
            ))
        } else {
            "/dev/null".into()
        };
        patch.extend_from_slice(
            format!(
                "--- {old_path}\n+++ {}\n",
                quote(&format!("b/{}", file.path))
            )
            .as_bytes(),
        );
        patch.extend_from_slice(&output[start + 1..]);
    } else if old.is_none() {
        patch.extend_from_slice(b"new file mode 100644\n");
    }
    Ok((patch, truncated || old_truncated || new_truncated))
}

pub(super) fn complete_lines(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |ix| ix + 1)]
}

#[cfg(all(test, feature = "desktop"))]
mod tests {
    use gpui_component::diff::{DiffFileStatus, DiffSide};

    use super::*;

    #[test]
    fn worktree_buffer_diff_preserves_paths_and_file_status() -> Result<()> {
        let path = "src/a file\twith \"quotes\" and \\ 漢.txt";
        let modified = worktree_buffer_diff(path, Some("old"), "new", 3)?;
        assert_eq!(modified.path().as_str(), path);
        assert_eq!(
            modified.original_path().map(|path| path.as_str()),
            Some(path)
        );
        assert_eq!(
            modified.modified_path().map(|path| path.as_str()),
            Some(path)
        );
        assert_eq!(modified.status(), DiffFileStatus::Modified);
        assert_eq!(
            modified.patch_source_lines().collect::<Vec<_>>(),
            [
                (DiffSide::Original, 1, "old"),
                (DiffSide::Modified, 1, "new")
            ]
        );

        let added = worktree_buffer_diff(path, None, "", 3)?;
        assert_eq!(added.path().as_str(), path);
        assert_eq!(added.status(), DiffFileStatus::Added);
        assert!(added.original_path().is_none());

        let unchanged = worktree_buffer_diff(path, Some("same"), "same", 3)?;
        assert_eq!(unchanged.path().as_str(), path);
        assert_eq!(unchanged.status(), DiffFileStatus::Unchanged);
        Ok(())
    }
}
