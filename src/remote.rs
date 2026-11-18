use rust_i18n::t;

mod client;
mod server;

#[cfg(feature = "desktop")]
pub(crate) use client::GitHelper;
#[cfg(feature = "desktop")]
pub(crate) use client::PendingCall;
pub(crate) use client::SshSession;
#[cfg(feature = "desktop")]
use client::helper_command;
pub use server::{serve, serve_lsp};

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{git, pty};

const VERSION: u32 = 1;
pub(crate) const BROWSE_VERSION: u32 = 2;
pub(crate) const GIT_EDIT_VERSION: u32 = 3;
pub(crate) const LOCATOR_VERSION: u32 = 4;
/// Batch patch request (`FileChangesBatch`). Clients fall back to per-file requests with older helpers.
pub(crate) const BATCH_VERSION: u32 = 5;
/// File metadata and line statistics can arrive separately.
pub(crate) const REVIEW_VERSION: u32 = 6;
pub(crate) const WATCH_VERSION: u32 = 7;
pub(crate) const WORKSPACE_FILES_VERSION: u32 = 8;
pub(crate) const LSP_EDITS_VERSION: u32 = 9;
pub(crate) const GIT_OPERATIONS_VERSION: u32 = 10;
pub(crate) const GIT_HISTORY_VERSION: u32 = 11;
pub(crate) const BLAME_VERSION: u32 = 12;
pub(crate) const RESTORE_HISTORY_TEXT_VERSION: u32 = 13;
pub(crate) const AI_CONTEXT_VERSION: u32 = 14;
pub(crate) const EXTERNAL_LSP_VERSION: u32 = 15;
pub(crate) const LAZY_TREE_VERSION: u32 = 16;
pub(crate) const DOCUMENT_PATHS_VERSION: u32 = 17;
pub(crate) const DOCUMENT_BATCH_VERSION: u32 = 18;
pub(crate) const REVISION_COMPARISON_VERSION: u32 = 19;
pub(crate) const WORKSPACE_TRANSFER_VERSION: u32 = 20;
pub(crate) const WORKTREE_LSP_VERSION: u32 = 21;
const MAX_FRAME: usize = 40 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SshTarget {
    pub host: String,
    pub port: Option<u16>,
    pub path: PathBuf,
}

impl SshTarget {
    pub fn parse(path: &Path) -> Result<Option<Self>> {
        let Some(value) = path.to_str().and_then(|path| path.strip_prefix("ssh://")) else {
            return Ok(None);
        };
        let (authority, root) = value.split_once('/').unwrap_or((value, ""));
        let (user, address) = authority
            .rsplit_once('@')
            .map_or((None, authority), |(user, address)| (Some(user), address));
        let (address, port) = if let Some(address) = address.strip_prefix('[') {
            let (address, suffix) = address
                .split_once(']')
                .ok_or_else(|| anyhow::anyhow!(t!("ssh.invalid_ipv6")))?;
            ensure!(
                address.parse::<std::net::Ipv6Addr>().is_ok(),
                t!("ssh.invalid_ipv6")
            );
            let port = if suffix.is_empty() {
                None
            } else {
                Some(
                    suffix
                        .strip_prefix(':')
                        .ok_or_else(|| anyhow::anyhow!(t!("ssh.invalid_port")))?,
                )
            };
            (address, port)
        } else {
            address
                .rsplit_once(':')
                .map_or((address, None), |(host, port)| (host, Some(port)))
        };
        let port = port.map(str::parse::<u16>).transpose()?;
        ensure!(port != Some(0), t!("ssh.invalid_port"));
        ensure!(
            !address.is_empty() && !address.starts_with('-'),
            t!("ssh.invalid_host")
        );
        ensure!(
            user.is_none_or(|user| !user.is_empty() && !user.contains('@')),
            t!("ssh.invalid_user")
        );
        let host = user.map_or_else(|| address.to_owned(), |user| format!("{user}@{address}"));
        ensure!(
            !host.is_empty()
                && !host.starts_with('-')
                && host
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-@[]:".contains(&byte)),
            t!("ssh.invalid_host_alias")
        );
        ensure!(
            !root.is_empty() && !root.contains('\0'),
            t!("ssh.absolute_repo_path")
        );
        Ok(Some(Self {
            host,
            port,
            path: PathBuf::from(format!("/{root}")),
        }))
    }

    pub fn authority(&self) -> String {
        let (user, address) = self
            .host
            .rsplit_once('@')
            .map_or((None, self.host.as_str()), |(user, address)| {
                (Some(user), address)
            });
        let mut authority = user.map_or_else(String::new, |user| format!("{user}@"));
        if address.contains(':') {
            authority.push_str(&format!("[{address}]"));
        } else {
            authority.push_str(address);
        }
        if let Some(port) = self.port {
            authority.push_str(&format!(":{port}"));
        }
        authority
    }

    pub fn locator(&self, path: &Path) -> PathBuf {
        PathBuf::from(format!("ssh://{}{}", self.authority(), path.display()))
    }
}

/// A UTF-8 path relative to the remote Git worktree, using POSIX separators on the wire.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct RepoPath(String);

impl RepoPath {
    pub fn from_local(path: &Path) -> Result<Self> {
        let mut parts = Vec::new();
        for component in path.components() {
            let std::path::Component::Normal(name) = component else {
                anyhow::bail!(t!("common.path_outside_repo"));
            };
            let name = name
                .to_str()
                .ok_or_else(|| anyhow::anyhow!(t!("ssh.path_encoding")))?;
            ensure!(
                name != ".git" && !name.contains(['\\', '/', '\0']),
                t!("ssh.invalid_repo_path")
            );
            parts.push(name);
        }
        ensure!(!parts.is_empty(), t!("ssh.repo_path_required"));
        Ok(Self(parts.join("/")))
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    #[cfg(feature = "desktop")]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Bytes(#[serde(with = "base64_bytes")] pub Vec<u8>);

mod base64_bytes {
    use super::*;

    pub fn serialize<S: serde::Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        STANDARD
            .decode(String::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Request {
    Hello(u32),
    Browse {
        path: String,
        base: Option<String>,
    },
    Complete {
        path: String,
        base: Option<String>,
    },
    Open(PathBuf),
    OpenWorkspace(PathBuf),
    /// Bind only the worktree root for the local Git helper, without constructing a `Repository` file tree.
    Bind(PathBuf),
    Worktrees,
    Exists(RepoPath),
    Openable(Vec<Option<String>>),
    Read(RepoPath, bool),
    Locator(crate::locator::TextQuery),
    LocatorPaths,
    Save {
        path: RepoPath,
        baseline: [u8; 32],
        bytes: Bytes,
    },
    WatchRepository,
    TreeSnapshot,
    DocumentPaths,
    FastDocumentPaths,
    DocumentBatch(u64),
    TreeBranch,
    TreeDirectories(Vec<PathBuf>),
    WorktreeStatus,
    ReadForSync(RepoPath),
    EntryVersion(RepoPath),
    FileOperation(RepoPath, crate::repository::files::FileOperation),
    ReadCopyChunk(RepoPath, crate::repository::files::EntryVersion, u64),
    History,
    Commits(String, String, usize),
    Commit(String),
    SearchRevisions(String),
    Changes(git::Comparison),
    ReviewFiles(git::Comparison),
    ReviewStats(git::Comparison, String),
    Patch(git::Comparison, String, usize, bool),
    Snapshot(git::Comparison, String, git::Side),
    Stage(git::WorktreeStatus, Vec<String>, bool),
    FileChanges(String, git::FileDiffOptions),
    FileChangesBatch(Vec<(String, git::FileDiffOptions)>),
    FileVersion(String),
    BlameBaseline(String, String),
    Blame(String, String, usize, usize),
    FileVersions(Vec<String>),
    LayerSnapshot(String, git::Layer, git::Side),
    EditFile(git::FileEdit),
    CommitState,
    StagedCommitContext(git::CommitState),
    AllowsHoverTranslation(RepoPath),
    CreateCommit(git::CommitState, String),
    StageOperation(git::WorktreeStatus, Vec<String>, bool),
    EditOperation(git::FileEdit),
    OperationHistory,
    UndoOperation(u64),
    StageWithHistory(git::WorktreeStatus, Vec<String>, bool),
    EditWithHistory(git::FileEdit),
    ReplayGitHistory(u64, bool),
    RestoreHistoryText(u64, bool),
    TerminalOpen(pty::PtySize),
    TerminalInput(u64, Bytes),
    TerminalResize(u64, pty::PtySize),
    TerminalPoll(u64),
    TerminalClose(u64),
    LspRoot(crate::language::LanguageServer, RepoPath),
    ExternalLspRoot(String, RepoPath),
    WorktreeLspConfig,
    WorktreeLspRoot(crate::language::worktree::ProjectRoots, RepoPath),
    LspEditTime,
    LspEditTarget(RepoPath, std::time::SystemTime),
}

#[derive(Serialize, Deserialize)]
pub(crate) enum TerminalEvent {
    Output(Bytes),
    Exit(u32),
    Error(String),
}

#[derive(Serialize, Deserialize)]
struct Call {
    id: u64,
    request: Request,
}

#[derive(Serialize, Deserialize)]
struct Reply {
    id: u64,
    result: Result<Value, String>,
}

fn read_frame<T: DeserializeOwned>(input: &mut impl Read) -> Result<Option<T>> {
    let mut length = [0; 4];
    if input.read(&mut length[..1])? == 0 {
        return Ok(None);
    }
    input.read_exact(&mut length[1..])?;
    let length = u32::from_be_bytes(length) as usize;
    ensure!(length <= MAX_FRAME, t!("ssh.response_limit"));
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn write_frame(output: &mut impl Write, frame: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(frame)?;
    ensure!(bytes.len() <= MAX_FRAME, t!("ssh.message_limit"));
    output.write_all(&(bytes.len() as u32).to_be_bytes())?;
    output.write_all(&bytes)?;
    output.flush()?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum LspTarget {
    Builtin(crate::language::LanguageServer),
    Worktree {
        id: String,
        fingerprint: String,
        snapshot: PathBuf,
    },
    External {
        id: String,
    },
}

#[derive(Serialize, Deserialize)]
pub(crate) struct LspLaunch {
    pub repository: PathBuf,
    pub project: PathBuf,
    pub server: LspTarget,
}

#[cfg(feature = "desktop")]
impl SshSession {
    pub(crate) fn language_command(
        &self,
        repository: PathBuf,
        project: PathBuf,
        server: LspTarget,
    ) -> Result<std::process::Command> {
        let options = LspLaunch {
            repository,
            project,
            server,
        };
        let encoded =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&options)?);
        Ok(helper_command(self.target(), &format!("--lsp {encoded}")))
    }
}

#[cfg(feature = "desktop")]
pub(crate) fn file_uri(path: &str) -> Result<String> {
    ensure!(path.starts_with('/'), "Invalid absolute remote path");
    let mut uri = url::Url::parse("file:///")?;
    uri.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Invalid file URI"))?
        .clear()
        .extend(path[1..].split('/'));
    Ok(uri.into())
}
