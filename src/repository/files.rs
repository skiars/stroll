use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, ensure};
use rust_i18n::t;
use serde::{Deserialize, Serialize};

use super::{Entry, Repository, git, resolve_file};

pub(crate) const COPY_CHUNK: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct EntryVersion(String);

#[derive(Serialize, Deserialize)]
pub(crate) enum FileOperation {
    Create {
        directory: bool,
    },
    Rename {
        name: String,
        version: EntryVersion,
    },
    Delete {
        version: EntryVersion,
    },
    Transfer {
        destination: crate::remote::RepoPath,
        version: EntryVersion,
        move_entry: bool,
    },
}

fn entry_kind(path: &Path) -> Result<(fs::Metadata, EntryVersion)> {
    let metadata = path.symlink_metadata()?;
    let kind = metadata.file_type();
    ensure!(
        kind.is_file() || kind.is_dir() || kind.is_symlink(),
        t!("workspace.tree_unsupported")
    );
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        (
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    };
    #[cfg(not(unix))]
    let identity = metadata.created().ok();
    let version = EntryVersion(format!(
        "{identity:?}/{kind:?}/{:?}/{}",
        metadata.modified().ok(),
        metadata.len()
    ));
    Ok((metadata, version))
}

/// Compare against the filesystem host's clock, including the Unix change
/// time so a same-length rewrite with a preserved mtime still invalidates.
pub(crate) fn lsp_edit_target(
    root: &Path,
    path: &Path,
    before: std::time::SystemTime,
) -> Result<EntryVersion> {
    let path = operation_path(root, path)?;
    let (metadata, version) = entry_kind(&path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "This action targets a non-editable file"
    );
    let modified = metadata.modified()?;
    #[cfg(unix)]
    let modified = {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.ctime() >= 0 {
            let changed = std::time::UNIX_EPOCH
                + std::time::Duration::new(metadata.ctime() as u64, metadata.ctime_nsec() as u32);
            modified.max(changed)
        } else {
            modified
        }
    };
    ensure!(
        modified < before,
        "This action is out of date. Request code actions again."
    );
    Ok(version)
}

fn operation_path(root: &Path, path: &Path) -> Result<PathBuf> {
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|part| matches!(part, Component::Normal(name)
                if name.to_str().is_some_and(valid_name))),
        t!("workspace.tree_invalid_name")
    );
    let target = root.join(path);
    let mut parent = root.to_owned();
    for part in path.parent().into_iter().flat_map(Path::components) {
        parent.push(part);
        let metadata = parent.symlink_metadata()?;
        ensure!(
            metadata.is_dir() && !is_link(&metadata),
            t!("common.path_outside_repo")
        );
    }
    Ok(target)
}

pub(crate) fn valid_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name != "."
        && name != ".."
        && !name.eq_ignore_ascii_case(".git")
        && !name.contains(['/', '\\', '\0'])
        && (!cfg!(windows)
            || (!name.contains([':', '<', '>', '"', '|', '?', '*']) && !name.ends_with(['.', ' '])))
        && matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
}

impl Repository {
    pub(crate) fn entry_version(&self, path: &Path) -> Result<EntryVersion> {
        if let Some(ssh) = &self.ssh {
            return ssh.call(crate::remote::Request::EntryVersion(
                crate::remote::RepoPath::from_local(path)?,
            ));
        }
        Ok(entry_kind(&operation_path(&self.root, path)?)?.1)
    }

    pub(crate) fn operate(&self, path: &Path, operation: FileOperation) -> Result<()> {
        if let Some(ssh) = &self.ssh {
            return ssh.call(crate::remote::Request::FileOperation(
                crate::remote::RepoPath::from_local(path)?,
                operation,
            ));
        }
        let target = operation_path(&self.root, path)?;
        match operation {
            FileOperation::Create { directory: true } => fs::create_dir(target)?,
            FileOperation::Create { directory: false } => {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(target)?;
            }
            FileOperation::Rename { name, version } => {
                ensure!(valid_name(&name), t!("workspace.tree_invalid_name"));
                ensure!(
                    entry_kind(&target)?.1 == version,
                    t!("workspace.tree_entry_changed")
                );
                let destination = target.with_file_name(name);
                if target != destination {
                    rename_no_replace(&target, &destination)?;
                }
            }
            FileOperation::Delete { version } => {
                let (metadata, current) = entry_kind(&target)?;
                ensure!(current == version, t!("workspace.tree_entry_changed"));
                if metadata.is_dir() {
                    fs::remove_dir_all(target)?;
                } else {
                    #[cfg(windows)]
                    {
                        use std::os::windows::fs::FileTypeExt;
                        if metadata.file_type().is_symlink_dir() {
                            fs::remove_dir(target)?;
                            return Ok(());
                        }
                    }
                    fs::remove_file(target)?;
                }
            }
            FileOperation::Transfer {
                destination,
                version,
                move_entry,
            } => {
                let destination = operation_path(&self.root, destination.as_path())?;
                transfer_entry(&target, &destination, &version, move_entry)?;
            }
        }
        Ok(())
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn import_entry(
        &self,
        source: &Path,
        destination: &Path,
        move_entry: bool,
    ) -> Result<Option<PathBuf>> {
        ensure!(self.ssh.is_none(), t!("storage.no_local_path"));
        ensure!(source.is_absolute(), t!("common.path_outside_repo"));
        let parent = source
            .parent()
            .context(t!("storage.no_parent"))?
            .canonicalize()?;
        let name = source
            .file_name()
            .context(t!("workspace.tree_invalid_name"))?;
        let source = parent.join(name);
        let relative = source.strip_prefix(&self.root).ok();
        if let Some(relative) = relative {
            operation_path(&self.root, relative)?;
        }
        let destination = operation_path(&self.root, destination)?;
        let version = entry_kind(&source)?.1;
        transfer_entry(&source, &destination, &version, move_entry)?;
        Ok(relative.map(Path::to_owned))
    }

    pub(crate) fn read_copy_chunk(
        &self,
        path: &Path,
        version: &EntryVersion,
        offset: u64,
    ) -> Result<Vec<u8>> {
        let path = resolve_file(&self.root, path)?;
        ensure!(
            &entry_kind(&path)?.1 == version,
            t!("workspace.tree_entry_changed")
        );
        let mut file = File::open(&path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut chunk = Vec::new();
        file.take(COPY_CHUNK as u64).read_to_end(&mut chunk)?;
        ensure!(
            &entry_kind(&path)?.1 == version,
            t!("workspace.tree_entry_changed")
        );
        Ok(chunk)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn copy_to(&self, source: &Path, output: &mut impl Write) -> Result<()> {
        if let Some(ssh) = &self.ssh {
            if !ssh.supports_workspace_files() {
                return Ok(output.write_all(&self.bytes(source, false)?)?);
            }
            let version = self.entry_version(source)?;
            let path = crate::remote::RepoPath::from_local(source)?;
            let mut offset = 0;
            loop {
                let chunk: crate::remote::Bytes = ssh.call(
                    crate::remote::Request::ReadCopyChunk(path.clone(), version.clone(), offset),
                )?;
                output.write_all(&chunk.0)?;
                offset += chunk.0.len() as u64;
                if chunk.0.len() < COPY_CHUNK {
                    break;
                }
            }
        } else {
            let source = resolve_file(&self.root, source)?;
            let version = entry_kind(&source)?.1;
            std::io::copy(&mut File::open(&source)?, output)?;
            ensure!(
                entry_kind(&source)?.1 == version,
                t!("workspace.tree_entry_changed")
            );
        }
        Ok(())
    }
}

fn transfer_entry(
    source: &Path,
    destination: &Path,
    version: &EntryVersion,
    move_entry: bool,
) -> Result<()> {
    let (metadata, current) = entry_kind(source)?;
    ensure!(&current == version, t!("workspace.tree_entry_changed"));
    ensure!(
        source != destination && !(metadata.is_dir() && destination.starts_with(source)),
        t!("workspace.tree_invalid_destination")
    );
    match destination.symlink_metadata() {
        Ok(_) => anyhow::bail!(t!("workspace.tree_destination_exists")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if move_entry {
        return rename_no_replace(source, destination);
    }

    let staging = tempfile::tempdir_in(destination.parent().context(t!("storage.no_parent"))?)?;
    let staged = staging.path().join("entry");
    copy_entry(source, &staged)?;
    ensure!(
        &entry_kind(source)?.1 == version,
        t!("workspace.tree_entry_changed")
    );
    rename_no_replace(&staged, destination)
}

fn copy_entry(source: &Path, destination: &Path) -> Result<()> {
    let (metadata, version) = entry_kind(source)?;
    ensure!(!is_link(&metadata), t!("workspace.tree_unsupported"));
    if metadata.is_dir() {
        ensure!(
            source.canonicalize()? == source,
            t!("common.path_outside_repo")
        );
        fs::create_dir(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let name = entry.file_name();
            ensure!(
                name.to_str().is_some_and(valid_name),
                t!("workspace.tree_invalid_name")
            );
            copy_entry(&entry.path(), &destination.join(name))?;
        }
    } else {
        ensure!(metadata.is_file(), t!("workspace.tree_unsupported"));
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut input = options.open(source)?;
        ensure!(
            input.metadata()?.is_file(),
            t!("workspace.tree_unsupported")
        );
        let mut output = File::create_new(destination)?;
        std::io::copy(&mut input, &mut output)?;
        output.set_permissions(metadata.permissions())?;
    }
    ensure!(
        entry_kind(source)?.1 == version,
        t!("workspace.tree_entry_changed")
    );
    Ok(())
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    metadata.file_type().is_symlink()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rename_no_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if result == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(windows)]
fn rename_no_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileW(source: *const u16, destination: *const u16) -> i32;
    }
    let source: Vec<_> = source.as_os_str().encode_wide().chain([0]).collect();
    let destination: Vec<_> = destination.as_os_str().encode_wide().chain([0]).collect();
    if unsafe { MoveFileW(source.as_ptr(), destination.as_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn rename_no_replace(_: &Path, _: &Path) -> Result<()> {
    anyhow::bail!(t!("workspace.tree_unsupported"))
}

pub(crate) fn check_ignored_paths(
    root: &Path,
    paths: &[PathBuf],
    include_tracked: bool,
) -> Result<BTreeSet<PathBuf>> {
    if paths.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut input = tempfile::tempfile()?;
    for path in paths {
        let path = path.to_str().context("Invalid workspace path encoding")?;
        input.write_all(b"./")?;
        input.write_all(path.replace(std::path::MAIN_SEPARATOR, "/").as_bytes())?;
        input.write_all(&[0])?;
    }
    input.rewind()?;
    let output = Command::new("git")
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false"])
        .arg("-C")
        .arg(root)
        .arg("check-ignore")
        .args(include_tracked.then_some("--no-index"))
        .args(["--stdin", "-z"])
        .env("LC_ALL", "C")
        .stdin(Stdio::from(input))
        .output()?;
    ensure!(
        output.status.success() || output.status.code() == Some(1),
        "git check-ignore failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|bytes| {
            std::str::from_utf8(bytes)
                .ok()
                .filter(|path| !path.is_empty())
                .map(|path| PathBuf::from(path.strip_prefix("./").unwrap_or(path)))
        })
        .collect())
}

fn ignored_paths(root: &Path, paths: &[PathBuf]) -> Result<BTreeSet<PathBuf>> {
    // A filesystem listing can include paths below an indexed Gitlink.
    let mut ignored = check_ignored_paths(root, paths, true)?;
    if !ignored.is_empty() {
        // Limit index output to candidates that match an ignore rule. Git includes
        // descendants of a directory pathspec, which keeps tracked ancestors visible.
        let candidates: Vec<_> = if ignored.len() <= 64 {
            ignored.iter().cloned().collect()
        } else {
            ignored
                .iter()
                .filter_map(|path| path.parent().map(Path::to_owned))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        };
        let mut candidates = candidates.iter().peekable();
        while candidates.peek().is_some() {
            let mut args = vec!["--literal-pathspecs", "ls-files", "--cached", "-z", "--"];
            let mut bytes = 0;
            while let Some(path) = candidates.peek() {
                let path = path.to_str().unwrap();
                if bytes > 0 && bytes + path.len() > 8_000 {
                    break;
                }
                bytes += path.len() + 1;
                args.push(if path.is_empty() { "." } else { path });
                candidates.next();
            }
            let tracked = git(root, &args)?;
            for path in tracked
                .split(|byte| *byte == 0)
                .filter(|bytes| !bytes.is_empty())
                .filter_map(|bytes| std::str::from_utf8(bytes).ok())
            {
                for ancestor in Path::new(path).ancestors() {
                    ignored.remove(ancestor);
                }
            }
        }
    }
    Ok(ignored)
}

fn mark_ignored(root: &Path, listing: &mut BTreeMap<PathBuf, Vec<Entry>>) -> Result<()> {
    let mut paths = BTreeMap::<&Path, Vec<PathBuf>>::new();
    let mut scopes = BTreeMap::<&Path, &Path>::new();
    for (directory, children) in listing.iter() {
        if children.is_empty() {
            continue;
        }
        let scope = directory
            .ancestors()
            .take_while(|parent| !parent.as_os_str().is_empty())
            .find_map(|parent| {
                scopes
                    .get(parent)
                    .copied()
                    .or_else(|| root.join(parent).join(".git").exists().then_some(parent))
            })
            .unwrap_or(Path::new(""));
        scopes.insert(directory, scope);
        paths.entry(scope).or_default().extend(
            children
                .iter()
                .map(|entry| entry.path.strip_prefix(scope).unwrap().to_owned()),
        );
    }
    let mut ignored = BTreeSet::new();
    for (scope, paths) in paths {
        let matches = ignored_paths(&root.join(scope), &paths);
        let matches = if scope.as_os_str().is_empty() {
            matches?
        } else {
            // A stale or inaccessible nested .git entry can still have browsable files.
            matches.unwrap_or_default()
        };
        ignored.extend(matches.into_iter().map(|path| scope.join(path)));
    }
    for entry in listing.values_mut().flatten() {
        entry.ignored = ignored.contains(&entry.path);
    }
    Ok(())
}

fn visit_tree_directory(
    root: &Path,
    parent: &Path,
    mut visit: impl FnMut(std::ffi::OsString, fs::FileType),
) -> Result<bool> {
    let directory = match fs::read_dir(root.join(parent)) {
        Ok(directory) => directory,
        Err(error)
            if !parent.as_os_str().is_empty()
                && matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::NotADirectory
                        | std::io::ErrorKind::PermissionDenied
                ) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    for entry in directory.flatten() {
        let name = entry.file_name();
        if name
            .to_str()
            .is_none_or(|name| name.eq_ignore_ascii_case(".git"))
        {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        visit(name, kind);
    }
    Ok(true)
}

fn read_tree_directory(root: &Path, parent: &Path) -> Result<Option<Vec<Entry>>> {
    let mut children = Vec::new();
    let readable = visit_tree_directory(root, parent, |name, kind| {
        if kind.is_file() || kind.is_dir() || kind.is_symlink() {
            children.push(Entry {
                path: parent.join(name),
                directory: kind.is_dir(),
                symlink: kind.is_symlink(),
                ignored: false,
            });
        }
    })?;
    children.sort_by(|a, b| (!a.directory, &a.path).cmp(&(!b.directory, &b.path)));
    Ok(readable.then_some(children))
}

pub(crate) fn fast_document_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let output = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "*.[mM][dD]",
            "*.[mM][dD][xX]",
            "*.[mM][dD][oO][wW][nN]",
            "*.[mM][kK][dD]",
            "*.[mM][aA][rR][kK][dD][oO][wW][nN]",
        ],
    )?;
    Ok(output
        .split(|byte| *byte == 0)
        .filter_map(|name| std::str::from_utf8(name).ok())
        .map(Path::new)
        .filter(|path| {
            super::is_markdown(path)
                && operation_path(root, path).is_ok_and(|absolute| {
                    absolute
                        .symlink_metadata()
                        .is_ok_and(|metadata| metadata.is_file())
                })
        })
        .map(Path::to_owned)
        .collect())
}

#[derive(Serialize, Deserialize)]
pub(crate) struct DocumentBatch {
    pub paths: Vec<PathBuf>,
    pub done: bool,
}

pub(crate) struct DocumentScan {
    pending: Vec<PathBuf>,
}

impl Default for DocumentScan {
    fn default() -> Self {
        Self {
            pending: vec![PathBuf::new()],
        }
    }
}

impl DocumentScan {
    pub(crate) fn next(&mut self, root: &Path) -> Result<DocumentBatch> {
        let mut paths = Vec::new();
        for _ in 0..4096 {
            let Some(parent) = self.pending.pop() else {
                break;
            };
            visit_tree_directory(root, &parent, |name, kind| {
                if kind.is_dir() {
                    self.pending.push(parent.join(name));
                } else if kind.is_file() && super::is_markdown(Path::new(&name)) {
                    paths.push(parent.join(name));
                }
            })?;
            if paths.len() >= 512 {
                break;
            }
        }
        Ok(DocumentBatch {
            paths,
            done: self.pending.is_empty(),
        })
    }
}

pub(crate) fn scan_document_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let mut scan = DocumentScan::default();
    let mut paths = Vec::new();
    loop {
        let batch = scan.next(root)?;
        paths.extend(batch.paths);
        if batch.done {
            return Ok(paths);
        }
    }
}

pub(crate) fn scan_tree(root: &Path) -> Result<BTreeMap<PathBuf, Vec<Entry>>> {
    let mut listing = BTreeMap::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(parent) = pending.pop() {
        if let Some(children) = read_tree_directory(root, &parent)? {
            pending.extend(
                children
                    .iter()
                    .filter(|entry| entry.directory)
                    .map(|entry| entry.path.clone()),
            );
            listing.insert(parent, children);
        }
    }
    mark_ignored(root, &mut listing)?;
    Ok(listing)
}

pub(crate) fn branch(root: &Path) -> String {
    git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .or_else(|_| git(root, &["rev-parse", "--short", "HEAD"]))
        .map(|bytes| String::from_utf8_lossy(&bytes).trim_end().to_owned())
        .unwrap_or_default()
}

pub(crate) fn scan_directories(
    root: &Path,
    directories: Vec<PathBuf>,
) -> Result<BTreeMap<PathBuf, Vec<Entry>>> {
    let mut listing = BTreeMap::new();
    for directory in directories {
        if !directory.as_os_str().is_empty()
            && !operation_path(root, &directory).is_ok_and(|path| {
                path.symlink_metadata()
                    .is_ok_and(|metadata| metadata.is_dir())
            })
        {
            continue;
        }
        if let Some(children) = read_tree_directory(root, &directory)? {
            listing.insert(directory, children);
        }
    }
    mark_ignored(root, &mut listing)?;
    Ok(listing)
}

#[cfg(feature = "desktop")]
impl Repository {
    pub(crate) fn refresh_directories(
        &self,
        directories: BTreeSet<PathBuf>,
        invalidate: BTreeSet<PathBuf>,
        refresh_branch: bool,
    ) -> Result<Self> {
        if !self.lazy_tree {
            return self.complete_tree();
        }
        let mut snapshot = self.clone();
        if refresh_branch {
            snapshot.branch = if let Some(ssh) = &self.ssh {
                ssh.call(crate::remote::Request::TreeBranch)?
            } else {
                branch(&self.root)
            };
        }
        let mut updates = if directories.is_empty() {
            BTreeMap::new()
        } else if let Some(ssh) = &self.ssh {
            ssh.call(crate::remote::Request::TreeDirectories(
                directories.iter().cloned().collect(),
            ))?
        } else {
            scan_directories(&self.root, directories.iter().cloned().collect())?
        };

        for path in invalidate {
            snapshot
                .children
                .retain(|parent, _| !parent.starts_with(&path));
        }
        for directory in directories {
            let old = snapshot.children.remove(&directory).unwrap_or_default();
            let Some(entries) = updates.remove(&directory) else {
                snapshot
                    .children
                    .retain(|parent, _| !parent.starts_with(&directory));
                continue;
            };
            let current_dirs: BTreeSet<_> = entries
                .iter()
                .filter(|entry| entry.directory)
                .map(|entry| &entry.path)
                .collect();
            for entry in old
                .iter()
                .filter(|entry| entry.directory && !current_dirs.contains(&entry.path))
            {
                snapshot
                    .children
                    .retain(|parent, _| !parent.starts_with(&entry.path));
            }
            snapshot.children.insert(directory, entries);
        }
        Ok(snapshot)
    }
}

#[cfg(feature = "desktop")]
pub(crate) struct CopyTarget {
    path: PathBuf,
    existing: Option<(fs::Permissions, EntryVersion)>,
}

#[cfg(feature = "desktop")]
impl CopyTarget {
    pub(crate) fn prepare(destination: PathBuf, source: Option<&Path>) -> Result<Self> {
        let parent = destination.parent().context(t!("storage.no_parent"))?;
        let name = destination
            .file_name()
            .context(t!("workspace.tree_invalid_name"))?;
        let path = parent.canonicalize()?.join(name);
        if let Some(source) = source {
            ensure!(
                source != path && path.canonicalize().ok().as_deref() != Some(source),
                t!("workspace.tree_same_file")
            );
        }
        let existing = match entry_kind(&path) {
            Ok((metadata, version)) => {
                ensure!(
                    metadata.file_type().is_file(),
                    t!("storage.local_regular_file")
                );
                Some((metadata.permissions(), version))
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        Ok(Self { path, existing })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn exists(&self) -> bool {
        self.existing.is_some()
    }

    pub(crate) fn write(
        self,
        copy: impl FnOnce(&mut tempfile::NamedTempFile) -> Result<()>,
    ) -> Result<()> {
        let mut output = tempfile::NamedTempFile::new_in(self.path.parent().unwrap())?;
        copy(&mut output)?;
        if let Some((permissions, version)) = self.existing {
            output.as_file().set_permissions(permissions)?;
            output.as_file().sync_all()?;
            ensure!(
                entry_kind(&self.path)?.1 == version,
                t!("workspace.tree_entry_changed")
            );
            output.persist(&self.path)?;
        } else {
            output.as_file().sync_all()?;
            output.persist_noclobber(&self.path)?;
        }
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod transfer_tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn worktree_transfer_copies_and_moves_without_overwriting() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().canonicalize()?;
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()?
                .success()
        );
        fs::create_dir_all(root.join("source/sub"))?;
        fs::create_dir(root.join("destination"))?;
        fs::write(root.join("source/sub/data.bin"), [0, 255])?;
        let repo = Repository::open(&root)?;
        let source = Path::new("source");
        let operation = |destination: &str, move_entry| -> Result<FileOperation> {
            Ok(FileOperation::Transfer {
                destination: crate::remote::RepoPath::from_local(destination.as_ref())?,
                version: repo.entry_version(source)?,
                move_entry,
            })
        };
        assert!(
            repo.operate(source, operation("source/sub/self", false)?)
                .is_err()
        );
        repo.operate(source, operation("destination/copied", false)?)?;
        assert_eq!(
            fs::read(root.join("destination/copied/sub/data.bin"))?,
            [0, 255]
        );
        assert!(
            repo.operate(source, operation("destination/copied", true)?)
                .is_err()
        );
        assert!(root.join("source/sub/data.bin").is_file());

        let pending = operation("destination/moved", true)?;
        fs::write(root.join("source/updated"), "new content")?;
        assert!(repo.operate(source, pending).is_err());
        repo.operate(source, operation("destination/moved", true)?)?;
        assert!(!root.join("source").exists());
        assert!(root.join("destination/moved/updated").is_file());
        Ok(())
    }

    #[test]
    fn directory_copy_cleans_staging_if_an_entry_is_unsupported() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().canonicalize()?;
        fs::create_dir(root.join("source"))?;
        fs::create_dir(root.join("destination"))?;
        fs::write(root.join("source/file.txt"), "contents")?;
        std::os::unix::fs::symlink("/tmp", root.join("source/link"))?;
        let source = root.join("source");
        let destination = root.join("destination");
        let version = entry_kind(&source)?.1;
        assert!(transfer_entry(&source, &destination.join("copy"), &version, false).is_err());
        assert_eq!(fs::read_dir(&destination)?.count(), 0);
        assert_eq!(fs::read(root.join("source/file.txt"))?, b"contents");
        Ok(())
    }
}
