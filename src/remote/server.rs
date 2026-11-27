use std::{
    collections::HashMap,
    io::{self, BufReader, BufWriter},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, to_value};

use super::{Bytes, Call, Reply, Request, TerminalEvent, VERSION, read_frame, write_frame};
use crate::{
    git,
    language::{
        LanguageServer,
        tools::Tools,
        worktree::{WorktreeLsp, WorktreeServer},
    },
    locator::{self, PathCache, Results, TextHit, TextSearch},
    pty,
    repository::{Repository, RepositoryInfo, files::WorkspaceImport, write_checked},
};

#[derive(Default)]
struct Server {
    repository: RwLock<Option<Arc<Repository>>>,
    mutations: Mutex<()>,
    imports: Mutex<HashMap<u64, WorkspaceImport>>,
    next_import: AtomicU64,
    documents: Mutex<Option<(u64, crate::repository::files::DocumentScan)>>,
    locator_paths: PathCache,
    locator: Mutex<ActiveSearch>,
    watch: Mutex<Option<git::watch::RepoWatcher>>,
    watch_replies: Option<mpsc::SyncSender<Reply>>,
    review_commit: Mutex<Option<Arc<git::Changes>>>,
    terminals: Mutex<HashMap<u64, Terminal>>,
    next_terminal: AtomicU64,
    lsp_snapshot: Mutex<Option<LspSnapshot>>,
    #[cfg(target_os = "linux")]
    lsp_memory: Mutex<HashMap<u64, tempfile::TempDir>>,
    #[cfg(target_os = "linux")]
    next_lsp_memory: AtomicU64,
}

#[derive(Default)]
struct ActiveSearch {
    id: u64,
    cancelled: bool,
    continuation: Option<TextSearch>,
}

#[cfg(target_os = "linux")]
const LSP_MEMORY_PREFIX: &str = "stroll-lsp-memory-";

#[cfg(target_os = "linux")]
fn publish_lsp_process(directory: &Path) -> Result<()> {
    use std::{io::Write as _, os::unix::fs::MetadataExt as _};

    let metadata = directory.symlink_metadata()?;
    // SAFETY: geteuid only reads the effective user ID.
    let uid = unsafe { libc::geteuid() };
    ensure!(
        directory.parent() == Some(std::env::temp_dir().canonicalize()?.as_path())
            && directory
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(LSP_MEMORY_PREFIX))
            && metadata.is_dir()
            && metadata.uid() == uid
            && metadata.mode() & 0o077 == 0,
        "Invalid language server memory registration"
    );
    let process = crate::process_memory::ProcessIdentity::current()
        .context("The language server process could not be identified")?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(&serde_json::to_vec(&process)?)?;
    file.persist_noclobber(directory.join("pid"))?;
    Ok(())
}

#[derive(Deserialize, Serialize)]
struct LspSnapshotFile {
    root: PathBuf,
    config: WorktreeLsp,
}

struct LspSnapshot {
    path: tempfile::TempPath,
    config: WorktreeLsp,
}

impl LspSnapshot {
    fn publish(
        current: &mut Option<Self>,
        root: &Path,
        config: WorktreeLsp,
    ) -> Result<WorktreeLsp> {
        if current
            .as_ref()
            .is_none_or(|snapshot| snapshot.config != config)
        {
            let bytes = serde_json::to_vec(&LspSnapshotFile {
                root: root.to_owned(),
                config: config.clone(),
            })?;
            // Reuse the same private pathname across saves. A launch can only
            // use a configuration the bound helper has already validated.
            if let Some(snapshot) = current.as_mut() {
                crate::repository::atomic_write(&snapshot.path, &bytes)?;
                snapshot.config = config;
            } else {
                use std::io::Write as _;
                let directory = std::env::temp_dir().canonicalize()?;
                let mut file = tempfile::Builder::new()
                    .prefix("stroll-lsp-")
                    .tempfile_in(directory)?;
                file.write_all(&bytes)?;
                *current = Some(Self {
                    path: file.into_temp_path(),
                    config,
                });
            }
        }
        let snapshot = current
            .as_ref()
            .expect("published language server configuration");
        Ok(snapshot.config.clone().for_remote(&snapshot.path))
    }

    fn read(path: &Path) -> Result<LspSnapshotFile> {
        ensure!(
            path.parent() == Some(std::env::temp_dir().canonicalize()?.as_path())
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("stroll-lsp-")),
            "Invalid worktree language server snapshot"
        );
        let mut options = std::fs::File::options();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options
            .open(path)
            .context("Could not open the worktree language server snapshot")?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() <= 2 * 1024 * 1024,
            "Invalid worktree language server snapshot"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            // SAFETY: geteuid reads the current process identity and takes no arguments.
            let uid = unsafe { libc::geteuid() };
            ensure!(
                metadata.uid() == uid && metadata.mode() & 0o077 == 0,
                "Invalid worktree language server snapshot permissions"
            );
        }
        Ok(serde_json::from_reader(file)?)
    }
}

struct Terminal {
    process: pty::Process,
    commands: mpsc::Sender<pty::Command>,
    events: async_channel::Receiver<pty::Event>,
    directory: Option<TerminalDirectory>,
}

struct TerminalDirectory {
    next_sample: Instant,
    last: Option<PathBuf>,
}

impl Server {
    fn begin_search(&self, id: u64) -> Result<()> {
        let mut current = self.locator.lock().unwrap();
        ensure!(id > current.id, "The workspace search was superseded");
        *current = ActiveSearch {
            id,
            ..Default::default()
        };
        Ok(())
    }

    fn search_cancelled(&self, id: u64) -> bool {
        let current = self.locator.lock().unwrap();
        current.id != id || current.cancelled
    }

    fn text_page(
        &self,
        repo: &Repository,
        id: u64,
        mut search: TextSearch,
    ) -> Result<Results<TextHit>> {
        let result = search.next_page(repo, || self.search_cancelled(id))?;
        let mut current = self.locator.lock().unwrap();
        if result.truncated && current.id == id && !current.cancelled {
            current.continuation = Some(search);
        }
        Ok(result)
    }

    fn request(&self, request: Request) -> Result<Value> {
        match request {
            Request::Hello(version) => {
                if cfg!(target_os = "linux") && version == super::LSP_MEMORY_VERSION {
                    return value(version);
                }
                ensure!(
                    matches!(
                        version,
                        VERSION
                            | super::BROWSE_VERSION
                            | super::GIT_EDIT_VERSION
                            | super::LOCATOR_VERSION
                            | super::BATCH_VERSION
                            | super::REVIEW_VERSION
                            | super::WATCH_VERSION
                            | super::WORKSPACE_FILES_VERSION
                            | super::LSP_EDITS_VERSION
                            | super::GIT_OPERATIONS_VERSION
                            | super::GIT_HISTORY_VERSION
                            | super::BLAME_VERSION
                            | super::RESTORE_HISTORY_TEXT_VERSION
                            | super::AI_CONTEXT_VERSION
                            | super::EXTERNAL_LSP_VERSION
                            | super::LAZY_TREE_VERSION
                            | super::DOCUMENT_PATHS_VERSION
                            | super::DOCUMENT_BATCH_VERSION
                            | super::REVISION_COMPARISON_VERSION
                            | super::WORKSPACE_TRANSFER_VERSION
                            | super::WORKTREE_LSP_VERSION
                            | super::IMAGE_DIFF_VERSION
                            | super::IMAGE_DIFF_RESOURCE_VERSION
                            | super::HISTORY_EDIT_VERSION
                            | super::TERMINAL_DIRECTORY_VERSION
                            | super::WORKSPACE_IMPORT_VERSION
                            | super::WORKTREE_IDENTITY_VERSION
                            | super::WORKSPACE_LOCATOR_VERSION
                            | super::LOCATOR_GLOB_VERSION
                            | super::GIT_MANAGEMENT_VERSION
                            | super::WORKTREE_REMOVAL_VERSION
                            | super::PUSH_REVIEW_VERSION
                            | super::PUSH_BRANCHES_VERSION
                            | super::WORKTREE_REMOVAL_PREVIEW_VERSION
                    ),
                    "SSH workspace protocol version mismatch. Update the remote helper."
                );
                return value(version);
            }
            Request::LocateWorktree {
                ref root,
                ref common,
                ref id,
            } => {
                return value(crate::repository::worktree_identity::locate(
                    root, common, id,
                )?);
            }
            Request::Browse { path, base } => {
                let directory = browse_path(&path, base.as_deref())?;
                return value(crate::repository::DirectoryListing::read(
                    directory.canonicalize()?,
                )?);
            }
            Request::Complete { path, base } => {
                let (parent, prefix) =
                    path.rsplit_once('/')
                        .map_or(("", path.as_str()), |(parent, prefix)| {
                            if parent.is_empty() {
                                ("/", prefix)
                            } else {
                                (parent, prefix)
                            }
                        });
                let parent = browse_path(parent, base.as_deref())?;
                return value(crate::repository::DirectoryEntry::complete(
                    &parent, prefix,
                )?);
            }
            Request::WatchRepository => {
                let repo = self
                    .repository
                    .read()
                    .unwrap()
                    .clone()
                    .context("Open a repository first")?;
                let mut watcher = self.watch.lock().unwrap();
                if watcher.is_none() {
                    let replies = self
                        .watch_replies
                        .clone()
                        .context("Repository watch is unavailable")?;
                    let watch = git::watch::RepoWatcher::open(&repo.root, &repo.common_dir)?;
                    let events = watch.events.clone();
                    let root = repo.root.clone();
                    thread::spawn(move || {
                        while events.wake.recv_blocking().is_ok() {
                            thread::sleep(git::watch::DEBOUNCE);
                            let mut changes = events.take();
                            changes.classify_ignored_paths(&root);
                            if !changes.is_empty()
                                && replies
                                    .send(Reply {
                                        id: 0,
                                        result: value(changes).map_err(|error| error.to_string()),
                                    })
                                    .is_err()
                            {
                                return;
                            }
                        }
                        _ = replies.send(Reply {
                            id: 0,
                            result: Err("Repository watch ended".into()),
                        });
                    });
                    *watcher = Some(watch);
                }
                return value(());
            }
            Request::Open(ref path) | Request::OpenWorkspace(ref path) => {
                let mut current = self.repository.write().unwrap();
                ensure!(
                    current.is_none(),
                    "This SSH connection is already bound to a workspace"
                );
                let mut repo = Repository::open_root(crate::repository::repository_root(path)?)?;
                if matches!(request, Request::Open(_)) {
                    // Older clients need a complete tree in the open response.
                    repo.children = crate::repository::files::scan_tree(&repo.root)?;
                }
                let info = RepositoryInfo::from(repo.clone());
                *current = Some(Arc::new(repo));
                return value(info);
            }
            _ => {}
        }
        let repo = self
            .repository
            .read()
            .unwrap()
            .clone()
            .context("Open a repository first")?;
        let root = &repo.root;
        let session = git::session_for(root);
        match request {
            Request::Hello(_)
            | Request::WatchRepository
            | Request::Open(_)
            | Request::OpenWorkspace(_)
            | Request::Browse { .. }
            | Request::Complete { .. }
            | Request::LocateWorktree { .. } => unreachable!(),
            Request::TreeDirectories(paths) => {
                value(crate::repository::files::scan_directories(root, paths)?)
            }
            Request::FastDocumentPaths => {
                value(crate::repository::files::fast_document_paths(root)?)
            }
            Request::DocumentBatch(generation) => {
                let mut scan = self.documents.lock().unwrap();
                if scan
                    .as_ref()
                    .is_none_or(|(current, _)| *current < generation)
                {
                    *scan = Some((generation, Default::default()));
                }
                let (active, cursor) = scan.as_mut().unwrap();
                ensure!(*active == generation, "The document scan was superseded");
                value(cursor.next(root)?)
            }
            Request::DocumentPaths => value(crate::repository::files::scan_document_paths(root)?),
            Request::TreeBranch => value(crate::repository::files::branch(root)),
            Request::TreeSnapshot => value((
                crate::repository::files::scan_tree(root)?,
                crate::repository::files::branch(root),
            )),
            Request::WorktreeStatus => value(session.call(|state| Ok(state.status()?.clone()))?),
            Request::LspEditTime => value(std::time::SystemTime::now()),
            Request::LspEditTarget(path, before) => value(
                crate::repository::files::lsp_edit_target(root, path.as_path(), before)?,
            ),
            #[cfg(target_os = "linux")]
            Request::RegisterLspMemory => {
                use std::os::unix::fs::PermissionsExt as _;

                let mut processes = self.lsp_memory.lock().unwrap();
                ensure!(
                    processes.len() < 128,
                    "Too many language server memory registrations"
                );
                let directory = tempfile::Builder::new()
                    .prefix(LSP_MEMORY_PREFIX)
                    .permissions(std::fs::Permissions::from_mode(0o700))
                    .tempdir_in(std::env::temp_dir().canonicalize()?)?;
                let id = self.next_lsp_memory.fetch_add(1, Ordering::Relaxed);
                let path = directory.path().to_owned();
                processes.insert(id, directory);
                value((id, path))
            }
            #[cfg(target_os = "linux")]
            Request::LspMemory(id) => {
                use std::{io::Read as _, os::unix::fs::OpenOptionsExt as _};

                let processes = self.lsp_memory.lock().unwrap();
                let memory = processes.get(&id).and_then(|directory| {
                    let mut file = std::fs::File::options()
                        .read(true)
                        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                        .open(directory.path().join("pid"))
                        .ok()?
                        .take(256);
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes).ok()?;
                    let process: crate::process_memory::ProcessIdentity =
                        serde_json::from_slice(&bytes).ok()?;
                    process.sample()
                });
                value(memory)
            }
            #[cfg(target_os = "linux")]
            Request::ReleaseLspMemory(id) => {
                self.lsp_memory.lock().unwrap().remove(&id);
                value(())
            }
            #[cfg(not(target_os = "linux"))]
            Request::RegisterLspMemory | Request::LspMemory(_) | Request::ReleaseLspMemory(_) => {
                anyhow::bail!("Language server memory sampling is unavailable")
            }
            Request::EntryVersion(path) => value(repo.entry_version(path.as_path())?),
            Request::FileOperation(path, operation) => {
                let _guard = self.mutations.lock().unwrap();
                repo.operate(path.as_path(), operation)?;
                session.invalidate();
                value(())
            }
            Request::ReadCopyChunk(path, version, offset) => value(Bytes(repo.read_copy_chunk(
                path.as_path(),
                &version,
                offset,
            )?)),
            Request::BeginImport(destination) => {
                let mut imports = self.imports.lock().unwrap();
                ensure!(imports.len() < 8, "Too many active workspace imports");
                let upload = WorkspaceImport::new(root, destination)?;
                let id = self.next_import.fetch_add(1, Ordering::Relaxed);
                imports.insert(id, upload);
                value(id)
            }
            Request::ImportEntry(id, path, entry) => {
                let imports = self.imports.lock().unwrap();
                let upload = imports.get(&id).context("The workspace import is closed")?;
                value(upload.create(path.as_path(), entry)?)
            }
            Request::ImportChunk(id, path, offset, bytes) => {
                let imports = self.imports.lock().unwrap();
                let upload = imports.get(&id).context("The workspace import is closed")?;
                value(upload.write(path.as_path(), offset, &bytes.0)?)
            }
            Request::FinishImport(id, publish) => {
                let _guard = self.mutations.lock().unwrap();
                let upload = self.imports.lock().unwrap().remove(&id);
                if publish {
                    upload
                        .context("The workspace import is closed")?
                        .publish(root)?;
                    session.invalidate();
                }
                value(())
            }
            Request::Worktrees => value(repo.worktrees()?),
            Request::WorktreeIdentity => value(crate::repository::worktree_identity::identity(
                root,
                &repo.common_dir,
            )?),
            Request::WorktreeMarker(admin, id) => value(
                crate::repository::worktree_identity::has_marker(&repo.common_dir, &admin, &id)?,
            ),
            Request::Exists(path) => value(repo.exists(path.as_path())),
            Request::Openable(paths) => value(git::openable(root, &paths)),
            Request::LocatorPaths => value(crate::repository::worktree_paths(root)?),
            Request::Locator(query) => value(locator::search_visible_text(&repo, &query)?),
            Request::SearchWorkspaceFiles {
                id,
                revision,
                query,
            } => {
                self.begin_search(id)?;
                let cancelled = || self.search_cancelled(id);
                let paths = self.locator_paths.load(&repo, revision, &cancelled)?;
                value(locator::search_files(&paths, &query, cancelled)?)
            }
            Request::SearchWorkspaceText {
                id,
                revision,
                query,
                file_type,
            } => {
                self.begin_search(id)?;
                let cancelled = || self.search_cancelled(id);
                let paths = self.locator_paths.load(&repo, revision, &cancelled)?;
                let search = TextSearch::new(paths, &query, file_type, cancelled)?;
                value(self.text_page(&repo, id, search)?)
            }
            Request::ContinueWorkspaceSearch(id) => {
                let search = {
                    let mut current = self.locator.lock().unwrap();
                    ensure!(
                        current.id == id && !current.cancelled,
                        "The workspace search was superseded"
                    );
                    current
                        .continuation
                        .take()
                        .context("The workspace search has finished")?
                };
                value(self.text_page(&repo, id, search)?)
            }
            Request::CancelWorkspaceSearch(id) => {
                let mut current = self.locator.lock().unwrap();
                if current.id <= id {
                    *current = ActiveSearch {
                        id,
                        cancelled: true,
                        continuation: None,
                    };
                }
                value(())
            }
            Request::ReadForSync(path) => value(repo.bytes_if_present(path.as_path())?.map(Bytes)),
            Request::Read(path, asset) => value(Bytes(repo.bytes(path.as_path(), asset)?)),
            Request::ReadResource {
                path,
                asset,
                version,
            } => value(repo.read_resource(path.as_path(), asset, version.as_ref())?),
            Request::ReadResourceChunk {
                path,
                asset,
                offset,
                version,
            } => {
                value(repo.read_resource_chunk(path.as_path(), asset, offset, version.as_ref())?)
            }
            Request::CheckResources(checks) => value(repo.check_resources(checks)?),
            Request::Save {
                path,
                baseline,
                bytes,
            } => {
                let _guard = self.mutations.lock().unwrap();
                value(write_checked(root, path.as_path(), baseline, &bytes.0)?)
            }
            Request::History => value(git::history(root)?),
            Request::Commits(oid, query, offset) => {
                value(git::commits(root, &oid, &query, offset)?)
            }
            Request::Commit(oid) => value(git::commit(root, &oid)?),
            Request::SearchRevisions(query) => value(git::search_revisions(root, &query)?),
            Request::Changes(comparison) => {
                let detail = Arc::new(session.call(|state| git::changes(state, comparison))?);
                self.remember_commit(&detail);
                value(detail)
            }
            Request::ReviewFiles(comparison) => {
                value(session.call(|state| self.patch_files(state, comparison))?)
            }
            Request::ReviewStats(comparison, base) => {
                value(session.call(|state| git::review_stats(state, comparison, &base))?)
            }
            Request::Patch(comparison, path, context, whitespace) => {
                ensure!(context <= 100_000, "Invalid number of diff context lines");
                value(session.call(|state| {
                    let detail = self.patch_files(state, comparison)?;
                    let file = detail
                        .files
                        .iter()
                        .find(|file| file.path == path)
                        .context("Changes are out of date. Refresh the workspace.")?;
                    git::raw_patch(state, &detail, file, context, whitespace)
                })?)
            }
            Request::Snapshot(comparison, path, side) => value(session.call(|state| {
                let detail = self.patch_files(state, comparison)?;
                let file = detail
                    .files
                    .iter()
                    .find(|file| file.path == path)
                    .context("Changes are out of date. Refresh the workspace.")?;
                git::snapshot(state, &detail, file, side)
            })?),
            request @ (Request::ImageDiffSide(..) | Request::ImageDiffResource(..)) => {
                let (comparison, base, path, side, expected) = match request {
                    Request::ImageDiffSide(comparison, base, path, side) => {
                        (comparison, base, path, side, None)
                    }
                    Request::ImageDiffResource(comparison, base, path, side, expected) => {
                        (comparison, base, path, side, Some(expected))
                    }
                    _ => unreachable!(),
                };
                session.call(|state| {
                    let detail = self.patch_files(state, comparison)?;
                    ensure!(
                        detail.base == base,
                        "Changes are out of date. Refresh the workspace."
                    );
                    let file = detail
                        .files
                        .iter()
                        .find(|file| file.path == path)
                        .context("Changes are out of date. Refresh the workspace.")?;
                    match expected {
                        Some(expected) => value(git::image_diff_resource(
                            state,
                            &detail,
                            file,
                            side,
                            expected.as_ref(),
                        )?),
                        None => value(git::image_diff_side(state, &detail, file, side)?),
                    }
                })
            }
            Request::Stage(expected, paths, staged) => {
                let _guard = self.mutations.lock().unwrap();
                git::stage(root, &expected, &paths, staged)?;
                session.invalidate();
                value(())
            }
            Request::FileChanges(path, options) => {
                value(session.call(|state| git::file_changes(state, &path, options))?)
            }
            Request::FileChangesBatch(requests) => {
                value(session.call(|state| git::file_changes_batch(state, &requests))?)
            }
            Request::FileVersions(paths) => value(session.call(|state| {
                Ok(paths
                    .iter()
                    .map(|path| git::file_version(state, path).ok())
                    .collect::<Vec<_>>())
            })?),
            Request::BlameBaseline(path, head) => {
                value(session.call(|state| git::blame_baseline(state, &path, &head))?)
            }
            Request::Blame(path, head, first, last) => {
                value(session.call(|state| git::blame_lines(state, &path, &head, first, last))?)
            }
            Request::FileVersion(path) => {
                value(session.call(|state| git::file_version(state, &path))?)
            }
            Request::LayerSnapshot(path, layer, side) => {
                value(session.call(|state| git::layer_snapshot(state, &path, layer, side))?)
            }
            Request::EditFile(edit) => {
                let _guard = self.mutations.lock().unwrap();
                git::edit_file(&session.open_state()?, &edit)?;
                session.invalidate();
                value(())
            }
            Request::CommitState => value(git::commit_state(root)?),
            Request::StagedCommitContext(expected) => {
                value(git::staged_commit_context(root, &expected)?)
            }
            Request::AllowsHoverTranslation(path) => value(
                crate::ai::context::allowed_hover_source(root, path.as_path()),
            ),
            Request::CreateCommit(expected, message) => {
                let _guard = self.mutations.lock().unwrap();
                let result = git::create_commit(root, &expected, &message);
                session.invalidate();
                value(result?)
            }
            Request::StageOperation(ref expected, ref paths, staged)
            | Request::StageWithHistory(ref expected, ref paths, staged) => {
                let _guard = self.mutations.lock().unwrap();
                let result = git::stage_operation(root, expected, paths, staged);
                session.invalidate();
                let operation = result?;
                if matches!(request, Request::StageWithHistory(..)) {
                    value(operation)
                } else {
                    value(())
                }
            }
            Request::EditOperation(ref edit) | Request::EditWithHistory(ref edit) => {
                let _guard = self.mutations.lock().unwrap();
                let result = git::edit_operation(root, edit);
                session.invalidate();
                let operation = result?;
                if matches!(request, Request::EditWithHistory(..)) {
                    value(operation)
                } else {
                    value(())
                }
            }
            Request::OperationHistory => value(git::operation_history(root)?),
            Request::GitManagement => value(git::git_management(root)?),
            Request::PreviewWorktreeRemoval(path, reference) => {
                value(git::preview_worktree_removal(root, &path, &reference)?)
            }
            Request::CompareUpstream(source, upstream) => {
                value(git::compare_upstream(root, &source, &upstream)?)
            }
            Request::PushOptions(source) => value(git::push_options(root, &source)?),
            Request::PushBranches(remote) => value(git::push_branches(root, &remote)?),
            Request::PreviewPush(target) => value(git::preview_push(root, target)?),
            Request::ManageGit(operation) => {
                let _guard = self.mutations.lock().unwrap();
                let result = git::manage_git(root, operation);
                session.invalidate();
                value(result?)
            }
            Request::RestoreHistoryText(id, undo) => {
                value(git::restore_history_text(root, id, undo)?)
            }
            Request::UndoOperation(id) => {
                let _guard = self.mutations.lock().unwrap();
                let result = git::undo_operation(root, id);
                session.invalidate();
                value(result?)
            }
            Request::ReplayGitHistory(id, undo) => {
                let _guard = self.mutations.lock().unwrap();
                let result = git::replay_operation(root, id, undo);
                session.invalidate();
                value(result?)
            }
            Request::CanCherryPick(tip, targets) => {
                value(git::can_cherry_pick(root, &tip, &targets)?)
            }
            Request::HistoryEditPlan(tip, targets) => {
                value(git::prepare_history_plan(root, tip, &targets)?)
            }
            Request::HistoryResetPlan(tip, target) => {
                value(git::prepare_history_reset(root, tip, &target)?)
            }
            Request::HistoryAmendMessage(tip, expected) => {
                value(git::amend_message(root, &tip, &expected)?)
            }
            Request::HistoryRewriteRecord(branch, oid) => {
                value(git::rewrite_record(root, &branch, &oid))
            }
            Request::HistoryEditProgress => {
                let _guard = self.mutations.lock().unwrap();
                value(git::history_edit_progress(root))
            }
            Request::RecoverHistoryBackup => {
                let _guard = self.mutations.lock().unwrap();
                value(git::recover_backup(root)?)
            }
            Request::EditHistory(edit, confirmed) => {
                let _guard = self.mutations.lock().unwrap();
                value(git::run_history_edit(root, edit, confirmed)?)
            }
            Request::TerminalOpen(size) | Request::TerminalOpenAt(size, _) => {
                let mut terminals = self.terminals.lock().unwrap();
                ensure!(terminals.len() < 32, "Remote terminal limit reached");
                let (start, directory) = if let Request::TerminalOpenAt(_, start) = request {
                    let start = start.map(|path| root.join(path));
                    let directory = TerminalDirectory {
                        next_sample: Instant::now(),
                        last: None,
                    };
                    (start, Some(directory))
                } else {
                    (None, None)
                };
                let (commands, requests) = mpsc::channel();
                let (output, events) = async_channel::bounded(16);
                let process = pty::Process::start(
                    pty::shell(),
                    pty::ShellMode::Login,
                    root,
                    start.as_deref(),
                    size,
                    requests,
                    output,
                )?;
                let id = self.next_terminal.fetch_add(1, Ordering::Relaxed);
                terminals.insert(
                    id,
                    Terminal {
                        process,
                        commands,
                        events,
                        directory,
                    },
                );
                value(id)
            }
            Request::TerminalInput(id, bytes) => {
                self.terminal_command(id, pty::Command::Input(bytes.0))
            }
            Request::TerminalResize(id, size) => {
                self.terminal_command(id, pty::Command::Resize(size))
            }
            Request::TerminalClose(id) => {
                let terminal = self.terminals.lock().unwrap().remove(&id);
                drop(terminal);
                value(())
            }
            Request::TerminalPoll(id) => {
                let mut terminals = self.terminals.lock().unwrap();
                let terminal = terminals
                    .get_mut(&id)
                    .context("The remote terminal is closed")?;
                let mut events = Vec::new();
                let mut exited = false;
                for _ in 0..16 {
                    let Ok(event) = terminal.events.try_recv() else {
                        break;
                    };
                    events.push(match event {
                        pty::Event::Output(bytes) => TerminalEvent::Output(Bytes(bytes)),
                        pty::Event::Exit(code) => {
                            exited = true;
                            TerminalEvent::Exit(code)
                        }
                        pty::Event::Error(error) => {
                            exited = true;
                            TerminalEvent::Error(error)
                        }
                    });
                }
                if exited {
                    terminals.remove(&id);
                } else if let Some(directory) = &mut terminal.directory
                    && Instant::now() >= directory.next_sample
                {
                    directory.next_sample = Instant::now() + Duration::from_millis(500);
                    if let Some(path) = terminal
                        .process
                        .working_directory()
                        .filter(|path| path.is_absolute() && path.to_str().is_some())
                        && directory.last.as_ref() != Some(&path)
                    {
                        directory.last = Some(path.clone());
                        events.push(TerminalEvent::Directory(path));
                    }
                }
                value(events)
            }
            Request::WorktreeLspConfig => {
                let mut snapshot = self.lsp_snapshot.lock().unwrap();
                let config = worktree_lsp(root)?;
                value(LspSnapshot::publish(&mut snapshot, root, config)?)
            }
            Request::WorktreeLspRoot(roots, file) => {
                roots.validate()?;
                crate::repository::resolve_file(root, file.as_path())?;
                value(roots.resolve(root, file.as_path()))
            }
            Request::ExternalLspRoot(id, file) => {
                crate::repository::resolve_file(root, file.as_path())?;
                value(external_server(&id)?.project_root(root, file.as_path()))
            }
            Request::RemoteLspProject(file, mut excluded, worktree_ids) => {
                use super::RemoteLspProject;
                use crate::language::{BuiltinProject, external};
                crate::repository::resolve_file(root, file.as_path())?;
                let entries = match external::load() {
                    Ok(entries) => entries,
                    Err(error) => {
                        eprintln!("Could not load external language servers: {error:#}");
                        Vec::new()
                    }
                };
                excluded.extend(LanguageServer::ALL.into_iter().filter(|server| {
                    entries
                        .iter()
                        .any(|entry| entry.disabled && entry.id == server.id())
                }));
                if let Some((mut server, language_id)) =
                    external::select(entries, file.as_path(), |id| {
                        worktree_ids.iter().any(|entry| entry == id)
                    })
                {
                    let root = server.project_root(root, file.as_path());
                    server.server.omit_host_launch_fields();
                    return value(Some(RemoteLspProject::External {
                        server: Box::new(server),
                        root,
                        language_id,
                    }));
                }
                value(
                    BuiltinProject::resolve(root, file.as_path(), &excluded)
                        .map(RemoteLspProject::Builtin),
                )
            }
            Request::LspSetupStatus(options) => {
                let root = lsp_root(&options)?;
                ensure!(
                    root == repo.root,
                    "The language server is for another worktree"
                );
                let environment = lsp_environment(&options.project)?;
                let Some(server) = managed_server(&options, &root, environment.as_ref())? else {
                    return value(None::<super::LspSetupStatus>);
                };
                let home = lsp_home(environment.as_ref())?;
                let account = lsp_account(&home, environment.as_ref());
                value(Some(super::LspSetupStatus {
                    account,
                    installed: Tools::new(home)?
                        .lookup(server.bundle().unwrap(), environment.as_ref())?,
                }))
            }
        }
    }

    fn patch_files(
        &self,
        state: &git::RepoState,
        comparison: git::Comparison,
    ) -> Result<Arc<git::Changes>> {
        if comparison.commit_target().is_some()
            && let Some(detail) = self.review_commit.lock().unwrap().as_ref()
            && detail.comparison == comparison
        {
            return Ok(detail.clone());
        }
        let detail = Arc::new(git::review_files(state, comparison)?);
        self.remember_commit(&detail);
        Ok(detail)
    }

    fn remember_commit(&self, detail: &Arc<git::Changes>) {
        if detail.comparison.commit_target().is_some() {
            *self.review_commit.lock().unwrap() = Some(detail.clone());
        }
    }

    fn terminal_command(&self, id: u64, command: pty::Command) -> Result<Value> {
        self.terminals
            .lock()
            .unwrap()
            .get(&id)
            .context("The remote terminal is closed")?
            .commands
            .send(command)
            .context("The remote terminal has exited")?;
        value(())
    }
}

fn value(value: impl Serialize) -> Result<Value> {
    Ok(to_value(value)?)
}

fn browse_path(path: &str, base: Option<&str>) -> Result<std::path::PathBuf> {
    use std::path::Path;
    ensure!(!path.contains('\0'), "Invalid remote path");
    if path.starts_with('/') {
        return Ok(path.into());
    }
    let home = dirs::home_dir().context("Could not locate the remote home directory")?;
    if path == "~" {
        return Ok(home);
    }
    if let Some(relative) = path.strip_prefix("~/") {
        return Ok(home.join(relative.trim_start_matches('/')));
    }
    let base = base
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .unwrap_or(&home);
    Ok(base.join(path))
}

pub fn serve() -> Result<()> {
    let mut output = BufWriter::new(io::stdout().lock());
    let (results, replies) = mpsc::sync_channel::<Reply>(16);
    let server = Arc::new(Server {
        watch_replies: Some(results.clone()),
        ..Default::default()
    });
    let (jobs, queue) = mpsc::sync_channel::<Call>(16);
    let (terminals, terminal_queue) = mpsc::sync_channel::<Call>(16);
    serve_queue(&server, &results, queue, 4);
    serve_queue(&server, &results, terminal_queue, 1);
    drop(server);
    thread::spawn(move || {
        let mut input = BufReader::new(io::stdin().lock());
        while let Ok(Some(call)) = read_frame::<Call>(&mut input) {
            let queue = match call.request {
                Request::TerminalOpen(_)
                | Request::TerminalOpenAt(..)
                | Request::TerminalInput(..)
                | Request::TerminalResize(..)
                | Request::TerminalPoll(_)
                | Request::TerminalClose(_) => &terminals,
                _ => &jobs,
            };
            if queue.send(call).is_err() {
                break;
            }
        }
    });
    drop(results);
    for mut reply in replies {
        if let Err(error) = write_frame(&mut output, &reply) {
            if error.downcast_ref::<io::Error>().is_some() {
                return Err(error);
            }
            reply.result = Err(format!("{error:#}"));
            write_frame(&mut output, &reply)?;
        }
    }
    Ok(())
}

fn serve_queue(
    server: &Arc<Server>,
    results: &mpsc::SyncSender<Reply>,
    queue: mpsc::Receiver<Call>,
    workers: usize,
) {
    let queue = Arc::new(Mutex::new(queue));
    for _ in 0..workers {
        let queue = queue.clone();
        let server = server.clone();
        let results = results.clone();
        thread::spawn(move || {
            loop {
                let job = queue.lock().unwrap().recv();
                let Ok(Call { id, request }) = job else { break };
                let result = server
                    .request(request)
                    .map_err(|error| format!("{error:#}"));
                if results.send(Reply { id, result }).is_err() {
                    break;
                }
            }
        });
    }
}

fn external_server(id: &str) -> Result<crate::language::external::ExternalLsp> {
    crate::language::external::load()?
        .into_iter()
        .find(|server| !server.disabled && server.id == id)
        .with_context(|| format!("Unknown external language server {id:?}"))
}

fn worktree_lsp(root: &std::path::Path) -> Result<crate::language::worktree::WorktreeLsp> {
    let environment = std::cell::OnceCell::new();
    crate::language::worktree::WorktreeLsp::load(root, |server| {
        #[cfg(unix)]
        let variables = environment
            .get_or_init(|| login_environment(root).ok())
            .as_ref();
        #[cfg(not(unix))]
        let variables = environment
            .get_or_init(|| None::<HashMap<std::ffi::OsString, std::ffi::OsString>>)
            .as_ref();
        let executable = server
            .executable_variable()
            .and_then(|name| lsp_variable(name, variables));
        match executable {
            Some(executable) => (server.environment_command(executable), false),
            None => (server.default_command(), true),
        }
    })
}

type LspEnvironment = HashMap<std::ffi::OsString, std::ffi::OsString>;

fn lsp_environment(project: &Path) -> Result<Option<LspEnvironment>> {
    #[cfg(unix)]
    return login_environment(project).map(Some);
    #[cfg(not(unix))]
    {
        let _ = project;
        Ok(None)
    }
}

fn lsp_variable(name: &str, environment: Option<&LspEnvironment>) -> Option<std::ffi::OsString> {
    match environment {
        Some(environment) => environment.get(std::ffi::OsStr::new(name)).cloned(),
        None => std::env::var_os(name),
    }
}

fn lsp_home(environment: Option<&LspEnvironment>) -> Result<PathBuf> {
    lsp_variable("STROLL_HOME", environment)
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".stroll")))
        .context("Could not determine the remote Stroll directory")
}

fn lsp_account(home: &Path, environment: Option<&LspEnvironment>) -> String {
    let account = lsp_variable(if cfg!(windows) { "USERNAME" } else { "USER" }, environment)
        .unwrap_or_default();
    format!("{}:{}", account.to_string_lossy(), home.display())
}

fn decode_launch(options: &str) -> Result<super::LspLaunch> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    Ok(serde_json::from_slice(&URL_SAFE_NO_PAD.decode(options)?)?)
}

fn lsp_root(options: &super::LspLaunch) -> Result<PathBuf> {
    let root = crate::repository::repository_root(&options.repository)?;
    let relative = options.project.strip_prefix(&root)?;
    ensure!(
        relative
            .components()
            .all(|part| matches!(part, Component::Normal(name) if name != ".git")),
        "Invalid language server directory"
    );
    ensure!(
        options.project.canonicalize()? == options.project && options.project.is_dir(),
        "Invalid language server directory"
    );
    Ok(root)
}

fn snapshot_server(
    root: &Path,
    id: &str,
    fingerprint: &str,
    snapshot: &Path,
) -> Result<WorktreeServer> {
    let snapshot = LspSnapshot::read(snapshot)?;
    ensure!(
        snapshot.root == root,
        "The language server snapshot is for another worktree"
    );
    let server = snapshot
        .config
        .entry(id)
        .filter(|server| server.enabled())
        .with_context(|| format!("Unknown worktree language server {id:?}"))?;
    ensure!(
        server.fingerprint == fingerprint,
        "The worktree language server configuration changed before launch"
    );
    Ok(server.clone())
}

fn managed_server(
    options: &super::LspLaunch,
    root: &Path,
    environment: Option<&LspEnvironment>,
) -> Result<Option<LanguageServer>> {
    Ok(match &options.server {
        super::LspTarget::Builtin(server) => server
            .bundle()
            .filter(|_| {
                server
                    .executable_variable()
                    .and_then(|name| lsp_variable(name, environment))
                    .is_none()
            })
            .map(|_| *server),
        super::LspTarget::Worktree {
            id,
            fingerprint,
            snapshot,
        } => snapshot_server(root, id, fingerprint, snapshot)?.managed,
        super::LspTarget::External { .. } => None,
    })
}

pub fn serve_lsp_setup(options: &str) -> Result<()> {
    let mut output = BufWriter::new(io::stdout().lock());
    let installed = (|| {
        let options = decode_launch(options)?;
        let root = lsp_root(&options)?;
        let environment = lsp_environment(&options.project)?;
        let server = managed_server(&options, &root, environment.as_ref())?
            .context("The selected language server does not use managed tools")?;
        let tools = Tools::new(lsp_home(environment.as_ref())?)?;
        tools.install(server.bundle().unwrap(), environment.as_ref(), |progress| {
            let _ = write_frame(&mut output, &super::LspSetupEvent::Progress(progress));
        })
    })();
    let event = match installed {
        Ok(installed) => super::LspSetupEvent::Installed(installed),
        Err(error) => super::LspSetupEvent::Failed(format!("{error:#}")),
    };
    write_frame(&mut output, &event)
}

pub fn serve_lsp(options: &str) -> Result<()> {
    let options = decode_launch(options)?;
    let root = lsp_root(&options)?;
    let environment = lsp_environment(&options.project)?;
    let tools = options
        .managed
        .as_ref()
        .map(|_| Tools::new(lsp_home(environment.as_ref())?))
        .transpose()?;
    let installed = tools.as_ref().zip(options.managed.as_ref());
    let mut command = match options.server {
        super::LspTarget::Worktree {
            id,
            fingerprint,
            snapshot,
        } => snapshot_server(&root, &id, &fingerprint, &snapshot)?.command(
            &options.project,
            environment.as_ref(),
            installed,
        )?,
        super::LspTarget::External { id } => {
            let server = external_server(&id)?;
            let mut command = server.command();
            if let Some(environment) = &environment {
                command
                    .env_clear()
                    .envs(environment)
                    .envs(&server.server.env);
            }
            command.current_dir(&options.project);
            command
        }
        super::LspTarget::Builtin(server) => {
            let mut command = if let Some(executable) = server
                .executable_variable()
                .and_then(|name| lsp_variable(name, environment.as_ref()))
            {
                server.environment_command(executable)
            } else if let Some((tools, installed)) = installed {
                tools.command(server, installed, None, environment.as_ref())?
            } else {
                ensure!(
                    server.bundle().is_none(),
                    "Managed language tools have not been installed"
                );
                server.default_command()
            };
            command.current_dir(&options.project);
            if let Some(environment) = &environment {
                command.env_clear().envs(environment);
            }
            command
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        #[cfg(target_os = "linux")]
        if let Some(path) = &options.memory {
            // Memory reporting is optional and must not prevent the language server from starting.
            let _ = publish_lsp_process(path);
        }
        let error = command.exec();
        Err(error).with_context(|| {
            format!(
                "Could not start {} in the remote login shell environment",
                command.get_program().to_string_lossy()
            )
        })
    }
    #[cfg(not(unix))]
    {
        ensure!(
            command.status()?.success(),
            "The language server exited unsuccessfully"
        );
        Ok(())
    }
}

#[cfg(unix)]
fn login_environment(
    directory: &std::path::Path,
) -> Result<HashMap<std::ffi::OsString, std::ffi::OsString>> {
    let shell = std::env::var_os("SHELL")
        .context("The SSH session did not provide the remote account's default shell")?;
    // Load interactive startup files and keep their output off the protocol stream.
    let output = crate::process::login_shell(&shell, directory, "-ilc")
        .output()
        .context("Could not start the remote account's default login shell")?;
    ensure!(
        output.status.success(),
        "Could not read the remote login shell environment: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    crate::process::parse_login_environment(&output.stdout)
        .context("Could not read the remote login shell environment")
}
