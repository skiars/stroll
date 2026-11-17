use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek, Write},
    path::{Component, Path},
};

use anyhow::{Context, Result, ensure};
use rust_i18n::t;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{DIFF_ARGS, Mode, RawPatch, RepoState, WorktreeFile, capture, command, read};

const DIFF_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) enum Layer {
    Staged,
    Unstaged,
}

impl Layer {
    #[cfg(feature = "desktop")]
    pub fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::Staged => t!("git.staged"),
            Self::Unstaged => t!("git.unstaged"),
        }
    }

    pub fn mode(self) -> Mode {
        match self {
            Self::Staged => Mode::Staged,
            Self::Unstaged => Mode::Unstaged,
        }
    }

    #[cfg(feature = "desktop")]
    pub fn actions(self) -> &'static [FileAction] {
        match self {
            Self::Staged => &[FileAction::Unstage],
            Self::Unstaged => &[FileAction::Stage, FileAction::Restore],
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) struct FileDiffOptions {
    pub mode: Mode,
    pub context: usize,
    pub whitespace: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileVersion {
    pub head: Option<String>,
    index: Vec<u8>,
    disk_mode: Option<u32>,
    pub disk: Option<[u8; 32]>,
    pub status: Option<WorktreeFile>,
}

#[cfg(feature = "desktop")]
impl FileVersion {
    pub fn blame_path<'a>(&'a self, path: &'a str) -> Option<&'a str> {
        if let Some(status) = &self.status {
            if status.conflict || status.untracked {
                return None;
            }
            Some(
                old_path(status, Layer::Staged)
                    .or_else(|| old_path(status, Layer::Unstaged))
                    .unwrap_or(path),
            )
        } else {
            Some(path)
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct FileChanges {
    pub version: FileVersion,
    pub layers: Vec<LayerPatch>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct LayerPatch {
    pub layer: Layer,
    pub patch: RawPatch,
    pub blocks: Vec<Block>,
}

impl LayerPatch {
    #[cfg(feature = "desktop")]
    pub fn whole_file(&self) -> bool {
        self.patch
            .text
            .lines()
            .take_while(|line| !line.starts_with("@@ "))
            .any(|line| {
                matches!(line, "--- /dev/null" | "+++ /dev/null")
                    || line.starts_with("new file mode ")
                    || line.starts_with("deleted file mode ")
            })
    }

    #[cfg(feature = "desktop")]
    pub fn deleted_lines(&self, block: &Block) -> Option<Vec<gpui::SharedString>> {
        let (_, hunks) = split_patch(self.patch.text.as_bytes())?;
        let hunk = hunks
            .into_iter()
            .find(|hunk| <[u8; 32]>::from(Sha256::digest(hunk)) == block.digest)?;
        Some(
            std::str::from_utf8(hunk)
                .ok()?
                .split_terminator('\n')
                .skip(1)
                .filter_map(|line| line.strip_prefix('-'))
                .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned().into())
                .collect(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Block {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub changes: Vec<BlockRange>,
    actionable: bool,
    pub digest: [u8; 32],
    pub target: Option<BlockTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BlockRange {
    /// Zero-based line boundary or the first added line.
    pub start: usize,
    pub removed: usize,
    pub added: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FileAction {
    Stage,
    Unstage,
    Restore,
}

impl FileAction {
    #[cfg(feature = "desktop")]
    pub fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::Stage => t!("git.stage"),
            Self::Unstage => t!("git.unstage"),
            Self::Restore => t!("git.restore"),
        }
    }

    pub fn layer(self) -> Layer {
        match self {
            Self::Stage | Self::Restore => Layer::Unstaged,
            Self::Unstage => Layer::Staged,
        }
    }

    pub fn available(self, status: &WorktreeFile) -> bool {
        !status.conflict
            && match self {
                Self::Stage => status.unstaged || status.untracked,
                Self::Unstage => status.staged,
                Self::Restore => status.untracked || status.unstaged && !status.unstaged_rename,
            }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BlockTarget {
    pub digest: [u8; 32],
    pub context: usize,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct FileEdit {
    pub path: String,
    pub version: FileVersion,
    pub action: FileAction,
    pub block: Option<BlockTarget>,
}

pub(super) fn validate_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && !path.contains('\0')
            && Path::new(path).components().all(
                |part| matches!(part, Component::Normal(name) if !name.eq_ignore_ascii_case(".git"))
            ),
        t!("git.invalid_file_path")
    );
    Ok(())
}

pub(crate) fn file_version(state: &RepoState, path: &str) -> Result<FileVersion> {
    validate_path(path)?;
    // Read index entries from the shared repository-wide `git ls-files --stage -z` snapshot.
    let index = state.index()?.record(path)?;
    let mut disk_mode = None;
    let disk = match std::fs::symlink_metadata(state.root().join(path)) {
        Ok(meta) if meta.is_file() || meta.is_symlink() => {
            disk_mode = Some(if meta.is_symlink() {
                0o120000
            } else {
                0o100000
            });
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                if meta.is_file() {
                    disk_mode = Some(meta.permissions().mode());
                }
            }
            let mut digest = Sha256::new();
            // read_disk checks the parent and does not follow a file symlink.
            if let Some((bytes, truncated)) = state.read_disk(path, DIFF_LIMIT)? {
                if truncated {
                    let mut file = std::fs::File::open(state.root().join(path))?;
                    let mut chunk = [0; 16 * 1024];
                    loop {
                        let len = file.read(&mut chunk)?;
                        if len == 0 {
                            break;
                        }
                        digest.update(&chunk[..len]);
                    }
                } else {
                    digest.update(bytes);
                }
                Some(digest.finalize().into())
            } else {
                None
            }
        }
        Ok(_) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    // Use the same repository-wide status as `Changes::status` in the panel.
    // Path-limited status can report the destination of a rename as a new file.
    let status = state.status()?.get(path).cloned();
    Ok(FileVersion {
        head: state.head()?,
        index,
        disk_mode,
        disk,
        status,
    })
}

pub(crate) fn file_changes(
    state: &RepoState,
    path: &str,
    options: FileDiffOptions,
) -> Result<FileChanges> {
    file_changes_batch(state, &[(path.to_owned(), options)])?
        .pop()
        .context(t!("git.invalid_changes"))
}

/// Generate patches for multiple files in one operation.
///
/// Files with the same layer, context, and whitespace options share one `git diff` process.
/// Query the index, HEAD, and status once. A single file uses the same path with a one-file group,
/// preserving per-file results without repeatedly spawning Git.
pub(crate) fn file_changes_batch(
    state: &RepoState,
    requests: &[(String, FileDiffOptions)],
) -> Result<Vec<FileChanges>> {
    for (_, options) in requests {
        ensure!(options.context <= 200, t!("git.invalid_diff_context"));
    }
    let versions = requests
        .iter()
        .map(|(path, _)| file_version(state, path))
        .collect::<Result<Vec<_>>>()?;

    // Plan each file's diff layers first. An untracked file's worktree snapshot is compared with an empty file
    // and must be handled separately.
    let mut groups: BTreeMap<DiffKey, BTreeSet<String>> = BTreeMap::new();
    let mut plans: Vec<Vec<(Layer, Option<DiffKey>)>> = Vec::with_capacity(requests.len());
    for (ix, (path, options)) in requests.iter().enumerate() {
        let mut plan = Vec::new();
        if let Some(status) = &versions[ix].status
            && !status.conflict
        {
            for layer in [Layer::Staged, Layer::Unstaged] {
                if options.mode != Mode::All && options.mode != layer.mode() {
                    continue;
                }
                let present = match layer {
                    Layer::Staged => status.staged,
                    Layer::Unstaged => status.unstaged || status.untracked,
                };
                if !present {
                    continue;
                }
                if layer == Layer::Unstaged && status.untracked {
                    plan.push((layer, None));
                    continue;
                }
                let key = DiffKey {
                    layer,
                    context: options.context,
                    whitespace: options.whitespace,
                    no_renames: old_path(status, layer).is_none(),
                };
                let paths = groups.entry(key).or_default();
                if let Some(old) = old_path(status, layer) {
                    paths.insert(old.to_owned());
                }
                paths.insert(path.clone());
                plan.push((layer, Some(key)));
            }
        }
        plans.push(plan);
    }

    let mut diffs: BTreeMap<DiffKey, BTreeMap<String, (Vec<u8>, bool)>> = BTreeMap::new();
    for (key, paths) in &groups {
        diffs.insert(*key, group_patch(state, *key, paths)?);
    }

    let mut changes = Vec::with_capacity(requests.len());
    // With whitespace ignored, only hunks identical to their unfiltered counterparts can be edited. Track layers to recompute.
    let mut canonical: Vec<(usize, Layer, DiffKey, BTreeSet<String>)> = Vec::new();
    for (ix, (path, options)) in requests.iter().enumerate() {
        let version = versions[ix].clone();
        let status = version.status.clone();
        let mut layers = Vec::new();
        for (layer, key) in &plans[ix] {
            let Some(status) = &status else { break };
            let (raw, truncated) = match key {
                Some(key) => diffs
                    .get(key)
                    .and_then(|group| group.get(path).cloned())
                    .unwrap_or_default(),
                None => layer_patch(state, path, &version, *layer, *options)?,
            };
            let layer_blocks = blocks(&raw, truncated, status, *layer);
            if options.whitespace && layer_blocks.iter().any(|block| block.actionable) {
                let mut paths = BTreeSet::new();
                if let Some(old) = old_path(status, *layer) {
                    paths.insert(old.to_owned());
                }
                paths.insert(path.clone());
                canonical.push((
                    ix,
                    *layer,
                    DiffKey {
                        layer: *layer,
                        context: options.context,
                        whitespace: false,
                        no_renames: old_path(status, *layer).is_none(),
                    },
                    paths,
                ));
            }
            layers.push(LayerPatch {
                layer: *layer,
                patch: RawPatch {
                    text: String::from_utf8_lossy(&raw).into_owned(),
                    truncated,
                    count_lines: status.untracked
                        && *layer == Layer::Unstaged
                        && !options.whitespace,
                },
                blocks: layer_blocks,
            });
        }
        changes.push(FileChanges { version, layers });
    }

    for (ix, layer, key, paths) in canonical {
        let Some(status) = &changes[ix].version.status else {
            continue;
        };
        let unfiltered = match diffs.get(&key) {
            Some(group) => group.clone(),
            None => group_patch(state, key, &paths)?,
        };
        let Some((raw, truncated)) = unfiltered.get(&requests[ix].0).cloned() else {
            continue;
        };
        let unfiltered = blocks(&raw, truncated, status, layer);
        if let Some(entry) = changes[ix]
            .layers
            .iter_mut()
            .find(|entry| entry.layer == layer)
        {
            for block in &mut entry.blocks {
                // Apply only a block whose complete patch is identical to the
                // unfiltered patch. Other whitespace stays untouched.
                block.actionable = unfiltered
                    .iter()
                    .any(|candidate| candidate.actionable && candidate.digest == block.digest);
            }
        }
    }

    for ix in 0..changes.len() {
        let context = requests[ix].1.context;
        for entry in &mut changes[ix].layers {
            for block in &mut entry.blocks {
                if block.actionable {
                    block.target = Some(BlockTarget {
                        digest: block.digest,
                        context,
                    });
                }
            }
        }
        // Discard results if the index, HEAD, or disk content changes during calculation. Check the index and HEAD
        // with a cheap fingerprint that spawns no process, and compare on-disk digests again.
        ensure!(
            state.unchanged() && versions[ix] == file_version(state, &requests[ix].0)?,
            t!("git.worktree_changed")
        );
    }
    Ok(changes)
}

fn old_path(status: &WorktreeFile, layer: Layer) -> Option<&str> {
    match layer {
        Layer::Staged => status.staged_rename,
        Layer::Unstaged => status.unstaged_rename,
    }
    .then_some(status.old_path.as_deref())
    .flatten()
}

/// Shared diff arguments for files with the same layer, context, whitespace, and rename options.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DiffKey {
    layer: Layer,
    context: usize,
    whitespace: bool,
    no_renames: bool,
}

impl DiffKey {
    fn base(&self, state: &RepoState) -> Result<Option<String>> {
        match self.layer {
            Layer::Staged => Ok(Some(
                state
                    .head()?
                    .map_or_else(|| state.empty_tree().map(str::to_owned), Ok)?,
            )),
            Layer::Unstaged => Ok(None),
        }
    }
}

/// Generate patches for a group in one Git process and return them by path.
fn group_patch(
    state: &RepoState,
    key: DiffKey,
    paths: &BTreeSet<String>,
) -> Result<BTreeMap<String, (Vec<u8>, bool)>> {
    let mut output = BTreeMap::new();
    if paths.is_empty() {
        return Ok(output);
    }
    let base = key.base(state)?;
    let context = format!("--unified={}", key.context);
    let args = DIFF_ARGS
        .iter()
        .copied()
        .chain(key.no_renames.then_some("--no-renames"))
        .chain([
            "--no-indent-heuristic",
            "--inter-hunk-context=0",
            "--full-index",
            "--patch",
            &context,
        ])
        .chain(key.whitespace.then_some("--ignore-all-space"))
        .chain(
            base.as_deref()
                .into_iter()
                .flat_map(|base| ["--cached", base]),
        )
        .chain(["--"])
        .chain(paths.iter().map(String::as_str));
    let limit = DIFF_LIMIT
        .saturating_mul(paths.len().max(1))
        .min(super::METADATA_LIMIT);
    let (bytes, truncated) = capture(command(state.root(), args), limit, false)?;
    for (path, chunk) in split_files(&bytes) {
        let mut chunk = chunk;
        let mut truncated = truncated;
        if chunk.len() > DIFF_LIMIT {
            let complete = super::changes::complete_lines(chunk).len();
            chunk = &chunk[..complete.min(DIFF_LIMIT)];
            truncated = true;
        }
        output.insert(path, (chunk.to_vec(), truncated));
    }
    Ok(output)
}

/// Split one `git diff --patch` result into per-file patches at `diff --git` headers.
fn split_files(patch: &[u8]) -> Vec<(String, &[u8])> {
    let mut files = Vec::new();
    let mut start = 0;
    let mut found = false;
    let mut offset = 0;
    for line in patch.split_inclusive(|&byte| byte == b'\n') {
        if line.starts_with(b"diff --git ") {
            if found && let Some(path) = chunk_path(&patch[start..offset]) {
                files.push((path, &patch[start..offset]));
            }
            start = offset;
            found = true;
        }
        offset += line.len();
    }
    if found && let Some(path) = chunk_path(&patch[start..]) {
        files.push((path, &patch[start..]));
    }
    files
}

/// Repository path for a single-file patch.
fn chunk_path(chunk: &[u8]) -> Option<String> {
    let mut diff = None;
    for line in chunk.split_inclusive(|&byte| byte == b'\n') {
        if line.starts_with(b"diff --git ") {
            diff = std::str::from_utf8(&line[11..]).ok();
        } else if line.starts_with(b"+++ ") {
            if let Some(path) = header_path(&line[4..], "b/") {
                return Some(path);
            }
        } else if line.starts_with(b"rename to ") {
            if let Some(path) = header_path(&line[10..], "") {
                return Some(path);
            }
        } else if line.starts_with(b"--- ") {
            if let Some(path) = header_path(&line[4..], "a/") {
                return Some(path);
            }
        }
    }
    let diff = diff?;
    // In `diff --git a/old b/new`, paths containing spaces are enclosed in quotes.
    let mut fields = split_quoted(diff);
    fields
        .pop()
        .and_then(|path| header_path(path.as_bytes(), "b/"))
}

fn header_path(value: &[u8], prefix: &str) -> Option<String> {
    let value = value.strip_suffix(b"\n").unwrap_or(value);
    let value = value.strip_suffix(b"\r").unwrap_or(value);
    // Paths containing spaces are terminated by a tab in `---` and `+++` lines.
    let value = value.strip_suffix(b"\t").unwrap_or(value);
    let text = std::str::from_utf8(value).ok()?;
    let path = unquote(text);
    if path == "/dev/null" {
        return None;
    }
    Some(path.strip_prefix(prefix).unwrap_or(&path).to_owned())
}

fn split_quoted(value: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in value.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                current.push(character);
            }
            ' ' if !quoted => {
                if !current.is_empty() {
                    fields.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }
    if !current.is_empty() {
        fields.push(current);
    }
    fields
}

/// Decode Git's C-style path quoting. `core.quotePath=false` still quotes control characters and quotes.
pub(super) fn unquote(value: &str) -> String {
    let Some(inner) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return value.to_owned();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        match chars.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b'r') => bytes.push(b'\r'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'a') => bytes.push(7),
            Some(b'b') => bytes.push(8),
            Some(b'f') => bytes.push(12),
            Some(b'v') => bytes.push(11),
            Some(digit @ b'0'..=b'7') => {
                let mut value = digit - b'0';
                for _ in 0..2 {
                    match chars.clone().next() {
                        Some(next @ b'0'..=b'7') => {
                            value = value * 8 + (next - b'0');
                            chars.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(value);
            }
            Some(other) => bytes.push(other),
            None => break,
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn blocks(raw: &[u8], truncated: bool, status: &WorktreeFile, layer: Layer) -> Vec<Block> {
    let Some((header, sections)) = split_patch(raw) else {
        return Vec::new();
    };
    let actionable = !truncated && allows_blocks(status, layer) && regular_patch(header);
    sections
        .into_iter()
        .filter_map(|section| parse_block(section, actionable))
        .collect()
}

fn allows_blocks(status: &WorktreeFile, layer: Layer) -> bool {
    match layer {
        Layer::Staged => !status.staged_rename,
        Layer::Unstaged => !status.untracked && !status.unstaged_rename,
    }
}

pub(crate) fn layer_snapshot(
    state: &RepoState,
    path: &str,
    layer: Layer,
    side: super::Side,
) -> Result<super::Snapshot> {
    validate_path(path)?;
    let detail = super::Changes {
        comparison: super::Comparison::Worktree {
            mode: layer.mode(),
            base: None,
        },
        base: state
            .head()?
            .map_or_else(|| state.empty_tree().map(str::to_owned), Ok)?,
        files: Vec::new(),
        status: None,
    };
    let mut file = super::ChangedFile::new(path.into(), 'M');
    if side == super::Side::Old {
        file.old_path = file_version(state, path)?
            .status
            .filter(|status| match layer {
                Layer::Staged => status.staged_rename,
                Layer::Unstaged => status.unstaged_rename,
            })
            .and_then(|status| status.old_path);
    }
    super::snapshot(state, &detail, &file, side)
}

fn layer_patch(
    state: &RepoState,
    path: &str,
    version: &FileVersion,
    layer: Layer,
    options: FileDiffOptions,
) -> Result<(Vec<u8>, bool)> {
    let status = version
        .status
        .as_ref()
        .context(t!("git.worktree_changed"))?;
    if layer == Layer::Unstaged && status.untracked {
        let mut file = super::ChangedFile::new(path.to_owned(), '?');
        file.worktree = Some(status.clone());
        let detail = super::Changes {
            comparison: super::Comparison::Worktree {
                mode: Mode::Unstaged,
                base: None,
            },
            base: String::new(),
            files: Vec::new(),
            status: None,
        };
        let patch = super::raw_patch(state, &detail, &file, options.context, options.whitespace)?;
        return Ok((patch.text.into_bytes(), patch.truncated));
    }
    let base = match layer {
        Layer::Staged => Some(
            version
                .head
                .clone()
                .map_or_else(|| state.empty_tree().map(str::to_owned), Ok)?,
        ),
        Layer::Unstaged => None,
    };
    let context = format!("--unified={}", options.context);
    let old_path = (match layer {
        Layer::Staged => status.staged_rename,
        Layer::Unstaged => status.unstaged_rename,
    })
    .then_some(status.old_path.as_deref())
    .flatten();
    let args = DIFF_ARGS
        .iter()
        .copied()
        .chain(old_path.is_none().then_some("--no-renames"))
        .chain([
            "--no-indent-heuristic",
            "--inter-hunk-context=0",
            "--full-index",
            "--patch",
            &context,
        ])
        .chain(options.whitespace.then_some("--ignore-all-space"))
        .chain(
            base.as_deref()
                .into_iter()
                .flat_map(|base| ["--cached", base]),
        )
        .chain(["--"])
        .chain(old_path)
        .chain([path]);
    let (mut bytes, truncated) = capture(command(state.root(), args), DIFF_LIMIT, false)?;
    if truncated {
        bytes.truncate(super::changes::complete_lines(&bytes).len());
    }
    Ok((bytes, truncated))
}

fn split_patch(patch: &[u8]) -> Option<(&[u8], Vec<&[u8]>)> {
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in patch.split_inclusive(|&byte| byte == b'\n') {
        if line.starts_with(b"@@ ") {
            starts.push(offset);
        } else if offset != 0 && line.starts_with(b"diff --git ") {
            return None;
        }
        offset += line.len();
    }
    let first = *starts.first()?;
    starts.push(patch.len());
    Some((
        &patch[..first],
        starts
            .windows(2)
            .map(|pair| &patch[pair[0]..pair[1]])
            .collect(),
    ))
}

fn regular_patch(header: &[u8]) -> bool {
    std::str::from_utf8(header).is_ok_and(|header| {
        header.contains("\n--- ")
            && header.contains("\n+++ ")
            && !header.lines().any(|line| {
                line.starts_with("old mode ")
                    || line.starts_with("new mode ")
                    || line.split_once(' ').is_some_and(|(prefix, _)| {
                        matches!(prefix, "index" | "new" | "deleted")
                            && matches!(
                                line.split_ascii_whitespace().last(),
                                Some("120000" | "160000")
                            )
                    })
            })
    })
}

fn parse_block(section: &[u8], actionable: bool) -> Option<Block> {
    let mut lines = section.split(|&byte| byte == b'\n');
    let header = std::str::from_utf8(lines.next()?).ok()?;
    let mut fields = header.split_ascii_whitespace();
    fields.next()?;
    let range = |field: &str, prefix| {
        let field = field.strip_prefix(prefix)?;
        let (start, count) = field.split_once(',').unwrap_or((field, "1"));
        Some((start.parse::<usize>().ok()?, count.parse::<usize>().ok()?))
    };
    let (old_start, old_count) = range(fields.next()?, '-')?;
    let (new_start, new_count) = range(fields.next()?, '+')?;
    let mut new_line = new_start.saturating_sub(usize::from(new_count > 0));
    let mut changes = Vec::new();
    let mut current: Option<BlockRange> = None;
    for line in lines {
        match line.first() {
            Some(b'-' | b'+') => {
                let change = current.get_or_insert(BlockRange {
                    start: new_line,
                    removed: 0,
                    added: 0,
                });
                if line[0] == b'-' {
                    change.removed += 1;
                } else {
                    change.added += 1;
                    new_line += 1;
                }
            }
            Some(b' ') => {
                changes.extend(current.take());
                new_line += 1;
            }
            _ => {}
        }
    }
    changes.extend(current);
    Some(Block {
        old_start,
        old_count,
        new_start,
        new_count,
        changes,
        actionable: actionable && std::str::from_utf8(section).is_ok(),
        digest: Sha256::digest(section).into(),
        target: None,
    })
}

#[cfg(feature = "desktop")]
pub(crate) fn buffer_diff_blocks(index: &str, text: &str) -> Vec<Block> {
    let patch = similar::TextDiff::from_lines(index, text)
        .unified_diff()
        .context_radius(0)
        .to_string();
    split_patch(patch.as_bytes())
        .into_iter()
        .flat_map(|(_, sections)| sections)
        .filter_map(|section| parse_block(section, false))
        .collect()
}

/// Read a verified unstaged hunk as worktree line ranges and index replacements.
#[cfg(feature = "desktop")]
pub(crate) fn restore_replacements(
    changes: &FileChanges,
    target: &BlockTarget,
) -> Result<Vec<(std::ops::Range<usize>, String)>> {
    let layer = changes
        .layers
        .iter()
        .find(|layer| layer.layer == Layer::Unstaged)
        .context(t!("git.worktree_changed"))?;
    let block = layer
        .blocks
        .iter()
        .find(|block| block.target.as_ref() == Some(target))
        .context(t!("git.worktree_changed"))?;
    let (_, sections) =
        split_patch(layer.patch.text.as_bytes()).context(t!("git.worktree_changed"))?;
    let section = sections
        .into_iter()
        .find(|section| <[u8; 32]>::from(Sha256::digest(section)) == target.digest)
        .context(t!("git.worktree_changed"))?;
    let section = std::str::from_utf8(section)?;
    let mut edits = Vec::new();
    let mut changes = block.changes.iter();
    let mut replacement = String::new();
    let mut in_change = false;
    let mut previous_removed = false;
    for line in section
        .split_inclusive('\n')
        .skip(1)
        .chain(std::iter::once(" "))
    {
        match line.as_bytes().first() {
            Some(b'-' | b'+') => {
                in_change = true;
                previous_removed = line.starts_with('-');
                if previous_removed {
                    replacement.push_str(&line[1..]);
                }
            }
            Some(b'\\') if previous_removed => {
                if replacement.ends_with('\n') {
                    replacement.pop();
                }
            }
            Some(b' ') if in_change => {
                let change = changes.next().context(t!("git.worktree_changed"))?;
                edits.push((
                    change.start..change.start + change.added,
                    std::mem::take(&mut replacement),
                ));
                in_change = false;
                previous_removed = false;
            }
            _ => {}
        }
    }
    ensure!(
        changes.next().is_none() && !edits.is_empty(),
        t!("git.worktree_changed")
    );
    Ok(edits)
}

pub(crate) fn edit_file(state: &RepoState, edit: &FileEdit) -> Result<()> {
    edit_file_in_index(state, edit, None)
}

pub(super) fn edit_file_in_index(
    state: &RepoState,
    edit: &FileEdit,
    index: Option<&Path>,
) -> Result<()> {
    let root = state.root();
    let path = &edit.path;
    let current = file_version(state, path)?;
    ensure!(current == edit.version, t!("git.worktree_changed"));
    let status = current
        .status
        .as_ref()
        .context(t!("git.worktree_changed"))?;
    ensure!(edit.action.available(status), t!("git.cannot_edit"));
    ensure!(
        !current
            .index
            .split(|&b| b == 0)
            .any(|entry| entry.starts_with(b"160000 "))
            || edit.block.is_none() && edit.action != FileAction::Restore,
        t!("git.cannot_edit")
    );
    if let Some(target) = &edit.block {
        let layer = edit.action.layer();
        ensure!(
            target.context <= 200 && allows_blocks(status, layer),
            t!("git.cannot_edit")
        );
        let (patch, truncated) = layer_patch(
            state,
            path,
            &current,
            layer,
            FileDiffOptions {
                mode: layer.mode(),
                context: target.context,
                whitespace: false,
            },
        )?;
        let (header, hunks) = split_patch(&patch).context(t!("git.worktree_changed"))?;
        ensure!(!truncated && regular_patch(header), t!("git.cannot_edit"));
        let mut matching = hunks
            .into_iter()
            .filter(|hunk| <[u8; 32]>::from(Sha256::digest(hunk)) == target.digest);
        let hunk = matching.next().context(t!("git.worktree_changed"))?;
        ensure!(matching.next().is_none(), t!("git.worktree_changed"));
        ensure!(std::str::from_utf8(hunk).is_ok(), t!("git.cannot_edit"));
        let mut input = tempfile::tempfile()?;
        input.write_all(header)?;
        input.write_all(hunk)?;
        input.rewind()?;
        ensure!(
            state.unchanged() && current == file_version(state, path)?,
            t!("git.worktree_changed")
        );
        let args = ["apply", "--whitespace=nowarn"]
            .into_iter()
            .chain((edit.action != FileAction::Restore).then_some("--cached"))
            .chain((edit.action != FileAction::Stage).then_some("--reverse"))
            .chain((target.context == 0).then_some("--unidiff-zero"))
            .chain(["-"]);
        let mut git = command(root, args);
        if let Some(index) = index {
            git.env("GIT_INDEX_FILE", index);
        }
        git.stdin(input);
        capture(git, super::METADATA_LIMIT, false)?;
    } else if edit.action == FileAction::Restore {
        if status.untracked {
            // Validate the parent and the file without following symlinks.
            ensure!(current.disk.is_some(), t!("git.cannot_edit"));
            ensure!(
                state.unchanged() && current == file_version(state, path)?,
                t!("git.worktree_changed")
            );
            std::fs::remove_file(root.join(path))?;
        } else {
            ensure!(state.unchanged(), t!("git.worktree_changed"));
            read(root, ["restore", "--worktree", "--", path])?;
        }
    } else {
        let expected = [(path.clone(), status.clone())].into_iter().collect();
        super::changes::stage_in_index(
            root,
            &expected,
            std::slice::from_ref(path),
            edit.action == FileAction::Stage,
            index,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> Result<tempfile::TempDir> {
        let dir = tempfile::tempdir()?;
        read(dir.path(), ["init", "-q"])?;
        read(dir.path(), ["config", "user.name", "Test"])?;
        read(dir.path(), ["config", "user.email", "test@example.invalid"])?;
        read(dir.path(), ["config", "commit.gpgsign", "false"])?;
        read(dir.path(), ["config", "core.autocrlf", "false"])?;
        Ok(dir)
    }

    // Use fresh state for each test call to avoid stale cached facts.
    fn state(root: &Path) -> RepoState {
        RepoState::oneshot(root)
    }

    fn edit_file(root: &Path, edit: &FileEdit) -> Result<()> {
        super::edit_file(&state(root), edit)
    }

    fn file_version(root: &Path, path: &str) -> Result<FileVersion> {
        super::file_version(&state(root), path)
    }

    fn changes(root: &Path, path: &str, context: usize) -> Result<FileChanges> {
        file_changes(
            &state(root),
            path,
            FileDiffOptions {
                mode: Mode::All,
                context,
                whitespace: false,
            },
        )
    }

    fn edit(
        path: &str,
        changes: &FileChanges,
        action: FileAction,
        block: usize,
        context: usize,
    ) -> Result<FileEdit> {
        let layer = changes
            .layers
            .iter()
            .find(|patch| patch.layer == action.layer())
            .unwrap();
        assert!(layer.blocks[block].actionable);
        Ok(FileEdit {
            path: path.into(),
            version: changes.version.clone(),
            action,
            block: Some(BlockTarget {
                digest: layer.blocks[block].digest,
                context,
            }),
        })
    }

    fn indexed(root: &Path, path: &str) -> Result<String> {
        Ok(String::from_utf8(read(
            root,
            ["show", &format!(":{path}")],
        )?)?)
    }

    #[test]
    fn stage_unstage_restore_target_one_hunk_and_validate_disk_bytes() -> Result<()> {
        let fixture = fixture()?;
        let root = fixture.path();
        let path = "sample with space.txt";
        let initial = (0..32).map(|i| format!("line {i}\n")).collect::<String>();
        let modified = initial
            .replace("line 2\n", "staged\n")
            .replace("line 23\n", "unstaged\n");
        fs::write(root.join(path), &initial)?;
        read(root, ["add", "--", path])?;
        read(root, ["commit", "-qm", "initial"])?;
        fs::write(root.join(path), &modified)?;

        let original = changes(root, path, 3)?;
        assert_eq!(original.layers[0].blocks.len(), 2);
        edit_file(root, &edit(path, &original, FileAction::Stage, 0, 3)?)?;
        let staged = initial.replace("line 2\n", "staged\n");
        assert_eq!(indexed(root, path)?, staged);
        assert_eq!(fs::read_to_string(root.join(path))?, modified);

        let partial = changes(root, path, 3)?;
        let old_restore = edit(path, &partial, FileAction::Restore, 0, 3)?;
        let modified_again = modified.replace("unstaged\n", "another edit\n");
        fs::write(root.join(path), &modified_again)?;
        assert!(edit_file(root, &old_restore).is_err());
        assert_eq!(fs::read_to_string(root.join(path))?, modified_again);

        let partial = changes(root, path, 3)?;
        edit_file(root, &edit(path, &partial, FileAction::Restore, 0, 3)?)?;
        assert_eq!(fs::read_to_string(root.join(path))?, staged);
        assert_eq!(indexed(root, path)?, staged);

        let partial = changes(root, path, 0)?;
        edit_file(root, &edit(path, &partial, FileAction::Unstage, 0, 0)?)?;
        assert_eq!(indexed(root, path)?, initial);
        assert_eq!(fs::read_to_string(root.join(path))?, staged);
        Ok(())
    }

    fn options(context: usize, whitespace: bool) -> FileDiffOptions {
        FileDiffOptions {
            mode: Mode::All,
            context,
            whitespace,
        }
    }

    /// Batch patches must match the results of individual file requests byte for byte.
    #[test]
    fn batched_patches_match_single_file_requests() -> Result<()> {
        let fixture = fixture()?;
        let root = fixture.path();
        fs::write(root.join("modified.txt"), "one\nTWO\nthree\n")?;
        fs::write(root.join("staged.txt"), "alpha\nbeta\ngamma\n")?;
        fs::write(root.join("ws.txt"), "keep\n  indented\n")?;
        fs::write(
            root.join("old name.txt"),
            "line one\nline two\nline three\n",
        )?;
        read(root, ["add", "--", "."])?;
        read(root, ["commit", "-qm", "initial"])?;

        fs::write(root.join("modified.txt"), "one\ntwo\nTHREE\n")?;
        fs::write(root.join("staged.txt"), "alpha\nBETA\ngamma\n")?;
        read(root, ["add", "--", "staged.txt"])?;
        // For whitespace-only changes, actionable hunks must match the per-file result.
        fs::write(root.join("ws.txt"), "keep\n\tindented\n")?;
        fs::write(
            root.join("old name.txt"),
            "line one\nline TWO\nline three\n",
        )?;
        read(root, ["add", "--", "old name.txt"])?;
        read(root, ["mv", "old name.txt", "renamed.txt"])?;
        fs::write(root.join("renamed.txt"), "line one\nline TWO\nline 3\n")?;
        fs::write(root.join("untracked.txt"), "new\nfile\n")?;

        let requests: Vec<(String, FileDiffOptions)> = [
            ("modified.txt", options(3, false)),
            ("modified.txt", options(0, false)),
            ("staged.txt", options(3, false)),
            ("staged.txt", options(3, true)),
            ("ws.txt", options(3, true)),
            ("renamed.txt", options(3, false)),
            ("untracked.txt", options(3, false)),
        ]
        .into_iter()
        .map(|(path, options)| (path.to_owned(), options))
        .collect();

        let batch = file_changes_batch(&state(root), &requests)?;
        assert_eq!(batch.len(), requests.len());
        #[cfg(feature = "desktop")]
        assert!(batch[6].layers[0].whole_file());
        for (ix, (path, options)) in requests.iter().enumerate() {
            let single = super::file_changes(&state(root), path, *options)?;
            assert_eq!(batch[ix].version, single.version, "{path}");
            assert_eq!(batch[ix].layers.len(), single.layers.len(), "{path}");
            for (batched, single) in batch[ix].layers.iter().zip(&single.layers) {
                assert_eq!(batched.layer, single.layer, "{path}");
                assert_eq!(batched.patch.text, single.patch.text, "{path}");
                assert_eq!(batched.patch.truncated, single.patch.truncated, "{path}");
                assert_eq!(
                    batched.patch.count_lines, single.patch.count_lines,
                    "{path}"
                );
                assert_eq!(batched.blocks, single.blocks, "{path}");
            }
        }
        // Extract the rename patch from the grouped output under the new path.
        let renamed = batch[5]
            .layers
            .iter()
            .find(|layer| layer.patch.text.contains("old name.txt"))
            .expect("renamed layer");
        assert!(renamed.patch.text.contains("renamed.txt"));
        Ok(())
    }

    #[test]
    fn zero_context_targets_keep_nearby_changes_independent() -> Result<()> {
        let fixture = fixture()?;
        let root = fixture.path();
        let path = "nearby.txt";
        let original = (1..=16)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let modified = original
            .replace("line 5\n", "first\n")
            .replace("line 9\n", "second\n");
        fs::write(root.join(path), &original)?;
        read(root, ["add", "--", path])?;
        read(root, ["commit", "-qm", "initial"])?;
        read(root, ["config", "diff.interHunkContext", "20"])?;
        fs::write(root.join(path), &modified)?;

        let display = changes(root, path, 3)?;
        assert_eq!(display.layers[0].blocks[0].changes.len(), 2);
        let actions = changes(root, path, 0)?;
        assert_eq!(actions.layers[0].blocks.len(), 2);
        let stage = edit(path, &actions, FileAction::Stage, 1, 0)?;
        edit_file(root, &stage)?;
        let staged = original.replace("line 9\n", "second\n");
        assert_eq!(indexed(root, path)?, staged);
        assert_eq!(fs::read_to_string(root.join(path))?, modified);

        let actions = changes(root, path, 0)?;
        let restore = edit(path, &actions, FileAction::Restore, 0, 0)?;
        #[cfg(feature = "desktop")]
        assert_eq!(
            restore_replacements(&actions, restore.block.as_ref().unwrap())?,
            vec![(4..5, "line 5\n".to_owned())]
        );
        edit_file(root, &restore)?;
        assert_eq!(indexed(root, path)?, staged);
        assert_eq!(fs::read_to_string(root.join(path))?, staged);
        Ok(())
    }

    #[test]
    fn layers_include_changes_that_cancel_out_and_restore_uses_index() -> Result<()> {
        let fixture = fixture()?;
        let root = fixture.path();
        let path = "same.txt";
        fs::write(root.join(path), "before\n")?;
        read(root, ["add", "--", path])?;
        read(root, ["commit", "-qm", "initial"])?;
        fs::write(root.join(path), "after\n")?;
        read(root, ["add", "--", path])?;
        fs::write(root.join(path), "before\n")?;

        let review = super::super::changes(
            &state(root),
            super::super::Comparison::Worktree {
                mode: Mode::All,
                base: None,
            },
        )?;
        assert_eq!(review.files.len(), 1);
        let partial = changes(root, path, 0)?;
        assert_eq!(partial.layers.len(), 2);
        edit_file(root, &edit(path, &partial, FileAction::Restore, 0, 0)?)?;
        assert_eq!(fs::read_to_string(root.join(path))?, "after\n");
        assert_eq!(indexed(root, path)?, "after\n");
        Ok(())
    }

    #[test]
    fn pure_deletion_at_end_restores_without_a_trailing_newline() -> Result<()> {
        let fixture = fixture()?;
        let root = fixture.path();
        let path = "ending.txt";
        fs::write(root.join(path), "first\nlast")?;
        read(root, ["add", "--", path])?;
        read(root, ["commit", "-qm", "initial"])?;
        fs::write(root.join(path), "first\n")?;
        let delta = changes(root, path, 0)?;
        assert_eq!(
            delta.layers[0].blocks[0].changes,
            vec![BlockRange {
                start: 1,
                removed: 1,
                added: 0
            }]
        );
        edit_file(root, &edit(path, &delta, FileAction::Restore, 0, 0)?)?;
        assert_eq!(fs::read_to_string(root.join(path))?, "first\nlast");
        Ok(())
    }

    #[test]
    fn file_restore_rejects_an_outdated_index_and_preserves_the_staged_version() -> Result<()> {
        let fixture = fixture()?;
        let root = fixture.path();
        let path = "file.txt";
        fs::write(root.join(path), "head\n")?;
        read(root, ["add", "--", path])?;
        read(root, ["commit", "-qm", "initial"])?;
        fs::write(root.join(path), "index\n")?;
        read(root, ["add", "--", path])?;
        fs::write(root.join(path), "disk\n")?;

        let restore = FileEdit {
            path: path.into(),
            version: file_version(root, path)?,
            action: FileAction::Restore,
            block: None,
        };
        fs::write(root.join(path), "new index\n")?;
        read(root, ["add", "--", path])?;
        fs::write(root.join(path), "disk\n")?;
        assert!(edit_file(root, &restore).is_err());
        assert_eq!(fs::read_to_string(root.join(path))?, "disk\n");
        edit_file(
            root,
            &FileEdit {
                version: file_version(root, path)?,
                ..restore
            },
        )?;
        assert_eq!(fs::read_to_string(root.join(path))?, "new index\n");
        assert_eq!(indexed(root, path)?, "new index\n");
        Ok(())
    }
}
