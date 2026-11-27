use rust_i18n::t;

mod edit_history;
pub(crate) mod files;
pub(crate) mod resources;
mod selection;
pub(crate) use selection::{Worktree, local_worktrees};
mod session_state;
mod state;
pub(crate) mod worktree_identity;
#[cfg(feature = "desktop")]
mod worktree_state;
#[cfg(feature = "desktop")]
pub(crate) use edit_history::{DraftRelocation, SavedEditHistory};

pub(crate) use selection::{DirectoryEntry, DirectoryListing, directory_name, repository_root};
#[cfg(feature = "desktop")]
pub(crate) use selection::{RecentRepository, project_name, ssh_project_name};
#[cfg(feature = "desktop")]
pub(crate) use worktree_state::WorktreeState;

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File},
    io::{ErrorKind, Read, Write},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::{
    remote::{Bytes, RepoPath, Request, SshSession, SshTarget},
    session::WorktreeSession,
    tabs::TabSession,
};

pub(crate) fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "md" | "markdown" | "mdown" | "mkd" | "mdx"
            )
        })
}

pub(crate) const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_ASSET_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct Repository {
    pub(crate) ssh: Option<SshSession>,
    remote_identity: Option<String>,
    #[cfg(feature = "desktop")]
    remote_resource_read: bool,
    #[cfg(feature = "desktop")]
    remote_resource_chunks: bool,
    #[cfg(feature = "desktop")]
    pub(crate) worktree_state: Option<WorktreeState>,
    #[cfg(feature = "desktop")]
    language_resources:
        std::sync::Arc<std::sync::OnceLock<crate::language::resources::LanguageResources>>,
    #[cfg(feature = "desktop")]
    pub(crate) worktree_lsp:
        std::sync::Arc<std::sync::RwLock<std::sync::Arc<crate::language::worktree::WorktreeLsp>>>,
    pub root: PathBuf,
    pub name: String,
    pub branch: String,
    pub children: BTreeMap<PathBuf, Vec<Entry>>,
    pub(crate) common_dir: PathBuf,
    pub(crate) lazy_tree: bool,
    #[cfg(feature = "desktop")]
    document_batches: std::sync::Arc<std::sync::OnceLock<bool>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub path: PathBuf,
    pub directory: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) symlink: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) ignored: bool,
}

impl Repository {
    #[cfg(feature = "desktop")]
    pub(crate) fn language_resources(&self) -> &crate::language::resources::LanguageResources {
        self.language_resources
            .get_or_init(|| crate::language::resources::LanguageResources::load(Some(self)))
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(target) = SshTarget::parse(path)? {
            let path = target.path.clone();
            return Self::open_ssh(SshSession::connect(target)?, path);
        }
        Self::open_root(repository_root(path)?)
    }

    pub(crate) fn open_ssh(ssh: SshSession, path: PathBuf) -> Result<Self> {
        let version = crate::remote::LAZY_TREE_VERSION;
        let lazy_tree = ssh
            .call::<u32>(Request::Hello(version))
            .is_ok_and(|reply| reply == version);
        let request = if lazy_tree {
            Request::OpenWorkspace(path)
        } else {
            Request::Open(path)
        };
        let info: RepositoryInfo = ssh.call(request)?;
        let root = ssh.target().locator(&info.root);
        let common_dir = ssh.target().locator(&info.common_dir);
        let remote_identity = Some(format!("{}\0{}", ssh.target().authority(), info.identity));
        Ok(Self {
            ssh: Some(ssh),
            remote_identity,
            #[cfg(feature = "desktop")]
            remote_resource_read: info.resource_read,
            #[cfg(feature = "desktop")]
            remote_resource_chunks: info.resource_chunks,
            #[cfg(feature = "desktop")]
            worktree_state: None,
            #[cfg(feature = "desktop")]
            language_resources: Default::default(),
            #[cfg(feature = "desktop")]
            worktree_lsp: Default::default(),
            root,
            common_dir,
            name: info.name,
            branch: info.branch,
            children: info.children,
            lazy_tree,
            #[cfg(feature = "desktop")]
            document_batches: Default::default(),
        })
    }

    pub(crate) fn open_root(root: PathBuf) -> Result<Self> {
        let common_dir = selection::common_dir(&root)?;
        let name = directory_name(&root);
        let branch = files::branch(&root);
        let children = files::scan_directories(&root, vec![PathBuf::new()])?;
        Ok(Self {
            ssh: None,
            remote_identity: None,
            #[cfg(feature = "desktop")]
            remote_resource_read: false,
            #[cfg(feature = "desktop")]
            remote_resource_chunks: false,
            #[cfg(feature = "desktop")]
            worktree_state: None,
            #[cfg(feature = "desktop")]
            language_resources: Default::default(),
            #[cfg(feature = "desktop")]
            worktree_lsp: Default::default(),
            root,
            name,
            branch,
            children,
            common_dir,
            lazy_tree: true,
            #[cfg(feature = "desktop")]
            document_batches: Default::default(),
        })
    }

    #[cfg(feature = "desktop")]
    fn file_tree(paths: Vec<PathBuf>) -> BTreeMap<PathBuf, Vec<Entry>> {
        let mut entries = BTreeMap::<PathBuf, BTreeSet<(bool, PathBuf)>>::new();
        for path in paths {
            let path = path.as_path();
            for (index, child) in path
                .ancestors()
                .take_while(|p| !p.as_os_str().is_empty())
                .enumerate()
            {
                entries
                    .entry(child.parent().unwrap_or(Path::new("")).to_owned())
                    .or_default()
                    .insert((index == 0, child.to_owned()));
            }
        }
        entries
            .into_iter()
            .map(|(path, entries)| {
                let entries = entries
                    .into_iter()
                    .map(|(file, path)| Entry {
                        path,
                        directory: !file,
                        symlink: false,
                        ignored: false,
                    })
                    .collect();
                (path, entries)
            })
            .collect()
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn supports_document_batches(&self) -> bool {
        *self.document_batches.get_or_init(|| {
            self.ssh.as_ref().is_none_or(|ssh| {
                let version = crate::remote::DOCUMENT_BATCH_VERSION;
                ssh.call::<u32>(Request::Hello(version))
                    .is_ok_and(|reply| reply == version)
            })
        })
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn fast_document_paths(&self) -> Result<Vec<PathBuf>> {
        if let Some(ssh) = &self.ssh {
            ssh.call(Request::FastDocumentPaths)
        } else {
            files::fast_document_paths(&self.root)
        }
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn document_paths(&self) -> Result<Option<Vec<PathBuf>>> {
        let Some(ssh) = &self.ssh else {
            return files::scan_document_paths(&self.root).map(Some);
        };
        let version = crate::remote::DOCUMENT_PATHS_VERSION;
        if ssh
            .call::<u32>(Request::Hello(version))
            .is_ok_and(|reply| reply == version)
        {
            return ssh.call(Request::DocumentPaths).map(Some);
        }
        Ok(None)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn complete_tree(&self) -> Result<Self> {
        let mut snapshot = self.clone();
        let Some(ssh) = &self.ssh else {
            snapshot.children = files::scan_tree(&self.root)?;
            snapshot.branch = files::branch(&self.root);
            return Ok(snapshot);
        };
        if ssh.supports_workspace_files() {
            (snapshot.children, snapshot.branch) = ssh.call(Request::TreeSnapshot)?;
        } else if ssh.supports_locator() {
            snapshot.children = Self::file_tree(ssh.call(Request::LocatorPaths)?);
            if let Some(tree) = self
                .worktrees()?
                .into_iter()
                .find(|tree| tree.root == self.root)
            {
                snapshot.branch = tree.reference;
            }
        } else {
            // Older helpers only return the file tree when opening a new connection.
            let fresh = Self::open(&self.root)?;
            ensure!(
                fresh.root == self.root && fresh.storage_key() == self.storage_key(),
                "The remote repository changed while refreshing"
            );
            snapshot.children = fresh.children;
            snapshot.branch = fresh.branch;
        }
        Ok(snapshot)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn git_backend(&self) -> crate::git::Backend {
        crate::git::Backend::new(self.root.clone(), self.ssh.clone())
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn allows_ai_hover(&self, path: &Path) -> bool {
        match &self.ssh {
            Some(ssh) => {
                ssh.supports_ai_context()
                    && RepoPath::from_local(path).ok().is_some_and(|path| {
                        ssh.call::<bool>(Request::AllowsHoverTranslation(path))
                            .unwrap_or(false)
                    })
            }
            None => crate::ai::context::allowed_hover_source(&self.root, path),
        }
    }

    pub(crate) fn resolve(&self, path: &Path) -> Result<PathBuf> {
        ensure!(self.ssh.is_none(), t!("storage.no_local_path"));
        resolve_file(&self.root, path)
    }

    /// Uses relative keys for the local worktree and absolute keys for standalone buffers.
    #[cfg(feature = "desktop")]
    pub(crate) fn local_file_key(&self, path: &Path) -> Result<PathBuf> {
        ensure!(path.is_absolute(), t!("storage.local_regular_file"));
        let target = canonical_local_file(path)?;
        ensure!(target.to_str().is_some(), t!("storage.local_path_encoding"));
        if self.ssh.is_none()
            && let Ok(relative) = target.strip_prefix(&self.root)
        {
            return Ok(relative.to_owned());
        }
        Ok(target)
    }

    pub(crate) fn storage_key(&self) -> &[u8] {
        #[cfg(feature = "desktop")]
        if let Some(state) = &self.worktree_state {
            return state.storage_key();
        }
        self.remote_identity
            .as_ref()
            .map_or(self.root.as_os_str().as_encoded_bytes(), |key| {
                key.as_bytes()
            })
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn same_worktree(&self, other: &Self) -> bool {
        match (&self.worktree_state, &other.worktree_state) {
            (Some(left), Some(right)) => left.id == right.id,
            (None, None) => self.storage_key() == other.storage_key(),
            _ => false,
        }
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn checkout_key(&self) -> &[u8] {
        self.worktree_state
            .as_ref()
            .map_or_else(|| self.storage_key(), |state| state.id.as_bytes())
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn source_path(&self, uri: &str) -> Option<PathBuf> {
        let uri = url::Url::parse(uri).ok()?;
        if uri.scheme() != "file" {
            return None;
        }
        if let Some(target) = SshTarget::parse(&self.root).ok().flatten() {
            if uri.host().is_some() {
                return None;
            }
            let path = percent_encoding::percent_decode_str(uri.path())
                .decode_utf8()
                .ok()?;
            let root = target.path.to_str()?;
            let relative = path
                .strip_prefix(root.trim_end_matches('/'))?
                .strip_prefix('/')?;
            if relative
                .split('/')
                .any(|part| matches!(part, "" | "." | ".." | ".git") || part.contains('\\'))
            {
                return None;
            }
            return Some(PathBuf::from(relative));
        }
        let path = uri.to_file_path().ok()?;
        // Windows file URIs omit the extended-length prefix found in canonical paths.
        #[cfg(windows)]
        let path = path.canonicalize().ok()?;
        let relative = path.strip_prefix(&self.root).ok()?;
        (!relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(name) if name != ".git")))
        .then(|| relative.to_owned())
    }

    pub(crate) fn exists(&self, path: &Path) -> bool {
        match &self.ssh {
            Some(ssh) => RepoPath::from_local(path)
                .and_then(|path| ssh.call::<bool>(Request::Exists(path)))
                .unwrap_or(false),
            None => self.resolve(path).is_ok(),
        }
    }

    pub(crate) fn bytes(&self, path: &Path, asset: bool) -> Result<Vec<u8>> {
        match &self.ssh {
            Some(ssh) => read_remote_bytes(ssh, path, asset),
            None if asset => read_asset(&self.root, path),
            None => read_bytes(&self.resolve(path)?),
        }
    }

    pub(crate) fn bytes_if_present(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        #[cfg(feature = "desktop")]
        if let Some(ssh) = &self.ssh {
            let remote = RepoPath::from_local(path)?;
            if ssh.supports_workspace_files() {
                let bytes: Option<Bytes> = ssh.call(Request::ReadForSync(remote))?;
                return Ok(bytes.map(|bytes| bytes.0));
            }
            return match self.bytes(path, false) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(_) if !ssh.call::<bool>(Request::Exists(remote)).unwrap_or(true) => Ok(None),
                Err(error) => Err(error),
            };
        }
        match self.bytes(path, false) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub fn read_markdown(&self, path: &Path) -> Result<String> {
        decode_markdown(self.bytes(path, false)?)
    }
}

pub(crate) fn read_remote_bytes(ssh: &SshSession, path: &Path, asset: bool) -> Result<Vec<u8>> {
    Ok(ssh
        .call::<Bytes>(Request::Read(RepoPath::from_local(path)?, asset))?
        .0)
}

pub(crate) fn decode_markdown(bytes: Vec<u8>) -> Result<String> {
    let text = std::str::from_utf8(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes))
        .context(t!("storage.text_only"))?;
    ensure!(!text.contains('\0'), t!("storage.text_only"));
    Ok(text.to_owned())
}

#[derive(Serialize, Deserialize)]
pub(crate) struct RepositoryInfo {
    pub identity: String,
    #[serde(default)]
    pub resource_read: bool,
    #[serde(default)]
    pub resource_chunks: bool,
    pub root: PathBuf,
    pub common_dir: PathBuf,
    pub name: String,
    pub branch: String,
    pub children: BTreeMap<PathBuf, Vec<Entry>>,
}

impl From<Repository> for RepositoryInfo {
    fn from(repo: Repository) -> Self {
        let identity = format!(
            "{:?}",
            (
                env::var("SSH_CONNECTION")
                    .unwrap_or_default()
                    .split_whitespace()
                    .skip(2)
                    .collect::<Vec<_>>(),
                env::var("USER").unwrap_or_default(),
                std::fs::read_to_string("/etc/machine-id")
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
                &repo.root,
            )
        );
        Self {
            identity,
            resource_read: true,
            resource_chunks: true,
            root: repo.root,
            common_dir: repo.common_dir,
            name: repo.name,
            branch: repo.branch,
            children: repo.children,
        }
    }
}

pub(crate) fn resolve_file(root: &Path, path: &Path) -> Result<PathBuf> {
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|p| matches!(p, Component::Normal(name) if name != ".git")),
        t!("common.path_outside_repo")
    );
    let target = root.join(path);
    ensure!(
        target.canonicalize()? == target && target.is_file(),
        t!("storage.regular_file")
    );
    Ok(target)
}

pub(crate) fn canonical_local_file(path: &Path) -> Result<PathBuf> {
    let target = path.canonicalize()?;
    ensure!(
        target.is_file() && target.components().all(|part| part.as_os_str() != ".git"),
        t!("storage.local_regular_file")
    );
    Ok(target)
}

fn resolve_local_file(path: &Path) -> Result<PathBuf> {
    let target = canonical_local_file(path)?;
    ensure!(target == path, t!("storage.local_regular_file"));
    Ok(target)
}

pub(crate) fn worktree_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let paths = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    Ok(paths
        .split(|&byte| byte == 0)
        .filter(|path| !path.is_empty())
        .filter_map(|path| std::str::from_utf8(path).ok())
        .map(PathBuf::from)
        .filter(|path| root.join(path).symlink_metadata().is_ok())
        .collect())
}

pub(crate) fn git(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = crate::process::background("git");
    command
        .env("LC_ALL", "C")
        .arg("-C")
        .arg(path)
        .args(args)
        .stdin(std::process::Stdio::null());
    crate::git::capture_repository(command)
}

#[derive(Clone)]
pub struct Document {
    pub text: String,
    pub saved_text: String,
    pub restored: bool,
    pub conflict: bool,
    // Tracks the on-disk representation after the editor text has been normalized.
    pub mixed_newlines: bool,
    pub(crate) missing: bool,
    pub(crate) identity: Option<LocalFileIdentity>,
    bom: bool,
    crlf: bool,
    baseline: [u8; 32],
    recovered_baseline: Option<[u8; 32]>,
}

pub(crate) type LocalFileIdentity = (u64, u64, Option<std::time::SystemTime>);

impl Document {
    pub fn open(repo: &Repository, path: &Path, store: &Store) -> Result<Self> {
        Self::open_with_draft(
            Self::read_for_sync(Some(repo), path)?,
            store.draft(repo, path)?,
        )
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn open_local(path: &Path, store: &Store) -> Result<Self> {
        Self::open_with_draft(
            Self::read_for_sync(None, path)?,
            store.editor_draft(None, path)?,
        )
    }

    fn open_with_draft(disk: Option<Self>, draft: Option<Draft>) -> Result<Self> {
        if let Some(mut document) = disk {
            document.apply_draft(draft);
            return Ok(document);
        }
        let draft = draft.ok_or_else(|| std::io::Error::from(ErrorKind::NotFound))?;
        Ok(Self {
            text: draft.text,
            saved_text: String::new(),
            restored: true,
            conflict: false,
            mixed_newlines: false,
            missing: true,
            identity: None,
            bom: draft.format & 1 != 0,
            crlf: draft.format & 2 != 0,
            baseline: draft.baseline,
            recovered_baseline: None,
        })
    }

    fn apply_draft(&mut self, draft: Option<Draft>) {
        if let Some(draft) = draft.filter(|draft| draft.text != self.saved_text) {
            self.restored = true;
            self.conflict = draft.baseline != self.baseline;
            self.recovered_baseline = self.conflict.then_some(draft.baseline);
            if self.conflict && self.mixed_newlines {
                self.crlf = draft.format & 2 != 0;
            }
            self.text = draft.text;
        }
    }

    #[cfg_attr(not(feature = "desktop"), allow(dead_code))]
    pub(crate) fn read(repo: &Repository, path: &Path) -> Result<Self> {
        if path.is_absolute() {
            return Self::read_local(path);
        }
        if repo.ssh.is_none() {
            return Self::read_resolved(&repo.resolve(path)?);
        }
        Self::decode(repo.bytes(path, false)?)
    }

    pub(crate) fn read_for_sync(repo: Option<&Repository>, path: &Path) -> Result<Option<Self>> {
        if let Some(repo) = repo.filter(|repo| !path.is_absolute() && repo.ssh.is_some()) {
            return repo.bytes_if_present(path)?.map(Self::decode).transpose();
        }
        match repo.map_or_else(|| Self::read_local(path), |repo| Self::read(repo, path)) {
            Ok(document) => Ok(Some(document)),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    #[cfg_attr(not(feature = "desktop"), allow(dead_code))]
    pub(crate) fn read_local(path: &Path) -> Result<Self> {
        Self::read_resolved(&resolve_local_file(path)?)
    }

    fn read_resolved(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let identity = file_identity(&file.metadata()?);
        let mut document = Self::decode(read_document_bytes(file)?)?;
        document.identity = identity;
        Ok(document)
    }

    pub(crate) fn decode(bytes: Vec<u8>) -> Result<Self> {
        let baseline = digest(&bytes);
        let bom = bytes.starts_with(b"\xef\xbb\xbf");
        let disk = std::str::from_utf8(if bom { &bytes[3..] } else { &bytes })
            .context(t!("storage.text_only"))?;
        ensure!(!disk.contains('\0'), t!("storage.text_only"));
        let mut endings = disk
            .match_indices('\n')
            .map(|(index, _)| index > 0 && disk.as_bytes()[index - 1] == b'\r');
        let crlf = endings.next().unwrap_or(false);
        let mut normalized = disk.replace("\r\n", "\n");
        let has_crlf = normalized.len() != disk.len();
        let has_lone_cr = normalized.contains('\r');
        let mixed_newlines = has_lone_cr || (has_crlf && endings.any(|ending| ending != crlf));
        if has_lone_cr {
            normalized = normalized.replace('\r', "\n");
        }
        Ok(Self {
            text: normalized.clone(),
            saved_text: normalized,
            restored: false,
            conflict: false,
            mixed_newlines,
            missing: false,
            identity: None,
            bom,
            crlf,
            baseline,
            recovered_baseline: None,
        })
    }

    pub fn persist_draft(
        &self,
        repo: &Repository,
        path: &Path,
        store: &Store,
        text: &str,
        dirty: bool,
    ) -> Result<()> {
        store.write_draft(repo, path, self.edit_baseline(), text, dirty)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn has_bom(&self) -> bool {
        self.bom
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn line_ending_label(&self) -> &'static str {
        if self.crlf { "CRLF" } else { "LF" }
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn draft_baseline(&self) -> [u8; 32] {
        self.baseline
    }

    pub(crate) fn edit_baseline(&self) -> [u8; 32] {
        self.recovered_baseline.unwrap_or(self.baseline)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn matches_recovered_baseline(expected: [u8; 32], text: &str) -> bool {
        // A recovered draft only stores the original file hash. The current
        // disk version may use a different BOM or newline convention.
        let crlf = text.replace('\n', "\r\n");
        [text, crlf.as_str()].into_iter().any(|text| {
            digest(text.as_bytes()) == expected
                || digest(format!("\u{feff}{text}").as_bytes()) == expected
        })
    }

    pub(crate) fn encode_text(&self, text: &str) -> Vec<u8> {
        let mut encoded = Vec::new();
        if self.bom {
            encoded.extend_from_slice(b"\xef\xbb\xbf");
        }
        if self.crlf {
            encoded.extend_from_slice(text.replace('\n', "\r\n").as_bytes());
        } else {
            encoded.extend_from_slice(text.as_bytes());
        }
        encoded
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn retain_buffer_line_ending(&mut self, buffer: &Self) {
        if self.mixed_newlines {
            self.crlf = buffer.crlf;
        }
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn draft_format(&self) -> u8 {
        u8::from(self.bom) | (u8::from(self.crlf) << 1)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn renamed_sibling(
        &self,
        repo: Option<&Repository>,
        path: &Path,
    ) -> Option<PathBuf> {
        let identity = self.identity?;
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            let repo = repo.filter(|repo| repo.ssh.is_none())?;
            repo.root.join(path)
        };
        let parent = absolute.parent()?;
        let mut matches = fs::read_dir(parent).ok()?.filter_map(|entry| {
            let entry = entry.ok()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirEntryExt as _;
                if entry.ino() != identity.1 {
                    return None;
                }
            }
            let candidate = entry.path();
            (candidate != absolute
                && local_file_identity(&candidate) == Some(identity)
                && canonical_local_file(&candidate).ok().as_ref() == Some(&candidate))
            .then_some(candidate)
        });
        let target = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        if path.is_absolute() {
            Some(target)
        } else {
            Some(path.with_file_name(target.file_name()?))
        }
    }

    pub fn save(
        &mut self,
        repo: &Repository,
        path: &Path,
        store: &Store,
        text: &str,
    ) -> Result<()> {
        self.save_to(repo, path, text)?;
        store.discard_draft(repo, path)
    }

    pub(crate) fn save_to(&mut self, repo: &Repository, path: &Path, text: &str) -> Result<()> {
        self.save_target(Some(repo), path, text)
    }

    pub(crate) fn save_target(
        &mut self,
        repo: Option<&Repository>,
        path: &Path,
        text: &str,
    ) -> Result<()> {
        ensure!(!self.conflict, t!("storage.draft_conflict"));
        let encoded = self.encode_text(text);
        let new_baseline = digest(&encoded);
        let written = if path.is_absolute() {
            write_resolved(|| resolve_local_file(path), self.baseline, &encoded)
        } else {
            let repo = repo.context("A relative file path requires a repository")?;
            match &repo.ssh {
                Some(ssh) => ssh.call(Request::Save {
                    path: RepoPath::from_local(path)?,
                    baseline: self.baseline,
                    bytes: Bytes(encoded),
                }),
                None => write_checked(&repo.root, path, self.baseline, &encoded),
            }
        };
        self.finish_save(written, new_baseline, text, repo, path)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn recreate_target(
        &mut self,
        repo: Option<&Repository>,
        path: &Path,
        text: &str,
    ) -> Result<()> {
        let encoded = self.encode_text(text);
        let new_baseline = digest(&encoded);
        let written = if path.is_absolute() {
            create_document_at(path, &encoded)
        } else {
            let repo = repo.context("A relative file path requires a repository")?;
            if let Some(ssh) = &repo.ssh {
                let version = crate::remote::RECREATE_DOCUMENT_VERSION;
                ensure!(
                    ssh.call::<u32>(Request::Hello(version))
                        .is_ok_and(|reply| reply == version),
                    t!("workspace.tree_requires_helper")
                );
                ssh.call(Request::RecreateDocument {
                    path: RepoPath::from_local(path)?,
                    bytes: Bytes(encoded),
                })
            } else {
                recreate_document(&repo.root, path, &encoded)
            }
        };
        self.finish_save(written, new_baseline, text, repo, path)
    }

    fn finish_save(
        &mut self,
        written: Result<bool>,
        baseline: [u8; 32],
        text: &str,
        repo: Option<&Repository>,
        path: &Path,
    ) -> Result<()> {
        match written {
            Ok(true) => {}
            Ok(false) => {
                self.conflict = true;
                bail!(t!("storage.changed_on_disk"));
            }
            Err(error) => return Err(error),
        }
        self.saved_text = text.to_owned();
        self.baseline = baseline;
        self.conflict = false;
        self.mixed_newlines = false;
        self.missing = false;
        self.identity = if path.is_absolute() {
            local_file_identity(path)
        } else {
            repo.filter(|repo| repo.ssh.is_none())
                .and_then(|repo| local_file_identity(&repo.root.join(path)))
        };
        self.restored = false;
        self.recovered_baseline = None;
        Ok(())
    }
}

pub(crate) fn recreate_document(root: &Path, path: &Path, bytes: &[u8]) -> Result<bool> {
    ensure!(
        root.canonicalize()? == root
            && !path.as_os_str().is_empty()
            && path
                .components()
                .all(|part| matches!(part, Component::Normal(name) if name != ".git")),
        t!("common.path_outside_repo")
    );
    create_document_at(&root.join(path), bytes)
}

fn create_document_at(target: &Path, bytes: &[u8]) -> Result<bool> {
    ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        t!("storage.file_limit")
    );
    ensure!(
        target.is_absolute() && target.components().all(|part| part.as_os_str() != ".git"),
        t!("storage.local_regular_file")
    );
    let parent = target.parent().context(t!("storage.no_parent"))?;
    ensure_document_parent(parent)?;
    let mut staged = NamedTempFile::new_in(parent)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    ensure!(
        parent.canonicalize()? == parent,
        t!("storage.local_regular_file")
    );
    match staged.persist_noclobber(target) {
        Ok(_) => Ok(true),
        Err(error) if error.error.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.error.into()),
    }
}

fn ensure_document_parent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            ensure_document_parent(path.parent().context(t!("storage.no_parent"))?)?;
            match fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(error) => return Err(error.into()),
    }
    ensure!(
        path.canonicalize()? == path && path.is_dir(),
        t!("storage.local_regular_file")
    );
    Ok(())
}

pub(crate) fn write_checked(
    root: &Path,
    path: &Path,
    expected: [u8; 32],
    bytes: &[u8],
) -> Result<bool> {
    write_resolved(|| resolve_file(root, path), expected, bytes)
}

fn write_resolved(
    resolve: impl Fn() -> Result<PathBuf>,
    expected: [u8; 32],
    bytes: &[u8],
) -> Result<bool> {
    ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        t!("storage.file_limit")
    );
    let target = resolve()?;
    let current = digest(&read_bytes(&target)?);
    if current == digest(bytes) {
        return Ok(true);
    }
    if current != expected {
        return Ok(false);
    }
    let mut temp = NamedTempFile::new_in(target.parent().context(t!("storage.no_parent"))?)?;
    temp.as_file()
        .set_permissions(fs::metadata(&target)?.permissions())?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    if digest(&read_bytes(&resolve()?)?) != expected {
        return Ok(false);
    }
    temp.persist(&target)?;
    Ok(true)
}

fn resolve_asset(root: &Path, path: &Path) -> Result<PathBuf> {
    ensure!(
        path.components()
            .all(|part| matches!(part, Component::Normal(name) if name != ".git")),
        t!("storage.invalid_image_path")
    );
    let target = root.join(path).canonicalize()?;
    ensure!(
        target.is_file()
            && target
                .strip_prefix(root)?
                .components()
                .all(|part| part.as_os_str() != ".git"),
        t!("storage.image_outside_repo")
    );
    Ok(target)
}

fn read_asset(root: &Path, path: &Path) -> Result<Vec<u8>> {
    let target = resolve_asset(root, path)?;
    let limit = MAX_ASSET_BYTES;
    let mut bytes = Vec::new();
    File::open(target)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, t!("storage.image_limit"));
    Ok(bytes)
}

fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    read_document_bytes(File::open(path)?)
}

fn read_document_bytes(file: File) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        t!("storage.legacy_file_limit")
    );
    Ok(bytes)
}

pub(crate) fn local_file_identity(path: &Path) -> Option<LocalFileIdentity> {
    file_identity(&path.symlink_metadata().ok()?)
}

fn file_identity(metadata: &fs::Metadata) -> Option<LocalFileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        (metadata.is_file() && metadata.nlink() == 1)
            .then(|| (metadata.dev(), metadata.ino(), metadata.created().ok()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Remembers window geometry per top-level window mode.
#[cfg(feature = "desktop")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WindowSlot {
    Document,
    Editor,
    Repository,
}

/// Persisted window placement in logical pixels.
#[cfg(feature = "desktop")]
#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
pub(crate) struct WindowGeometry {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
}

pub struct Store {
    root: PathBuf,
    #[cfg(feature = "desktop")]
    ai: std::sync::OnceLock<crate::ai::Service>,
    state: std::sync::Mutex<Option<rusqlite::Connection>>,
}

pub(crate) struct Draft {
    baseline: [u8; 32],
    text: String,
    format: u8,
}

#[derive(Deserialize, Serialize)]
pub struct Location {
    pub root: PathBuf,
    pub file: Option<PathBuf>,
}

impl Store {
    #[cfg(feature = "desktop")]
    pub(crate) fn ai(&self, config: &crate::config::ConfigFile) -> crate::ai::Service {
        self.ai
            .get_or_init(|| {
                crate::ai::Service::new(
                    config.current().ai.clone(),
                    self.root.join("translations.sqlite3"),
                    config.path().ok(),
                )
            })
            .clone()
    }

    pub fn from_environment() -> Result<Self> {
        let root = env::var_os("STROLL_GPUI_DATA")
            .map(PathBuf::from)
            .or_else(|| env::var_os("XDG_STATE_HOME").map(|p| PathBuf::from(p).join("stroll")))
            .or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .or_else(dirs::home_dir)
                    .map(|home| home.join(".local/state/stroll"))
            })
            .context(t!("storage.no_state_directory"))?;
        Self::new(root)
    }

    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            #[cfg(feature = "desktop")]
            ai: Default::default(),
            state: Default::default(),
        })
    }

    fn draft(&self, repo: &Repository, path: &Path) -> Result<Option<Draft>> {
        self.editor_draft(Some(repo), path)
    }

    pub fn tabs(&self, repo: &Repository) -> Result<TabSession> {
        Ok(self.session(repo)?.tabs)
    }

    pub(crate) fn session(&self, repo: &Repository) -> Result<WorktreeSession> {
        let mut session = self.read_worktree_session(repo)?;
        self.restore_session_tabs(repo, &mut session)?;
        Ok(session)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn session_for_open(
        &self,
        repo: &Repository,
    ) -> Result<(WorktreeSession, WorktreeSession)> {
        let baseline = self.read_worktree_session(repo)?;
        let mut session = baseline.clone();
        self.restore_session_tabs(repo, &mut session)?;
        Ok((baseline, session))
    }

    pub(crate) fn restore_session_tabs(
        &self,
        repo: &Repository,
        session: &mut WorktreeSession,
    ) -> Result<()> {
        let remote_files = if let Some(ssh) = &repo.ssh
            && repo.lazy_tree
        {
            let paths: Vec<_> = session
                .tabs
                .paths()
                .iter()
                .filter(|path| !path.is_absolute())
                .collect();
            let openable = if paths.is_empty() {
                Vec::new()
            } else {
                let request = paths
                    .iter()
                    .map(|path| path.to_str().map(str::to_owned))
                    .collect();
                ssh.call::<Vec<bool>>(Request::Openable(request))?
            };
            Some(
                paths
                    .into_iter()
                    .zip(openable)
                    .filter_map(|(path, exists)| exists.then(|| path.clone()))
                    .collect::<BTreeSet<_>>(),
            )
        } else {
            None
        };
        session.tabs.restore(|path| {
            if let Some(files) = &remote_files
                && !path.is_absolute()
            {
                files.contains(path) || self.has_draft(repo, path)
            } else {
                self.session_file_exists(repo, path)
            }
        });
        session
            .editor_previews
            .retain(|path, _| session.tabs.index(path).is_some());
        Ok(())
    }

    pub(crate) fn session_file_exists(&self, repo: &Repository, path: &Path) -> bool {
        if path.is_absolute() {
            resolve_local_file(path).is_ok() || self.has_draft(repo, path)
        } else if repo.ssh.is_some() {
            repo.children
                .get(path.parent().unwrap_or(Path::new("")))
                .is_some_and(|entries| {
                    entries
                        .iter()
                        .any(|entry| !entry.directory && entry.path == path)
                })
                || self.has_draft(repo, path)
        } else {
            repo.exists(path) || self.has_draft(repo, path)
        }
    }

    pub(crate) fn write_draft(
        &self,
        repo: &Repository,
        path: &Path,
        baseline: [u8; 32],
        text: &str,
        dirty: bool,
    ) -> Result<()> {
        self.update_editor_draft(Some(repo), path, Some(baseline), dirty.then_some(text))
    }

    pub(crate) fn has_draft(&self, repo: &Repository, path: &Path) -> bool {
        self.draft(repo, path).is_ok_and(|draft| draft.is_some())
    }

    pub(crate) fn discard_draft(&self, repo: &Repository, path: &Path) -> Result<()> {
        self.update_editor_draft(Some(repo), path, None, None)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn discard_local_editor_draft(&self, path: &Path) -> Result<()> {
        self.update_editor_draft(None, path, None, None)
    }
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context(t!("storage.missing_state_directory"))?;
    fs::create_dir_all(parent)?;
    let mut temp = NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}

#[cfg(test)]
#[test]
fn document_newlines_normalize_without_losing_the_disk_baseline() -> Result<()> {
    for (raw, expected, encoded, mixed) in [
        ("", "", "", false),
        ("one\ntwo", "one\ntwo", "one\ntwo", false),
        ("one\r\ntwo", "one\ntwo", "one\r\ntwo", false),
        ("one\ntwo\r\n", "one\ntwo\n", "one\ntwo\n", true),
        (
            "\u{feff}一\r\ntwo\n",
            "一\ntwo\n",
            "\u{feff}一\r\ntwo\r\n",
            true,
        ),
        (
            "\rfirst\r\nlast\r",
            "\nfirst\nlast\n",
            "\r\nfirst\r\nlast\r\n",
            true,
        ),
        ("one\rtwo\r", "one\ntwo\n", "one\ntwo\n", true),
    ] {
        let bytes = raw.as_bytes();
        let document = Document::decode(bytes.to_vec())?;
        assert_eq!(document.saved_text, expected, "{raw:?}");
        assert_eq!(document.text, expected, "{raw:?}");
        assert_eq!(document.mixed_newlines, mixed, "{raw:?}");
        assert_eq!(document.edit_baseline(), digest(bytes), "{raw:?}");
        assert_eq!(
            document.encode_text(&document.text),
            encoded.as_bytes(),
            "{raw:?}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod document_recreation_tests {
    use super::*;

    #[test]
    fn recreation_creates_missing_parents_and_reserves_the_name_without_clobbering() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().canonicalize()?;
        let path = Path::new("deleted/directory/file.txt");
        assert!(recreate_document(&root, path, b"first\n")?);
        assert!(!recreate_document(&root, path, b"other\n")?);
        assert_eq!(fs::read(root.join(path))?, b"first\n");
        assert!(recreate_document(&root, Path::new("../outside"), b"no").is_err());
        assert!(recreate_document(&root, Path::new(".git/new-file"), b"no").is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recreation_rejects_symlinked_parents_and_preserves_dangling_symlinks() -> Result<()> {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir()?;
        let root = fixture.path().canonicalize()?;
        fs::create_dir(root.join("real"))?;
        symlink(root.join("real"), root.join("linked"))?;
        symlink(root.join("absent"), root.join("target.txt"))?;
        assert!(recreate_document(&root, Path::new("linked/file.txt"), b"no").is_err());
        assert!(!recreate_document(&root, Path::new("target.txt"), b"no")?);
        assert!(!root.join("real/file.txt").exists() && !root.join("absent").exists());
        assert!(root.join("target.txt").is_symlink());
        Ok(())
    }
}
