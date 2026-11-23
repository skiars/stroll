pub(crate) mod actions;
mod completion;
mod editor;
mod indentation;
mod process_memory;
mod transport;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use futures_channel::oneshot;
use gpui_component::input::{Rope, RopeExt};
use lsp_types::{
    CodeAction, CompletionContext, CompletionOptions, CompletionResponse, Diagnostic,
    DocumentSelector, FormattingOptions, GotoDefinitionResponse, Hover, HoverProviderCapability,
    Location, LocationLink, OneOf, Position, ProgressParams, ProgressParamsValue, ProgressToken,
    PublishDiagnosticsParams, Range, RegistrationParams, SemanticTokens, SemanticTokensFullOptions,
    SemanticTokensLegend, SemanticTokensServerCapabilities, ServerCapabilities,
    TextDocumentRegistrationOptions, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextDocumentSyncSaveOptions, UnregistrationParams, WorkDoneProgress,
};

use serde_json::{Value, json};
use url::Url;

use actions::ActionSnapshot;
pub(crate) use completion::CompletionActivation;
pub(crate) use editor::{
    DECLARATION, DEPRECATED, EditorLsp, READONLY, SEMANTIC_ROLES, SEMANTIC_VARIANTS,
    editor_diagnostics, editor_inlay_hints, editor_position, semantic_style_key,
};
pub(crate) use indentation::{Newline, NewlineServer};
pub(crate) use process_memory::{LocalProcess, ProcessMemory};
use transport::Transport;

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(12);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
const DOCUMENT_FORMATTING: &str = "textDocument/formatting";
const WORKSPACE_CONFIGURATION_CHANGED: &str = "workspace/didChangeConfiguration";
const DIAGNOSTIC_DEBOUNCE: Duration = Duration::from_millis(150);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(150);

pub use crate::language::LanguageServer;

#[derive(Clone)]
pub struct Client(Arc<ClientHandle>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionState {
    Starting,
    Ready,
    Stopping,
    Stopped,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServerProgress {
    pub title: String,
    pub message: Option<String>,
    pub percentage: Option<u32>,
}

type ActiveProgress = Arc<Mutex<Vec<(ProgressToken, ServerProgress)>>>;

struct ClientHandle {
    events: Sender<Event>,
    state: Arc<Mutex<SessionState>>,
    state_changes: async_channel::Receiver<()>,
    progress: ActiveProgress,
    local_process: Option<LocalProcess>,
    next_document: AtomicU64,
    stopped: async_channel::Receiver<()>,
    actions: Arc<Mutex<actions::Capabilities>>,
    references: Arc<AtomicBool>,
    completion: Arc<Mutex<Option<CompletionOptions>>>,
    signature: Arc<Mutex<Option<lsp_types::SignatureHelpOptions>>>,
}

impl Drop for ClientHandle {
    fn drop(&mut self) {
        _ = self.events.send(Event::Stop);
    }
}

pub(crate) struct ServerOptions {
    pub initialization: Value,
    pub workspace: Option<Value>,
}

impl From<Value> for ServerOptions {
    fn from(initialization: Value) -> Self {
        Self {
            initialization,
            workspace: None,
        }
    }
}

impl Client {
    pub fn start(command: Command, root: PathBuf, initialization: Value) -> Result<Self> {
        Self::start_with_command(move || Ok(command), root, initialization.into())
    }

    pub(crate) fn start_with_command(
        command: impl FnOnce() -> Result<Command> + Send + 'static,
        root: PathBuf,
        options: ServerOptions,
    ) -> Result<Self> {
        let uri = file_uri(&root)?;
        Self::start_session(command, Some(root), uri, options)
    }

    pub(crate) fn start_with_uri(
        command: impl FnOnce() -> Result<Command> + Send + 'static,
        uri: String,
        options: ServerOptions,
    ) -> Result<Self> {
        Self::start_session(command, None, uri, options)
    }

    pub(crate) fn stopped(&self) -> async_channel::Receiver<()> {
        self.0.stopped.clone()
    }

    fn start_session(
        command: impl FnOnce() -> Result<Command> + Send + 'static,
        cwd: Option<PathBuf>,
        uri: String,
        options: ServerOptions,
    ) -> Result<Self> {
        let process_id = cwd.as_ref().map(|_| std::process::id());
        let local_process = (cwd.is_some() && ProcessMemory::SUPPORTED).then(LocalProcess::default);
        let (events, inbox) = mpsc::channel();
        let state = Arc::new(Mutex::new(SessionState::Starting));
        let (changed, state_changes) = async_channel::bounded(1);
        let (running, stopped) = async_channel::bounded(1);
        let actions = Arc::new(Mutex::new(actions::Capabilities::default()));
        let references = Arc::new(AtomicBool::new(false));
        let completion = Arc::new(Mutex::new(None));
        let signature = Arc::new(Mutex::new(None));
        let progress = ActiveProgress::default();
        let handle = ClientHandle {
            events: events.clone(),
            state: state.clone(),
            state_changes,
            progress: progress.clone(),
            local_process: local_process.clone(),
            next_document: AtomicU64::new(0),
            stopped,
            actions: actions.clone(),
            references: references.clone(),
            completion: completion.clone(),
            signature: signature.clone(),
        };
        thread::Builder::new()
            .name("lsp-session".into())
            .spawn(move || {
                let stderr = Arc::new(Mutex::new(Vec::new()));
                let (stderr_done, stderr_drained) = mpsc::channel();
                let mut program = "startup".to_owned();
                let result = (|| {
                    let mut command = command()?;
                    if *state.lock().unwrap() == SessionState::Stopping {
                        return Ok(());
                    }
                    program = command.get_program().to_string_lossy().into_owned();
                    let wire = Transport::start(
                        &mut command,
                        cwd.as_deref(),
                        local_process,
                        events,
                        stderr.clone(),
                        stderr_done,
                    )
                    .with_context(|| format!("Could not start {program}"))?;
                    let mut session =
                        Session::new(wire, inbox, options, progress.clone(), changed.clone());
                    session.run(&uri, process_id, |capabilities| {
                        *actions.lock().unwrap() = actions::Capabilities::from(capabilities);
                        references.store(supports_references(capabilities), Ordering::Relaxed);
                        *completion.lock().unwrap() = capabilities.completion_provider.clone();
                        *signature.lock().unwrap() = capabilities.signature_help_provider.clone();
                        let mut state = state.lock().unwrap();
                        if *state == SessionState::Starting {
                            *state = SessionState::Ready;
                            _ = changed.try_send(());
                        }
                    })
                })();
                let final_state = match result {
                    Ok(()) => SessionState::Stopped,
                    Err(error) => {
                        eprintln!("LSP {program}: {error:#}");
                        // Descendant processes can keep the stderr pipe open after the server exits.
                        _ = stderr_drained.recv_timeout(Duration::from_millis(50));
                        let stderr = stderr.lock().unwrap();
                        let details = String::from_utf8_lossy(&stderr);
                        let details = details.trim();
                        SessionState::Failed(if details.is_empty() {
                            format!("{error:#}")
                        } else {
                            format!("{error:#}\n{details}")
                        })
                    }
                };
                *state.lock().unwrap() = final_state;
                progress.lock().unwrap().clear();
                _ = changed.try_send(());
                drop(running);
            })?;
        Ok(Self(Arc::new(handle)))
    }

    pub(crate) fn stop_on_quit(&self, cx: &gpui::App) {
        let client = Arc::downgrade(&self.0);
        let stopped = self.0.stopped.clone();
        cx.on_app_quit(move |_| {
            if let Some(client) = client.upgrade() {
                _ = client.events.send(Event::Stop);
            }
            let stopped = stopped.clone();
            async move { _ = stopped.recv().await }
        })
        .detach();
    }

    pub fn is_alive(&self) -> bool {
        matches!(
            *self.0.state.lock().unwrap(),
            SessionState::Starting | SessionState::Ready | SessionState::Stopping
        )
    }

    pub(crate) fn state(&self) -> SessionState {
        self.0.state.lock().unwrap().clone()
    }

    pub(crate) fn state_changes(&self) -> async_channel::Receiver<()> {
        self.0.state_changes.clone()
    }

    pub(crate) fn local_process(&self) -> Option<LocalProcess> {
        let process = self.0.local_process.as_ref()?;
        if matches!(self.state(), SessionState::Starting | SessionState::Ready)
            && process.is_running()
        {
            Some(process.clone())
        } else {
            None
        }
    }

    pub(crate) fn progress(&self) -> Option<ServerProgress> {
        if !matches!(
            *self.0.state.lock().unwrap(),
            SessionState::Starting | SessionState::Ready
        ) {
            return None;
        }
        self.0
            .progress
            .lock()
            .unwrap()
            .last()
            .map(|(_, progress)| progress.clone())
    }

    pub(crate) fn stop(&self) {
        let mut state = self.0.state.lock().unwrap();
        if matches!(*state, SessionState::Starting | SessionState::Ready) {
            *state = SessionState::Stopping;
            _ = self.0.events.send(Event::Stop);
        }
    }

    pub fn open_document(
        &self,
        path: &Path,
        language: &str,
        text: Rope,
        saved: Rope,
    ) -> Result<(Document, async_channel::Receiver<()>)> {
        self.open_uri(file_uri(path)?, language, text, saved)
    }

    pub(crate) fn open_uri(
        &self,
        uri: String,
        language: &str,
        text: Rope,
        saved: Rope,
    ) -> Result<(Document, async_channel::Receiver<()>)> {
        let id = self.0.next_document.fetch_add(1, Ordering::Relaxed);
        let (refresh, listener) = async_channel::bounded(1);
        let updates = SharedDocumentUpdates::default();
        let synced_version = Arc::new(AtomicI32::new(1));
        self.0
            .events
            .send(Event::Open {
                document: DocumentId {
                    uri: uri.clone(),
                    id,
                },
                language: language.to_owned(),
                text,
                saved,
                refresh,
                updates: updates.clone(),
                synced_version: synced_version.clone(),
            })
            .map_err(|_| anyhow!("The LSP session is closed"))?;
        Ok((
            Document {
                client: self.clone(),
                key: DocumentId { uri, id },
                updates,
                synced_version,
            },
            listener,
        ))
    }
}

#[derive(Clone, PartialEq, Eq)]
struct DocumentId {
    uri: String,
    id: u64,
}

type SharedDocumentUpdates = Arc<Mutex<DocumentUpdates>>;

#[derive(Default)]
struct DocumentUpdates {
    pending_changes: usize,
    diagnostics: Option<(Rope, Vec<Diagnostic>)>,
    semantic_refresh: bool,
    inlay_refresh: bool,
}

pub struct Document {
    client: Client,
    key: DocumentId,
    updates: SharedDocumentUpdates,
    synced_version: Arc<AtomicI32>,
}

impl Drop for Document {
    fn drop(&mut self) {
        _ = self.client.0.events.send(Event::Close(self.key.clone()));
    }
}

impl Document {
    pub(crate) fn take_updates(&self, text: &Rope) -> (bool, bool, Option<Vec<Diagnostic>>) {
        let mut updates = self.updates.lock().unwrap();
        // An edit can return to the same bytes before the worker has processed
        // both versions. Wait for queued changes before comparing snapshots.
        let diagnostics = if updates.pending_changes == 0 {
            updates
                .diagnostics
                .take()
                .and_then(|(snapshot, diagnostics)| (&snapshot == text).then_some(diagnostics))
        } else {
            None
        };
        (
            std::mem::take(&mut updates.semantic_refresh),
            std::mem::take(&mut updates.inlay_refresh),
            diagnostics,
        )
    }

    pub(crate) fn changed(&self, text: Rope) {
        let mut updates = self.updates.lock().unwrap();
        if self
            .client
            .0
            .events
            .send(Event::Change(self.key.clone(), text))
            .is_ok()
        {
            updates.pending_changes += 1;
        }
    }

    pub fn saved(&self, text: Rope) {
        _ = self
            .client
            .0
            .events
            .send(Event::Save(self.key.clone(), text));
    }

    pub(crate) fn completion_options(&self) -> Option<CompletionOptions> {
        self.client.0.completion.lock().unwrap().clone()
    }

    fn completions(
        &self,
        text: Rope,
        offset: usize,
        context: CompletionContext,
    ) -> impl Future<Output = Result<Option<CompletionResponse>>> + use<> {
        let position = lsp_position(&text, offset);
        let request = self.begin_request(Query::Completion(position, context), text);
        async move {
            let response = request?.await.context("The LSP session is closed")??;
            Ok(serde_json::from_value(response.value)?)
        }
    }

    pub async fn hover(&self, text: Rope, offset: usize) -> Result<Option<Hover>> {
        let position = lsp_position(&text, offset);
        let response = self.request(Query::Hover(position), text).await?;
        Ok(serde_json::from_value(response.value)?)
    }

    pub(crate) fn document_highlights(
        &self,
        text: Rope,
        offset: usize,
    ) -> impl Future<Output = Result<Vec<lsp_types::DocumentHighlight>>> + use<> {
        let position = lsp_position(&text, offset);
        let request = self.begin_request(Query::DocumentHighlights(position), text);
        async move {
            let response = request?.await.context("The LSP session is closed")??;
            Ok(serde_json::from_value::<Option<Vec<_>>>(response.value)?.unwrap_or_default())
        }
    }

    pub(crate) fn signature_options(&self) -> Option<lsp_types::SignatureHelpOptions> {
        self.client.0.signature.lock().unwrap().clone()
    }

    pub(crate) fn signature_help(
        &self,
        text: Rope,
        offset: usize,
        context: lsp_types::SignatureHelpContext,
    ) -> impl Future<Output = Result<Option<lsp_types::SignatureHelp>>> + use<> {
        let position = lsp_position(&text, offset);
        let request = self.begin_request(Query::SignatureHelp(position, context), text);
        async move {
            let response = request?.await.context("The LSP session is closed")??;
            Ok(serde_json::from_value(response.value)?)
        }
    }

    pub async fn definitions(&self, text: Rope, offset: usize) -> Result<Vec<LocationLink>> {
        self.definitions_at_version(text, offset, None).await
    }

    pub(crate) fn synced_version(&self) -> i32 {
        self.synced_version.load(Ordering::Acquire)
    }

    fn definitions_at_version(
        &self,
        text: Rope,
        offset: usize,
        version: Option<i32>,
    ) -> impl Future<Output = Result<Vec<LocationLink>>> + use<> {
        let position = lsp_position(&text, offset);
        let request = self.begin_request(Query::Definition(position, version), text);
        async move {
            let response = request?.await.context("The LSP session is closed")??;
            let response: Option<GotoDefinitionResponse> = serde_json::from_value(response.value)?;
            let locations = match response {
                Some(GotoDefinitionResponse::Link(links)) => return Ok(links),
                Some(GotoDefinitionResponse::Scalar(location)) => vec![location],
                Some(GotoDefinitionResponse::Array(locations)) => locations,
                None => Vec::new(),
            };
            Ok(locations
                .into_iter()
                .map(|location| LocationLink {
                    origin_selection_range: None,
                    target_uri: location.uri,
                    target_range: location.range,
                    target_selection_range: location.range,
                })
                .collect())
        }
    }

    pub(crate) async fn locator_symbols(&self, text: Rope, query: Option<String>) -> Result<Value> {
        let query = query.map_or(Query::DocumentSymbols, Query::WorkspaceSymbols);
        Ok(self.request(query, text).await?.value)
    }

    pub(crate) fn supports_references(&self) -> bool {
        self.client.state() == SessionState::Ready
            && self.client.0.references.load(Ordering::Relaxed)
    }

    pub(crate) async fn references(
        &self,
        text: Rope,
        offset: usize,
        include_declaration: bool,
    ) -> Result<Vec<Location>> {
        let position = lsp_position(&text, offset);
        let response = self
            .request(Query::References(position, include_declaration), text)
            .await?;
        Ok(serde_json::from_value::<Option<Vec<Location>>>(response.value)?.unwrap_or_default())
    }

    pub(crate) async fn resolve_locator_symbol(&self, text: Rope, symbol: Value) -> Result<Value> {
        Ok(self
            .request(Query::ResolveSymbol(symbol), text)
            .await?
            .value)
    }

    pub fn semantic_tokens(
        &self,
        text: Rope,
    ) -> impl Future<Output = Result<Option<(SemanticTokens, SemanticTokensLegend)>>> + use<> {
        let end = lsp_position(&text, text.len());
        let request = self.begin_request(Query::Tokens(Range::new(Position::default(), end)), text);
        async move {
            let response = request?.await.context("The LSP session is closed")??;
            let tokens: Option<SemanticTokens> = serde_json::from_value(response.value)?;
            Ok(tokens.zip(response.legend))
        }
    }

    pub(crate) fn inlay_hints(
        &self,
        text: &Rope,
        offsets: std::ops::Range<usize>,
    ) -> impl Future<Output = Result<Vec<lsp_types::InlayHint>>> + use<> {
        let range = Range::new(
            lsp_position(text, offsets.start),
            lsp_position(text, offsets.end),
        );
        // Enqueue against the snapshot captured by the UI before yielding to later edits.
        let request = self.begin_request(Query::InlayHints(range), text.clone());
        async move {
            let response = request?.await.context("The LSP session is closed")??;
            let hints: Option<Vec<lsp_types::InlayHint>> = serde_json::from_value(response.value)?;
            Ok(hints.unwrap_or_default())
        }
    }

    async fn request(&self, query: Query, text: Rope) -> Result<Response> {
        self.begin_request(query, text)?
            .await
            .context("The LSP session is closed")?
    }

    fn begin_request(
        &self,
        query: Query,
        text: Rope,
    ) -> Result<oneshot::Receiver<Result<Response>>> {
        let (reply, result) = oneshot::channel();
        self.client
            .0
            .events
            .send(Event::Query {
                document: self.key.clone(),
                query,
                text,
                reply,
            })
            .map_err(|_| anyhow!("The LSP session is closed"))?;
        Ok(result)
    }
}

enum Query {
    Completion(Position, CompletionContext),
    Hover(Position),
    DocumentHighlights(Position),
    SignatureHelp(Position, lsp_types::SignatureHelpContext),
    Definition(Position, Option<i32>),
    References(Position, bool),
    Tokens(Range),
    InlayHints(Range),
    DocumentSymbols,
    WorkspaceSymbols(String),
    ResolveSymbol(Value),
    CodeActions(Range),
    ResolveAction(CodeAction, i32),
    PrepareRename(Position),
    Rename(Position, String, Option<i32>),
    Format(FormattingOptions),
    IndentOnType(Position),
    RustOnEnter(Position, Rope),
}

#[derive(Default)]
struct Response {
    value: Value,
    legend: Option<SemanticTokensLegend>,
    snapshot: Option<ActionSnapshot>,
}

type Reply = oneshot::Sender<Result<Response>>;

enum Event {
    Open {
        document: DocumentId,
        language: String,
        text: Rope,
        saved: Rope,
        refresh: async_channel::Sender<()>,
        updates: SharedDocumentUpdates,
        synced_version: Arc<AtomicI32>,
    },
    Close(DocumentId),
    Change(DocumentId, Rope),
    Save(DocumentId, Rope),
    Query {
        document: DocumentId,
        query: Query,
        text: Rope,
        reply: Reply,
    },
    Incoming(Value),
    Disconnected(anyhow::Error),
    Stop,
}

struct OpenDocument {
    id: u64,
    language: String,
    text: Rope,
    version: i32,
    synced_version: Arc<AtomicI32>,
    diagnostics: Vec<Diagnostic>,
    refresh: async_channel::Sender<()>,
    updates: SharedDocumentUpdates,
    pull_due: Option<Instant>,
    pull_request: Option<u64>,
}

impl OpenDocument {
    fn publish_diagnostics(&mut self, version: Option<i32>, diagnostics: Vec<Diagnostic>) {
        if version.is_none_or(|version| self.version == version) {
            self.diagnostics.clone_from(&diagnostics);
            self.updates.lock().unwrap().diagnostics = Some((self.text.clone(), diagnostics));
            _ = self.refresh.try_send(());
        }
    }
}

struct Pending {
    deadline: Instant,
    kind: PendingKind,
}

enum PendingKind {
    Query {
        reply: Reply,
        legend: Option<SemanticTokensLegend>,
        snapshot: Option<ActionSnapshot>,
    },
    Diagnostics {
        document: DocumentId,
        version: i32,
    },
}

struct Session {
    wire: Transport,
    inbox: Receiver<Event>,
    options: ServerOptions,
    progress: ActiveProgress,
    changed: async_channel::Sender<()>,
    capabilities: ServerCapabilities,
    formatting_registrations: HashMap<String, Option<DocumentSelector>>,
    documents: HashMap<String, OpenDocument>,
    pending: HashMap<u64, Pending>,
    next_request: u64,
}

impl Session {
    fn new(
        wire: Transport,
        inbox: Receiver<Event>,
        options: ServerOptions,
        progress: ActiveProgress,
        changed: async_channel::Sender<()>,
    ) -> Self {
        Self {
            wire,
            inbox,
            options,
            progress,
            changed,
            capabilities: ServerCapabilities::default(),
            formatting_registrations: HashMap::new(),
            documents: HashMap::new(),
            pending: HashMap::new(),
            next_request: 1,
        }
    }

    fn run(
        &mut self,
        root_uri: &str,
        process_id: Option<u32>,
        initialized: impl FnOnce(&ServerCapabilities),
    ) -> Result<()> {
        let uri = root_uri.to_owned();
        self.wire.send(json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "processId": process_id,
                "clientInfo": { "name": "stroll" },
                "rootUri": uri,
                "workspaceFolders": [{ "uri": uri, "name": root_uri.rsplit('/').next().unwrap_or_default() }],
                "initializationOptions": self.options.initialization,
                "capabilities": {
                    "general": { "positionEncodings": ["utf-16"] },
                    "window": { "workDoneProgress": true },
                    "workspace": {
                        "configuration": true,
                        "workspaceEdit": { "documentChanges": true },
                        "symbol": { "resolveSupport": { "properties": ["location.range"] } },
                        "semanticTokens": { "refreshSupport": true },
                        "inlayHint": { "refreshSupport": true },
                        "diagnostics": { "refreshSupport": true }
                    },
                    "textDocument": {
                        "hover": { "contentFormat": ["markdown", "plaintext"] },
                        "documentHighlight": {},
                        "signatureHelp": {
                            "contextSupport": true,
                            "signatureInformation": {
                                "parameterInformation": { "labelOffsetSupport": true },
                                "activeParameterSupport": true
                            }
                        },
                        "completion": {
                            "contextSupport": true,
                            "completionItem": {
                                "documentationFormat": ["markdown", "plaintext"],
                                "insertReplaceSupport": true,
                                "snippetSupport": false
                            }
                        },
                        "definition": { "linkSupport": true },
                        "references": {},
                        "inlayHint": {},
                        "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                        "publishDiagnostics": { "versionSupport": true },
                        "diagnostic": { "relatedDocumentSupport": false },
                        "codeAction": {
                            "codeActionLiteralSupport": { "codeActionKind": { "valueSet": ["", "quickfix", "refactor", "source"] } },
                            "isPreferredSupport": true,
                            "disabledSupport": true,
                            "dataSupport": true,
                            "resolveSupport": { "properties": ["edit"] }
                        },
                        "rename": { "prepareSupport": true },
                        "formatting": { "dynamicRegistration": true },
                        "onTypeFormatting": { "dynamicRegistration": false },
                        "semanticTokens": {
                            "requests": { "range": true, "full": true },
                            "tokenTypes": editor::TOKEN_TYPES,
                            "tokenModifiers": editor::TOKEN_MODIFIERS,
                            "formats": ["relative"],
                            "multilineTokenSupport": false,
                            "overlappingTokenSupport": false
                        }
                    }
                }
            }
        }))?;
        let deadline = Instant::now() + INITIALIZE_TIMEOUT;
        let mut deferred = Vec::new();
        loop {
            match self
                .inbox
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .context("LSP initialization timed out or disconnected")?
            {
                Event::Incoming(message)
                    if message["id"] == 0 && message.get("method").is_none() =>
                {
                    self.capabilities =
                        serde_json::from_value(response_value(&message)?["capabilities"].clone())?;
                    ensure!(
                        self.capabilities
                            .position_encoding
                            .as_ref()
                            .is_none_or(|encoding| encoding.as_str() == "utf-16"),
                        "The language server used a position encoding that was not negotiated"
                    );
                    break;
                }
                Event::Incoming(message) => self.incoming(message)?,
                Event::Disconnected(error) => {
                    return Err(error.context("LSP initialization was interrupted"));
                }
                Event::Stop => return Ok(()),
                event => deferred.push(event),
            }
        }
        self.notify("initialized", json!({}))?;
        if let Some(settings) = &self.options.workspace {
            self.notify(
                WORKSPACE_CONFIGURATION_CHANGED,
                json!({ "settings": settings }),
            )?;
        }
        initialized(&self.capabilities);
        for event in deferred {
            self.handle(event)?;
        }
        loop {
            self.expire_requests()?;
            self.pull_due_diagnostics()?;
            let deadline = self
                .pending
                .values()
                .map(|item| item.deadline)
                .chain(self.documents.values().filter_map(|doc| doc.pull_due))
                .min();
            let event = match deadline {
                Some(deadline) => self
                    .inbox
                    .recv_timeout(deadline.saturating_duration_since(Instant::now())),
                None => self
                    .inbox
                    .recv()
                    .map_err(|_| RecvTimeoutError::Disconnected),
            };
            match event {
                Ok(Event::Stop) => return self.shutdown(),
                Ok(event) => self.handle(event)?,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }

    fn handle(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Open {
                document,
                language,
                text,
                saved,
                refresh,
                updates,
                synced_version,
            } => {
                let uri = document.uri;
                let pull_due = self.diagnostic_deadline();
                let (open_close, _) = self.sync_options();
                if open_close {
                    if self.documents.contains_key(&uri) {
                        self.notify(
                            "textDocument/didClose",
                            json!({ "textDocument": { "uri": uri } }),
                        )?;
                    }
                    self.notify("textDocument/didOpen", json!({
                        "textDocument": { "uri": uri, "languageId": language, "version": 1, "text": text.to_string() }
                    }))?;
                }
                self.documents.insert(
                    uri.clone(),
                    OpenDocument {
                        id: document.id,
                        language,
                        text: if open_close { text.clone() } else { saved },
                        version: 1,
                        synced_version,
                        diagnostics: Vec::new(),
                        refresh,
                        updates,
                        pull_due,
                        pull_request: None,
                    },
                );
                if !open_close {
                    self.sync(
                        &DocumentId {
                            uri,
                            id: document.id,
                        },
                        text,
                    )?;
                }
            }
            Event::Close(document) => {
                if self
                    .documents
                    .get(&document.uri)
                    .is_some_and(|open| open.id == document.id)
                {
                    self.documents.remove(&document.uri);
                    if self.sync_options().0 {
                        self.notify(
                            "textDocument/didClose",
                            json!({ "textDocument": { "uri": document.uri } }),
                        )?;
                    }
                }
            }
            Event::Change(document, text) => {
                self.sync(&document, text)?;
                if let Some(open) = self
                    .documents
                    .get(&document.uri)
                    .filter(|open| open.id == document.id)
                {
                    let mut updates = open.updates.lock().unwrap();
                    updates.pending_changes -= 1;
                    if updates.pending_changes == 0 && updates.diagnostics.is_some() {
                        _ = open.refresh.try_send(());
                    }
                }
            }
            Event::Save(document, text) => {
                if self.sync(&document, text.clone())? {
                    let pull_due = self.diagnostic_deadline();
                    if let Some(open) = self.documents.get_mut(&document.uri) {
                        open.pull_due = pull_due;
                    }
                    let save = match self.capabilities.text_document_sync.as_ref() {
                        Some(TextDocumentSyncCapability::Options(options)) => options.save.as_ref(),
                        _ => None,
                    };
                    if let Some(save) = save
                        && *save != TextDocumentSyncSaveOptions::Supported(false)
                    {
                        let mut params = json!({ "textDocument": { "uri": document.uri } });
                        if let TextDocumentSyncSaveOptions::SaveOptions(options) = save
                            && options.include_text == Some(true)
                        {
                            params["text"] = json!(text.to_string());
                        }
                        self.notify("textDocument/didSave", params)?;
                    }
                }
            }
            Event::Query {
                document,
                query,
                text,
                reply,
            } => {
                if !reply.is_canceled() {
                    self.query(document, query, text, reply)?;
                }
            }
            Event::Incoming(message) => self.incoming(message)?,
            Event::Disconnected(error) => {
                return Err(error.context("The language server disconnected"));
            }
            Event::Stop => unreachable!("handled by the session loop"),
        }
        Ok(())
    }

    fn sync_options(&self) -> (bool, TextDocumentSyncKind) {
        match self.capabilities.text_document_sync.as_ref() {
            Some(TextDocumentSyncCapability::Options(options)) => (
                options.open_close.unwrap_or(false),
                options.change.unwrap_or(TextDocumentSyncKind::NONE),
            ),
            Some(TextDocumentSyncCapability::Kind(kind)) => {
                (*kind != TextDocumentSyncKind::NONE, *kind)
            }
            None => (false, TextDocumentSyncKind::NONE),
        }
    }

    fn sync(&mut self, key: &DocumentId, text: Rope) -> Result<bool> {
        let (_, kind) = self.sync_options();
        let pull_due = self.diagnostic_deadline();
        let Some(document) = self
            .documents
            .get_mut(&key.uri)
            .filter(|open| open.id == key.id)
        else {
            return Ok(false);
        };
        if document.text != text {
            if kind == TextDocumentSyncKind::NONE {
                return Ok(false);
            }
            let mut change = json!({ "text": text.to_string() });
            if kind == TextDocumentSyncKind::INCREMENTAL {
                change["range"] = json!(Range::new(
                    Position::default(),
                    lsp_position(&document.text, document.text.len())
                ));
            }
            document.version += 1;
            document
                .synced_version
                .store(document.version, Ordering::Release);
            document.text = text;
            document.diagnostics.clear();
            document.pull_due = pull_due;
            document.pull_request = None;
            document.updates.lock().unwrap().diagnostics = None;
            let version = document.version;
            self.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": key.uri, "version": version },
                    "contentChanges": [change]
                }),
            )?;
        }
        Ok(true)
    }

    fn query(
        &mut self,
        document: DocumentId,
        query: Query,
        text: Rope,
        reply: Reply,
    ) -> Result<()> {
        if let Query::Definition(_, Some(version)) = &query
            && self.documents.get(&document.uri).is_some_and(|open| {
                open.id == document.id && open.version != *version && open.text != text
            })
        {
            _ = reply.send(Ok(Response::default()));
            return Ok(());
        }
        if matches!(query, Query::Completion(..))
            && (reply.is_canceled() || self.capabilities.completion_provider.is_none())
        {
            _ = reply.send(Ok(Response::default()));
            return Ok(());
        }
        if matches!(query, Query::Tokens(_) | Query::InlayHints(_)) && reply.is_canceled() {
            return Ok(());
        }
        let supported = match &query {
            Query::DocumentHighlights(_) => self
                .capabilities
                .document_highlight_provider
                .as_ref()
                .is_some_and(|capability| *capability != OneOf::Left(false)),
            Query::SignatureHelp(..) => self.capabilities.signature_help_provider.is_some(),
            _ => true,
        };
        if !supported
            || (matches!(
                query,
                Query::DocumentHighlights(_) | Query::SignatureHelp(..)
            ) && reply.is_canceled())
        {
            _ = reply.send(Ok(Response::default()));
            return Ok(());
        }
        let supports_newline = match &query {
            Query::IndentOnType(_) => self
                .capabilities
                .document_on_type_formatting_provider
                .as_ref()
                .is_some_and(|options| {
                    options.first_trigger_character == "\n"
                        || options
                            .more_trigger_character
                            .as_ref()
                            .is_some_and(|triggers| triggers.iter().any(|trigger| trigger == "\n"))
                }),
            Query::RustOnEnter(..) => {
                self.capabilities
                    .experimental
                    .as_ref()
                    .and_then(|options| options.get("onEnter"))
                    .and_then(Value::as_bool)
                    == Some(true)
            }
            _ => true,
        };
        if !supports_newline
            || (matches!(query, Query::IndentOnType(_) | Query::RustOnEnter(..))
                && reply.is_canceled())
        {
            _ = reply.send(Ok(Response::default()));
            return Ok(());
        }
        if matches!(query, Query::InlayHints(_))
            && self
                .capabilities
                .inlay_hint_provider
                .as_ref()
                .is_none_or(|capability| matches!(capability, OneOf::Left(false)))
        {
            _ = reply.send(Ok(Response::default()));
            return Ok(());
        }
        if matches!(query, Query::Format(_)) {
            if reply.is_canceled() {
                return Ok(());
            }
            if let Some(open) = self.documents.get(&document.uri)
                && open.id == document.id
                && !self.supports_formatting(&document.uri, &open.language)
            {
                _ = reply.send(Ok(Response::default()));
                return Ok(());
            }
        }
        let action_request = matches!(
            query,
            Query::CodeActions(..)
                | Query::ResolveAction(..)
                | Query::PrepareRename(..)
                | Query::Rename(..)
                | Query::Format(..)
        );
        let workspace_request = matches!(query, Query::Rename(..));
        let expected = match &query {
            Query::ResolveAction(_, version) => Some(*version),
            Query::Rename(_, _, version) => *version,
            _ => None,
        };
        if let Some(version) = expected
            && !self.documents.get(&document.uri).is_some_and(|open| {
                open.id == document.id && open.version == version && open.text == text
            })
        {
            _ = reply.send(Err(actions::stale()));
            return Ok(());
        }
        if !self.sync(&document, text)? {
            _ = reply.send(if action_request {
                Err(actions::stale())
            } else {
                Ok(Response::default())
            });
            return Ok(());
        }
        let mut params = json!({ "textDocument": { "uri": document.uri } });
        let open = &self.documents[&document.uri];
        let mut after_enter = None;
        let (method, legend) = match query {
            Query::IndentOnType(position) => {
                let tab = gpui_component::input::TabSize::default();
                params["position"] = json!(position);
                params["ch"] = json!("\n");
                params["options"] =
                    json!({ "tabSize": tab.tab_size, "insertSpaces": !tab.hard_tabs });
                (Some("textDocument/onTypeFormatting"), None)
            }
            Query::RustOnEnter(position, after) => {
                params["position"] = json!(position);
                after_enter = Some(after);
                (Some("experimental/onEnter"), None)
            }
            Query::Completion(position, context) => {
                params["position"] = json!(position);
                params["context"] = json!(context);
                (Some("textDocument/completion"), None)
            }
            Query::Format(options) => {
                params["options"] = json!(options);
                (Some(DOCUMENT_FORMATTING), None)
            }
            Query::CodeActions(range)
                if self
                    .capabilities
                    .code_action_provider
                    .as_ref()
                    .is_some_and(|cap| {
                        *cap != lsp_types::CodeActionProviderCapability::Simple(false)
                    }) =>
            {
                let diagnostics: Vec<_> = open
                    .diagnostics
                    .iter()
                    .filter(|item| item.range.start <= range.end && item.range.end >= range.start)
                    .collect();
                params["range"] = json!(range);
                params["context"] = json!({
                    "diagnostics": diagnostics,
                    "triggerKind": 1,
                });
                (Some("textDocument/codeAction"), None)
            }
            Query::ResolveAction(action, _)
                if matches!(self.capabilities.code_action_provider,
                    Some(lsp_types::CodeActionProviderCapability::Options(ref options))
                    if options.resolve_provider == Some(true)) =>
            {
                params = json!(action);
                (Some("codeAction/resolve"), None)
            }
            Query::PrepareRename(position)
                if matches!(self.capabilities.rename_provider,
                    Some(OneOf::Right(ref options)) if options.prepare_provider == Some(true)) =>
            {
                params["position"] = json!(position);
                (Some("textDocument/prepareRename"), None)
            }
            Query::Rename(position, name, _)
                if self
                    .capabilities
                    .rename_provider
                    .as_ref()
                    .is_some_and(|cap| *cap != OneOf::Left(false)) =>
            {
                params["position"] = json!(position);
                params["newName"] = json!(name);
                (Some("textDocument/rename"), None)
            }
            Query::DocumentHighlights(position) => {
                params["position"] = json!(position);
                (Some("textDocument/documentHighlight"), None)
            }
            Query::SignatureHelp(position, context) => {
                params["position"] = json!(position);
                params["context"] = json!(context);
                (Some("textDocument/signatureHelp"), None)
            }
            Query::Hover(position)
                if self
                    .capabilities
                    .hover_provider
                    .as_ref()
                    .is_some_and(|cap| *cap != HoverProviderCapability::Simple(false)) =>
            {
                params["position"] = json!(position);
                (Some("textDocument/hover"), None)
            }
            Query::Definition(position, _)
                if self
                    .capabilities
                    .definition_provider
                    .as_ref()
                    .is_some_and(|cap| *cap != OneOf::Left(false)) =>
            {
                params["position"] = json!(position);
                (Some("textDocument/definition"), None)
            }
            Query::References(position, include_declaration)
                if supports_references(&self.capabilities) =>
            {
                params["position"] = json!(position);
                params["context"] = json!({ "includeDeclaration": include_declaration });
                (Some("textDocument/references"), None)
            }
            Query::DocumentSymbols
                if self
                    .capabilities
                    .document_symbol_provider
                    .as_ref()
                    .is_some_and(|cap| *cap != OneOf::Left(false)) =>
            {
                (Some("textDocument/documentSymbol"), None)
            }
            Query::WorkspaceSymbols(query)
                if self
                    .capabilities
                    .workspace_symbol_provider
                    .as_ref()
                    .is_some_and(|cap| *cap != OneOf::Left(false)) =>
            {
                params = json!({ "query": query });
                (Some("workspace/symbol"), None)
            }
            Query::ResolveSymbol(symbol)
                if matches!(
                    self.capabilities.workspace_symbol_provider,
                    Some(OneOf::Right(ref options)) if options.resolve_provider == Some(true)
                ) =>
            {
                params = symbol;
                (Some("workspaceSymbol/resolve"), None)
            }
            Query::References(..)
            | Query::DocumentSymbols
            | Query::WorkspaceSymbols(_)
            | Query::ResolveSymbol(_) => {
                _ = reply.send(Err(anyhow!(
                    "The language service does not support this feature"
                )));
                return Ok(());
            }
            Query::InlayHints(range) => {
                params["range"] = json!(range);
                (Some("textDocument/inlayHint"), None)
            }
            Query::Tokens(range) => {
                let options = self
                    .capabilities
                    .semantic_tokens_provider
                    .as_ref()
                    .map(|cap| match cap {
                        SemanticTokensServerCapabilities::SemanticTokensOptions(options) => options,
                        SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(
                            options,
                        ) => &options.semantic_tokens_options,
                    });
                let method = options.and_then(|options| {
                    if options
                        .full
                        .as_ref()
                        .is_some_and(|full| *full != SemanticTokensFullOptions::Bool(false))
                    {
                        Some("textDocument/semanticTokens/full")
                    } else if options.range == Some(true) {
                        params["range"] = json!(range);
                        Some("textDocument/semanticTokens/range")
                    } else {
                        None
                    }
                });
                (method, options.map(|options| options.legend.clone()))
            }
            _ => (None, None),
        };
        if let Some(method) = method {
            let snapshot = action_request.then(|| ActionSnapshot {
                source: document.clone(),
                version: open.version,
                documents: if workspace_request {
                    self.documents
                        .iter()
                        .map(|(uri, open)| (uri.clone(), (open.version, open.text.clone())))
                        .collect()
                } else {
                    HashMap::new()
                },
            });
            let id = self.next_request;
            self.next_request += 1;
            self.wire
                .send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
            self.pending.insert(
                id,
                Pending {
                    deadline: Instant::now() + REQUEST_TIMEOUT,
                    kind: PendingKind::Query {
                        reply,
                        legend,
                        snapshot,
                    },
                },
            );
        } else {
            let _ = reply.send(if action_request {
                Err(anyhow!(
                    "The language service does not support this feature"
                ))
            } else {
                Ok(Response::default())
            });
        }
        if let Some(after) = after_enter {
            self.sync(&document, after)?;
        }
        Ok(())
    }

    fn supports_formatting(&self, uri: &str, language: &str) -> bool {
        if self
            .capabilities
            .document_formatting_provider
            .as_ref()
            .is_some_and(|capability| !matches!(capability, OneOf::Left(false)))
        {
            return true;
        }
        let Ok(uri) = Url::parse(uri) else {
            return false;
        };
        // Match the URI's decoded path on either host, including SSH files
        // whose path syntax differs from the desktop's.
        let path = percent_encoding::percent_decode_str(uri.path()).decode_utf8_lossy();
        self.formatting_registrations.values().any(|selector| {
            selector.as_ref().is_none_or(|filters| {
                filters.iter().any(|filter| {
                    filter.language.as_deref().is_none_or(|id| id == language)
                        && filter
                            .scheme
                            .as_deref()
                            .is_none_or(|scheme| scheme == uri.scheme())
                        && filter.pattern.as_deref().is_none_or(|pattern| {
                            globset::GlobBuilder::new(pattern)
                                .literal_separator(true)
                                .build()
                                .is_ok_and(|glob| glob.compile_matcher().is_match(path.as_ref()))
                        })
                })
            })
        })
    }

    fn update_registrations(&mut self, method: &str, params: Value) -> Result<()> {
        if method == "client/registerCapability" {
            let params: RegistrationParams = serde_json::from_value(params)?;
            let registrations = params
                .registrations
                .into_iter()
                .filter(|registration| registration.method != WORKSPACE_CONFIGURATION_CHANGED)
                .map(|registration| {
                    ensure!(
                        registration.method == DOCUMENT_FORMATTING,
                        "Unsupported registration method"
                    );
                    let options = registration
                        .register_options
                        .map(serde_json::from_value::<Option<TextDocumentRegistrationOptions>>)
                        .transpose()?
                        .flatten()
                        .unwrap_or_default();
                    Ok((registration.id, options.document_selector))
                })
                .collect::<Result<Vec<_>>>()?;
            self.formatting_registrations.extend(registrations);
        } else {
            let params: UnregistrationParams = serde_json::from_value(params)?;
            ensure!(
                params.unregisterations.iter().all(|item| matches!(
                    item.method.as_str(),
                    DOCUMENT_FORMATTING | WORKSPACE_CONFIGURATION_CHANGED
                )),
                "Unsupported registration method"
            );
            for item in params.unregisterations {
                self.formatting_registrations.remove(&item.id);
            }
        }
        Ok(())
    }

    fn incoming(&mut self, message: Value) -> Result<()> {
        if let Some(method) = message["method"].as_str() {
            if let Some(id) = message.get("id") {
                let response = match method {
                    "workspace/configuration" => {
                        let values: Vec<_> = message["params"]["items"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|item| self.configuration(item["section"].as_str()))
                            .collect();
                        json!({ "result": values })
                    }
                    "workspace/semanticTokens/refresh" => {
                        for document in self.documents.values() {
                            document.updates.lock().unwrap().semantic_refresh = true;
                            _ = document.refresh.try_send(());
                        }
                        json!({ "result": null })
                    }
                    "workspace/inlayHint/refresh" => {
                        for document in self.documents.values() {
                            document.updates.lock().unwrap().inlay_refresh = true;
                            _ = document.refresh.try_send(());
                        }
                        json!({ "result": null })
                    }
                    "workspace/diagnostic/refresh" => {
                        let due = self.diagnostic_deadline();
                        for document in self.documents.values_mut() {
                            document.pull_due = due;
                        }
                        json!({ "result": null })
                    }
                    "window/workDoneProgress/create" => json!({ "result": null }),
                    "client/registerCapability" | "client/unregisterCapability" => {
                        match self.update_registrations(method, message["params"].clone()) {
                            Ok(()) => json!({ "result": null }),
                            Err(error) => {
                                json!({ "error": { "code": -32602, "message": error.to_string() } })
                            }
                        }
                    }
                    _ => json!({ "error": { "code": -32601, "message": "Method not found" } }),
                };
                let mut response = response;
                response["jsonrpc"] = json!("2.0");
                response["id"] = id.clone();
                self.wire.send(response)?;
            } else if method == "$/progress" {
                if let Ok(params) = serde_json::from_value(message["params"].clone()) {
                    self.update_progress(params);
                }
            } else if method == "textDocument/publishDiagnostics"
                && let Ok(params) =
                    serde_json::from_value::<PublishDiagnosticsParams>(message["params"].clone())
                && let Some(document) = self.documents.get_mut(params.uri.as_str())
            {
                document.publish_diagnostics(params.version, params.diagnostics);
            }
        } else if let Some(id) = message["id"].as_u64()
            && let Some(pending) = self.pending.remove(&id)
        {
            match pending.kind {
                PendingKind::Query {
                    reply,
                    legend,
                    snapshot,
                } => {
                    _ = reply.send(response_value(&message).and_then(|value| {
                        if let Some(snapshot) = &snapshot
                            && !self
                                .documents
                                .get(&snapshot.source.uri)
                                .is_some_and(|open| {
                                    open.id == snapshot.source.id
                                        && open.version == snapshot.version
                                })
                        {
                            return Err(actions::stale());
                        }
                        Ok(Response {
                            value,
                            legend,
                            snapshot,
                        })
                    }));
                }
                PendingKind::Diagnostics { document, version } => {
                    if let Some(open) = self.documents.get_mut(&document.uri)
                        && open.id == document.id
                        && open.pull_request == Some(id)
                    {
                        open.pull_request = None;
                        if let Ok(result) = response_value(&message)
                            && let Ok(lsp_types::DocumentDiagnosticReportResult::Report(
                                lsp_types::DocumentDiagnosticReport::Full(report),
                            )) = serde_json::from_value(result)
                        {
                            open.publish_diagnostics(
                                Some(version),
                                report.full_document_diagnostic_report.items,
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn update_progress(&self, params: ProgressParams) {
        let ProgressParamsValue::WorkDone(update) = params.value;
        let mut tasks = self.progress.lock().unwrap();
        let index = tasks.iter().position(|(token, _)| token == &params.token);
        // Keep the newest active task visible until it ends, even when older tasks report.
        match update {
            WorkDoneProgress::Begin(begin) => {
                if let Some(index) = index {
                    tasks.remove(index);
                }
                tasks.push((
                    params.token,
                    ServerProgress {
                        title: begin.title,
                        message: begin.message,
                        percentage: begin.percentage.filter(|value| *value <= 100),
                    },
                ));
            }
            WorkDoneProgress::Report(report) => {
                let Some(index) = index else { return };
                let progress = &mut tasks[index].1;
                if let Some(message) = report.message {
                    progress.message = Some(message);
                }
                if let Some(value) = report.percentage.filter(|value| *value <= 100) {
                    progress.percentage = Some(value);
                }
            }
            WorkDoneProgress::End(_) => {
                let Some(index) = index else { return };
                tasks.remove(index);
            }
        }
        _ = self.changed.try_send(());
    }

    fn diagnostic_deadline(&self) -> Option<Instant> {
        self.capabilities
            .diagnostic_provider
            .as_ref()
            .map(|_| Instant::now() + DIAGNOSTIC_DEBOUNCE)
    }

    fn pull_due_diagnostics(&mut self) -> Result<()> {
        let Some(provider) = &self.capabilities.diagnostic_provider else {
            return Ok(());
        };
        let identifier = match provider {
            lsp_types::DiagnosticServerCapabilities::Options(options) => &options.identifier,
            lsp_types::DiagnosticServerCapabilities::RegistrationOptions(options) => {
                &options.diagnostic_options.identifier
            }
        };
        let now = Instant::now();
        for (uri, open) in &mut self.documents {
            if !open.pull_due.is_some_and(|deadline| deadline <= now) {
                continue;
            }
            let mut params = json!({ "textDocument": { "uri": uri } });
            if let Some(identifier) = identifier {
                params["identifier"] = json!(identifier);
            }
            let id = self.next_request;
            if !self.wire.try_send(json!({
                "jsonrpc": "2.0", "id": id, "method": "textDocument/diagnostic", "params": params
            }))? {
                open.pull_due = Some(now + DIAGNOSTIC_DEBOUNCE);
                continue;
            }
            self.next_request += 1;
            open.pull_due = None;
            open.pull_request = Some(id);
            self.pending.insert(
                id,
                Pending {
                    deadline: now + REQUEST_TIMEOUT,
                    kind: PendingKind::Diagnostics {
                        document: DocumentId {
                            uri: uri.clone(),
                            id: open.id,
                        },
                        version: open.version,
                    },
                },
            );
        }
        Ok(())
    }

    fn configuration(&self, section: Option<&str>) -> Value {
        // Preserve settings lookup for existing servers without workspace settings.
        let legacy = self.options.workspace.is_none();
        let mut value = self
            .options
            .workspace
            .as_ref()
            .unwrap_or(&self.options.initialization);
        if let Some(section) = section {
            for part in section
                .split('.')
                .filter(|part| !legacy || *part != "rust-analyzer")
            {
                value = &value[part];
            }
        }
        value.clone()
    }

    fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.wire
            .send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn expire_requests(&mut self) -> Result<()> {
        let now = Instant::now();
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter_map(|(&id, request)| {
                (request.deadline <= now
                    || matches!(&request.kind, PendingKind::Query { reply, .. } if reply.is_canceled()))
                    .then_some(id)
            })
            .collect();
        for id in expired {
            if let Some(request) = self.pending.remove(&id) {
                if let PendingKind::Query { reply, .. } = request.kind {
                    _ = reply.send(Err(anyhow!("The LSP request timed out")));
                }
                self.notify("$/cancelRequest", json!({ "id": id }))?;
            }
        }
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        let id = self.next_request;
        self.wire
            .send(json!({ "jsonrpc": "2.0", "id": id, "method": "shutdown" }))?;
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        let mut exiting = false;
        while let Ok(event) = self
            .inbox
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            match event {
                Event::Incoming(message)
                    if !exiting && message["id"] == id && message.get("method").is_none() =>
                {
                    self.wire
                        .send(json!({ "jsonrpc": "2.0", "method": "exit" }))?;
                    exiting = true;
                }
                Event::Incoming(message) if !exiting => self.incoming(message)?,
                Event::Disconnected(_) => break,
                _ => {}
            }
        }
        Ok(())
    }
}

fn response_value(message: &Value) -> Result<Value> {
    if let Some(error) = message.get("error") {
        bail!(
            "LSP: {}",
            error["message"].as_str().unwrap_or("Request failed")
        );
    }
    Ok(message["result"].clone())
}

fn supports_references(capabilities: &ServerCapabilities) -> bool {
    capabilities
        .references_provider
        .as_ref()
        .is_some_and(|provider| *provider != OneOf::Left(false))
}

fn file_uri(path: &Path) -> Result<String> {
    Url::from_file_path(path)
        .map(String::from)
        .map_err(|_| anyhow!("Invalid LSP file path"))
}

pub(crate) fn lsp_position(text: &Rope, offset: usize) -> Position {
    let point = text.offset_to_point(offset);
    let line = text.slice_line(point.row);
    let column = point
        .column
        .min(line.len() - usize::from(line.len() > 0 && line.byte(line.len() - 1) == b'\r'));
    let column = line.byte_to_utf16_idx(column);
    Position::new(point.row as u32, column as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::future::{block_on, race};

    #[test]
    fn delayed_definition_cannot_resync_an_older_document() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut command = Command::new(if cfg!(windows) { "python" } else { "python3" });
        command.args(["-u", "-c", r#"
import json, sys
stream = sys.stdin.buffer
out = sys.stdout.buffer
while header := stream.readline():
    length = int(header.split(b":", 1)[1])
    while stream.readline().strip():
        pass
    message = json.loads(stream.read(length))
    if "id" not in message:
        continue
    method = message["method"]
    result = None
    if method == "initialize":
        result = {"capabilities": {"textDocumentSync": 1, "definitionProvider": True}}
    if method == "textDocument/definition":
        point = message["params"]["position"]
        result = {"uri": message["params"]["textDocument"]["uri"], "range": {"start": point, "end": point}}
    reply = json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}).encode()
    out.write(b"Content-Length: %d\r\n\r\n" % len(reply) + reply)
    out.flush()
"#]);
        let client = Client::start(command, dir.path().to_owned(), Value::Null)?;
        let old = Rope::from("[[Old]]\n");
        let current = Rope::from("[[Current]]\n");
        let (document, _) = client.open_document(
            &dir.path().join("notes.md"),
            "markdown",
            old.clone(),
            old.clone(),
        )?;
        let before_lookup = document.synced_version();
        document.changed(current.clone());

        let outdated = block_on(document.definitions_at_version(old, 2, Some(before_lookup)))?;
        assert!(outdated.is_empty());
        assert_eq!(document.synced_version(), before_lookup + 1);
        let current = block_on(document.definitions_at_version(current, 2, Some(before_lookup)))?;
        assert_eq!(current.len(), 1);
        assert_eq!(document.synced_version(), before_lookup + 1);
        Ok(())
    }

    #[test]
    fn invalid_header_reports_the_error_chain_and_server_stderr() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut command = Command::new(if cfg!(windows) { "python" } else { "python3" });
        command.args([
            "-u",
            "-c",
            r#"
import sys
stream = sys.stdin.buffer
size = int(stream.readline().split(b":", 1)[1])
stream.readline()
stream.read(size)
print("Server stderr", file=sys.stderr)
print("Unexpected startup output")
"#,
        ]);
        let client = Client::start(command, dir.path().to_owned(), Value::Null)?;
        let _ = block_on(client.0.stopped.recv());
        assert_eq!(
            client.state(),
            SessionState::Failed(
                "LSP initialization was interrupted: Invalid LSP message header\nServer stderr"
                    .into(),
            ),
        );
        Ok(())
    }

    #[test]
    fn push_and_pull_diagnostics_follow_the_document_version() -> Result<()> {
        for mode in ["push", "pull"] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("test.rs");
            let old = Rope::from("😀 x\n");
            let new = Rope::from("😀 y\n");
            let fixed = Rope::from("😀 z\n");
            let mut command = Command::new(if cfg!(windows) { "python" } else { "python3" });
            command.args([
                "-u",
                "-c",
                include_str!("../tests/fixtures/lsp_diagnostics.py"),
                mode,
            ]);
            let client = Client::start(command, dir.path().to_owned(), Value::Null)?;
            let (document, refresh) =
                client.open_document(&path, "rust", old.clone(), old.clone())?;
            let (timeout, timed_out) = async_channel::bounded::<()>(1);
            thread::spawn(move || {
                thread::sleep(Duration::from_secs(5));
                _ = timeout.send_blocking(());
            });
            let next = |text: &Rope| {
                block_on(race(
                    async {
                        loop {
                            refresh.recv().await?;
                            if let (_, _, Some(diagnostics)) = document.take_updates(text) {
                                return Ok(diagnostics);
                            }
                        }
                    },
                    async {
                        _ = timed_out.recv().await;
                        bail!("missing {mode} diagnostics");
                    },
                ))
            };

            if mode == "push" {
                assert_eq!(next(&old)?[0].message, "initial");
            } else {
                // Leave the first pull outstanding while editing.
                let deadline = Instant::now() + Duration::from_secs(5);
                while !dir.path().join("first-pull").exists() {
                    ensure!(Instant::now() < deadline, "missing the first pull");
                    thread::sleep(Duration::from_millis(10));
                }
            }
            document.changed(new.clone());
            if mode == "push" {
                // The response follows both notifications on the server's output stream.
                block_on(document.hover(new.clone(), "😀 ".len()))?;
            }
            let diagnostics = editor_diagnostics(&new, next(&new)?);
            assert_eq!(diagnostics.len(), 1);
            assert_eq!(diagnostics[0].message, "current");
            assert_eq!(diagnostics[0].range.start.character, 2);
            assert_eq!(diagnostics[0].range.end.character, 3);
            if mode == "push" {
                document.changed(fixed.clone());
                assert!(next(&fixed)?.is_empty());
            } else {
                assert!(next(&new)?.is_empty());
            }
        }
        Ok(())
    }
}
