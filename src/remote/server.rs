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
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, to_value};

use super::{Bytes, Call, Reply, Request, TerminalEvent, VERSION, read_frame, write_frame};
use crate::{
    git,
    language::worktree::WorktreeLsp,
    pty,
    repository::{Repository, RepositoryInfo, write_checked},
};

#[derive(Default)]
struct Server {
    repository: RwLock<Option<Arc<Repository>>>,
    mutations: Mutex<()>,
    documents: Mutex<Option<(u64, crate::repository::files::DocumentScan)>>,
    watch: Mutex<Option<git::watch::RepoWatcher>>,
    watch_replies: Option<mpsc::SyncSender<Reply>>,
    review_commit: Mutex<Option<Arc<git::Changes>>>,
    terminals: Mutex<HashMap<u64, Terminal>>,
    next_terminal: AtomicU64,
    lsp_snapshot: Mutex<Option<LspSnapshot>>,
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
    _process: pty::Process,
    commands: mpsc::Sender<pty::Command>,
    events: async_channel::Receiver<pty::Event>,
}

impl Server {
    fn request(&self, request: Request) -> Result<Value> {
        match request {
            Request::Hello(version) => {
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
                            | super::BUILTIN_LSP_PROJECT_VERSION
                    ),
                    "SSH workspace protocol version mismatch. Update the remote helper."
                );
                return value(version);
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
            | Request::Complete { .. } => unreachable!(),
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
            Request::Worktrees => value(repo.worktrees()?),
            Request::Exists(path) => value(repo.exists(path.as_path())),
            Request::Openable(paths) => value(git::openable(root, &paths)),
            Request::LocatorPaths => value(crate::repository::worktree_paths(root)?),
            Request::Locator(query) => value(crate::locator::search_text(&repo, &query, || false)?),
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
            Request::TerminalOpen(size) => {
                let mut terminals = self.terminals.lock().unwrap();
                ensure!(terminals.len() < 32, "Remote terminal limit reached");
                let (commands, requests) = mpsc::channel();
                let (output, events) = async_channel::bounded(16);
                let process = pty::Process::start(
                    pty::shell(),
                    pty::ShellMode::Login,
                    root,
                    size,
                    requests,
                    output,
                )?;
                let id = self.next_terminal.fetch_add(1, Ordering::Relaxed);
                terminals.insert(
                    id,
                    Terminal {
                        _process: process,
                        commands,
                        events,
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
                    .get(&id)
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
            Request::BuiltinLspProject(file, excluded) => {
                crate::repository::resolve_file(root, file.as_path())?;
                value(crate::language::BuiltinProject::resolve(
                    root,
                    file.as_path(),
                    &excluded,
                ))
            }
            Request::LspRoot(server, file) => {
                crate::repository::resolve_file(root, file.as_path())?;
                value(server.project_root(root, file.as_path()))
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
        .find(|server| server.id == id)
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
        let variable = std::ffi::OsStr::new(server.executable_variable());
        let executable = match variables {
            Some(variables) => variables.get(variable).cloned(),
            None => std::env::var_os(variable),
        };
        match executable {
            Some(executable) => (server.command_for(executable), false),
            None => (server.default_command(), true),
        }
    })
}

pub fn serve_lsp(options: &str) -> Result<()> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let options: super::LspLaunch = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(options)?)?;
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
    #[cfg(unix)]
    let environment = Some(login_environment(&options.project)?);
    #[cfg(not(unix))]
    let environment: Option<HashMap<std::ffi::OsString, std::ffi::OsString>> = None;
    let mut command = match options.server {
        super::LspTarget::Worktree {
            id,
            fingerprint,
            snapshot,
        } => {
            let snapshot = LspSnapshot::read(&snapshot)?;
            ensure!(
                snapshot.root == root,
                "The language server snapshot is for another worktree"
            );
            let server = snapshot
                .config
                .entry(&id)
                .filter(|server| server.enabled())
                .with_context(|| format!("Unknown worktree language server {id:?}"))?;
            ensure!(
                server.fingerprint == fingerprint,
                "The worktree language server configuration changed before launch"
            );
            server.command(&options.project, environment.as_ref())?
        }
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
            let variable = server.executable_variable();
            let executable = match &environment {
                Some(environment) => environment.get(std::ffi::OsStr::new(variable)).cloned(),
                None => std::env::var_os(variable),
            };
            if let Some(executable) = executable {
                let mut command = server.command_for(executable);
                command.current_dir(&options.project);
                if let Some(environment) = &environment {
                    command.env_clear().envs(environment);
                }
                command
            } else {
                server.resolve_default_command(&options.project, environment.as_ref())?
            }
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

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
    // Capture startup output separately from the language server's protocol stream.
    let output = crate::process::login_shell(&shell, directory)
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
