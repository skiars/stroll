use rust_i18n::t;

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, BufWriter},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{Call, GIT_EDIT_VERSION, Reply, Request, SshTarget, VERSION, read_frame, write_frame};

type Response = Result<Value, String>;
type Pending = Arc<Mutex<HashMap<u64, mpsc::SyncSender<Response>>>>;

pub(crate) struct PendingCall {
    id: u64,
    pending: Pending,
    response: mpsc::Receiver<Response>,
    deadline: Instant,
}

impl PendingCall {
    fn decode<T: DeserializeOwned>(response: Response) -> Result<T> {
        Ok(serde_json::from_value(
            response.map_err(|error| anyhow!("{error}"))?,
        )?)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn try_reply<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        match self.response.try_recv() {
            Ok(response) => Self::decode(response).map(Some),
            Err(mpsc::TryRecvError::Empty) if Instant::now() < self.deadline => Ok(None),
            Err(error) => Err(error).context(t!("ssh.timeout")),
        }
    }
}

impl Drop for PendingCall {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

#[derive(Clone)]
pub(crate) struct SshSession(Arc<Connection>);

struct Connection {
    /// `None` identifies a local Git helper.
    target: Option<SshTarget>,
    child: Mutex<Child>,
    writer: mpsc::SyncSender<Call>,
    pending: Pending,
    next_id: AtomicU64,
    git_edit: AtomicBool,
    #[cfg(feature = "desktop")]
    git_operations: AtomicBool,
    #[cfg(feature = "desktop")]
    ai_context: AtomicBool,
    #[cfg(feature = "desktop")]
    git_history: AtomicBool,
    #[cfg(feature = "desktop")]
    blame: AtomicBool,
    batch: AtomicBool,
    #[cfg(feature = "desktop")]
    revision_comparison: std::sync::OnceLock<bool>,
    #[cfg(feature = "desktop")]
    image_diff: std::sync::OnceLock<bool>,
    #[cfg(feature = "desktop")]
    worktree_lsp: std::sync::OnceLock<bool>,
    repository_events: crate::git::watch::RepoEvents,
    #[cfg(feature = "desktop")]
    repository_watch: AtomicBool,
    #[cfg(feature = "desktop")]
    review: AtomicBool,
    #[cfg(feature = "desktop")]
    locator: AtomicBool,
    #[cfg(feature = "desktop")]
    workspace_files: AtomicBool,
    #[cfg(feature = "desktop")]
    workspace_transfer: AtomicBool,
    #[cfg(feature = "desktop")]
    lsp_edits: AtomicBool,
    failure: Arc<Mutex<Option<String>>>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Ok(child) = self.child.get_mut() {
            _ = child.kill();
            _ = child.wait();
        }
    }
}

pub(crate) fn helper_command(target: &SshTarget, arguments: &str) -> Command {
    let remote_command = format!(
        "if command -v stroll-remote >/dev/null 2>&1; then exec stroll-remote {arguments}; else exec \"$HOME/.local/bin/stroll-remote\" {arguments}; fi"
    );
    let mut command = crate::process::background("ssh");
    command.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=10",
        "-o",
        "ServerAliveCountMax=2",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ForwardAgent=no",
        "-o",
        "ForwardX11=no",
    ]);
    if let Some(port) = target.port {
        command.arg("-p").arg(port.to_string());
    }
    command.args(["--", &target.host, &remote_command]);
    command
}

impl SshSession {
    pub fn connect(target: SshTarget) -> Result<Self> {
        Ok(Self(Connection::open(
            helper_command(&target, ""),
            Some(target),
        )?))
    }

    pub fn target(&self) -> &SshTarget {
        self.0.target.as_ref().expect("Expected an SSH session")
    }

    #[cfg(feature = "desktop")]
    pub fn supports_git_edit(&self) -> bool {
        self.0.git_edit.load(Ordering::Relaxed)
    }

    #[cfg(feature = "desktop")]
    pub fn supports_git_operations(&self) -> bool {
        self.0.git_operations.load(Ordering::Relaxed)
    }

    #[cfg(feature = "desktop")]
    pub fn supports_ai_context(&self) -> bool {
        self.0.ai_context.load(Ordering::Relaxed)
    }

    #[cfg(feature = "desktop")]
    pub fn supports_git_history(&self) -> bool {
        self.0.git_history.load(Ordering::Relaxed)
    }

    #[cfg(feature = "desktop")]
    pub fn restore_history_text(
        &self,
        id: u64,
        undo: bool,
    ) -> Result<Option<(crate::remote::Bytes, [u8; 32])>> {
        let version = super::RESTORE_HISTORY_TEXT_VERSION;
        if self
            .call::<u32>(Request::Hello(version))
            .is_ok_and(|reply| reply == version)
        {
            self.call(Request::RestoreHistoryText(id, undo))
        } else {
            Ok(None)
        }
    }

    #[cfg(feature = "desktop")]
    pub fn supports_blame(&self) -> bool {
        self.0.blame.load(Ordering::Relaxed)
    }

    #[cfg(feature = "desktop")]
    pub fn supports_batch(&self) -> bool {
        self.0.batch.load(Ordering::Relaxed)
    }

    #[cfg(feature = "desktop")]
    pub fn supports_review_files(&self) -> bool {
        self.0.review.load(Ordering::Relaxed)
    }

    #[cfg(feature = "desktop")]
    pub fn supports_image_diff(&self) -> bool {
        self.0.supports_image_diff()
    }

    #[cfg(feature = "desktop")]
    pub fn supports_revision_comparison(&self) -> bool {
        self.0.supports_revision_comparison()
    }

    #[cfg(feature = "desktop")]
    pub fn supports_locator(&self) -> bool {
        self.0.locator.load(Ordering::Relaxed)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn watch_repository(&self) -> Result<crate::git::watch::RepoEvents> {
        let version = super::WATCH_VERSION;
        ensure!(
            self.call::<u32>(Request::Hello(version))? == version,
            "Repository watch is unavailable"
        );
        self.0.repository_watch.store(true, Ordering::Relaxed);
        self.call::<()>(Request::WatchRepository)?;
        Ok(self.0.repository_events.clone())
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn supports_repository_watch(&self) -> bool {
        self.0.repository_watch.load(Ordering::Relaxed)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn supports_workspace_files(&self) -> bool {
        self.0.workspace_files.load(Ordering::Relaxed)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn supports_workspace_transfer(&self) -> bool {
        self.0.workspace_transfer.load(Ordering::Relaxed)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn supports_lsp_edits(&self) -> bool {
        self.0.lsp_edits.load(Ordering::Relaxed)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn supports_worktree_lsp(&self) -> bool {
        *self.0.worktree_lsp.get_or_init(|| {
            let version = super::WORKTREE_LSP_VERSION;
            self.call::<u32>(Request::Hello(version)).ok() == Some(version)
        })
    }

    pub fn failure(&self) -> Option<String> {
        self.0.failure.lock().unwrap().clone()
    }

    pub fn call<T: DeserializeOwned>(&self, request: Request) -> Result<T> {
        self.0.call(request)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn try_terminal_call(
        &self,
        request: Request,
    ) -> Result<Result<PendingCall, Request>> {
        self.0.try_call(request, Duration::from_secs(35))
    }
}

/// The local Git helper is a persistent child process that runs the same `Backend` over the SSH framing protocol.
///
/// Local and remote calls share the same path, and Git operations run outside the main process and its worker pool.
/// If the helper crashes, `Backend` falls back to running in-process.
#[cfg(feature = "desktop")]
#[derive(Clone)]
pub(crate) struct GitHelper(Arc<Connection>);

#[cfg(feature = "desktop")]
impl GitHelper {
    /// Start the helper and bind it to the worktree root.
    pub fn connect(root: &std::path::Path) -> Result<Self> {
        let command = helper_process()?;
        let connection = Connection::open(command, None)?;
        let bound: std::path::PathBuf = connection.call(Request::Bind(root.to_owned()))?;
        ensure!(
            bound == root || crate::paths::simplified(&bound) == crate::paths::simplified(root),
            t!("git.helper_bind_mismatch")
        );
        Ok(Self(connection))
    }

    pub fn failure(&self) -> Option<String> {
        self.0.failure.lock().unwrap().clone()
    }

    pub fn supports_review_files(&self) -> bool {
        self.0.review.load(Ordering::Relaxed)
    }

    pub fn supports_blame(&self) -> bool {
        self.0.blame.load(Ordering::Relaxed)
    }

    pub fn supports_image_diff(&self) -> bool {
        self.0.supports_image_diff()
    }

    #[cfg(feature = "desktop")]
    pub fn supports_revision_comparison(&self) -> bool {
        self.0.supports_revision_comparison()
    }

    pub fn call<T: DeserializeOwned>(&self, request: Request) -> Result<T> {
        self.0.call(request)
    }
}

/// Prefer the smaller sibling `stroll-remote` for faster startup, then fall back to the current executable with a hidden argument.
///
/// `STROLL_GIT_HELPER_PATH` lets installers or read-only deployments specify another helper location.
#[cfg(feature = "desktop")]
fn helper_process() -> Result<Command> {
    if let Some(path) = std::env::var_os("STROLL_GIT_HELPER_PATH") {
        return Ok(crate::process::background(path));
    }
    let executable = std::env::current_exe().context(t!("git.helper_start_failed"))?;
    let sibling = executable.with_file_name(if cfg!(windows) {
        "stroll-remote.exe"
    } else {
        "stroll-remote"
    });
    if sibling.is_file() {
        return Ok(crate::process::background(sibling));
    }
    let mut command = crate::process::background(executable);
    command.arg("--git-helper");
    Ok(command)
}

impl Connection {
    fn open(mut command: Command, target: Option<SshTarget>) -> Result<Arc<Self>> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context(t!("ssh.start_failed"))?;
        let mut input = BufReader::new(child.stdout.take().unwrap());
        let mut output = BufWriter::new(child.stdin.take().unwrap());
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let (writer, outgoing) = mpsc::sync_channel(16);
        let pending: Pending = Arc::default();
        let failure = Arc::new(Mutex::new(None));
        let connection = Arc::new(Connection {
            target,
            child: Mutex::new(child),
            writer,
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            git_edit: AtomicBool::new(false),
            #[cfg(feature = "desktop")]
            git_operations: AtomicBool::new(false),
            #[cfg(feature = "desktop")]
            ai_context: AtomicBool::new(false),
            #[cfg(feature = "desktop")]
            git_history: AtomicBool::new(false),
            #[cfg(feature = "desktop")]
            blame: AtomicBool::new(false),
            batch: AtomicBool::new(false),
            #[cfg(feature = "desktop")]
            revision_comparison: Default::default(),
            #[cfg(feature = "desktop")]
            image_diff: Default::default(),
            #[cfg(feature = "desktop")]
            worktree_lsp: Default::default(),
            repository_events: Default::default(),
            #[cfg(feature = "desktop")]
            repository_watch: AtomicBool::new(false),
            #[cfg(feature = "desktop")]
            workspace_files: AtomicBool::new(false),
            #[cfg(feature = "desktop")]
            workspace_transfer: AtomicBool::new(false),
            #[cfg(feature = "desktop")]
            lsp_edits: AtomicBool::new(false),
            #[cfg(feature = "desktop")]
            review: AtomicBool::new(false),
            #[cfg(feature = "desktop")]
            locator: AtomicBool::new(false),
            failure: failure.clone(),
        });
        let diagnostics = Arc::new(Mutex::new(Vec::new()));
        let tail = diagnostics.clone();
        thread::spawn(move || {
            while let Ok(chunk) = stderr.fill_buf() {
                if chunk.is_empty() {
                    break;
                }
                let length = chunk.len();
                let mut tail = tail.lock().unwrap();
                tail.extend_from_slice(&chunk[length.saturating_sub(4096)..]);
                let excess = tail.len().saturating_sub(4096);
                tail.drain(..excess);
                stderr.consume(length);
            }
        });
        let events = connection.repository_events.clone();
        thread::spawn(move || {
            let error = loop {
                match read_frame::<Reply>(&mut input) {
                    Ok(Some(reply)) if reply.id == 0 => {
                        match reply
                            .result
                            .ok()
                            .and_then(|value| serde_json::from_value(value).ok())
                        {
                            Some(changes) => events.push(changes),
                            None => events.close(),
                        }
                    }
                    Ok(Some(reply)) => {
                        if let Some(receiver) = pending.lock().unwrap().remove(&reply.id) {
                            let result = reply.result.map_err(|diagnostic| {
                                format!("{}\n{diagnostic}", t!("ssh.operation_failed"))
                            });
                            _ = receiver.try_send(result);
                        }
                    }
                    Ok(None) => break t!("ssh.closed").into(),
                    Err(error) => {
                        break t!("ssh.read_failed", error = format!("{error:#}")).into_owned();
                    }
                }
            };
            events.close();
            let tail = diagnostics.lock().unwrap();
            let stderr = String::from_utf8_lossy(&tail);
            let message = if stderr.trim().is_empty() {
                error
            } else {
                format!("{error}\n{}", stderr.trim())
            };
            *failure.lock().unwrap() = Some(message.clone());
            for (_, reply) in pending.lock().unwrap().drain() {
                _ = reply.try_send(Err(message.clone()));
            }
        });
        let writer_connection = Arc::downgrade(&connection);
        thread::spawn(move || {
            for call in outgoing {
                if let Err(error) = write_frame(&mut output, &call) {
                    if let Some(connection) = writer_connection.upgrade()
                        && let Some(reply) = connection.pending.lock().unwrap().remove(&call.id)
                    {
                        _ = reply.try_send(Err(t!(
                            "ssh.send_failed",
                            error = format!("{error:#}")
                        )
                        .into_owned()));
                    }
                }
            }
        });
        let version: u32 = connection
            .call(Request::Hello(VERSION))
            .context(t!("ssh.handshake_failed"))?;
        ensure!(version == VERSION, t!("ssh.version_mismatch"));
        let git_edit = connection
            .call::<u32>(Request::Hello(GIT_EDIT_VERSION))
            .is_ok_and(|version| version == GIT_EDIT_VERSION);
        connection.git_edit.store(git_edit, Ordering::Relaxed);
        let batch = super::BATCH_VERSION;
        let supported = connection
            .call::<u32>(Request::Hello(batch))
            .is_ok_and(|reply| reply == batch);
        connection.batch.store(supported, Ordering::Relaxed);
        #[cfg(feature = "desktop")]
        {
            let review = super::REVIEW_VERSION;
            let supported = connection
                .call::<u32>(Request::Hello(review))
                .is_ok_and(|reply| reply == review);
            connection.review.store(supported, Ordering::Relaxed);
            let version = super::BLAME_VERSION;
            let supported = connection
                .call::<u32>(Request::Hello(version))
                .is_ok_and(|reply| reply == version);
            connection.blame.store(supported, Ordering::Relaxed);
            if connection.target.is_some() {
                let version = super::AI_CONTEXT_VERSION;
                let supports_ai_context = connection
                    .call::<u32>(Request::Hello(version))
                    .is_ok_and(|reply| reply == version);
                connection
                    .ai_context
                    .store(supports_ai_context, Ordering::Relaxed);

                let version = super::GIT_OPERATIONS_VERSION;
                let supported = connection
                    .call::<u32>(Request::Hello(version))
                    .is_ok_and(|reply| reply == version);
                connection
                    .git_operations
                    .store(supported, Ordering::Relaxed);
                let version = super::GIT_HISTORY_VERSION;
                let supported = supported
                    && connection
                        .call::<u32>(Request::Hello(version))
                        .is_ok_and(|reply| reply == version);
                connection.git_history.store(supported, Ordering::Relaxed);
                let version = super::WORKSPACE_FILES_VERSION;
                let supported = connection
                    .call::<u32>(Request::Hello(version))
                    .is_ok_and(|reply| reply == version);
                connection
                    .workspace_files
                    .store(supported, Ordering::Relaxed);
                let version = super::WORKSPACE_TRANSFER_VERSION;
                let supported = supported
                    && connection
                        .call::<u32>(Request::Hello(version))
                        .is_ok_and(|reply| reply == version);
                connection
                    .workspace_transfer
                    .store(supported, Ordering::Relaxed);
                let version = super::LSP_EDITS_VERSION;
                let supported = connection
                    .call::<u32>(Request::Hello(version))
                    .is_ok_and(|reply| reply == version);
                connection.lsp_edits.store(supported, Ordering::Relaxed);
            }
            let version = super::LOCATOR_VERSION;
            let supported = connection
                .call::<u32>(Request::Hello(version))
                .is_ok_and(|reply| reply == version);
            connection.locator.store(supported, Ordering::Relaxed);
        }
        Ok(connection)
    }

    #[cfg(feature = "desktop")]
    fn supports_image_diff(&self) -> bool {
        *self.image_diff.get_or_init(|| {
            let version = super::IMAGE_DIFF_VERSION;
            self.call::<u32>(Request::Hello(version))
                .is_ok_and(|reply| reply == version)
        })
    }

    #[cfg(feature = "desktop")]
    fn supports_revision_comparison(&self) -> bool {
        *self.revision_comparison.get_or_init(|| {
            let version = super::REVISION_COMPARISON_VERSION;
            self.call::<u32>(Request::Hello(version))
                .is_ok_and(|reply| reply == version)
        })
    }

    fn call<T: DeserializeOwned>(&self, request: Request) -> Result<T> {
        let timeout = match request {
            Request::TerminalInput(..)
            | Request::TerminalResize(..)
            | Request::TerminalPoll(_)
            | Request::TerminalClose(_) => Duration::from_secs(5),
            _ => Duration::from_secs(35),
        };
        let call = self.try_call(request, timeout)?.map_err(|request| {
            anyhow!(
                t!(
                    "ssh.queue_failed",
                    error = mpsc::TrySendError::Full(request)
                )
                .into_owned()
            )
        })?;
        let response = call
            .response
            .recv_timeout(call.deadline.saturating_duration_since(Instant::now()))
            .context(t!("ssh.timeout"))?;
        PendingCall::decode(response)
    }

    fn try_call(
        &self,
        request: Request,
        timeout: Duration,
    ) -> Result<Result<PendingCall, Request>> {
        let failure = self.failure.lock().unwrap();
        if let Some(error) = failure.as_ref() {
            bail!("{error}");
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, response) = mpsc::sync_channel(1);
        self.pending.lock().unwrap().insert(id, reply);
        drop(failure);
        let call = PendingCall {
            id,
            pending: self.pending.clone(),
            response,
            deadline: Instant::now() + timeout,
        };
        match self.writer.try_send(Call { id, request }) {
            Ok(()) => Ok(Ok(call)),
            Err(mpsc::TrySendError::Full(call)) => Ok(Err(call.request)),
            Err(error) => bail!(t!("ssh.queue_failed", error = error)),
        }
    }
}
