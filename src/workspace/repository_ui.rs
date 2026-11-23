use rust_i18n::t;

mod hosts;
#[cfg(all(test, target_os = "linux"))]
mod picker_tests;
#[cfg(all(test, target_os = "linux"))]
mod ssh_tests;

use std::{
    borrow::Cow,
    collections::BTreeSet,
    env,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Disableable as _, InteractiveElementExt as _, MODAL_POPUP_PRIORITY,
    PopupFrame, PopupMotion, PopupMotionKind, Selectable as _, StyleSized as _, StyledExt as _,
    ThemeStyled as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    popover::Popover,
    scroll::{ScrollableElement as _, ScrollbarViewport as _},
    tooltip::{ManagedTooltipExt as _, Tooltip},
    v_flex, window_corner_radii,
};
use gpui_kit_assets::IconName;

use crate::{
    paths,
    remote::{Request, SshSession, SshTarget},
};

use crate::repository::{
    DirectoryEntry, DirectoryListing, Location, RecentRepository, Repository, Store, Worktree,
    directory_name, project_name, ssh_project_name,
};

use super::{
    CloseRepositoryPicker, CompleteRepositoryPath, NextRepositoryPath, OpenRepository, Palette,
    PreviousRepositoryPath, Workspace, action_button, compact_icon_button, icon, icon_button,
    quiet_button, text_base_size, topbar_segment, topbar_segments,
};

const REPOSITORY_PICKER_SCROLL_GUTTER: Pixels = px(11.);

#[derive(Clone, Copy, PartialEq)]
enum PickerTab {
    Recent,
    Browse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathAction {
    Open,
    Browse,
    OpenOrBrowse,
    ConnectAndGo,
}

impl PathAction {
    fn label(self) -> Cow<'static, str> {
        match self {
            Self::Open => t!("repository.open"),
            Self::Browse => t!("repository.browse"),
            Self::OpenOrBrowse => t!("repository.open_or_browse"),
            Self::ConnectAndGo => t!("repository.connect_and_go"),
        }
    }
}

struct RemotePicker {
    input: Option<Entity<InputState>>,
    _events: Option<Subscription>,
    active: bool,
    suggestions: bool,
    index: usize,
    hosts: Vec<String>,
    discovery: Task<()>,
    connection: Option<SshSession>,
    connecting: bool,
    task: Task<()>,
    error: Option<String>,
    legacy: bool,
    completion_cache: Option<(String, Option<String>, Instant, Vec<DirectoryEntry>)>,
}

pub(super) struct RepositoryUi {
    input: Option<Entity<InputState>>,
    _input_events: Option<Subscription>,
    remote: RemotePicker,
    previous_focus: Option<FocusHandle>,
    menu_focus: FocusHandle,
    trigger_focus: FocusHandle,
    menu_rows: Vec<FocusHandle>,
    menu_scroll: ScrollHandle,
    directory_focus: FocusHandle,
    pub(super) menu_open: bool,
    pub(super) picker_open: bool,
    motion: Option<PopupMotion>,
    pub(super) opening: Option<PathBuf>,
    pub(super) open_task: Task<()>,
    pub(super) error: Option<String>,
    pub(super) menu_error: Option<String>,
    completions: Vec<DirectoryEntry>,
    completion_index: usize,
    completion_scroll: ScrollHandle,
    completion_task: Task<()>,
    tab: PickerTab,
    home: Option<PathBuf>,
    recents: Vec<RecentRepository>,
    unavailable: BTreeSet<PathBuf>,
    recents_task: Task<()>,
    worktrees: Vec<Worktree>,
    worktree_error: Option<String>,
    worktrees_loading: bool,
    worktree_task: Task<()>,
    directory: Option<DirectoryListing>,
    directory_scroll: ScrollHandle,
    browsing: Option<PathBuf>,
    directory_error: Option<String>,
    directory_task: Task<()>,
}

impl RemotePicker {
    fn input(&self) -> &Entity<InputState> {
        self.input.as_ref().expect("repository host input")
    }

    fn connected(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|session| session.failure().is_none())
    }
}

impl RepositoryUi {
    fn input(&self) -> &Entity<InputState> {
        self.input.as_ref().expect("repository path input")
    }

    fn path_action(&self, cx: &App) -> (PathAction, Option<PathBuf>) {
        let value = self.input().read(cx).value();
        let value = value.trim();
        let ssh_address = !self.remote.active && value.starts_with("ssh://");
        let path = if self.remote.active || ssh_address {
            PathBuf::from(value)
        } else {
            resolve_typed_path(value, self.home.as_deref())
        };
        let connected = self.remote.connected();
        let action = if self.remote.active && !connected {
            PathAction::ConnectAndGo
        } else if ssh_address || (self.remote.active && self.remote.legacy) {
            PathAction::Open
        } else if value.is_empty() {
            PathAction::OpenOrBrowse
        } else {
            match self.known_path_kind(&path) {
                Some(true) => PathAction::Open,
                Some(false) => PathAction::Browse,
                None => PathAction::OpenOrBrowse,
            }
        };
        let available = !value.is_empty()
            && self.opening.is_none()
            && self.browsing.is_none()
            && !self.remote.connecting
            && (action != PathAction::ConnectAndGo
                || !self.remote.input().read(cx).value().trim().is_empty());
        (action, available.then_some(path))
    }

    fn known_path_kind(&self, path: &Path) -> Option<bool> {
        let remote = self.remote.active;
        // Only submit a cached SSH path directly when it is absolute on the remote host.
        let matches = |candidate: &Path| {
            if remote {
                path.to_str()
                    .zip(candidate.to_str())
                    .is_some_and(|(path, candidate)| {
                        path.starts_with('/')
                            && path.trim_end_matches('/') == candidate.trim_end_matches('/')
                    })
            } else {
                let path = paths::simplified(path);
                let candidate = paths::simplified(candidate);
                path == candidate
                    || (path.is_relative()
                        && env::current_dir().is_ok_and(|cwd| cwd.join(path) == candidate))
            }
        };
        if let Some(directory) = &self.directory {
            if matches(&directory.path) {
                return Some(directory.repository);
            }
            if let Some(entry) = directory.entries.iter().find(|entry| matches(&entry.path)) {
                return Some(entry.repository);
            }
        }
        let authority = self
            .remote
            .connection
            .as_ref()
            .map(|session| session.target().authority());
        self.recents
            .iter()
            .filter(|item| !self.unavailable.contains(&item.root))
            .any(|item| match SshTarget::parse(&item.root) {
                Ok(Some(target)) => {
                    remote
                        && authority.as_deref() == Some(&target.authority())
                        && matches(&target.path)
                }
                Ok(None) => !remote && matches(&item.root),
                Err(_) => false,
            })
            .then_some(true)
    }

    pub(super) fn refresh_language(&self, window: &mut Window, cx: &mut App) {
        if let Some(input) = &self.remote.input {
            input.update(cx, |input, cx| {
                input.set_placeholder(t!("repository.ssh_host_hint"), window, cx);
            });
        }
    }

    pub(super) fn new(
        store: Option<&Store>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let (input, input_events, host, host_events) =
            store.map_or((None, None, None, None), |_| {
                let input =
                    cx.new(|cx| InputState::new(window, cx).placeholder("~/projects/example"));
                let input_events = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
                    if !this.repository_picker_visible() {
                        return;
                    }
                    match event {
                        InputEvent::Change => {
                            this.repository_ui.error = None;
                            if !this.accept_ssh_address(window, cx) {
                                this.refresh_path_completions(window, cx);
                            }
                        }
                        InputEvent::Blur => {
                            this.repository_ui.close_path_completions();
                            cx.notify();
                        }
                        InputEvent::PressEnter { .. } => this.confirm_repository_path(window, cx),
                        _ => {}
                    }
                });
                let host = cx.new(|cx| {
                    InputState::new(window, cx).placeholder(t!("repository.ssh_host_hint"))
                });
                let host_events =
                    cx.subscribe_in(&host, window, |this, _, event, window, cx| match event {
                        InputEvent::Change => {
                            if this.repository_ui.directory.is_some() {
                                this.repository_ui
                                    .input()
                                    .update(cx, |input, cx| input.set_value("", window, cx));
                            }
                            this.reset_ssh_picker();
                            this.repository_ui.remote.suggestions = true;
                            this.repository_ui.remote.index = 0;
                            cx.notify();
                        }
                        InputEvent::Focus => {
                            this.repository_ui.remote.suggestions = true;
                            cx.notify();
                        }
                        InputEvent::Blur => {
                            this.repository_ui.remote.suggestions = false;
                            cx.notify();
                        }
                        InputEvent::PressEnter { .. } => this.confirm_ssh_host(window, cx),
                        InputEvent::GutterClick(_) => {}
                    });
                (
                    Some(input),
                    Some(input_events),
                    Some(host),
                    Some(host_events),
                )
            });
        let (recents, error) = store.map_or((Vec::new(), None), |store| {
            match store.recent_repositories() {
                Ok(recents) => (recents, None),
                Err(error) => (Vec::new(), Some(format!("{error:#}"))),
            }
        });
        Self {
            input,
            _input_events: input_events,
            remote: RemotePicker {
                input: host,
                _events: host_events,
                active: false,
                suggestions: false,
                index: 0,
                hosts: Vec::new(),
                discovery: Task::ready(()),
                connection: None,
                connecting: false,
                task: Task::ready(()),
                error: None,
                legacy: false,
                completion_cache: None,
            },
            previous_focus: None,
            menu_focus: cx.focus_handle(),
            trigger_focus: cx.focus_handle(),
            menu_rows: Vec::new(),
            menu_scroll: ScrollHandle::new(),
            directory_focus: cx.focus_handle(),
            menu_open: false,
            picker_open: false,
            motion: None,
            opening: None,
            open_task: Task::ready(()),
            error,
            menu_error: None,
            completions: Vec::new(),
            completion_index: 0,
            completion_scroll: ScrollHandle::new(),
            completion_task: Task::ready(()),
            tab: PickerTab::Recent,
            home: env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(PathBuf::from),
            recents,
            unavailable: BTreeSet::new(),
            recents_task: Task::ready(()),
            worktrees: Vec::new(),
            worktree_error: None,
            worktrees_loading: false,
            worktree_task: Task::ready(()),
            directory: None,
            directory_scroll: ScrollHandle::new(),
            browsing: None,
            directory_error: None,
            directory_task: Task::ready(()),
        }
    }

    pub(super) fn close_path_completions(&mut self) {
        self.completion_task = Task::ready(());
        self.completions.clear();
    }

    pub(super) fn picker_opacity(&mut self, window: &Window, cx: &App) -> f32 {
        let opacity = self
            .motion
            .as_ref()
            .map_or(0., |motion| motion.opacity(window, cx));
        if !self.picker_open && opacity <= 0. {
            self.motion = None;
        }
        opacity
    }

    pub(super) fn reset_worktrees(&mut self) {
        self.worktree_task = Task::ready(());
        self.worktrees.clear();
        self.worktree_error = None;
        self.worktrees_loading = false;
    }

    pub(super) fn record_open(&mut self, repo: &Repository) {
        RecentRepository::record(&mut self.recents, repo);
        self.unavailable.remove(&repo.root);
    }

    pub(super) fn has_recent(&self, repo: &Repository) -> bool {
        self.recents
            .iter()
            .any(|item| item.common_dir == repo.common_dir)
    }

    fn host_candidates(&self, cx: &App) -> Vec<String> {
        let query = self
            .remote
            .input()
            .read(cx)
            .value()
            .trim()
            .to_ascii_lowercase();
        let mut seen = BTreeSet::new();
        self.recents
            .iter()
            .filter_map(|item| SshTarget::parse(&item.root).ok().flatten())
            .map(|target| target.authority())
            .chain(self.remote.hosts.iter().cloned())
            .filter(|host| {
                host.to_ascii_lowercase().contains(&query) && seen.insert(host.to_ascii_lowercase())
            })
            .take(12)
            .collect()
    }

    fn path_label(&self, path: &Path) -> String {
        let path = paths::simplified(path);
        if let Some(relative) = self
            .home
            .as_deref()
            .map(paths::simplified)
            .and_then(|home| path.strip_prefix(&home).ok())
        {
            return if relative.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", paths::display(&relative))
            };
        }
        paths::display(&path)
    }

    fn repository_path_label(&self, root: &Path, common_dir: &Path, home: Option<&Path>) -> String {
        ssh_repository_path_label(root, common_dir, home).unwrap_or_else(|| self.path_label(root))
    }
}

impl Workspace {
    pub(super) fn repository_overlay_open(&self) -> bool {
        self.repository_ui.menu_open || self.repository_ui.picker_open
    }

    fn repository_picker_visible(&self) -> bool {
        self.standalone.is_none() && (self.repository.is_none() || self.repository_ui.picker_open)
    }

    pub fn show_repository_start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.prepare_repository_picker(cx);
        self.repository_ui
            .input()
            .update(cx, |input, cx| input.focus(window, cx));
    }

    pub(super) fn choose_repository(
        &mut self,
        _: &OpenRepository,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.standalone.is_some()
            || self.confirming_close
            || self.repository_ui.opening.is_some()
        {
            return;
        }
        if self.repository.is_none() {
            self.repository_ui
                .input()
                .update(cx, |input, cx| input.focus(window, cx));
        } else if !self.repository_ui.picker_open {
            if let Some(file) = self.active_file() {
                file.editor
                    .update(cx, |editor, cx| editor.dismiss_lsp_overlays(cx));
            }
            self.open_files_list = false;
            self.repository_ui.menu_open = false;
            self.repository_ui.error = None;
            self.repository_ui.previous_focus =
                if self.repository_ui.menu_focus.contains_focused(window, cx) {
                    Some(self.repository_ui.trigger_focus.clone())
                } else {
                    window.focused(cx)
                };
            self.repository_ui.picker_open = true;
            let now = Instant::now();
            self.repository_ui
                .motion
                .get_or_insert_with(|| PopupMotion::new(PopupMotionKind::Popover, now))
                .retarget(true, now);
            self.repository_ui.input().update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
            self.repository_ui.remote.active = false;
            self.prepare_repository_picker(cx);
            if let Some(ssh) = self.repository.as_ref().and_then(|repo| repo.ssh.as_ref()) {
                let host = ssh.target().authority();
                let path = SshTarget::parse(&self.repository.as_ref().unwrap().root)
                    .ok()
                    .flatten()
                    .map(|target| target.path);
                self.repository_ui.remote.active = true;
                self.reset_ssh_picker();
                self.repository_ui.tab = PickerTab::Recent;
                self.repository_ui
                    .remote
                    .input()
                    .update(cx, |input, cx| input.set_value(host, window, cx));
                if let Some(path) = path {
                    self.repository_ui.input().update(cx, |input, cx| {
                        input.set_value(path.display().to_string(), window, cx)
                    });
                }
                self.discover_ssh_hosts(cx);
            }
        } else {
            self.repository_ui
                .input()
                .update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    fn prepare_repository_picker(&mut self, cx: &mut Context<Self>) {
        let ui = &mut self.repository_ui;
        ui.tab = if ui.recents.is_empty() {
            PickerTab::Browse
        } else {
            PickerTab::Recent
        };
        ui.directory_task = Task::ready(());
        ui.directory = None;
        ui.directory_scroll = ScrollHandle::new();
        ui.browsing = None;
        ui.directory_error = None;
        self.refresh_recents(cx);
        if !self.repository_ui.remote.active && self.repository_ui.tab == PickerTab::Browse {
            self.load_initial_directory(cx);
        }
    }

    fn discover_ssh_hosts(&mut self, cx: &mut Context<Self>) {
        let Some(home) = self.repository_ui.home.clone() else {
            return;
        };
        let task = cx.background_spawn(async move { hosts::aliases(&home) });
        self.repository_ui.remote.discovery = cx.spawn(async move |this, cx| {
            let hosts = task.await;
            _ = this.update(cx, |this, cx| {
                this.repository_ui.remote.hosts = hosts;
                cx.notify();
            });
        });
    }

    fn reset_ssh_picker(&mut self) {
        let ui = &mut self.repository_ui;
        ui.remote.task = Task::ready(());
        ui.remote.connection = None;
        ui.remote.connecting = false;
        ui.remote.error = None;
        ui.remote.legacy = false;
        ui.remote.completion_cache = None;
        ui.directory_task = Task::ready(());
        ui.directory = None;
        ui.directory_error = None;
        ui.browsing = None;
        ui.close_path_completions();
    }

    fn select_location_mode(&mut self, remote: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.repository_ui.remote.active == remote {
            return;
        }
        self.reset_ssh_picker();
        self.repository_ui.remote.active = remote;
        self.repository_ui.error = None;
        self.repository_ui.input().update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.set_placeholder(
                if remote {
                    "~/projects/repo"
                } else {
                    "~/projects/example"
                },
                window,
                cx,
            );
        });
        if remote {
            self.discover_ssh_hosts(cx);
            self.repository_ui
                .remote
                .input()
                .update(cx, |input, cx| input.focus(window, cx));
        } else {
            self.repository_ui
                .input()
                .update(cx, |input, cx| input.focus(window, cx));
            if self.repository_ui.tab == PickerTab::Browse {
                self.load_initial_directory(cx);
            }
        }
        cx.notify();
    }

    fn accept_ssh_address(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let value = self.repository_ui.input().read(cx).value();
        let Ok(Some(target)) = SshTarget::parse(Path::new(value.trim())) else {
            return false;
        };
        self.select_location_mode(true, window, cx);
        self.repository_ui.remote.input().update(cx, |input, cx| {
            input.set_value(target.authority(), window, cx)
        });
        self.repository_ui.input().update(cx, |input, cx| {
            input.set_value(target.path.display().to_string(), window, cx)
        });
        self.connect_ssh_picker(target.authority(), Some(target.path), false, window, cx);
        true
    }

    fn confirm_ssh_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ui = &self.repository_ui;
        if ui.opening.is_some() || ui.remote.connecting {
            return;
        }
        let typed = ui.remote.input().read(cx).value().trim().to_owned();
        let hosts = ui.host_candidates(cx);
        let authority = if ui.remote.suggestions
            && (typed.is_empty()
                || ui.remote.index > 0
                || (hosts.len() == 1
                    && hosts[0]
                        .to_ascii_lowercase()
                        .starts_with(&typed.to_ascii_lowercase())))
        {
            hosts.get(ui.remote.index).cloned().unwrap_or(typed)
        } else {
            typed
        };
        if !authority.is_empty()
            && !ui.remote.connection.as_ref().is_some_and(|session| {
                session.failure().is_none() && session.target().authority() == authority
            })
        {
            self.choose_ssh_host(authority, window, cx);
        }
    }

    fn choose_ssh_host(&mut self, authority: String, window: &mut Window, cx: &mut Context<Self>) {
        self.repository_ui.remote.input().update(cx, |input, cx| {
            input.set_value(authority.clone(), window, cx)
        });
        let path = if self.repository_ui.directory.is_none() {
            self.repository_ui
                .input()
                .read(cx)
                .value()
                .trim()
                .to_owned()
        } else {
            String::new()
        };
        self.connect_ssh_picker(
            authority,
            (!path.is_empty()).then(|| PathBuf::from(path)),
            false,
            window,
            cx,
        );
    }

    fn connect_ssh_picker(
        &mut self,
        authority: String,
        path: Option<PathBuf>,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reset_ssh_picker();
        let ui = &mut self.repository_ui;
        ui.remote.suggestions = false;
        ui.remote.connecting = true;
        ui.tab = PickerTab::Browse;
        let previous = ui
            .recents
            .iter()
            .filter_map(|item| SshTarget::parse(&item.root).ok().flatten())
            .find(|target| target.authority() == authority)
            .map(|target| target.path);
        let browse_path = path.or(previous).unwrap_or_else(|| PathBuf::from("~"));
        let query = browse_path.to_string_lossy().into_owned();
        let task = cx.background_spawn(async move {
            let target = SshTarget::parse(Path::new(&format!("ssh://{authority}/.")))?
                .ok_or_else(|| anyhow::anyhow!(t!("repository.ssh_host_required")))?;
            let session = SshSession::connect(target)?;
            let supports_browse = session
                .call::<u32>(Request::Hello(crate::remote::BROWSE_VERSION))
                .is_ok_and(|version| version == crate::remote::BROWSE_VERSION);
            let directory = supports_browse.then(|| session.browse_directory(query, None));
            Ok::<_, anyhow::Error>((session, directory))
        });
        ui.remote.task = cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, move |this, window, cx| {
                let ui = &mut this.repository_ui;
                ui.remote.connecting = false;
                match result {
                    Ok((session, directory)) => {
                        ui.remote.legacy = directory.is_none();
                        ui.remote.connection = Some(session);
                        ui.input().update(cx, |input, cx| input.focus(window, cx));
                        match directory {
                            Some(Ok(directory)) => {
                                this.apply_ssh_directory(directory, open, window, cx)
                            }
                            Some(Err(error)) => ui.directory_error = Some(format!("{error:#}")),
                            None if open => this.submit_repository_path(window, cx),
                            None => {}
                        }
                    }
                    Err(error) => ui.remote.error = Some(format!("{error:#}")),
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    fn apply_ssh_directory(
        &mut self,
        directory: DirectoryListing,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ui = &mut self.repository_ui;
        ui.input().update(cx, |input, cx| {
            input.set_value(directory.path.display().to_string(), window, cx)
        });
        ui.directory_scroll.set_offset(point(px(0.), px(0.)));
        let should_open = open && directory.repository;
        let path = directory.path.clone();
        ui.directory = Some(directory);
        if should_open {
            self.open_browsed_directory(path, window, cx);
        }
    }

    fn open_browsed_directory(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.repository_ui.remote.active {
            let remote = &mut self.repository_ui.remote;
            let session = remote.connection.take();
            let root = if let Some(session) = &session {
                session.target().locator(&path)
            } else {
                let authority = remote.input().read(cx).value();
                let address = format!("ssh://{}/.", authority.trim());
                let Ok(Some(target)) = SshTarget::parse(Path::new(&address)) else {
                    return;
                };
                target.locator(&path)
            };
            self.repository_ui.close_path_completions();
            self.open_repository_with_ssh(Location { root, file: None }, session, window, cx);
        } else {
            self.open_directory_path(path, window, cx);
        }
    }

    fn browse_ssh_directory(
        &mut self,
        path: PathBuf,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ui = &mut self.repository_ui;
        if ui.remote.connecting || ui.remote.legacy {
            return;
        }
        let Some(session) = ui
            .remote
            .connection
            .clone()
            .filter(|session| session.failure().is_none())
        else {
            let authority = ui.remote.input().read(cx).value().trim().to_owned();
            if !authority.is_empty() {
                self.connect_ssh_picker(authority, Some(path), open, window, cx);
            }
            return;
        };
        let base = ui
            .directory
            .as_ref()
            .map(|dir| dir.path.display().to_string());
        ui.browsing = Some(path.clone());
        ui.directory_error = None;
        ui.tab = PickerTab::Browse;
        let query = cx.background_spawn(async move {
            session.browse_directory(path.display().to_string(), base)
        });
        ui.directory_task = cx.spawn_in(window, async move |this, cx| {
            let result = query.await;
            _ = this.update_in(cx, move |this, window, cx| {
                this.repository_ui.browsing = None;
                match result {
                    Ok(directory) => this.apply_ssh_directory(directory, open, window, cx),
                    Err(error) => this.repository_ui.directory_error = Some(format!("{error:#}")),
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    fn navigate_directory(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.repository_ui.remote.active {
            self.browse_ssh_directory(path, false, window, cx);
        } else {
            self.submit_local_directory(path, false, window, cx);
        }
    }

    pub(super) fn close_repository_picker(
        &mut self,
        _: &CloseRepositoryPicker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.repository_ui.completions.is_empty()
            && self
                .repository_ui
                .input()
                .focus_handle(cx)
                .is_focused(window)
        {
            self.repository_ui.close_path_completions();
            cx.notify();
            return;
        }
        if self.repository_ui.remote.suggestions {
            self.repository_ui.remote.suggestions = false;
            cx.notify();
            return;
        }
        if self.repository_ui.remote.connecting {
            self.reset_ssh_picker();
            cx.notify();
            return;
        }
        if self.cancel_ssh_open(cx) {
            return;
        }
        if !self.repository_ui.picker_open {
            cx.propagate();
            return;
        }
        if self.repository_ui.opening.is_none() {
            self.dismiss_repository_picker(window, cx);
            cx.notify();
        }
    }

    pub(super) fn dismiss_repository_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ui = &mut self.repository_ui;
        ui.close_path_completions();
        if !ui.picker_open {
            return;
        }
        ui.picker_open = false;
        ui.remote.task = Task::ready(());
        ui.remote.connection = None;
        ui.remote.connecting = false;
        ui.directory_task = Task::ready(());
        ui.error = None;
        if let Some(motion) = &mut ui.motion {
            motion.retarget(false, Instant::now());
        }
        ui.previous_focus
            .take()
            .unwrap_or_else(|| self.focus.clone())
            .focus(window, cx);
    }

    fn open_directory_path(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.repository_ui.close_path_completions();
        self.open_repository(Location { root, file: None }, window, cx);
    }

    fn submit_repository_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ui = &mut self.repository_ui;
        let (action, path) = ui.path_action(cx);
        let Some(path) = path else {
            return;
        };
        ui.close_path_completions();
        ui.error = None;
        match action {
            PathAction::Open if ui.remote.active && !path.to_string_lossy().starts_with('/') => {
                ui.error = Some(t!("repository.absolute_path_required").into());
                cx.notify();
            }
            PathAction::Open => self.open_browsed_directory(path, window, cx),
            PathAction::ConnectAndGo => {
                let authority = ui.remote.input().read(cx).value().trim().to_owned();
                self.connect_ssh_picker(authority, Some(path), true, window, cx);
            }
            PathAction::Browse | PathAction::OpenOrBrowse => {
                let open = action == PathAction::OpenOrBrowse;
                if ui.remote.active {
                    self.browse_ssh_directory(path, open, window, cx);
                } else {
                    self.submit_local_directory(path, open, window, cx);
                }
            }
        }
    }

    fn submit_local_directory(
        &mut self,
        root: PathBuf,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ui = &mut self.repository_ui;
        if ui.browsing.is_some() || ui.opening.is_some() {
            return;
        }
        ui.close_path_completions();
        ui.error = None;
        ui.browsing = Some(root.clone());
        ui.directory_error = None;
        let input_before = ui.input().read(cx).value();
        let path = root.clone();
        let task = cx.background_spawn(async move {
            if open && crate::repository::repository_root(&path).is_ok() {
                Ok(None)
            } else {
                DirectoryListing::read(path).map(Some)
            }
        });
        ui.directory_task = cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, move |this, window, cx| {
                let ui = &mut this.repository_ui;
                ui.browsing = None;
                match result {
                    Ok(None) => this.open_directory_path(root, window, cx),
                    Ok(Some(directory)) => {
                        if ui.input().read(cx).value() == input_before {
                            ui.input().update(cx, |input, cx| {
                                input.set_value(paths::display(&directory.path), window, cx)
                            });
                        }
                        ui.tab = PickerTab::Browse;
                        ui.directory = Some(directory);
                        ui.directory_scroll.set_offset(point(px(0.), px(0.)));
                    }
                    Err(error) => ui.error = Some(format!("{error:#}")),
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    fn refresh_path_completions(&mut self, window: &Window, cx: &mut Context<Self>) {
        let ui = &mut self.repository_ui;
        ui.close_path_completions();
        cx.notify();
        let value = ui.input().read(cx).value();
        let value = value.trim();
        if value.is_empty()
            || value.starts_with("ssh://")
            || ui.opening.is_some()
            || !ui.input().focus_handle(cx).is_focused(window)
        {
            return;
        }
        let value = value.to_owned();
        let remote = ui.remote.active;
        let session = ui.remote.connection.clone();
        if remote && (session.is_none() || ui.remote.legacy) {
            return;
        }
        let base = ui
            .directory
            .as_ref()
            .map(|dir| dir.path.display().to_string());
        if remote
            && let Some((cached, cached_base, at, entries)) = &ui.remote.completion_cache
            && cached == &value
            && cached_base == &base
            && at.elapsed() < Duration::from_secs(3)
        {
            ui.completions = entries.clone();
            ui.completion_index = 0;
            return;
        }
        let home = ui.home.clone();
        ui.completion_task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let query = value.clone();
            let query_base = base.clone();
            let matches = cx
                .background_spawn(async move {
                    if let Some(session) = session {
                        session
                            .call(Request::Complete {
                                path: query,
                                base: query_base,
                            })
                            .unwrap_or_default()
                    } else {
                        let (parent, prefix) = completion_query(&query, home.as_deref());
                        DirectoryEntry::complete(&parent, &prefix).unwrap_or_default()
                    }
                })
                .await;
            _ = this.update_in(cx, |this, window, cx| {
                let visible = this.repository_picker_visible();
                let ui = &mut this.repository_ui;
                if visible && ui.opening.is_none() && ui.input().focus_handle(cx).is_focused(window)
                {
                    if remote {
                        ui.remote.completion_cache =
                            Some((value, base, Instant::now(), matches.clone()));
                    }
                    ui.completions = matches;
                    ui.completion_index = 0;
                    ui.completion_scroll.set_offset(point(px(0.), px(0.)));
                    cx.notify();
                }
            });
        });
    }

    fn move_path_completion(&mut self, next: bool, window: &Window, cx: &mut Context<Self>) {
        let ui = &mut self.repository_ui;
        let count = ui.completions.len();
        if count == 0 {
            if next {
                self.refresh_path_completions(window, cx);
            }
            return;
        }
        let step = if next { 1 } else { count - 1 };
        ui.completion_index = (ui.completion_index + step) % count;
        ui.completion_scroll.scroll_to_item(ui.completion_index);
        cx.notify();
    }

    fn confirm_repository_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ui = &self.repository_ui;
        if let Some(entry) = ui.completions.get(ui.completion_index) {
            let value = ui.input().read(cx).value();
            if ui.remote.active {
                let path = entry.path.clone();
                if value.trim().trim_end_matches('/')
                    != path.to_string_lossy().trim_end_matches('/')
                {
                    self.choose_path_completion(path, window, cx);
                    return;
                }
                self.submit_repository_path(window, cx);
                return;
            }
            let typed = paths::simplified(&resolve_typed_path(value.trim(), ui.home.as_deref()));
            let entry_path = paths::simplified(&entry.path);
            let same_path = typed == entry_path
                || (typed.is_relative()
                    && env::current_dir().is_ok_and(|cwd| cwd.join(&typed) == entry_path));
            if !same_path {
                self.choose_path_completion(entry.path.clone(), window, cx);
                return;
            }
        }
        if ui.remote.active && ui.input().read(cx).value().trim().is_empty() {
            self.confirm_ssh_host(window, cx);
        } else {
            self.submit_repository_path(window, cx);
        }
    }

    fn choose_path_completion(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ui = &self.repository_ui;
        let value = ui.input().read(cx).value();
        let mut completed = if value.starts_with('~') && !ui.remote.active {
            ui.path_label(&path)
        } else {
            paths::display(&path)
        };
        let separator = if ui.remote.active {
            '/'
        } else {
            std::path::MAIN_SEPARATOR
        };
        if !completed.ends_with(separator)
            && !(!ui.remote.active && completed.ends_with(std::path::is_separator))
        {
            completed.push(separator);
        }
        ui.input().update(cx, |input, cx| {
            input.set_value(completed, window, cx);
        });
        self.repository_ui.close_path_completions();
        cx.notify();
    }

    fn repository_path_input(
        &self,
        busy: bool,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement + use<> {
        let ui = &self.repository_ui;
        let has_completions = !ui.completions.is_empty() && !busy;
        v_flex()
            .id("repository-path-input")
            .flex_1()
            .min_w_0()
            .relative()
            .when(has_completions, |field| {
                field.key_context("RepoPathSuggestions")
            })
            .on_action(cx.listener(|this, _: &NextRepositoryPath, window, cx| {
                this.move_path_completion(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PreviousRepositoryPath, window, cx| {
                this.move_path_completion(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CompleteRepositoryPath, window, cx| {
                let ui = &this.repository_ui;
                if let Some(entry) = ui.completions.get(ui.completion_index) {
                    this.choose_path_completion(entry.path.clone(), window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .child(
                Input::new(ui.input())
                    .readonly(busy)
                    .aria_label(if ui.remote.active {
                        t!("repository.remote_path")
                    } else {
                        t!("repository.local_or_ssh_path")
                    }),
            )
            .when(has_completions, |field| {
                field.child(
                    deferred(
                        v_flex()
                            .id("repository-path-completions")
                            .absolute()
                            .top_full()
                            .mt_1()
                            .left_0()
                            .w_full()
                            .role(Role::ListBox)
                            .aria_label(t!("repository.path_suggestions"))
                            .occlude()
                            .popover_style(cx)
                            .p_1()
                            .child(
                                v_flex()
                                    .id("repository-path-options")
                                    .max_h(px(186.))
                                    .overflow_y_scroll()
                                    .track_scroll(&ui.completion_scroll)
                                    .children(ui.completions.iter().enumerate().map(
                                        |(index, entry)| {
                                            let path = entry.path.clone();
                                            let glyph = if entry.repository {
                                                IconName::FolderGit2
                                            } else {
                                                IconName::Folder
                                            };
                                            picker_row(
                                                ("path-option", index),
                                                glyph,
                                                directory_name(&path),
                                                None,
                                                None,
                                                index == ui.completion_index,
                                                colors,
                                            )
                                            .role(Role::ListBoxOption)
                                            .tab_stop(false)
                                            .on_click(
                                                cx.listener(move |this, _, window, cx| {
                                                    this.choose_path_completion(
                                                        path.clone(),
                                                        window,
                                                        cx,
                                                    )
                                                }),
                                            )
                                        },
                                    ))
                                    .vertical_popup_scrollbar(&ui.completion_scroll),
                            )
                            .on_mouse_down_out(cx.listener(
                                |this, event: &MouseDownEvent, _, cx| {
                                    if !this
                                        .repository_ui
                                        .input()
                                        .read(cx)
                                        .input_bounds()
                                        .contains(&event.position)
                                    {
                                        this.repository_ui.close_path_completions();
                                        cx.notify();
                                    }
                                },
                            )),
                    )
                    .with_priority(MODAL_POPUP_PRIORITY + 1),
                )
            })
    }

    fn refresh_recents(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<_> = self
            .repository_ui
            .recents
            .iter()
            .map(|repo| repo.root.clone())
            .collect();
        let store = self.store.clone();
        let check = cx.background_spawn(async move {
            paths
                .into_iter()
                .filter(|path| {
                    crate::remote::SshTarget::parse(path)
                        .ok()
                        .flatten()
                        .is_none()
                        && !path.is_dir()
                        && store.resolve_saved_worktree(path).ok().flatten().is_none()
                })
                .collect()
        });
        self.repository_ui.recents_task = cx.spawn(async move |this, cx| {
            let unavailable = check.await;
            _ = this.update(cx, |this, cx| {
                this.repository_ui.unavailable = unavailable;
                this.update_repository_menu_focus(cx);
                cx.notify();
            });
        });
    }

    fn forget_repository(&mut self, key: &Path, cx: &mut Context<Self>) {
        if self.repository_ui.opening.is_some() {
            return;
        }
        match self.store.forget_recent(key) {
            Ok(()) => {
                self.recent_ssh_repositories
                    .retain(|repo| repo.common_dir != key);
                self.repository_ui
                    .recents
                    .retain(|repo| repo.common_dir != key);
                self.repository_ui.error = None;
                self.refresh_recents(cx);
            }
            Err(error) => self.repository_ui.error = Some(format!("{error:#}")),
        }
        cx.notify();
    }

    fn refresh_worktrees(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repository.clone() else {
            return;
        };
        self.repository_ui.worktrees_loading = true;
        self.repository_ui.worktree_error = None;
        let recent = self
            .repository_ui
            .recents
            .iter()
            .find(|item| item.common_dir == repo.common_dir)
            .cloned();
        let query = cx.background_spawn(async move {
            let mut trees = repo.worktrees()?;
            if let Some(recent) = recent {
                recent.sort_worktrees(&mut trees, repo.ssh.is_some());
            }
            Ok::<_, anyhow::Error>(trees)
        });
        self.repository_ui.worktree_task = cx.spawn(async move |this, cx| {
            let result = query.await;
            _ = this.update(cx, |this, cx| {
                let ui = &mut this.repository_ui;
                ui.worktrees_loading = false;
                match result {
                    Ok(trees) => ui.worktrees = trees,
                    Err(error) => {
                        ui.worktree_error = Some(
                            t!("repository.worktrees_failed", error = format!("{error:#}"))
                                .into_owned(),
                        )
                    }
                }
                this.update_repository_menu_focus(cx);
                cx.notify();
            });
        });
        cx.notify();
    }

    fn load_initial_directory(&mut self, cx: &mut Context<Self>) {
        let initial = self
            .repository
            .as_ref()
            .filter(|repo| repo.ssh.is_none())
            .and_then(|repo| repo.root.parent().map(paths::simplified))
            .or_else(|| self.repository_ui.home.clone())
            .or_else(|| env::current_dir().ok());
        if let Some(path) = initial {
            self.load_repository_directory(path, cx);
        }
    }

    fn load_repository_directory(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.repository_ui.browsing = Some(path.clone());
        self.repository_ui.directory_error = None;
        let query = cx.background_spawn(async move { DirectoryListing::read(path) });
        self.repository_ui.directory_task = cx.spawn(async move |this, cx| {
            let result = query.await;
            _ = this.update(cx, |this, cx| {
                let ui = &mut this.repository_ui;
                ui.browsing = None;
                match result {
                    Ok(directory) => {
                        ui.directory = Some(directory);
                        ui.directory_scroll.set_offset(point(px(0.), px(0.)));
                    }
                    Err(error) => ui.directory_error = Some(format!("{error:#}")),
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    fn pick_repository_tab(&mut self, tab: PickerTab, cx: &mut Context<Self>) {
        let ui = &mut self.repository_ui;
        ui.close_path_completions();
        ui.tab = tab;
        if !ui.remote.active
            && tab == PickerTab::Browse
            && ui.directory.is_none()
            && ui.browsing.is_none()
        {
            self.load_initial_directory(cx);
        }
        cx.notify();
    }

    fn quick_recents(&self) -> impl Iterator<Item = &RecentRepository> {
        self.repository_ui.recents.iter().filter(|item| {
            self.repository
                .as_ref()
                .is_some_and(|repo| repo.common_dir != item.common_dir)
                && !self.repository_ui.unavailable.contains(&item.root)
        })
    }

    fn update_repository_menu_focus(&mut self, cx: &mut Context<Self>) {
        let count = self
            .repository_ui
            .worktrees
            .iter()
            .filter(|tree| tree.available)
            .count()
            + self.quick_recents().count()
            + 1;
        self.repository_ui
            .menu_rows
            .resize_with(count, || cx.focus_handle());
    }

    fn cancel_ssh_open(&mut self, cx: &mut Context<Self>) -> bool {
        if !self
            .repository_ui
            .opening
            .as_deref()
            .and_then(|path| path.to_str())
            .is_some_and(|path| path.starts_with("ssh://"))
        {
            return false;
        }
        self.repository_ui.open_task = Task::ready(());
        self.repository_ui.opening = None;
        cx.notify();
        true
    }

    pub(super) fn reconnect_ssh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(repo) = &self.repository {
            self.open_repository(
                Location {
                    root: repo.root.clone(),
                    file: None,
                },
                window,
                cx,
            );
        }
    }

    pub(super) fn repository_trigger(
        &self,
        window: &Window,
        cx: &Context<Self>,
        colors: Palette,
        available_width: Option<Pixels>,
    ) -> AnyElement {
        let ui = &self.repository_ui;
        let max_width = (window.viewport_size().width * 0.31).min(px(330.));
        let max_width = available_width.map_or(max_width, |width| max_width.min(width));
        let Some(repo) = &self.repository else {
            return quiet_button("open-repository-trigger")
                .h(px(28.))
                .px_2()
                .icon(IconName::FolderOpen)
                .label(t!("repository.open"))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, window, cx| {
                    this.choose_repository(&OpenRepository, window, cx)
                }))
                .into_any_element();
        };
        let workspace = cx.weak_entity();
        let name = project_name(&repo.root, &repo.common_dir);
        let branch = ui
            .worktrees
            .iter()
            .find(|tree| tree.root == repo.root)
            .map(|tree| tree.reference.as_str())
            .unwrap_or_else(|| &self.repository_snapshot.as_deref().unwrap_or(repo).branch)
            .to_owned();
        let label = format!("{name} · {branch}");
        Popover::new("repository-menu")
            .dropdown_motion()
            .anchor(Anchor::TopLeft)
            .offset(px(4.))
            .p(px(6.))
            .scrollable(false)
            .overlay_closable(ui.opening.is_none())
            .open(ui.menu_open)
            .track_focus(&ui.menu_focus)
            .on_open_change(cx.listener(|this, &open, window, cx| {
                if open && (this.repository_ui.picker_open || this.confirming_close) {
                    if this.repository_ui.picker_open {
                        this.repository_ui
                            .input()
                            .update(cx, |input, cx| input.focus(window, cx));
                    }
                    cx.notify();
                    return;
                }
                if !open && this.repository_ui.opening.is_some() {
                    cx.notify();
                    return;
                }
                this.repository_ui.menu_open = open;
                if open {
                    this.repository_ui.menu_error = None;
                    this.repository_ui
                        .menu_scroll
                        .set_offset(point(px(0.), px(0.)));
                    this.open_files_list = false;
                    if let Some(file) = this.active_file() {
                        file.editor
                            .update(cx, |editor, cx| editor.dismiss_lsp_overlays(cx));
                    }
                    this.update_repository_menu_focus(cx);
                    this.refresh_worktrees(cx);
                    this.refresh_recents(cx);
                }
                cx.notify();
            }))
            .trigger(
                quiet_button("repository-menu-trigger")
                    .debug_selector(|| "repository-menu-trigger".into())
                    .disabled(ui.picker_open)
                    .input_h(gpui_component::Size::default())
                    .label_size(cx.theme().control_font_size)
                    .px(px(7.))
                    .max_w(max_width)
                    .accessibility_label(t!("repository.switch"))
                    .tooltip(label)
                    .track_focus(&ui.trigger_focus)
                    .child(icon(IconName::FolderGit2, colors.muted, 15.))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_semibold()
                            .text_color(colors.ink)
                            .child(name),
                    )
                    .when(!branch.is_empty(), |button| {
                        button.child(
                            div()
                                .min_w_0()
                                .max_w(max_width * 0.46)
                                .truncate()
                                .rounded_sm()
                                .px_1()
                                .text_size(cx.theme().control_font_size - px(1.))
                                .text_color(colors.muted)
                                .font_family(super::ui_code_font(cx))
                                .child(branch),
                        )
                    })
                    .child(icon(IconName::ChevronDown, colors.muted, 14.)),
            )
            .content(move |_, window, cx| {
                workspace
                    .update(cx, |this, cx| this.repository_menu(window, cx, colors))
                    .map_or_else(|_| div().into_any_element(), IntoElement::into_any_element)
            })
            .into_any_element()
    }

    fn repository_menu(
        &self,
        window: &Window,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement + use<> {
        let ui = &self.repository_ui;
        let repo = self
            .repository
            .as_ref()
            .expect("repository menu has a repository");
        let recents: Vec<_> = self.quick_recents().collect();
        let path = paths::display(&repo.root);
        let remote = repo.ssh.is_some();
        let path_label = ui.repository_path_label(
            &repo.root,
            &repo.common_dir,
            repo.ssh.as_ref().and_then(SshSession::home),
        );
        let opening = ui.opening.is_some();
        let mut handles = ui.menu_rows.iter();
        let menu_scroll = ui.menu_scroll.clone();
        let worktree_feedback = ui
            .worktree_error
            .as_deref()
            .map(|error| feedback(error, true, colors))
            .or_else(|| {
                (ui.worktrees_loading && ui.worktrees.is_empty())
                    .then(|| feedback(t!("repository.loading_worktrees"), false, colors))
            });
        let first_worktree = 1 + usize::from(worktree_feedback.is_some());
        let first_recent = first_worktree + ui.worktrees.len() + 1;
        let scroll_rows: Vec<_> = ui
            .worktrees
            .iter()
            .enumerate()
            .filter(|(_, tree)| tree.available)
            .map(|(index, _)| first_worktree + index)
            .chain(first_recent..first_recent + recents.len())
            .collect();
        let worktrees: Vec<_> = ui
            .worktrees
            .iter()
            .map(|tree| {
                let active = tree.root == repo.root;
                let root = tree.root.clone();
                let tooltip = paths::display(&root);
                let focus = if tree.available { handles.next() } else { None };
                let status = if !tree.available {
                    t!("repository.missing_directory")
                } else if tree.main {
                    t!("repository.main_worktree")
                } else {
                    Cow::Borrowed("")
                };
                picker_row(
                    format!("worktree:{tooltip}"),
                    IconName::GitBranch,
                    tree.reference.clone(),
                    None,
                    None,
                    active,
                    colors,
                )
                .flex_none()
                .disabled(!tree.available || opening)
                .tooltip(tooltip)
                .when_some(focus, |button, focus| button.track_focus(focus))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(colors.muted)
                        .child(status),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_directory_path(root.clone(), window, cx)
                }))
            })
            .collect();
        let recent_rows: Vec<_> = recents
            .iter()
            .map(|item| {
                self.recent_repository_row(item, handles.next(), false, cx, colors)
                    .flex_none()
            })
            .collect();
        let open_focus = handles.next();
        let rows = ui.menu_rows.clone();
        v_flex()
            .id("repository-menu-content")
            .track_focus(&ui.menu_focus)
            .w(px(324.).min(window.viewport_size().width - px(28.)))
            .on_action(cx.listener(Self::choose_repository))
            .on_key_down(move |event, window, cx| {
                if opening || !matches!(event.keystroke.key.as_str(), "up" | "down") {
                    return;
                }
                let down = event.keystroke.key == "down";
                let index = rows.iter().position(|focus| focus.is_focused(window));
                let next = match index {
                    Some(index) if down => (index + 1) % rows.len(),
                    Some(index) => index.checked_sub(1).unwrap_or(rows.len() - 1),
                    None if down => 0,
                    None => rows.len().saturating_sub(1),
                };
                if let Some(focus) = rows.get(next) {
                    if let Some(&row) = scroll_rows.get(next) {
                        menu_scroll.scroll_to_item(row);
                        window.refresh();
                    }
                    focus.focus(window, cx);
                    cx.stop_propagation();
                }
            })
            .child(
                v_flex()
                    .px_2()
                    .pt_1()
                    .pb_2()
                    .gap(px(2.))
                    .child(
                        h_flex()
                            .justify_between()
                            .child(section_title(t!("repository.current"), colors))
                            .when(remote, |row| {
                                row.child(
                                    quiet_button("reconnect-ssh")
                                        .label(t!("repository.reconnect_ssh"))
                                        .disabled(opening)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.reconnect_ssh(window, cx)
                                        })),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .justify_between()
                            .gap_1()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_semibold()
                                    .child(project_name(&repo.root, &repo.common_dir)),
                            )
                            .child(
                                compact_icon_button(
                                    "copy-repository-path",
                                    IconName::Copy,
                                    t!("repository.copy_path"),
                                )
                                .on_click(cx.listener({
                                    let path = path.clone();
                                    move |_, _, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            path.clone(),
                                        ))
                                    }
                                })),
                            ),
                    )
                    .child(
                        path_line(colors, cx)
                            .truncate()
                            .child(path_label)
                            .managed_tooltip(move |_, cx| {
                                cx.new(|_| Tooltip::new(path.clone())).into()
                            }),
                    ),
            )
            .child(
                v_flex()
                    .id("repository-menu-lists")
                    .debug_selector(|| "repository-menu-lists".into())
                    .max_h(
                        (window.viewport_size().height - px(230.))
                            .max(px(100.))
                            .min(px(380.)),
                    )
                    .pb_1()
                    .track_scroll(&ui.menu_scroll)
                    .overflow_y_scroll()
                    .lock_scroll_axis()
                    .vertical_popup_scrollbar(&ui.menu_scroll)
                    .child(
                        menu_section(colors).pb_0().flex_none().child(
                            h_flex()
                                .pl_2()
                                .pr_1()
                                .justify_between()
                                .child(section_title(t!("repository.worktrees"), colors))
                                .child(
                                    compact_icon_button(
                                        "refresh-worktrees",
                                        IconName::RefreshCw,
                                        t!("repository.refresh_worktrees"),
                                    )
                                    .disabled(ui.worktrees_loading)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.refresh_worktrees(cx)),
                                    ),
                                ),
                        ),
                    )
                    .children(worktree_feedback)
                    .children(worktrees)
                    .when(!recent_rows.is_empty(), |lists| {
                        lists
                            .child(
                                menu_section(colors)
                                    .pb_0()
                                    .mt_1()
                                    .px_2()
                                    .flex_none()
                                    .child(section_title(t!("repository.recent"), colors)),
                            )
                            .children(recent_rows)
                    }),
            )
            .child(
                menu_section(colors).child(
                    picker_row(
                        "open-other-repository",
                        IconName::FolderOpen,
                        t!("repository.open_another").into(),
                        None,
                        None,
                        false,
                        colors,
                    )
                    .disabled(opening)
                    .when_some(open_focus, |button, focus| button.track_focus(focus))
                    .child(div().text_size(px(11.)).text_color(colors.muted).child(
                        if cfg!(target_os = "macos") {
                            "⌘O"
                        } else {
                            "Ctrl+O"
                        },
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.choose_repository(&OpenRepository, window, cx)
                    })),
                ),
            )
            .child(
                quiet_button("open-local-file-menu")
                    .icon(IconName::FileText)
                    .label(t!("workspace.open_local_file"))
                    .disabled(opening)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.choose_local_file(window, cx)),
                    ),
            )
            .when_some(ui.menu_error.as_deref(), |panel, error| {
                panel.child(feedback(error, true, colors))
            })
            .when(opening, |panel| {
                panel.child(feedback(t!("repository.opening"), false, colors))
            })
    }

    pub(super) fn repository_start(
        &self,
        window: &Window,
        cx: &Context<Self>,
        colors: Palette,
    ) -> Stateful<Div> {
        let radii = window_corner_radii(window, cx);
        v_flex()
            .id("repository-start")
            .key_context("RepoPicker")
            .flex_1()
            .min_h_0()
            .w_full()
            .p_4()
            .items_center()
            .justify_center()
            .bg(colors.soft)
            .rounded_bl(radii.bottom_left)
            .rounded_br(radii.bottom_right)
            .child(self.repository_picker(window, cx, colors, false))
            .child(
                quiet_button("open-local-file-start")
                    .icon(IconName::FileText)
                    .label(t!("workspace.open_local_file"))
                    .disabled(self.repository_ui.opening.is_some())
                    .on_click(
                        cx.listener(|this, _, window, cx| this.choose_local_file(window, cx)),
                    ),
            )
    }

    pub(super) fn repository_popup(
        &self,
        window: &Window,
        cx: &Context<Self>,
        colors: Palette,
        opacity: f32,
    ) -> impl IntoElement {
        let open = self.repository_ui.picker_open;
        let height = window.viewport_size().height;
        let list_height = repository_picker_list_height(height, true);
        // Anchor the header while the fields and feedback change the panel height.
        let top = ((height - list_height - px(192.)) / 2.).max(px(48.));
        deferred(PopupFrame::new(
            v_flex()
                .id("repository-picker-panel")
                .absolute()
                .inset_0()
                .pt(top)
                .px_4()
                .key_context("RepoPicker")
                .items_center()
                .justify_start()
                .opacity(opacity)
                .occlude()
                .tab_group()
                .child(self.repository_picker(window, cx, colors, true)),
            !open,
        ))
        .with_priority(MODAL_POPUP_PRIORITY)
    }

    fn ssh_host_input(&self, cx: &Context<Self>, colors: Palette) -> impl IntoElement {
        let ui = &self.repository_ui;
        let remote = &ui.remote;
        let connected = remote.connected();
        let label = if connected {
            t!("repository.connected")
        } else if remote.error.is_some() || remote.connection.is_some() {
            t!("common.retry")
        } else {
            t!("repository.connect")
        };
        let candidates = if remote.suggestions {
            self.repository_ui.host_candidates(cx)
        } else {
            Vec::new()
        };
        let count = candidates.len();
        v_flex()
            .gap_1()
            .child(section_title(t!("common.ssh_host"), colors))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        v_flex()
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .id("ssh-host-field")
                            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                                let remote = &mut this.repository_ui.remote;
                                match event.keystroke.key.as_str() {
                                    "down" if count > 0 => {
                                        remote.index = (remote.index + 1) % count
                                    }
                                    "up" if count > 0 => {
                                        remote.index = (remote.index + count - 1) % count
                                    }
                                    "escape" if remote.suggestions => remote.suggestions = false,
                                    _ => return,
                                }
                                cx.stop_propagation();
                                cx.notify();
                            }))
                            .child(
                                Input::new(remote.input())
                                    .aria_label(t!("common.ssh_host"))
                                    .readonly(ui.opening.is_some() || remote.connecting),
                            )
                            .when(count > 0, |field| {
                                field.child(
                                    deferred(
                                        v_flex()
                                            .id("ssh-host-suggestions")
                                            .absolute()
                                            .top_full()
                                            .left_0()
                                            .w_full()
                                            .mt_1()
                                            .role(Role::ListBox)
                                            .aria_label(t!("repository.ssh_host_suggestions"))
                                            .occlude()
                                            .popover_style(cx)
                                            .p_1()
                                            .child(
                                                v_flex()
                                                    .id("ssh-host-options")
                                                    .max_h(px(210.))
                                                    .overflow_y_popup_scrollbar()
                                                    .children(
                                                        candidates.into_iter().enumerate().map(
                                                            |(ix, host)| {
                                                                let label = host.clone();
                                                                picker_row(
                                                        format!("ssh-host:{host}"),
                                                        IconName::Network,
                                                        label,
                                                        None,
                                                        None,
                                                        ix == remote.index,
                                                        colors,
                                                    )
                                                    .role(Role::ListBoxOption)
                                                    .tab_stop(false)
                                                    .on_mouse_down(
                                                        MouseButton::Left,
                                                        cx.listener(move |this, _, window, cx| {
                                                            this.choose_ssh_host(
                                                                host.clone(),
                                                                window,
                                                                cx,
                                                            );
                                                            cx.stop_propagation();
                                                        }),
                                                    )
                                                            },
                                                        ),
                                                    ),
                                            ),
                                    )
                                    .with_priority(MODAL_POPUP_PRIORITY + 1),
                                )
                            }),
                    )
                    .child(if remote.connecting {
                        action_button("cancel-ssh-browse")
                            .secondary()
                            .h(rems(2.))
                            .flex_shrink_0()
                            .label(t!("common.cancel"))
                            .accessibility_label(t!("repository.cancel_connection"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.reset_ssh_picker();
                                cx.notify();
                            }))
                    } else {
                        action_button("select-ssh-host")
                            .secondary()
                            .h(rems(2.))
                            .debug_selector(|| "select-ssh-host".into())
                            .flex_shrink_0()
                            .label(label)
                            .when(connected, |button| button.text_color(cx.theme().success))
                            .disabled(
                                ui.opening.is_some()
                                    || remote.input().read(cx).value().trim().is_empty()
                                    || connected,
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.confirm_ssh_host(window, cx)
                                }),
                            )
                    }),
            )
    }

    fn repository_picker_header(
        &self,
        modal: bool,
        busy: bool,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement + use<> {
        let remote_active = self.repository_ui.remote.active;
        h_flex()
            .relative()
            .justify_center()
            .flex_shrink_0()
            .child(
                h_flex()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .text_size(px(18.))
                    .font_semibold()
                    .child(t!("repository.open")),
            )
            .child(
                topbar_segments(colors)
                    .relative()
                    .top(px(1.))
                    .id("repository-location-modes")
                    .role(Role::Group)
                    .children(
                        [
                            (
                                "repository-location:local",
                                false,
                                IconName::Monitor,
                                t!("repository.local"),
                            ),
                            (
                                "repository-location:ssh",
                                true,
                                IconName::Network,
                                Cow::Borrowed("SSH"),
                            ),
                        ]
                        .map(|(id, remote, glyph, label)| {
                            let selected = remote_active == remote;
                            topbar_segment(id, selected, colors)
                                .w(px(84.))
                                .content_gap(px(5.))
                                .icon(glyph)
                                .icon_size(px(14.))
                                .label(label)
                                .toggled(selected)
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.select_location_mode(remote, window, cx);
                                }))
                        }),
                    ),
            )
            .when(modal, |header| {
                header.child(
                    h_flex().absolute().right_0().top_0().bottom_0().child(
                        icon_button("close-repository-picker", IconName::X, t!("common.close"))
                            .icon_size(px(text_base_size(cx)))
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dismiss_repository_picker(window, cx);
                                cx.notify();
                            })),
                    ),
                )
            })
    }

    fn repository_picker_status(&self, cx: &Context<Self>, colors: Palette) -> Option<Div> {
        let ui = &self.repository_ui;
        let remote_error = ui.remote.error.as_deref().filter(|_| ui.remote.active);
        let status = if let Some(error) = ui.error.as_deref().or(remote_error) {
            Some((SharedString::from(error.to_owned()), true))
        } else if let Some(path) = ui.opening.as_deref() {
            Some((
                t!("repository.opening_path", path = ui.path_label(path))
                    .into_owned()
                    .into(),
                false,
            ))
        } else if ui.remote.connecting {
            Some((t!("repository.connecting").into(), false))
        } else if ui.remote.legacy && ui.remote.connected() {
            Some((t!("repository.browse_requires_helper_update").into(), false))
        } else {
            None
        };
        let (message, error) = status?;
        Some(
            h_flex()
                .flex_shrink_0()
                .child(
                    feedback(message.clone(), error, colors)
                        .flex_1()
                        .line_clamp(1)
                        .tooltip(move |window, cx| Tooltip::new(message.clone()).build(window, cx)),
                )
                .when(
                    ui.opening
                        .as_deref()
                        .and_then(Path::to_str)
                        .is_some_and(|path| path.starts_with("ssh://")),
                    |row| {
                        row.child(
                            quiet_button("cancel-ssh-open")
                                .icon(IconName::X)
                                .label(t!("repository.cancel_connection"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.cancel_ssh_open(cx);
                                })),
                        )
                    },
                ),
        )
    }

    fn repository_picker(
        &self,
        window: &Window,
        cx: &Context<Self>,
        colors: Palette,
        modal: bool,
    ) -> impl IntoElement + use<> {
        let ui = &self.repository_ui;
        let busy = ui.opening.is_some();
        let (action, path) = ui.path_action(cx);
        let label = action.label();
        let list_height = repository_picker_list_height(window.viewport_size().height, modal);
        v_flex()
            .id("repository-picker-surface")
            .debug_selector(|| "repository-picker-surface".into())
            .role(if modal { Role::Dialog } else { Role::Group })
            .aria_label(t!("repository.open"))
            .w(px(564.))
            .max_w_full()
            .p_4()
            .gap_2()
            .rounded(px(12.))
            .map(|panel| {
                if modal {
                    panel.popover_style(cx)
                } else {
                    panel
                        .bg(colors.elevated)
                        .border_1()
                        .border_color(colors.border)
                }
            })
            .child(self.repository_picker_header(modal, busy, cx, colors))
            .child(
                v_flex()
                    .flex_shrink_0()
                    .gap_1()
                    .when(ui.remote.active, |fields| {
                        fields.child(self.ssh_host_input(cx, colors))
                    })
                    .child(
                        v_flex()
                            .gap_1()
                            .child(section_title(
                                if ui.remote.active {
                                    t!("repository.remote_path")
                                } else {
                                    t!("repository.local_path")
                                },
                                colors,
                            ))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(self.repository_path_input(
                                        busy || ui.remote.connecting,
                                        cx,
                                        colors,
                                    ))
                                    .child(
                                        action_button("submit-repository")
                                            .debug_selector(|| "submit-repository".into())
                                            .primary()
                                            .size(rems(2.))
                                            .rounded(window.rem_size())
                                            .p_0()
                                            .flex_shrink_0()
                                            .icon(IconName::ArrowRight)
                                            .accessibility_label(label.clone())
                                            .tooltip(label)
                                            .disabled(path.is_none())
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.submit_repository_path(window, cx);
                                            })),
                                    ),
                            ),
                    ),
            )
            .children(self.repository_picker_status(cx, colors))
            .child(
                h_flex()
                    .id("repository-picker-tabs")
                    .debug_selector(|| "repository-picker-tabs".into())
                    .role(Role::TabList)
                    .gap_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .children(
                        [
                            (PickerTab::Recent, t!("repository.recent_tab")),
                            (PickerTab::Browse, t!("repository.browse")),
                        ]
                        .map(|(tab, label)| {
                            let selected = ui.tab == tab;
                            let id = match tab {
                                PickerTab::Recent => "repository-tab-recent",
                                PickerTab::Browse => "repository-tab-browse",
                            };
                            h_flex()
                                .pb(px(2.))
                                .border_b_1()
                                .border_color(if selected {
                                    colors.accent
                                } else {
                                    transparent_black()
                                })
                                .child(
                                    quiet_button(id)
                                        .debug_selector(move || id.into())
                                        .role(Role::Tab)
                                        .selected(selected)
                                        .label(label)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.pick_repository_tab(tab, cx)
                                        })),
                                )
                        }),
                    ),
            )
            .child(match ui.tab {
                PickerTab::Recent => self
                    .recent_repository_list(list_height, cx, colors)
                    .into_any_element(),
                PickerTab::Browse => self
                    .repository_directory(list_height, cx, colors)
                    .into_any_element(),
            })
    }

    fn recent_repository_list(
        &self,
        height: Pixels,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement + use<> {
        let ui = &self.repository_ui;
        let recents = ui
            .recents
            .iter()
            .filter(|item| {
                SshTarget::parse(&item.root).ok().flatten().is_some() == ui.remote.active
            })
            .collect::<Vec<_>>();
        let list = v_flex()
            .id("recent-repositories")
            .h(height)
            .pr(REPOSITORY_PICKER_SCROLL_GUTTER)
            .overflow_y_popup_scrollbar()
            .when(recents.is_empty(), |list| {
                list.child(feedback(t!("repository.no_recent"), false, colors))
            })
            .children(recents.into_iter().map(|item| {
                let key = item.common_dir.clone();
                let row = self.recent_repository_row(item, None, true, cx, colors);
                let active = self
                    .repository
                    .as_ref()
                    .is_some_and(|repo| repo.root == item.root);
                let row_id = format!("recent-row:{}", item.root.display());
                h_flex()
                    .id(SharedString::from(row_id.clone()))
                    .debug_selector(move || row_id)
                    .gap_1()
                    .pr_1()
                    .rounded_md()
                    .when(active, |row| row.bg(colors.accent.opacity(0.14)))
                    .when(!active, |row| row.hover(|style| style.bg(colors.hover)))
                    .child(row)
                    .child(
                        compact_icon_button(
                            format!("forget-repository:{}", item.root.display()),
                            IconName::X,
                            t!("repository.remove_recent"),
                        )
                        .disabled(ui.opening.is_some())
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.forget_repository(&key, cx);
                                this.repository_ui
                                    .input()
                                    .update(cx, |input, cx| input.focus(window, cx));
                            },
                        )),
                    )
            }));
        // Let the scrollbar use the dialog padding while keeping the rows aligned with the tabs.
        div().mr(-REPOSITORY_PICKER_SCROLL_GUTTER).child(list)
    }

    fn recent_repository_row(
        &self,
        item: &RecentRepository,
        focus: Option<&FocusHandle>,
        show_missing: bool,
        cx: &Context<Self>,
        colors: Palette,
    ) -> Button {
        let ui = &self.repository_ui;
        let active = self
            .repository
            .as_ref()
            .is_some_and(|repo| repo.root == item.root);
        let root = item.root.clone();
        let missing = show_missing && ui.unavailable.contains(&root);
        let ssh = self.online_repository_ssh(&root);
        let online = ssh.is_some();
        let detail = path_line(colors, cx)
            .debug_selector(|| "recent-repository-path".into())
            .truncate()
            .child(ui.repository_path_label(
                &root,
                &item.common_dir,
                ssh.and_then(SshSession::home),
            ));
        picker_row(
            format!("recent-repository:{}", root.display()),
            IconName::FolderGit2,
            item.name(),
            online.then(|| online_dot(cx)),
            Some(detail),
            active,
            colors,
        )
        .debug_selector(|| format!("recent-repository:{}", root.display()))
        .flex_1()
        .when(show_missing, |button| button.bg(transparent_black()))
        .disabled(ui.opening.is_some())
        .tooltip(paths::display(&root))
        .when_some(focus, |button, focus| button.track_focus(focus))
        .when(online, |button| {
            button.accessibility_label(format!("{} · {}", item.name(), t!("repository.connected")))
        })
        .when(missing, |button| {
            button.child(
                div()
                    .text_size(px(11.))
                    .text_color(colors.warning)
                    .child(t!("repository.missing_directory")),
            )
        })
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_recent_repository(root.clone(), window, cx)
        }))
    }

    fn online_repository_ssh(&self, root: &Path) -> Option<&SshSession> {
        self.repository
            .iter()
            .chain(&self.recent_ssh_repositories)
            .find(|repo| repo.root == root)
            .and_then(|repo| repo.ssh.as_ref())
            .filter(|ssh| ssh.failure().is_none())
    }

    fn repository_directory(
        &self,
        height: Pixels,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement + use<> {
        let ui = &self.repository_ui;
        let path = ui
            .directory
            .as_ref()
            .map(|dir| dir.path.as_path())
            .or(ui.browsing.as_deref());
        let remote = ui.remote.active;
        let parent = if remote {
            path.and_then(|path| path.to_str()).and_then(|path| {
                let (parent, child) = path.rsplit_once('/')?;
                (!child.is_empty())
                    .then(|| PathBuf::from(if parent.is_empty() { "/" } else { parent }))
            })
        } else {
            path.and_then(Path::parent).map(Path::to_owned)
        };
        let home = if remote {
            ui.remote
                .connection
                .as_ref()
                .filter(|_| !ui.remote.legacy)
                .map(|_| PathBuf::from("~"))
        } else {
            ui.home.clone()
        };
        let busy = ui.opening.is_some() || ui.browsing.is_some();
        v_flex()
            .h(height)
            .gap_1()
            .child(
                h_flex()
                    .gap_1()
                    .pb_1()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        compact_icon_button(
                            "repository-directory-up",
                            IconName::ArrowUp,
                            t!("repository.parent_directory"),
                        )
                        .track_focus(&ui.directory_focus)
                        .disabled(parent.is_none() || busy)
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                if let Some(parent) = &parent {
                                    this.navigate_directory(parent.clone(), window, cx);
                                }
                            },
                        )),
                    )
                    .child(
                        compact_icon_button(
                            "repository-directory-home",
                            IconName::House,
                            t!("repository.home_directory"),
                        )
                        .disabled(home.is_none() || busy)
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                if let Some(home) = &home {
                                    this.navigate_directory(home.clone(), window, cx);
                                }
                            },
                        )),
                    )
                    .when_some(path, |header, path| {
                        let crumbs = if remote {
                            let value = path.to_string_lossy();
                            let mut current = String::new();
                            let mut crumbs = vec![(String::from("/"), PathBuf::from("/"))];
                            for segment in value.split('/').filter(|segment| !segment.is_empty()) {
                                current.push('/');
                                current.push_str(segment);
                                crumbs.push((segment.to_owned(), PathBuf::from(&current)));
                            }
                            crumbs
                        } else {
                            path.ancestors()
                                .filter(|path| !path.as_os_str().is_empty())
                                .map(|path| (directory_name(path), path.to_owned()))
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect()
                        };
                        header.child(
                            h_flex()
                                .id("repository-breadcrumbs")
                                .flex_1()
                                .min_w_0()
                                .overflow_x_scroll()
                                .children(crumbs.into_iter().enumerate().map(
                                    |(ix, (name, path))| {
                                        h_flex()
                                            .min_w_0()
                                            .when(ix > 0, |crumb| {
                                                crumb.child(
                                                    div().text_color(colors.muted).child("›"),
                                                )
                                            })
                                            .child(
                                                quiet_button(format!("repository-crumb:{ix}"))
                                                    .h(px(25.))
                                                    .label(name)
                                                    .tooltip(paths::display(&path))
                                                    .disabled(busy)
                                                    .on_click(cx.listener(
                                                        move |this, _, window, cx| {
                                                            this.navigate_directory(
                                                                path.clone(),
                                                                window,
                                                                cx,
                                                            );
                                                        },
                                                    )),
                                            )
                                    },
                                )),
                        )
                    })
                    .when_some(
                        ui.directory.as_ref().filter(|dir| dir.repository),
                        |header, dir| {
                            header.child(open_directory_button(
                                "open-current-directory",
                                dir.path.clone(),
                                busy,
                                cx,
                            ))
                        },
                    ),
            )
            .when(
                remote
                    && ui.remote.connection.is_none()
                    && !ui.remote.connecting
                    && ui.remote.error.is_none(),
                |panel| panel.child(feedback(t!("repository.select_ssh_host"), false, colors)),
            )
            .when_some(ui.directory_error.as_deref(), |panel, error| {
                panel.child(feedback(error, true, colors))
            })
            .when(ui.browsing.is_some(), |panel| {
                panel.child(feedback(t!("repository.loading_directory"), false, colors))
            })
            .child(
                v_flex()
                    .id("repository-directory-entries")
                    .flex_1()
                    .min_h_0()
                    .mr(-REPOSITORY_PICKER_SCROLL_GUTTER)
                    .pr(REPOSITORY_PICKER_SCROLL_GUTTER)
                    .overflow_y_scroll()
                    .track_scroll(&ui.directory_scroll)
                    .lock_scroll_axis()
                    .when_some(ui.directory.as_ref(), |list, dir| {
                        list.when(dir.entries.is_empty(), |list| {
                            list.child(feedback(t!("repository.no_subdirectories"), false, colors))
                        })
                        .children(dir.entries.iter().map(|entry| {
                            let path = entry.path.clone();
                            let folder = path.clone();
                            let name = directory_name(&path);
                            let icon = if entry.repository {
                                IconName::FolderGit2
                            } else {
                                IconName::Folder
                            };
                            let label = paths::display(&path);
                            let row_id = format!("browse-directory-row:{label}");
                            h_flex()
                                .id(SharedString::from(row_id.clone()))
                                .debug_selector(move || row_id)
                                .rounded(px(5.))
                                .when(!busy, |row| row.hover(|style| style.bg(colors.hover)))
                                .child(
                                    picker_row(
                                        format!("browse-directory:{label}"),
                                        icon,
                                        name,
                                        None,
                                        None,
                                        false,
                                        colors,
                                    )
                                    .flex_1()
                                    .text()
                                    .disabled(busy)
                                    .tooltip(label.clone())
                                    .when(entry.repository, |button| {
                                        button.child(
                                            div()
                                                .text_size(px(11.))
                                                .text_color(colors.muted)
                                                .child(t!("repository.git_repository")),
                                        )
                                    })
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.repository_ui.directory_focus.focus(window, cx);
                                            this.navigate_directory(folder.clone(), window, cx);
                                        },
                                    )),
                                )
                                .when(entry.repository, |row| {
                                    let action_id = format!("open-listed-directory:{label}");
                                    row.child(
                                        open_directory_button(
                                            SharedString::from(action_id.clone()),
                                            path,
                                            busy,
                                            cx,
                                        )
                                        .debug_selector(move || action_id)
                                        .text()
                                        .h(px(31.))
                                        .w(px(34.)),
                                    )
                                })
                        }))
                    })
                    .vertical_popup_scrollbar(&ui.directory_scroll),
            )
    }
}

fn resolve_typed_path(value: &str, home: Option<&Path>) -> PathBuf {
    #[cfg(windows)]
    let value = paths::unquote_windows_path(value);
    #[cfg(windows)]
    if is_windows_drive(value) {
        return PathBuf::from(format!("{value}\\"));
    }

    let tilde = value
        .strip_prefix('~')
        .filter(|tail| tail.is_empty() || tail.starts_with(std::path::is_separator));
    match (tilde, home) {
        (Some(tail), Some(home)) => home.join(tail.trim_start_matches(std::path::is_separator)),
        _ => PathBuf::from(value),
    }
}

fn completion_query(value: &str, home: Option<&Path>) -> (PathBuf, String) {
    #[cfg(windows)]
    let value = paths::unquote_windows_path(value);
    if value == "~" || is_windows_drive(value) {
        return (resolve_typed_path(value, home), String::new());
    }
    if let Some(index) = value.rfind(std::path::is_separator) {
        let (parent, prefix) = value.split_at(index + 1);
        return (resolve_typed_path(parent, home), prefix.to_owned());
    }
    (
        env::current_dir().unwrap_or_else(|_| home.unwrap_or(Path::new(".")).to_owned()),
        value.to_owned(),
    )
}

fn is_windows_drive(value: &str) -> bool {
    cfg!(windows) && matches!(value.as_bytes(), [letter, b':'] if letter.is_ascii_alphabetic())
}

#[cfg(all(test, windows))]
mod tests {
    use super::{PathBuf, completion_query, resolve_typed_path};

    #[test]
    fn local_repository_picker_resolves_windows_drive_and_copied_path() {
        assert_eq!(
            completion_query("D:", None),
            (PathBuf::from(r"D:\"), String::new())
        );
        assert_eq!(
            completion_query(r#""D:\Work Trees\str""#, None),
            (PathBuf::from(r"D:\Work Trees\"), "str".into())
        );
        assert_eq!(
            resolve_typed_path(r#""D:\Work Trees\stroll""#, None),
            PathBuf::from(r"D:\Work Trees\stroll")
        );
    }
}

fn menu_section(colors: Palette) -> Div {
    v_flex()
        .pt_1()
        .pb_1()
        .border_t_1()
        .border_color(colors.border)
}

fn repository_picker_list_height(viewport_height: Pixels, modal: bool) -> Pixels {
    (viewport_height - px(if modal { 310. } else { 270. })).clamp(px(125.), px(276.))
}

fn section_title(text: impl Into<SharedString>, colors: Palette) -> Div {
    div()
        .text_size(px(12.))
        .text_color(colors.muted)
        .child(text.into())
}

fn feedback(message: impl Into<SharedString>, error: bool, colors: Palette) -> Stateful<Div> {
    div()
        .id("repository-feedback")
        .role(if error { Role::Alert } else { Role::Status })
        .min_w_0()
        .px_2()
        .py_1()
        .line_clamp(3)
        .text_size(px(12.))
        .text_color(if error { colors.warning } else { colors.muted })
        .child(message.into())
}

fn ssh_repository_path_label(
    root: &Path,
    common_dir: &Path,
    home: Option<&Path>,
) -> Option<String> {
    let target = SshTarget::parse(root).ok()??;
    let project = ssh_project_name(&target, common_dir);
    let path = target.path.to_str()?;
    let path = match path.rsplit_once('/') {
        Some((_, name)) if name == project => &path[..path.len() - name.len()],
        _ => path,
    };
    if let Some(home) = home.and_then(Path::to_str) {
        if path == home {
            return Some("~".into());
        }
        let home = home.trim_end_matches('/');
        if let Some(relative) = path.strip_prefix(home).filter(|path| path.starts_with('/')) {
            return Some(format!("~{relative}"));
        }
    }
    Some(path.to_owned())
}

#[cfg(test)]
#[::core::prelude::v1::test]
fn ssh_repository_path_label_omits_only_redundant_components() {
    let root = Path::new("ssh://remote-laptop/home/gwl/Documents/code/silken-hollow");
    let common_dir = Path::new("ssh://remote-laptop/home/gwl/Documents/code/silken-hollow/.git");
    assert_eq!(
        ssh_repository_path_label(root, common_dir, None).as_deref(),
        Some("/home/gwl/Documents/code/")
    );
    let home = Some(Path::new("/home/gwl"));
    assert_eq!(
        ssh_repository_path_label(root, common_dir, home).as_deref(),
        Some("~/Documents/code/")
    );
    let other_home = Some(Path::new("/home/gw"));
    assert_eq!(
        ssh_repository_path_label(root, common_dir, other_home).as_deref(),
        Some("/home/gwl/Documents/code/")
    );
    let main_git_dir = Path::new("ssh://remote-laptop/home/gwl/Documents/code/main-checkout/.git");
    assert_eq!(
        ssh_repository_path_label(root, main_git_dir, home).as_deref(),
        Some("~/Documents/code/silken-hollow")
    );
    let repo = Path::new("ssh://remote-laptop/repo");
    assert_eq!(
        ssh_repository_path_label(repo, &repo.join(".git"), None).as_deref(),
        Some("/")
    );
}

fn path_line(colors: Palette, cx: &App) -> Stateful<Div> {
    div()
        .id("repository-path-label")
        .min_w_0()
        .text_size(px(11.5))
        .text_color(colors.muted)
        .font_family(super::ui_code_font(cx))
}

fn online_dot(cx: &App) -> Stateful<Div> {
    let green = cx.theme().success;
    div()
        .id("ssh-online")
        .debug_selector(|| "ssh-online".into())
        .flex_shrink_0()
        .size(px(6.))
        .rounded_full()
        .bg(green)
        .shadow(vec![BoxShadow {
            color: green.opacity(0.65),
            offset: point(px(0.), px(0.)),
            blur_radius: px(5.),
            spread_radius: px(1.),
            inset: false,
        }])
}

fn picker_row(
    id: impl Into<ElementId>,
    glyph: IconName,
    title: String,
    title_suffix: Option<Stateful<Div>>,
    detail: Option<Stateful<Div>>,
    active: bool,
    colors: Palette,
) -> Button {
    let height = if detail.is_some() { 43. } else { 31. };
    quiet_button(id)
        .w_full()
        .h(px(height))
        .px_2()
        .selected(active)
        .when(active, |button| {
            button
                .bg(colors.accent.opacity(0.14))
                .text_color(colors.accent)
        })
        .accessibility_label(title.clone())
        .child(icon(
            glyph,
            if active { colors.accent } else { colors.muted },
            15.,
        ))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .items_start()
                .child(match title_suffix {
                    Some(suffix) => h_flex()
                        .w_full()
                        .gap(px(4.))
                        .child(
                            div()
                                .id("repository-row-title")
                                .debug_selector(|| "repository-row-title".into())
                                .min_w_0()
                                .truncate()
                                .child(title),
                        )
                        .child(suffix),
                    None => div().w_full().truncate().child(title),
                })
                .when_some(detail, |row, detail| row.child(detail.w_full())),
        )
}

fn open_directory_button(
    id: impl Into<ElementId>,
    path: PathBuf,
    busy: bool,
    cx: &Context<Workspace>,
) -> Button {
    let label = t!("repository.open_path", path = paths::display(&path)).into_owned();
    quiet_button(id)
        .size(px(26.))
        .p_0()
        .icon(IconName::ArrowRight)
        .tooltip(label.clone())
        .disabled(busy)
        .flex_shrink_0()
        .accessibility_label(label)
        .on_click(cx.listener(move |this, _, window, cx| {
            this.open_browsed_directory(path.clone(), window, cx)
        }))
}
