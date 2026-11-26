use std::collections::HashMap;

use gpui_component::{
    WindowExt as _,
    notification::{Notification, NotificationDelivery},
    progress::Progress as ProgressBar,
};

use super::*;
use crate::{
    language::tools::{Bundle, InstallStage, Installed, Progress, Tools},
    remote::{LspLaunch, LspSetupStatus, Request, SshSession},
};

#[derive(Clone)]
enum Source {
    Local(Tools),
    Remote(SshSession, LspLaunch),
}

impl Source {
    fn install(&self, bundle: Bundle, progress: impl FnMut(Progress)) -> anyhow::Result<Installed> {
        match self {
            Self::Local(tools) => tools.install(bundle, None, progress),
            Self::Remote(ssh, options) => ssh.install_language_tools(options, progress),
        }
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct InstallKey {
    host: String,
    bundle: Bundle,
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct PromptKey {
    install: InstallKey,
    server: LanguageServer,
}

impl PromptKey {
    fn element_id(&self) -> SharedString {
        format!("{}:{}", self.server.id(), self.install.host).into()
    }
}

struct SetupPrompt;

#[derive(Default)]
struct SetupRegistry {
    installs: HashMap<InstallKey, Entity<Installation>>,
    shown: HashMap<PromptKey, AnyWindowHandle>,
}

impl Global for SetupRegistry {}

enum InstallState {
    Missing,
    Installing(Option<Progress>),
    Failed(String),
    Ready(Installed),
}

struct Installation {
    state: InstallState,
    _task: Task<()>,
}

pub(in crate::workspace) struct Setup {
    source: Source,
    prompt: PromptKey,
    shared: Entity<Installation>,
    _watch: Subscription,
}

impl Setup {
    pub(super) fn status(&self, cx: &App) -> ServiceStatus {
        match &self.shared.read(cx).state {
            InstallState::Missing => ServiceStatus::NotInstalled,
            InstallState::Installing(_) | InstallState::Ready(_) => ServiceStatus::Installing,
            InstallState::Failed(_) => ServiceStatus::InstallFailed,
        }
    }

    pub(super) fn version(&self) -> &str {
        self.prompt.install.bundle.version()
    }

    pub(super) fn progress_panel(&self, cx: &App, colors: Palette) -> Option<Div> {
        let state = &self.shared.read(cx).state;
        let (bundle, version, progress) = match state {
            InstallState::Installing(Some(progress)) => {
                (progress.bundle, progress.version.as_str(), Some(progress))
            }
            InstallState::Installing(None) => (self.prompt.install.bundle, self.version(), None),
            InstallState::Ready(installed) => (installed.bundle, installed.version.as_str(), None),
            _ => return None,
        };
        let (stage, archive, transfer) = match progress.map(|progress| &progress.stage) {
            None if matches!(state, InstallState::Ready(_)) => {
                (t!("lsp.install_publishing"), None, None)
            }
            None => (t!("common.starting"), None, None),
            Some(InstallStage::Waiting) => (t!("lsp.install_waiting"), None, None),
            Some(InstallStage::Downloading {
                archive,
                received,
                size,
            }) => (
                t!("lsp.install_downloading"),
                Some(archive),
                Some((*received, *size)),
            ),
            Some(InstallStage::Verifying(archive)) => {
                (t!("lsp.install_verifying"), Some(archive), None)
            }
            Some(InstallStage::Extracting(archive)) => {
                (t!("lsp.install_extracting"), Some(archive), None)
            }
            Some(InstallStage::Publishing) => (t!("lsp.install_publishing"), None, None),
        };
        let percent = transfer.and_then(|(received, size)| {
            size.filter(|size| *size > 0)
                .map(|size| received as f32 / size as f32 * 100.)
        });
        // The anchored panel keeps the same height as archives and transfer details come and go.
        Some(
            v_flex()
                .gap_1()
                .debug_selector(|| "lsp-install-progress".into())
                .child(progress_detail(
                    t!("lsp.install_progress_label"),
                    stage,
                    colors,
                ))
                .child(progress_detail(
                    t!("lsp.install_current_tool"),
                    format!("{} {version}", bundle.display_name()),
                    colors,
                ))
                .child(div().h(PROGRESS_ROW_HEIGHT).when_some(
                    archive.zip(progress),
                    |slot, (archive, progress)| {
                        let archive: SharedString = archive.clone().into();
                        slot.child(
                            progress_detail(
                                t!(
                                    "lsp.install_archive",
                                    index = progress.completed + 1,
                                    total = progress.total
                                ),
                                archive.clone(),
                                colors,
                            )
                            .id("lsp-install-archive")
                            .managed_tooltip(move |window, cx| {
                                Tooltip::new(archive.clone()).build(window, cx)
                            }),
                        )
                    },
                ))
                .child(div().h(PROGRESS_ROW_HEIGHT).when_some(
                    transfer,
                    |slot, (received, size)| {
                        let received = download_size(received);
                        let value = size.map_or_else(
                            || {
                                t!("lsp.install_received", received = received.as_str())
                                    .into_owned()
                            },
                            |size| format!("{received} / {}", download_size(size)),
                        );
                        slot.child(progress_detail(t!("lsp.install_transfer"), value, colors))
                    },
                ))
                .child(
                    ProgressBar::new("lsp-install-download")
                        .xsmall()
                        .color(colors.accent)
                        .accessibility_label(t!("lsp.install_progress_label"))
                        .loading(percent.is_none())
                        .value(percent.unwrap_or_default()),
                ),
        )
    }

    pub(super) fn error(&self, cx: &App) -> Option<String> {
        match &self.shared.read(cx).state {
            InstallState::Failed(error) => Some(error.clone()),
            _ => None,
        }
    }
}

const PROGRESS_ROW_HEIGHT: Pixels = px(18.);

fn progress_detail(
    label: impl Into<SharedString>,
    value: impl Into<SharedString>,
    colors: Palette,
) -> Div {
    service_detail_columns(
        div().truncate().child(label.into()),
        div().truncate().child(value.into()),
        colors,
    )
    .h(PROGRESS_ROW_HEIGHT)
    .line_height(PROGRESS_ROW_HEIGHT)
}

fn download_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

struct Probe {
    source: Source,
    host: String,
    installed: Option<Installed>,
}

impl Workspace {
    // Return early for commands that cannot use a Stroll bundle.
    pub(super) fn check_language_tools(&mut self, key: &ServerKey, cx: &mut Context<Self>) -> bool {
        let server = match &key.server {
            ServiceServer::Builtin(server)
                if key.remote
                    || matches!(
                        server.local_command(&self.startup_lsp.server(*server).command),
                        Some((_, CommandSource::Path))
                    ) =>
            {
                Some(*server)
            }
            ServiceServer::Worktree(server) => server.0.managed,
            _ => None,
        };
        let Some((server, bundle)) = server.and_then(|server| Some((server, server.bundle()?)))
        else {
            return false;
        };
        let remote = key.remote.then(|| self.remote_lsp_launch(key)).transpose();
        let checking = cx.background_spawn(async move {
            if let Some((ssh, options)) = remote? {
                let Some(LspSetupStatus { account, installed }) =
                    ssh.call(Request::LspSetupStatus(options.clone()))?
                else {
                    return Ok(None);
                };
                let target = ssh.target();
                let address = target
                    .host
                    .rsplit_once('@')
                    .map_or(target.host.as_str(), |(_, address)| address);
                let port = target.port.unwrap_or(22);
                return Ok(Some(Probe {
                    host: format!("ssh:[{address}]:{port}:{account}"),
                    source: Source::Remote(ssh, options),
                    installed,
                }));
            }
            let home = crate::language::language_home()
                .ok_or_else(|| anyhow::anyhow!("Could not determine the Stroll directory"))?;
            let tools = Tools::new(home)?;
            let installed = tools.lookup(bundle, None)?;
            Ok::<_, anyhow::Error>(Some(Probe {
                host: "local".into(),
                source: Source::Local(tools),
                installed,
            }))
        });
        let key = key.clone();
        let checked = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let result = checking.await;
                _ = this.update(cx, |this, cx| {
                    if !this.can_start_language_service(&key)
                        || this.language_service_files(&key).is_empty()
                    {
                        this.language_servers.remove(&key);
                        return;
                    }
                    match result {
                        Ok(Some(probe)) => {
                            this.language_tools_checked(&key, server, bundle, probe, cx)
                        }
                        Ok(None) => this.launch_language_service(&key, None, cx),
                        Err(error) => {
                            this.language_servers
                                .insert(key, LanguageService::Failed(format!("{error:#}")));
                            cx.notify();
                        }
                    }
                });
            }
        });
        self.language_servers
            .insert(key, LanguageService::Checking { _task: checked });
        cx.notify();
        true
    }

    fn language_tools_checked(
        &mut self,
        key: &ServerKey,
        server: LanguageServer,
        bundle: Bundle,
        probe: Probe,
        cx: &mut Context<Self>,
    ) {
        let Probe {
            source,
            host,
            installed,
        } = probe;
        if let Some(installed) = installed {
            self.launch_language_service(key, Some(installed), cx);
            return;
        }
        let install = InstallKey { host, bundle };
        let shared = cx
            .default_global::<SetupRegistry>()
            .installs
            .get(&install)
            .cloned()
            .unwrap_or_else(|| {
                let shared = cx.new(|_| Installation {
                    state: InstallState::Missing,
                    _task: Task::ready(()),
                });
                cx.default_global::<SetupRegistry>()
                    .installs
                    .insert(install.clone(), shared.clone());
                shared
            });
        let prompt = PromptKey { install, server };
        let watch = cx.observe(&shared, {
            let key = key.clone();
            move |this, shared, cx| {
                if let InstallState::Ready(installed) = &shared.read(cx).state {
                    if this.can_start_language_service(&key)
                        && !this.language_service_files(&key).is_empty()
                        && matches!(this.language_servers.get(&key),
                            Some(LanguageService::Setup(setup)) if setup.shared == shared)
                    {
                        this.launch_language_service(&key, Some(installed.clone()), cx);
                    } else if matches!(this.language_servers.get(&key),
                        Some(LanguageService::Setup(setup)) if setup.shared == shared)
                    {
                        this.language_servers.remove(&key);
                    }
                }
                cx.notify();
            }
        });
        if let InstallState::Ready(installed) = &shared.read(cx).state {
            self.launch_language_service(key, Some(installed.clone()), cx);
            return;
        }
        let setup = Setup {
            source,
            prompt,
            shared,
            _watch: watch,
        };
        self.show_install_prompt(key, &setup, cx);
        self.language_servers
            .insert(key.clone(), LanguageService::Setup(setup));
        cx.notify();
    }

    fn show_install_prompt(&self, key: &ServerKey, setup: &Setup, cx: &mut Context<Self>) {
        if !matches!(setup.shared.read(cx).state, InstallState::Missing) {
            return;
        }
        let prompt = setup.prompt.clone();
        let handle = self.lsp_window;
        if cx
            .default_global::<SetupRegistry>()
            .shown
            .contains_key(&prompt)
            || self
                .store
                .lsp_prompt_dismissed(&prompt.install.host, prompt.server.id())
                .unwrap_or(false)
        {
            return;
        }
        let message = match &setup.source {
            Source::Local(_) => t!("lsp.install_prompt", server = prompt.server.name()),
            Source::Remote(ssh, _) => {
                let target = ssh.target();
                let host = target.port.map_or_else(
                    || target.host.clone(),
                    |port| format!("{}:{port}", target.host),
                );
                t!(
                    "lsp.install_prompt_remote",
                    server = prompt.server.name(),
                    host = host
                )
            }
        }
        .into_owned();
        let store = self.store.clone();
        let workspace = cx.weak_entity();
        let key = key.clone();
        cx.default_global::<SetupRegistry>()
            .shown
            .insert(prompt.clone(), handle);
        cx.defer(move |cx| {
            _ = handle.update(cx, |_, window, cx| {
                let notification = Notification::info(message)
                    .delivery(NotificationDelivery::InApp)
                    .id1::<SetupPrompt>(prompt.element_id())
                    .content(move |_, _, _| {
                        let (store, workspace, key, prompt) = (
                            store.clone(),
                            workspace.clone(),
                            key.clone(),
                            prompt.clone(),
                        );
                        let dismiss_id = prompt.element_id();
                        let host = prompt.install.host.clone();
                        let server = prompt.server.id();
                        h_flex()
                            .mt_2()
                            .gap_2()
                            .justify_end()
                            .child(
                                action_button("dismiss-language-server-toast")
                                    .debug_selector(|| "dismiss-language-server-toast".into())
                                    .small()
                                    .ghost()
                                    .label(t!("lsp.dismiss"))
                                    .on_click(move |_, window, cx| {
                                        _ = store.dismiss_lsp_prompt(&host, server);
                                        window.remove_notification1::<SetupPrompt>(
                                            dismiss_id.clone(),
                                            cx,
                                        );
                                    }),
                            )
                            .child(
                                action_button("install-language-server-toast")
                                    .debug_selector(|| "install-language-server-toast".into())
                                    .small()
                                    .secondary()
                                    .label(t!("lsp.install"))
                                    .on_click(move |_, _, cx| {
                                        _ = workspace.update(cx, |this, cx| {
                                            this.install_language_tools(&key, Some(&prompt), cx)
                                        });
                                    }),
                            )
                            .into_any_element()
                    });
                super::super::toast::push(notification, window, cx);
            });
        });
    }

    pub(super) fn install_active_language_tools(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self
            .active_file()
            .and_then(|file| file.server_project.as_ref())
            .map(|project| project.key.clone())
        {
            self.install_language_tools(&key, None, cx);
        }
    }

    fn install_language_tools(
        &mut self,
        key: &ServerKey,
        prompt: Option<&PromptKey>,
        cx: &mut Context<Self>,
    ) {
        if !self.can_start_language_service(key) || self.language_service_files(key).is_empty() {
            return;
        }
        let Some(LanguageService::Setup(setup)) = self.language_servers.get(key) else {
            return;
        };
        if prompt.is_some_and(|prompt| prompt != &setup.prompt)
            || !matches!(
                setup.shared.read(cx).state,
                InstallState::Missing | InstallState::Failed(_)
            )
        {
            return;
        }
        let source = setup.source.clone();
        let install = setup.prompt.install.clone();
        let prompts = cx
            .default_global::<SetupRegistry>()
            .shown
            .iter()
            .filter(|(prompt, _)| prompt.install == install)
            .map(|(prompt, handle)| (prompt.element_id(), *handle))
            .collect::<Vec<_>>();
        cx.defer(move |cx| {
            for (id, handle) in prompts {
                _ = handle.update(cx, |_, window, cx| {
                    window.remove_notification1::<SetupPrompt>(id, cx)
                });
            }
        });
        let (update, updates) = async_channel::unbounded();
        let work = cx.background_spawn(async move {
            source.install(install.bundle, |progress| {
                _ = update.send_blocking(progress);
            })
        });
        setup.shared.update(cx, |shared, cx| {
            shared.state = InstallState::Installing(None);
            shared._task = cx.spawn(async move |this, cx| {
                while let Ok(progress) = updates.recv().await {
                    _ = this.update(cx, |shared, cx| {
                        shared.state = InstallState::Installing(Some(progress));
                        cx.notify();
                    });
                }
                let result = work.await;
                _ = this.update(cx, |shared, cx| {
                    shared.state = match result {
                        Ok(installed) => InstallState::Ready(installed),
                        Err(error) => InstallState::Failed(format!("{error:#}")),
                    };
                    cx.notify();
                });
            });
            cx.notify();
        });
        if prompt.is_some()
            && self
                .active_file()
                .and_then(|file| file.server_project.as_ref())
                .is_some_and(|project| &project.key == key)
        {
            self.open_language_service_panel(cx);
            cx.notify();
        }
    }
}

#[cfg(test)]
#[path = "setup_tests.rs"]
mod tests;
