mod setup;
mod worktree_config;
pub(super) use worktree_config::{WorktreeLanguageState, read_config as read_worktree_lsp};

use rust_i18n::t;

use std::{
    collections::HashSet,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _,
    button::ButtonVariants,
    h_flex,
    popover::Popover,
    progress::ProgressCircle,
    spinner::Spinner,
    tooltip::{ManagedTooltipExt as _, Tooltip},
    v_flex,
};
use gpui_kit_assets::IconName;

use crate::{
    language::{
        BuiltinProject, CommandSource, external,
        tools::{Bundle, Installed, Tools},
        worktree::{WorktreeLsp, WorktreeServer},
    },
    lsp::{
        Client, CompletionActivation, EditorLsp, LanguageProcess, LanguageServer, NewlineServer,
        ProcessMemory, RemoteProcess, SessionState,
    },
    paths,
    symbols::LocalSymbols,
};

use super::{
    OpenFile, Palette, Workspace, action_button, baseline_aligned, compact_icon_button, icon,
    language_for, status_button,
};

impl Workspace {
    pub(super) fn block_pending_completion_acceptance(&self, window: &Window, cx: &mut App) {
        if let Some(file) = self.active_file() {
            let editor = file.editor.read(cx);
            let menu = editor.completion_menu_state();
            if menu.open
                && menu.origin().0 != editor.document_revision()
                && editor.focus_handle(cx).is_focused(window)
            {
                // Let the refresh finish without inserting a stale item or editing the buffer.
                cx.stop_propagation();
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct ServerKey {
    server: ServiceServer,
    root: PathBuf,
    remote: bool,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum ServiceServer {
    Builtin(LanguageServer),
    External(Arc<crate::language::external::ExternalLsp>),
    Worktree(WorktreeTarget),
}

#[derive(Clone)]
struct WorktreeTarget(Arc<WorktreeServer>);

impl PartialEq for WorktreeTarget {
    fn eq(&self, other: &Self) -> bool {
        self.0.entry.id == other.0.entry.id && self.0.fingerprint == other.0.fingerprint
    }
}

impl Eq for WorktreeTarget {}

impl Hash for WorktreeTarget {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.entry.id.hash(state);
        self.0.fingerprint.hash(state);
    }
}

impl ServerKey {
    pub(super) fn is_remote(&self) -> bool {
        self.remote
    }

    pub(super) fn project_path(&self) -> &Path {
        &self.root
    }

    fn server_id(&self) -> &str {
        match &self.server {
            ServiceServer::Builtin(server) => server.id(),
            ServiceServer::External(server) => &server.id,
            ServiceServer::Worktree(server) => &server.0.entry.id,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ServerProject {
    pub(super) key: ServerKey,
    language_id: SharedString,
}

impl ServerProject {
    pub(super) fn newline_server(&self) -> Option<NewlineServer> {
        match self.language_id.as_ref() {
            "c" | "cpp" => Some(NewlineServer::OnType),
            "rust" => Some(NewlineServer::RustAnalyzer),
            _ => None,
        }
    }

    pub(super) fn for_file(repo: &crate::repository::Repository, path: &Path) -> Option<Self> {
        let config = repo.worktree_lsp.read().unwrap().clone();
        Self::with_config(repo, path, &config)
    }

    pub(super) fn with_config(
        repo: &crate::repository::Repository,
        path: &Path,
        config: &WorktreeLsp,
    ) -> Option<Self> {
        if path.is_absolute() {
            return Self::for_local_file(path);
        }
        if let Some((server, language_id)) = config.select(path) {
            let root = if let Some(ssh) = &repo.ssh {
                ssh.call(crate::remote::Request::WorktreeLspRoot(
                    server.roots.clone(),
                    crate::remote::RepoPath::from_local(path).ok()?,
                ))
                .ok()?
            } else {
                server.roots.resolve(&repo.root, path)
            };
            return Some(Self {
                key: ServerKey {
                    server: ServiceServer::Worktree(WorktreeTarget(Arc::new(server.clone()))),
                    root,
                    remote: repo.ssh.is_some(),
                },
                language_id: language_id.into(),
            });
        }
        Self::resolve(repo.ssh.as_ref(), &repo.root, path, Some(config))
    }

    pub(super) fn for_local_file(path: &Path) -> Option<Self> {
        Self::resolve(None, path.parent()?, path, None)
    }

    fn resolve(
        ssh: Option<&crate::remote::SshSession>,
        root: &Path,
        path: &Path,
        config: Option<&WorktreeLsp>,
    ) -> Option<Self> {
        let entries = match external::load() {
            Ok(entries) => entries,
            Err(error) => {
                eprintln!("Could not load external language servers: {error:#}");
                Vec::new()
            }
        };
        let disabled: HashSet<_> = entries
            .iter()
            .filter(|entry| entry.disabled)
            .map(|entry| entry.id.clone())
            .collect();
        if let Some((server, language_id)) = external::select(entries, path, |id| {
            config.is_some_and(|config| config.entry(id).is_some())
        }) {
            let root = match ssh {
                Some(ssh) => {
                    let version = crate::remote::EXTERNAL_LSP_VERSION;
                    if ssh.call::<u32>(crate::remote::Request::Hello(version)).ok() != Some(version)
                    {
                        return None;
                    }
                    ssh.call(crate::remote::Request::ExternalLspRoot(
                        server.id.clone(),
                        crate::remote::RepoPath::from_local(path).ok()?,
                    ))
                    .ok()?
                }
                None => server.project_root(root, path),
            };
            return Some(Self {
                key: ServerKey {
                    server: ServiceServer::External(server.into()),
                    root,
                    remote: ssh.is_some(),
                },
                language_id: language_id.into(),
            });
        }
        let excluded = LanguageServer::ALL
            .into_iter()
            .filter(|server| {
                disabled.contains(server.id())
                    || config.is_some_and(|config| config.entry(server.id()).is_some())
            })
            .collect::<Vec<_>>();
        let project = match ssh {
            Some(ssh) => {
                use crate::remote::{RemoteLspProject, RepoPath, Request};
                let worktree_ids = config
                    .into_iter()
                    .flat_map(WorktreeLsp::ids)
                    .map(str::to_owned)
                    .collect();
                let project = ssh
                    .call(Request::RemoteLspProject(
                        RepoPath::from_local(path).ok()?,
                        excluded,
                        worktree_ids,
                    ))
                    .ok()
                    .flatten()?;
                match project {
                    RemoteLspProject::Builtin(project) => Some(project),
                    RemoteLspProject::External {
                        server,
                        root,
                        language_id,
                    } => {
                        return Some(Self {
                            key: ServerKey {
                                server: ServiceServer::External(Arc::from(server)),
                                root,
                                remote: true,
                            },
                            language_id: language_id.into(),
                        });
                    }
                }
            }
            None => BuiltinProject::resolve(root, path, &excluded),
        }?;
        Some(Self {
            key: ServerKey {
                server: ServiceServer::Builtin(project.server),
                root: project.root,
                remote: ssh.is_some(),
            },
            language_id: project.language_id.into(),
        })
    }
}

pub(super) enum LanguageService {
    Checking {
        _task: Task<()>,
    },
    Setup(setup::Setup),
    Session {
        client: Client,
        restart: bool,
        managed: Option<Installed>,
        _watch: Task<()>,
    },
    Failed(String),
}

pub(super) struct LanguageServicePanel {
    memory: Option<(LanguageProcess, ProcessMemory)>,
    _poll_memory: Option<Task<()>>,
}

#[derive(PartialEq, Eq)]
enum ServiceStatus {
    Unsupported,
    Disabled,
    Idle,
    NotInstalled,
    Installing,
    InstallFailed,
    Starting,
    Ready,
    Stopping,
    Stopped,
    Failed(String),
}

impl ServiceStatus {
    fn label(&self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::Unsupported => t!("lsp.unsupported"),
            Self::Disabled => t!("lsp.disabled"),
            Self::Idle => t!("lsp.idle"),
            Self::NotInstalled => t!("lsp.not_installed"),
            Self::Installing => t!("lsp.installing"),
            Self::InstallFailed => t!("lsp.install_failed"),
            Self::Starting => t!("common.starting"),
            Self::Ready => t!("lsp.ready"),
            Self::Stopping => t!("lsp.stopping"),
            Self::Stopped => t!("lsp.stopped"),
            Self::Failed(_) => t!("lsp.failed"),
        }
    }

    fn controls(&self) -> bool {
        !matches!(
            self,
            Self::Unsupported
                | Self::Disabled
                | Self::NotInstalled
                | Self::Installing
                | Self::InstallFailed
        )
    }

    fn pending(&self) -> bool {
        matches!(self, Self::Starting | Self::Stopping | Self::Installing)
    }
}

impl Workspace {
    fn can_start_language_service(&self, key: &ServerKey) -> bool {
        self.startup_lsp.enabled
            && (!matches!(key.server, ServiceServer::Worktree(_))
                || self.session.worktree_lsp_trusted)
    }

    pub(super) fn start_language_service(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        if self.can_start_language_service(key) && !self.check_language_tools(key, cx) {
            self.launch_language_service(key, None, cx);
        }
    }

    fn remote_lsp_launch(
        &self,
        key: &ServerKey,
    ) -> anyhow::Result<(crate::remote::SshSession, crate::remote::LspLaunch)> {
        use crate::remote::{LspLaunch, LspTarget};
        let repo = self
            .repository
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Missing SSH worktree"))?;
        let ssh = repo
            .ssh
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Missing SSH connection"))?;
        let repository = crate::remote::SshTarget::parse(&repo.root)?.unwrap().path;
        let server = match &key.server {
            ServiceServer::Builtin(server) => LspTarget::Builtin(*server),
            ServiceServer::External(server) => LspTarget::External {
                id: server.id.clone(),
            },
            ServiceServer::Worktree(server) => LspTarget::Worktree {
                id: server.0.entry.id.clone(),
                fingerprint: server.0.fingerprint.clone(),
                snapshot: server.0.remote_snapshot.clone().ok_or_else(|| {
                    anyhow::anyhow!("Missing remote worktree LSP configuration snapshot")
                })?,
            },
        };
        Ok((
            ssh.clone(),
            LspLaunch {
                repository,
                project: key.root.clone(),
                server,
                memory: None,
                managed: None,
            },
        ))
    }

    fn launch_language_service(
        &mut self,
        key: &ServerKey,
        managed: Option<Installed>,
        cx: &mut Context<Self>,
    ) {
        let barriers = &mut self.worktree_language.retiring;
        barriers.retain(|(_, _, stopped)| !stopped.is_closed());
        let wait: Vec<_> = barriers
            .iter()
            .filter_map(|(id, remote, stopped)| {
                (id == key.server_id() && *remote == key.remote).then(|| stopped.clone())
            })
            .collect();
        let wait_for_exit = move || {
            for stopped in wait {
                _ = stopped.recv_blocking();
            }
        };
        let mut options = match &key.server {
            ServiceServer::Builtin(server) => server.lsp_options(),
            ServiceServer::External(server) => server.server.lsp_options(),
            ServiceServer::Worktree(server) => server.0.entry.server.lsp_options(),
        };
        if managed
            .as_ref()
            .is_some_and(|tools| tools.bundle == Bundle::Yaml)
        {
            // YAML registers its formatter dynamically only after refreshing SchemaStore.
            options.dynamic_formatting = false;
        }
        let start = (|| {
            if key.remote {
                let (ssh, mut launch) = self.remote_lsp_launch(key)?;
                launch.managed = managed.clone();
                let process = RemoteProcess::new(ssh.clone());
                let monitor = process.clone();
                let uri = crate::remote::file_uri(
                    key.root
                        .to_str()
                        .ok_or_else(|| anyhow::anyhow!("The remote path is not UTF-8"))?,
                )?;
                Client::start_with_uri(
                    move || {
                        wait_for_exit();
                        launch.memory = monitor.prepare();
                        ssh.language_command(&launch, "--lsp")
                    },
                    process,
                    uri,
                    options,
                )
            } else {
                let project = key.root.clone();
                let installed = managed
                    .clone()
                    .map(|installed| -> anyhow::Result<_> {
                        let home = crate::language::language_home().ok_or_else(|| {
                            anyhow::anyhow!("Could not determine the Stroll directory")
                        })?;
                        Ok((Tools::new(home)?, installed))
                    })
                    .transpose()?;
                match &key.server {
                    ServiceServer::Builtin(server) => {
                        let server = *server;
                        let (command, source) = server
                            .local_command(&self.startup_lsp.server(server).command)
                            .expect("startup LSP configuration was validated");
                        Client::start_with_command(
                            move || {
                                wait_for_exit();
                                match source {
                                    CommandSource::Path => {
                                        if let Some((tools, installed)) = installed {
                                            tools.command(server, &installed, None, None)
                                        } else {
                                            anyhow::ensure!(
                                                server.bundle().is_none(),
                                                "Managed language tools have not been installed"
                                            );
                                            Ok(command)
                                        }
                                    }
                                    _ => Ok(command),
                                }
                            },
                            key.root.clone(),
                            options,
                        )
                    }
                    ServiceServer::External(server) => {
                        let command = server.command();
                        Client::start_with_command(
                            move || {
                                wait_for_exit();
                                Ok(command)
                            },
                            project,
                            options,
                        )
                    }
                    ServiceServer::Worktree(server) => {
                        let server = server.0.clone();
                        let directory = project.clone();
                        Client::start_with_command(
                            move || {
                                wait_for_exit();
                                server.command(
                                    &directory,
                                    None,
                                    installed
                                        .as_ref()
                                        .map(|(tools, installed)| (tools, installed)),
                                )
                            },
                            project,
                            options,
                        )
                    }
                }
            }
        })();
        let client = match start {
            Ok(client) => client,
            Err(error) => {
                self.language_servers.insert(
                    key.clone(),
                    LanguageService::Failed(format!(
                        "Could not start the language server: {error:#}"
                    )),
                );
                for path in self.language_service_files(key) {
                    self.connect_file_to_language_service(&path, cx);
                }
                cx.notify();
                return;
            }
        };

        client.stop_on_quit(cx);
        let changes = client.state_changes();
        let watched_key = key.clone();
        let watch = cx.spawn(async move |this, cx| {
            while changes.recv().await.is_ok() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
            _ = this.update(cx, |this, cx| {
                this.language_service_exited(&watched_key, cx)
            });
        });
        self.language_servers.insert(
            key.clone(),
            LanguageService::Session {
                client,
                restart: false,
                managed,
                _watch: watch,
            },
        );

        for path in self.language_service_files(key) {
            self.connect_file_to_language_service(&path, cx);
        }
        cx.notify();
    }

    fn language_service_files(&self, key: &ServerKey) -> Vec<PathBuf> {
        self.files
            .values()
            .filter(|file| {
                file.server_project
                    .as_ref()
                    .is_some_and(|project| &project.key == key)
            })
            .map(|file| file.path.clone())
            .collect()
    }

    pub(super) fn connect_file_to_language_service(&mut self, path: &Path, cx: &mut Context<Self>) {
        let file = &self.files[path];
        let connection = file.server_project.as_ref().and_then(|project| {
            let Some(LanguageService::Session { client, .. }) =
                self.language_servers.get(&project.key)
            else {
                return None;
            };
            if !matches!(client.state(), SessionState::Starting | SessionState::Ready) {
                return None;
            }
            if project.key.remote {
                let repo = self.repository.as_ref()?;
                let target = crate::remote::SshTarget::parse(&repo.root).ok()??;
                let relative = crate::remote::RepoPath::from_local(path).ok()?;
                let absolute = format!("{}/{}", target.path.to_str()?, relative.as_str());
                let uri = crate::remote::file_uri(&absolute).ok()?;
                client
                    .open_uri(
                        uri,
                        &project.language_id,
                        file.committed.clone(),
                        file.saved.clone(),
                    )
                    .ok()
            } else {
                let absolute = if path.is_absolute() {
                    path.to_owned()
                } else {
                    self.repository.as_ref()?.root.join(path)
                };
                client
                    .open_document(
                        &absolute,
                        &project.language_id,
                        file.committed.clone(),
                        file.saved.clone(),
                    )
                    .ok()
            }
        });
        let language = language_for(path);
        let is_markdown = crate::docs::is_markdown(path);
        let completion_activation = if is_markdown {
            CompletionActivation::Link
        } else if !matches!(language, "text" | "plaintext")
            || self.source_resources(path).is_source_file(path)
        {
            CompletionActivation::Code
        } else {
            CompletionActivation::TriggerOnly
        };
        let (server, refresh) = connection
            .map(|(document, refresh)| {
                let context = self
                    .repository
                    .as_ref()
                    .filter(|_| !path.is_absolute())
                    .map(|repo| (self.ai.clone(), repo.clone(), path.to_owned()));
                (
                    EditorLsp::new(document, language, completion_activation, context),
                    refresh,
                )
            })
            .unzip();
        let workspace = cx.weak_entity();
        let markdown = is_markdown.then(|| super::markdown_links::MarkdownLinks {
            path: path.to_owned(),
            repository: self.repository.clone(),
            server: server.as_ref().map(|server| server.document.clone()),
            editor: file.editor.downgrade(),
        });
        let provider =
            server
                .as_ref()
                .zip(file.server_project.as_ref())
                .map(|(server, project)| {
                    super::language_actions::LanguageActions::new(
                        workspace.clone(),
                        server.document.clone(),
                        path.to_owned(),
                        match project.key.server {
                            ServiceServer::Builtin(server) => Some(server),
                            ServiceServer::External(_) => None,
                            ServiceServer::Worktree(ref server) => server.0.builtin,
                        },
                    )
                });
        let file = self.files.get_mut(path).unwrap();
        let inlay_provider: Option<super::inlay_hints::Provider> = server.as_ref().map(|server| {
            let document = server.document.clone();
            Rc::new(move |text: &_, rows: &[_], cx: &App| {
                crate::lsp::editor_inlay_hints(&document, text, rows, cx)
            }) as _
        });
        file.inlay_hints.set_provider(inlay_provider, cx);
        file.caret_hints.clear(cx);
        file.editor.update(cx, |editor, cx| {
            editor.dismiss_lsp_overlays(cx);
            if let Some(diagnostics) = editor.diagnostics_mut() {
                diagnostics.clear();
            }
            // Drop the outgoing providers, their pending requests and cached semantic tokens.
            *editor.lsp_mut() = Default::default();
            let features = editor.lsp_mut();
            if let Some(server) = &server {
                if let Some(provider) = provider {
                    features.code_action_providers.push(provider);
                }
                features.completion_provider = Some(server.clone());
                features.set_completion_highlighter(Rc::new(
                    gpui_component::input::highlight_characters,
                ));
                features.hover_provider = Some(server.clone());
                features.semantic_tokens_provider = Some(server.clone());
                features.definition_provider = Some(server.clone());
            } else {
                features.hover_provider = Some(Rc::new(LocalSymbols(language)));
            }
            if let Some(markdown) = markdown {
                features.definition_provider = Some(Rc::new(markdown));
            }
            if features.definition_provider.is_some() {
                features.show_document = Some(Rc::new(move |params, window, cx| {
                    let params = params.clone();
                    let workspace = workspace.clone();
                    // The source editor is still being updated by GPUI.
                    window.defer(cx, move |window, cx| {
                        _ = workspace
                            .update(cx, |this, cx| this.open_definition(params, window, cx));
                    });
                    true
                }));
            }
            editor.refresh(cx);
        });
        let document = server.as_ref().map(|server| server.document.clone());
        file.language_server = server;
        file._lsp_refresh = refresh.zip(document).map_or_else(
            || Task::ready(()),
            |(refresh, document)| {
                let editor = file.editor.downgrade();
                let inlay_hints = file.inlay_hints.clone();
                let path = path.to_owned();
                cx.spawn(async move |workspace, cx| {
                    while refresh.recv().await.is_ok() {
                        let Ok(inlay_refresh) = editor.update(cx, |editor, cx| {
                            let text = editor.text().clone();
                            let (semantic_refresh, inlay_refresh, diagnostics) =
                                document.take_updates(&text);
                            if let Some(diagnostics) = diagnostics
                                && let Some(set) = editor.diagnostics_mut()
                            {
                                set.reset(&text);
                                set.extend(crate::lsp::editor_diagnostics(&text, diagnostics));
                                editor.invalidate_code_action_indicator();
                                cx.notify();
                            }
                            if semantic_refresh {
                                editor.refresh(cx);
                            }
                            inlay_refresh
                        }) else {
                            break;
                        };
                        if inlay_refresh {
                            cx.update(|cx| inlay_hints.refresh(cx));
                            if let Ok(Some(panel)) =
                                workspace.read_with(cx, |workspace, _| workspace.git.clone())
                            {
                                panel.update(cx, |panel, cx| {
                                    panel.invalidate_source_inlay_hints(&path, cx)
                                });
                            }
                        }
                    }
                })
            },
        );
        if let Some(panel) = &self.git {
            panel.update(cx, |panel, cx| {
                panel.invalidate_source_inlay_hints(path, cx);
                panel.schedule_snapshot_alignment(cx);
            });
        }
    }

    fn language_service_exited(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        let Some(LanguageService::Session { restart, .. }) = self.language_servers.get(key) else {
            return;
        };
        if *restart {
            self.start_language_service(key, cx);
        } else {
            for path in self.language_service_files(key) {
                if self.files[&path].language_server.is_some() {
                    self.connect_file_to_language_service(&path, cx);
                }
            }
            cx.notify();
        }
    }

    fn control_language_service(&mut self, start: bool, cx: &mut Context<Self>) {
        if !self.startup_lsp.enabled {
            return;
        }
        let Some(key) = self
            .active_file()
            .and_then(|file| file.server_project.as_ref())
            .map(|project| project.key.clone())
        else {
            return;
        };
        if matches!(
            self.language_servers.get(&key),
            Some(LanguageService::Checking { .. } | LanguageService::Setup(_))
        ) {
            return;
        }
        if let Some(LanguageService::Session {
            client, restart, ..
        }) = self.language_servers.get_mut(&key)
        {
            match client.state() {
                SessionState::Starting if start => return,
                SessionState::Starting | SessionState::Ready => {
                    *restart = start;
                    client.stop();
                    for path in self.language_service_files(&key) {
                        self.connect_file_to_language_service(&path, cx);
                    }
                    cx.notify();
                    return;
                }
                SessionState::Stopping => return,
                SessionState::Stopped | SessionState::Failed(_) => {}
            }
        }
        if start {
            self.start_language_service(&key, cx);
        }
    }

    fn language_service_state(&self, project: Option<&ServerProject>, cx: &App) -> ServiceStatus {
        let Some(project) = project else {
            return ServiceStatus::Unsupported;
        };
        if !self.startup_lsp.enabled {
            return ServiceStatus::Disabled;
        }
        match self.language_servers.get(&project.key) {
            None => ServiceStatus::Idle,
            Some(LanguageService::Checking { .. }) => ServiceStatus::Starting,
            Some(LanguageService::Setup(setup)) => setup.status(cx),
            Some(LanguageService::Failed(error)) => ServiceStatus::Failed(error.clone()),
            Some(LanguageService::Session { client, .. }) => match client.state() {
                SessionState::Starting => ServiceStatus::Starting,
                SessionState::Ready => ServiceStatus::Ready,
                SessionState::Stopping => ServiceStatus::Stopping,
                SessionState::Stopped => ServiceStatus::Stopped,
                SessionState::Failed(error) => ServiceStatus::Failed(error),
            },
        }
    }

    fn language_memory_process(&self) -> Option<LanguageProcess> {
        if self.terminal_maximized() {
            return None;
        }
        let project = self.active_file()?.server_project.as_ref()?;
        match self.language_servers.get(&project.key)? {
            LanguageService::Session { client, .. } => client.memory_process(),
            _ => None,
        }
    }

    fn language_service_memory(&self) -> Option<ProcessMemory> {
        let (process, memory) = self.lsp_panel.as_ref()?.memory.as_ref()?;
        (self.language_memory_process().as_ref() == Some(process)).then_some(*memory)
    }

    fn open_language_service_panel(&mut self, cx: &mut Context<Self>) {
        let remotely = self
            .repository
            .as_ref()
            .is_some_and(|repo| repo.ssh.is_some());
        let poll =
            (self.startup_lsp.enabled && (ProcessMemory::SUPPORTED || remotely)).then(|| {
                cx.spawn(async move |this, cx| {
                    loop {
                        let Ok(process) =
                            this.read_with(cx, |this, _| this.language_memory_process())
                        else {
                            return;
                        };
                        if let Some(process) = process {
                            let sampler = process.clone();
                            let sample = cx.background_spawn(async move { sampler.sample() }).await;
                            if this
                                .update(cx, |this, cx| {
                                    if this.language_memory_process().as_ref() == Some(&process)
                                        && let Some(panel) = &mut this.lsp_panel
                                    {
                                        let memory = sample.map(|sample| (process, sample));
                                        if panel.memory != memory {
                                            panel.memory = memory;
                                            cx.notify();
                                        }
                                    }
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                        cx.background_executor().timer(Duration::from_secs(2)).await;
                    }
                })
            });
        self.lsp_panel = Some(LanguageServicePanel {
            memory: None,
            _poll_memory: poll,
        });
    }

    pub(super) fn language_service_progress(
        &self,
        file: &OpenFile,
        status_size: f32,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Option<impl IntoElement> {
        let project = file.server_project.as_ref()?;
        let LanguageService::Session { client, .. } = self.language_servers.get(&project.key)?
        else {
            return None;
        };
        let progress = client.progress()?;
        let label: SharedString = match progress.message.filter(|message| !message.is_empty()) {
            Some(message) => format!("{} · {message}", progress.title),
            None => progress.title,
        }
        .into();
        Some(
            baseline_aligned(
                cx,
                status_size,
                status_size,
                h_flex()
                    .id("language-service-progress")
                    .debug_selector(|| "language-service-progress".into())
                    .min_w_0()
                    .gap_1()
                    .managed_tooltip({
                        let label = label.clone();
                        move |window, cx| Tooltip::new(label.clone()).build(window, cx)
                    })
                    .child(
                        ProgressCircle::new("language-service-progress-circle")
                            .small()
                            .flex_shrink_0()
                            .color(colors.accent)
                            .accessibility_label(label.clone())
                            .loading(progress.percentage.is_none())
                            .value(progress.percentage.unwrap_or_default() as f32),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .debug_selector(|| format!("language-service-progress: {label}"))
                            .child(label),
                    ),
            )
            .max_w(px(280.))
            .min_w_0(),
        )
    }

    pub(super) fn language_service_status(
        &self,
        file: &OpenFile,
        label_size: Pixels,
        colors: Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let status = self.language_service_state(file.server_project.as_ref(), cx);
        let tooltip = t!("lsp.tooltip", status = status.label()).into_owned();
        let workspace = cx.weak_entity();
        Popover::new("language-service")
            .dropdown_motion()
            .anchor(Anchor::BottomRight)
            .offset(px(6.))
            .p_3()
            .open(self.lsp_panel.is_some())
            .on_open_change(cx.listener(|this, &open, _, cx| {
                if open {
                    this.open_language_service_panel(cx);
                    if let Some(file) = this.active_file() {
                        file.editor
                            .update(cx, |editor, cx| editor.dismiss_lsp_overlays(cx));
                    }
                } else {
                    this.lsp_panel = None;
                }
                cx.notify();
            }))
            .trigger(
                status_button("language-service-trigger")
                    .min_w(px(48.))
                    .label_size(label_size)
                    .accessibility_label(tooltip.clone())
                    .tooltip(tooltip)
                    .when(status.pending(), |button| {
                        button.icon(Spinner::new().color(colors.accent))
                    })
                    .when(!status.pending(), |button| {
                        let (glyph, color) = match status {
                            ServiceStatus::Ready => (IconName::Check, cx.theme().success),
                            ServiceStatus::Failed(_) | ServiceStatus::InstallFailed => {
                                (IconName::TriangleAlert, colors.warning)
                            }
                            _ => (IconName::Minus, colors.muted),
                        };
                        button.icon(icon(glyph, color, 12.))
                    })
                    .label("LSP"),
            )
            .content(move |_, window, cx| {
                workspace
                    .read_with(cx, |this, cx| {
                        this.language_service_panel(
                            workspace.clone(),
                            window.viewport_size(),
                            colors,
                            cx,
                        )
                        .into_any_element()
                    })
                    .unwrap_or_else(|_| div().into_any_element())
            })
    }

    fn language_service_panel(
        &self,
        workspace: WeakEntity<Self>,
        viewport: Size<Pixels>,
        colors: Palette,
        cx: &App,
    ) -> impl IntoElement {
        let project = self
            .active_file()
            .and_then(|file| file.server_project.as_ref());
        let status = self.language_service_state(project, cx);
        let service = project.and_then(|project| self.language_servers.get(&project.key));
        let setup = match service {
            Some(LanguageService::Setup(setup)) => Some(setup),
            _ => None,
        };
        let managed = match service {
            Some(LanguageService::Session { managed, .. }) => managed.as_ref(),
            _ => None,
        };
        let version = match service {
            Some(LanguageService::Session { client, .. }) => client
                .server_version()
                .or_else(|| managed.map(|tool| tool.version.clone())),
            _ => setup.map(|setup| setup.version().to_owned()),
        };
        let running = status == ServiceStatus::Ready;
        let stop_disabled = !matches!(service, Some(LanguageService::Session { .. }))
            || !matches!(status, ServiceStatus::Starting | ServiceStatus::Ready);
        let start_disabled = status.pending();
        let memory = self.language_service_memory();
        let mut panel = v_flex()
            .debug_selector(|| "language-service-panel".into())
            .w(px(312.).min((viewport.width - px(42.)).max(px(0.))))
            .gap_2()
            .text_size(px(12.))
            .child(
                div()
                    .font_semibold()
                    .text_size(px(13.))
                    .child(t!("common.language_server")),
            )
            .when_some(project, |panel, project| {
                let ssh = self
                    .repository
                    .as_ref()
                    .filter(|_| project.key.remote)
                    .and_then(|repo| repo.ssh.as_ref());
                let program = match &project.key.server {
                    ServiceServer::Builtin(server)
                        if ssh.is_some() || setup.is_some() || managed.is_some() =>
                    {
                        server.name().to_owned()
                    }
                    ServiceServer::Builtin(server) => {
                        server.local_display_name(&self.startup_lsp.server(*server).command)
                    }
                    ServiceServer::External(server) => server.server.command.clone(),
                    ServiceServer::Worktree(server) => server.0.entry.server.command.clone(),
                };
                let service = version.as_deref().map_or_else(
                    || program.clone(),
                    |version| format!("{program} · {version}"),
                );
                panel
                    .child(
                        service_detail(t!("lsp.server"), service.clone(), colors)
                            .debug_selector(move || format!("language-service-server: {service}")),
                    )
                    .when_some(ssh, |panel, ssh| {
                        panel.child(service_detail(
                            t!("common.ssh_host"),
                            ssh.target().host.clone(),
                            colors,
                        ))
                    })
                    .child(service_detail(
                        t!("lsp.project_root"),
                        paths::display(&project.key.root),
                        colors,
                    ))
            })
            .child(service_detail(t!("lsp.status"), status.label(), colors))
            .when(project.is_some() && setup.is_none(), |panel| {
                let rss = memory
                    .map(|sample| format!("{:.1} MiB", sample.bytes as f64 / 1_048_576.))
                    .unwrap_or_else(|| t!("lsp.memory_unavailable").into_owned());
                panel.child(
                    service_detail(t!("lsp.memory_rss"), rss, colors)
                        .id("language-service-memory")
                        .debug_selector(|| "language-service-memory".into())
                        .managed_tooltip(|window, cx| {
                            Tooltip::new(t!("lsp.memory_rss_hint").into_owned()).build(window, cx)
                        }),
                )
            });

        let worktree_workspace = workspace.clone();
        if let Some(setup) = setup {
            panel = panel
                .children(setup.progress_panel(cx, colors))
                .when_some(setup.error(cx), |panel, error| {
                    panel.child(last_error(error, colors))
                })
                .when(
                    matches!(
                        status,
                        ServiceStatus::NotInstalled | ServiceStatus::InstallFailed
                    ),
                    |panel| {
                        panel.child(
                            action_button("install-language-service")
                                .debug_selector(|| "install-language-service".into())
                                .small()
                                .secondary()
                                .label(if status == ServiceStatus::InstallFailed {
                                    t!("lsp.retry_install")
                                } else {
                                    t!("lsp.install")
                                })
                                .on_click(move |_, _, cx| {
                                    _ = workspace.update(cx, |this, cx| {
                                        this.install_active_language_tools(cx)
                                    });
                                }),
                        )
                    },
                );
        } else if status.controls() {
            let stop_workspace = workspace.clone();
            panel = panel
                .when_some(
                    match status {
                        ServiceStatus::Failed(error) => Some(error),
                        _ => None,
                    },
                    |panel, error| panel.child(last_error(error, colors)),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            action_button("start-language-service")
                                .small()
                                .secondary()
                                .disabled(start_disabled)
                                .label(if running {
                                    t!("lsp.restart")
                                } else {
                                    t!("lsp.start")
                                })
                                .tooltip(t!("lsp.shared_server"))
                                .on_click(move |_, _, cx| {
                                    _ = workspace.update(cx, |this, cx| {
                                        this.control_language_service(true, cx);
                                    });
                                }),
                        )
                        .child(
                            action_button("stop-language-service")
                                .small()
                                .ghost()
                                .disabled(stop_disabled)
                                .label(t!("lsp.stop"))
                                .tooltip(t!("lsp.shared_server"))
                                .on_click(move |_, _, cx| {
                                    _ = stop_workspace.update(cx, |this, cx| {
                                        this.control_language_service(false, cx);
                                    });
                                }),
                        ),
                );
        }
        panel.children(self.worktree_language_controls(worktree_workspace, colors))
    }
}

fn last_error(error: String, colors: Palette) -> impl IntoElement {
    let error: SharedString = error.into();
    v_flex()
        .gap_1()
        .p_2()
        .rounded_md()
        .bg(colors.soft)
        .text_color(colors.warning)
        .child(
            h_flex()
                .justify_between()
                .child(div().font_semibold().child(t!("lsp.last_error")))
                .child(
                    compact_icon_button("copy-lsp-error", IconName::Copy, t!("lsp.copy_error"))
                        .on_click({
                            let error = error.clone();
                            move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(error.to_string()));
                            }
                        }),
                ),
        )
        .child(div().whitespace_normal().child(error))
}

fn service_detail(
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    colors: Palette,
) -> Div {
    service_detail_columns(
        div().child(label.into()),
        div().whitespace_normal().child(value.into()),
        colors,
    )
}

fn service_detail_columns(label: Div, value: Div, colors: Palette) -> Div {
    h_flex()
        .items_start()
        .gap_3()
        .child(label.w(px(84.)).flex_shrink_0().text_color(colors.muted))
        .child(value.min_w_0().flex_1())
}

#[cfg(all(test, unix))]
mod completion_tests;

#[cfg(all(test, unix))]
mod configuration_tests;

#[cfg(all(test, unix))]
mod indentation_tests;

#[cfg(all(test, unix))]
mod markdown_tests;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod memory_tests;

#[cfg(test)]
mod tests {
    use super::{Palette, last_error};
    use gpui::{
        Context, IntoElement, Modifiers, ParentElement as _, Render, Styled, TestAppContext,
        TestDispatcher, Window, div, point, px,
    };
    use gpui_component::ThemeMode;

    #[cfg(unix)]
    #[test]
    fn language_service_progress_updates_and_hides_on_completion() -> anyhow::Result<()> {
        use super::*;
        use crate::{
            config::ConfigFile,
            repository::{Location, Store},
            workspace::WorkspaceView,
        };
        use futures_lite::future::block_on;
        use gpui_component::{Root, input::Rope};
        use std::{
            fs,
            process::Command,
            time::{Duration, Instant},
        };

        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repository");
        fs::create_dir(&root)?;
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()?
                .success()
        );
        let file = PathBuf::from("main.rs");
        let source = "fn main() {}\n";
        fs::write(root.join(&file), source)?;
        let script = fixture.path().join("server.py");
        fs::write(
            &script,
            include_str!("../../tests/fixtures/lsp_progress.py"),
        )?;
        let config_path = fixture.path().join("config.toml");
        fs::write(
            &config_path,
            format!("[lsp.rust]\ncommand = 'python3 -u {}'\n", script.display()),
        )?;
        let config = ConfigFile::load(Some(config_path));
        let store = std::sync::Arc::new(Store::new(fixture.path().join("state"))?);
        let mut app = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        app.update(gpui_component::init);
        let mut workspace = None;
        let (_, visual) = app.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(740.), px(480.)));
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root,
                        file: Some(file),
                    },
                    window,
                    cx,
                );
            });
        });
        let draw = |visual: &mut VisualTestContext| {
            visual.executor().advance_clock(Duration::from_millis(20));
            visual.run_until_parked();
            visual.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
            });
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let (client, document) = loop {
            draw(visual);
            if visual
                .debug_bounds("language-service-progress: Fetching")
                .is_some()
            {
                break visual.update(|_, cx| {
                    let view = workspace.read(cx);
                    let file = view.active_file().unwrap();
                    let key = &file.server_project.as_ref().unwrap().key;
                    let LanguageService::Session { client, .. } = &view.language_servers[key]
                    else {
                        panic!("Expected a running language server");
                    };
                    (
                        client.clone(),
                        file.language_server.as_ref().unwrap().document.clone(),
                    )
                });
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "Timed out waiting for the Fetching indicator"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(client.progress().unwrap().percentage, None);

        let right_status_bounds = |visual: &mut VisualTestContext| {
            [
                "status-file-language",
                "status-file-encoding",
                "status-file-controls",
            ]
            .map(|selector| visual.debug_bounds(selector).unwrap())
        };
        let right_status = right_status_bounds(visual);
        let path_bounds = visual.debug_bounds("status-file-path").unwrap();
        let progress_left = visual
            .debug_bounds("language-service-progress")
            .unwrap()
            .left();
        let gap = visual.update(|window, _| window.rem_size() * 0.75);
        assert!((progress_left - path_bounds.right() - gap).abs() <= px(1.));
        for (label, percentage) in [
            (
                "language-service-progress: Fetching · Blocking waiting for file lock on package cache",
                None,
            ),
            ("language-service-progress: Indexing", Some(20)),
            ("language-service-progress: Indexing · 12/25", Some(48)),
            (
                "language-service-progress: Fetching · cargo metadata: finished",
                None,
            ),
        ] {
            block_on(document.hover(Rope::from(source), 0))?;
            draw(visual);
            let bounds = visual
                .debug_bounds(label)
                .unwrap_or_else(|| panic!("Missing {label}"));
            assert!(bounds.size.width > px(0.));
            assert_eq!(client.progress().unwrap().percentage, percentage);
            let progress_bounds = visual.debug_bounds("language-service-progress").unwrap();
            assert_eq!(progress_bounds.left(), progress_left);
            assert!(progress_bounds.right() <= right_status[0].left());
            assert_eq!(visual.debug_bounds("status-file-path"), Some(path_bounds));
            assert_eq!(right_status_bounds(visual), right_status);
        }
        block_on(document.hover(Rope::from(source), 0))?;
        draw(visual);
        assert!(client.progress().is_none());
        assert_eq!(client.state(), SessionState::Ready);
        assert!(visual.debug_bounds("language-service-progress").is_none());
        assert_eq!(visual.debug_bounds("status-file-path"), Some(path_bounds));
        assert_eq!(right_status_bounds(visual), right_status);
        Ok(())
    }

    #[test]
    fn copies_the_full_language_server_error() {
        const ERROR: &str =
            "LSP initialization was interrupted: Invalid LSP message header\nServer stderr";

        struct ErrorCard;
        impl Render for ErrorCard {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().w(px(312.)).child(last_error(
                    ERROR.into(),
                    Palette::for_mode(ThemeMode::Light),
                ))
            }
        }

        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(cfg!(target_os = "linux")),
        );
        cx.update(gpui_component::init);
        let (_, visual) = cx.add_window_view(|_, _| ErrorCard);
        visual.update(|window, cx| window.draw(cx).clear(cx));
        visual.simulate_click(point(px(292.), px(20.)), Modifiers::default());
        assert_eq!(
            visual.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some(ERROR.to_owned()),
        );
    }
}

#[cfg(test)]
mod worktree_tests;
