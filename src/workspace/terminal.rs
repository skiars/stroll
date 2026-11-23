use rust_i18n::t;

mod surface;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use gpui::{prelude::FluentBuilder as _, *};
use gpui_base::motion::{Easing, Transition, TransitionState};
use gpui_component::{
    ActiveTheme, Disableable as _, ElementExt as _, InteractiveElementExt as _, Selectable as _,
    StyledExt as _, WindowExt as _,
    button::{ButtonCustomVariant, ButtonVariants as _},
    h_flex,
    scroll::ScrollableMask,
    tooltip::{ManagedTooltipExt as _, Tooltip},
    v_flex,
};
use gpui_kit_assets::IconName;

use crate::{
    remote::{RepoPath, SshTarget},
    repository::{Repository, Store},
    session::{TerminalSession, TerminalTab, WorktreeSession},
    terminal::Process,
};

use super::{
    FileOpen, NextTab, Palette, PreviousTab, SourcePosition, ToggleTerminal, Workspace,
    WorkspaceView, compact_icon_button, icon_button,
};
use surface::{Session, SessionEvent, Status};

const DEFAULT_HEIGHT: f32 = 300.;
const MIN_HEIGHT: f32 = 100.;
const MAX_SESSIONS: usize = 32;

pub(super) fn maximum_height(window: &Window) -> Pixels {
    ((window.viewport_size().height - px(68.)) * 0.65).max(px(MIN_HEIGHT))
}

pub(super) struct Terminals {
    panels: HashMap<Vec<u8>, Panel>,
    height: Pixels,
    resizing: Option<Resize>,
    retired: Vec<Process>,
}

struct Panel {
    root: PathBuf,
    session_path: PathBuf,
    editor_open: bool,
    docs_open: bool,
    maximized: bool,
    focus: FocusHandle,
    resizer_focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    tabs: Vec<Tab>,
    active: usize,
    next_number: u32,
    scroll: ScrollHandle,
}

impl Panel {
    fn new(repo: &Repository, store: &Store, cx: &App) -> Self {
        Self {
            root: repo.root.clone(),
            session_path: store.session_path(repo),
            editor_open: false,
            docs_open: false,
            maximized: false,
            focus: cx.focus_handle(),
            resizer_focus: cx.focus_handle().tab_stop(true),
            previous_focus: None,
            tabs: Vec::new(),
            active: 0,
            next_number: 1,
            scroll: ScrollHandle::new(),
        }
    }

    fn is_open(&self, view: WorkspaceView) -> bool {
        match view {
            WorkspaceView::Editor => self.editor_open,
            WorkspaceView::Docs => self.docs_open,
            WorkspaceView::Git => false,
        }
    }

    fn set_open(&mut self, view: WorkspaceView, open: bool) {
        match view {
            WorkspaceView::Editor => self.editor_open = open,
            WorkspaceView::Docs => self.docs_open = open,
            WorkspaceView::Git => {}
        }
    }

    fn snapshot(&self, restore_scrollback: bool, cx: &App) -> Option<TerminalSession> {
        let active = self.tabs.get(self.active)?.number;
        Some(TerminalSession {
            open: self.editor_open,
            docs_open: Some(self.docs_open),
            active: Some(active),
            tabs: self
                .tabs
                .iter()
                .map(|tab| {
                    let session = tab.session.read(cx);
                    let directory = session.working_directory().map(|directory| {
                        directory
                            .strip_prefix(&self.root)
                            .map(Path::to_owned)
                            .unwrap_or(directory)
                    });
                    TerminalTab {
                        number: tab.number,
                        directory,
                        scrollback: session.scrollback_snapshot(restore_scrollback),
                    }
                })
                .collect(),
        })
    }
}

struct Tab {
    number: u32,
    session: Entity<Session>,
    focus: FocusHandle,
    close_focus: FocusHandle,
    indicator: TransitionState<f32>,
    _subscription: Subscription,
}

struct Resize {
    start: Pixels,
    pointer: Pixels,
}

#[derive(Clone, Copy)]
struct ResizeDrag;

impl Render for ResizeDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl Terminals {
    pub(super) fn new(store: Option<&Store>) -> Self {
        let height = store
            .and_then(|store| store.terminal_height().ok().flatten())
            .filter(|value| value.is_finite());
        Self {
            panels: HashMap::new(),
            height: px(height.unwrap_or(DEFAULT_HEIGHT).max(MIN_HEIGHT)),
            resizing: None,
            retired: Vec::new(),
        }
    }
}

impl Workspace {
    pub fn bind_terminal_keys(cx: &mut App) {
        // GPUI dispatches shortcuts before the terminal's key-down handler.
        cx.bind_keys([
            KeyBinding::new("tab", NoAction, Some("TerminalInput")),
            KeyBinding::new("shift-tab", NoAction, Some("TerminalInput")),
            KeyBinding::new("ctrl-tab", NextTab, Some("TerminalInput")),
            KeyBinding::new("ctrl-shift-tab", PreviousTab, Some("TerminalInput")),
            KeyBinding::new("ctrl-c", NoAction, Some("TerminalInput")),
            KeyBinding::new("ctrl-v", NoAction, Some("TerminalInput")),
            KeyBinding::new("shift-insert", NoAction, Some("TerminalInput")),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-shift-c", NoAction, Some("TerminalInput")),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-shift-v", NoAction, Some("TerminalInput")),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-c", NoAction, Some("TerminalInput")),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-v", NoAction, Some("TerminalInput")),
        ]);
    }

    fn current_terminal(&self) -> Option<&Panel> {
        self.terminals
            .panels
            .get(self.repository.as_ref()?.storage_key())
    }

    fn current_terminal_mut(&mut self) -> Option<&mut Panel> {
        self.terminals
            .panels
            .get_mut(self.repository.as_ref()?.storage_key())
    }

    pub(super) fn terminal_focused(&self, window: &Window, cx: &App) -> bool {
        matches!(self.view, WorkspaceView::Editor | WorkspaceView::Docs)
            && self.current_terminal().is_some_and(|panel| {
                panel.is_open(self.view)
                    && (panel.focus.contains_focused(window, cx)
                        || panel.tabs[panel.active]
                            .session
                            .read(cx)
                            .focus
                            .is_focused(window))
            })
    }

    pub(super) fn terminal_maximized(&self) -> bool {
        self.current_terminal()
            .is_some_and(|panel| panel.is_open(self.view) && panel.maximized)
    }

    pub(super) fn terminal_open(&self) -> bool {
        self.current_terminal()
            .is_some_and(|panel| panel.is_open(self.view))
    }

    pub(super) fn terminal_view_changed(&mut self) {
        if let Some(panel) = self.current_terminal_mut() {
            panel.maximized = false;
            panel.previous_focus = None;
        }
        self.terminals.resizing = None;
    }

    pub(super) fn capture_terminal_session(&mut self, cx: &App) {
        if let Some(panel) = self.current_terminal() {
            let terminal = panel.snapshot(self.config.current().terminal.restore_scrollback, cx);
            if self.session.terminal != terminal {
                self.session.terminal = terminal;
                self.session_revision += 1;
            }
        }
    }

    pub(super) fn flush_inactive_terminal_sessions(&self, cx: &App) {
        let current = self.repository.as_ref().map(|repo| repo.storage_key());
        let restore_scrollback = self.config.current().terminal.restore_scrollback;
        let _write = self.session_write.lock().unwrap();
        for (key, panel) in &self.terminals.panels {
            if current == Some(key.as_slice()) {
                continue;
            }
            let result = (|| -> anyhow::Result<()> {
                let mut session = Store::read_session_file::<WorktreeSession>(&panel.session_path)?
                    .unwrap_or_default();
                let terminal = panel.snapshot(restore_scrollback, cx);
                if session.terminal != terminal {
                    session.terminal = terminal;
                    Store::write_session_file(&panel.session_path, &session)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                eprintln!("terminal session write failed: {error:#}");
            }
        }
    }

    pub(super) fn restore_terminals(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repository.clone() else {
            return;
        };
        let key = repo.storage_key().to_vec();
        if let Some(panel) = self.terminals.panels.get_mut(&key) {
            if panel.root != repo.root {
                for tab in &panel.tabs {
                    tab.session
                        .update(cx, |session, _| session.relocate_root(&repo.root));
                }
                panel.root = repo.root.clone();
            }
            return;
        }

        let Some(saved) = self.session.terminal.clone() else {
            return;
        };
        let mut panel = Panel::new(&repo, &self.store, cx);
        let mut numbers = HashSet::new();
        for mut tab in saved
            .tabs
            .into_iter()
            .filter(|tab| tab.number > 0 && numbers.insert(tab.number))
            .take(MAX_SESSIONS)
        {
            if !self.config.current().terminal.restore_scrollback {
                tab.scrollback = Default::default();
            }
            panel.next_number = panel.next_number.max(tab.number.saturating_add(1));
            panel.tabs.push(self.terminal_tab(&repo, tab, window, cx));
        }
        panel.active = panel
            .tabs
            .iter()
            .position(|tab| Some(tab.number) == saved.active)
            .unwrap_or(0);
        let legacy_docs =
            saved.docs_open.is_none() && self.session.view == Some(WorkspaceView::Docs);
        if !panel.tabs.is_empty() {
            panel.editor_open = saved.open && !legacy_docs;
            panel.docs_open = saved.docs_open.unwrap_or(legacy_docs && saved.open);
        }
        self.terminals.panels.insert(key, panel);
    }

    pub(super) fn restore_terminal_split(&mut self) {
        if let Some(panel) = self.current_terminal_mut() {
            panel.maximized = false;
        }
    }

    pub(super) fn toggle_terminal_shortcut(
        &mut self,
        _: &ToggleTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.repository_overlay_open() || self.confirming_close || window.has_active_dialog(cx) {
            return;
        }
        if self.terminal_focused(window, cx)
            && self
                .current_terminal()
                .is_some_and(|panel| panel.tabs[panel.active].session.read(cx).composing())
        {
            return;
        }
        if self.view == WorkspaceView::Docs {
            self.toggle_terminal_menu(window, cx);
        } else if self.terminal_focused(window, cx) {
            self.hide_terminal(window, cx);
        } else {
            self.show_terminal(window, cx);
        }
    }

    pub(super) fn toggle_terminal_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal_open() {
            self.hide_terminal(window, cx);
        } else {
            self.show_terminal(window, cx);
        }
    }

    fn show_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirming_close
            || self.repository_overlay_open()
            || self.repository_ui.opening.is_some()
        {
            return;
        }
        let Some(repo) = self.repository.clone() else {
            return;
        };
        if self.view == WorkspaceView::Git {
            self.switch_view(WorkspaceView::Editor, window, cx);
        }
        let previous_focus = window
            .focused(cx)
            .filter(|_| !self.terminal_focused(window, cx));
        let panel = self
            .terminals
            .panels
            .entry(repo.storage_key().to_vec())
            .or_insert_with(|| Panel::new(&repo, &self.store, cx));
        if previous_focus.is_some() {
            panel.previous_focus = previous_focus;
        }
        if panel.tabs.is_empty() {
            self.create_terminal(window, cx);
        } else {
            panel.set_open(self.view, true);
            focus_session(&panel.tabs[panel.active].session, window, cx);
            cx.notify();
        }
    }

    fn create_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminals
            .retired
            .retain(|process| !process.is_finished());
        let repo = self.repository.clone().unwrap();
        let panel = self.terminals.panels.get(repo.storage_key()).unwrap();
        if panel.tabs.len() >= MAX_SESSIONS {
            return;
        }
        let number = (panel.next_number..=u32::MAX)
            .chain(1..panel.next_number)
            .find(|&number| panel.tabs.iter().all(|tab| tab.number != number))
            .unwrap();
        let tab = self.terminal_tab(
            &repo,
            TerminalTab {
                number,
                ..Default::default()
            },
            window,
            cx,
        );
        focus_session(&tab.session, window, cx);
        let panel = self.terminals.panels.get_mut(repo.storage_key()).unwrap();
        panel.next_number = number.checked_add(1).unwrap_or(1);
        panel.active = panel.tabs.len();
        panel.tabs.push(tab);
        panel.set_open(self.view, true);
        cx.notify();
    }

    fn terminal_tab(
        &self,
        repo: &Repository,
        tab: TerminalTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Tab {
        let number = tab.number;
        let shell = self.config.current().terminal.executable();
        let shell = if shell.is_empty() {
            crate::terminal::shell()
        } else {
            shell.into()
        };
        let ssh = repo.ssh.clone();
        let session = cx.new(|cx| Session::new(tab, &repo.root, shell, ssh, window, cx));
        let key = repo.storage_key().to_vec();
        let subscription =
            cx.subscribe_in(
                &session,
                window,
                move |this, _, event, window, cx| match event {
                    SessionEvent::Exited => {
                        let current = this
                            .repository
                            .as_ref()
                            .is_some_and(|repo| repo.storage_key() == key);
                        let take_focus = current && this.terminal_focused(window, cx);
                        this.close_terminal(&key, number, take_focus, window, cx);
                    }
                    SessionEvent::StatusChanged => cx.notify(),
                    SessionEvent::OpenFile { path, position } => {
                        let Some(repo) = this.repository.as_ref().filter(|repo| {
                            repo.storage_key() == key
                                && matches!(this.view, WorkspaceView::Editor | WorkspaceView::Docs)
                        }) else {
                            return;
                        };
                        if this.confirming_close
                            || this.repository_ui.opening.is_some()
                            || this.repository_overlay_open()
                        {
                            return;
                        }
                        match terminal_file_key(repo, path) {
                            Ok(path) => {
                                if this.view == WorkspaceView::Docs {
                                    this.switch_view(WorkspaceView::Editor, window, cx);
                                }
                                this.request_file(
                                    path,
                                    FileOpen::Select {
                                        preview: false,
                                        position: Some(SourcePosition::Terminal(*position)),
                                    },
                                    window,
                                    cx,
                                );
                            }
                            Err(error) => this.show_error(error, window, cx),
                        }
                    }
                },
            );
        Tab {
            number,
            session,
            focus: cx.focus_handle().tab_stop(true),
            close_focus: cx.focus_handle().tab_stop(true),
            indicator: TransitionState::default(),
            _subscription: subscription,
        }
    }

    fn close_terminal(
        &mut self,
        key: &[u8],
        number: u32,
        take_focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.terminals.panels.get_mut(key) else {
            return;
        };
        let Some(index) = panel.tabs.iter().position(|tab| tab.number == number) else {
            return;
        };
        let tab = panel.tabs.remove(index);
        if let Some(process) = tab.session.update(cx, |session, _| session.terminate()) {
            self.terminals.retired.push(process);
        }
        if panel.tabs.is_empty() {
            panel.editor_open = false;
            panel.docs_open = false;
            panel.maximized = false;
            panel.active = 0;
            let previous = panel.previous_focus.take();
            if take_focus {
                self.restore_terminal_focus(previous, window, cx);
            }
        } else {
            if index < panel.active || panel.active == panel.tabs.len() {
                panel.active -= 1;
            }
            if take_focus {
                panel.tabs[panel.active]
                    .session
                    .read(cx)
                    .focus
                    .clone()
                    .focus(window, cx);
            }
        }
        self.terminals
            .retired
            .retain(|process| !process.is_finished());
        cx.notify();
    }

    fn close_active_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.current_terminal() {
            let key = self.repository.as_ref().unwrap().storage_key().to_vec();
            let number = panel.tabs[panel.active].number;
            self.close_terminal(&key, number, true, window, cx);
        }
    }

    fn hide_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.view;
        if let Some(panel) = self
            .current_terminal_mut()
            .filter(|panel| panel.is_open(view))
        {
            panel.set_open(view, false);
            panel.maximized = false;
            let previous = panel.previous_focus.take();
            self.restore_terminal_focus(previous, window, cx);
            cx.notify();
        }
    }

    fn restore_terminal_focus(
        &self,
        previous: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view == WorkspaceView::Editor
            && let Some(file) = self.active_file()
        {
            file.editor
                .update(cx, |editor, cx| editor.focus(window, cx));
        } else if let Some(previous) =
            previous.filter(|previous| self.focus.contains(previous, window))
        {
            previous.focus(window, cx);
        } else if self.view == WorkspaceView::Docs
            && let Some(doc) = &self.doc
        {
            doc.update(cx, |doc, cx| doc.focus_content(window, cx));
        } else {
            self.focus.focus(window, cx);
        }
    }

    pub(super) fn shutdown_terminals(&mut self, cx: &mut Context<Self>) {
        // Request all stops first so process trees are cleaned up concurrently.
        for (_, mut panel) in self.terminals.panels.drain() {
            for tab in panel.tabs.drain(..) {
                if let Some(process) = tab.session.update(cx, |session, _| session.terminate()) {
                    self.terminals.retired.push(process);
                }
            }
        }
        self.terminals.retired.clear();
    }

    fn select_terminal(
        &mut self,
        index: usize,
        enter: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.current_terminal_mut()
            && let Some(tab) = panel.tabs.get(index)
        {
            panel.active = index;
            if enter {
                tab.session.read(cx).focus.clone().focus(window, cx);
            } else {
                tab.focus.focus(window, cx);
            }
            cx.notify();
        }
    }

    fn cycle_terminal_tabs(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.current_terminal().filter(|panel| panel.tabs.len() > 1) else {
            return;
        };
        let count = panel.tabs.len();
        let next = if forward {
            (panel.active + 1) % count
        } else {
            (panel.active + count - 1) % count
        };
        self.select_terminal(next, true, window, cx);
    }

    fn set_terminal_height(
        &mut self,
        value: Pixels,
        maximum: Pixels,
        save: bool,
        cx: &mut Context<Self>,
    ) {
        self.terminals.height = px(value.clamp(px(MIN_HEIGHT), maximum).as_f32().round());
        if save
            && let Err(error) = self
                .store
                .save_terminal_height(self.terminals.height.as_f32())
        {
            // A terminal height is only a preference: report it without asking
            // a window for a toast while a drag is in flight.
            eprintln!("terminal height not saved: {error:#}");
        }
        cx.notify();
    }

    fn end_terminal_resize(&mut self, maximum: Pixels, cx: &mut Context<Self>) {
        if self.terminals.resizing.take().is_some() {
            self.set_terminal_height(self.terminals.height, maximum, true, cx);
        }
    }

    pub(super) fn terminal_panel(
        &self,
        maximum: Pixels,
        colors: Palette,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let panel = self
            .current_terminal()
            .filter(|panel| panel.is_open(self.view))?;
        let dark = cx.theme().mode == gpui_component::ThemeMode::Dark;
        let background = rgb(if dark { 0x17191f } else { 0xffffff });
        let active = &panel.tabs[panel.active];
        let height = self.terminals.height.clamp(px(MIN_HEIGHT), maximum);
        let status: Option<SharedString> = match &active.session.read(cx).status {
            Status::Starting => Some(t!("common.starting").into()),
            Status::Running => None,
            Status::Exited(code) => Some(t!("terminal.exited", code = code).into_owned().into()),
            Status::Failed(error) => Some(t!("terminal.failed", error = error).into_owned().into()),
        };
        let resizer = super::pane_resize_handle("terminal-resizer", Axis::Vertical, false)
            .with_hit_padding(px(0.), super::PANE_RESIZE_HIT_WIDTH - px(1.))
            .track_focus(&panel.resizer_focus)
            .role(Role::Splitter)
            .aria_orientation(Orientation::Horizontal)
            .aria_label(t!("terminal.resize"))
            .aria_min_numeric_value(f64::from(MIN_HEIGHT))
            .aria_max_numeric_value(maximum.to_f64())
            .aria_numeric_value(height.to_f64())
            .aria_numeric_value_step(10.)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    if let Some(panel) = this.current_terminal() {
                        panel.resizer_focus.focus(window, cx);
                    }
                    if event.click_count == 2 {
                        this.terminals.resizing = None;
                        this.set_terminal_height(px(DEFAULT_HEIGHT), maximum, true, cx);
                    } else {
                        this.terminals.resizing = Some(Resize {
                            start: height,
                            pointer: event.position.y,
                        });
                    }
                }),
            )
            .on_drag(ResizeDrag, |_, _, _, cx| cx.new(|_| ResizeDrag))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                let step = if event.keystroke.modifiers.shift {
                    px(30.)
                } else {
                    px(10.)
                };
                let value = match event.keystroke.key.as_str() {
                    "up" => height + step,
                    "down" => height - step,
                    "home" => px(MIN_HEIGHT),
                    "end" => maximum,
                    _ => return,
                };
                this.set_terminal_height(value, maximum, true, cx);
                cx.stop_propagation();
            }));

        let toolbar = h_flex()
            .id("terminal-toolbar")
            .debug_selector(|| "terminal-toolbar".into())
            .relative()
            .w_full()
            .h(px(28.))
            .flex_shrink_0()
            .gap(px(4.))
            .pl(px(4.))
            .pr(px(2.))
            .text_size(px(12.))
            .when(!panel.maximized && !dark, |bar| {
                let black = Hsla::black();
                let shadow = linear_gradient(
                    180.,
                    linear_color_stop(black.opacity(0.04), 0.),
                    linear_color_stop(black.opacity(0.), 1.),
                );
                bar.child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, (), window, _| window.paint_quad(fill(bounds, shadow)),
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(3.)),
                )
            })
            .child(
                div()
                    .flex_shrink_0()
                    .mr(px(4.))
                    .font_semibold()
                    .child(t!("common.terminal")),
            )
            .child(self.terminal_tab_strip(panel, colors, cx))
            .child(
                icon_button("terminal-add", IconName::Plus, t!("terminal.new"))
                    .size(px(22.))
                    .disabled(panel.tabs.len() >= MAX_SESSIONS)
                    .on_click(cx.listener(|this, _, window, cx| this.create_terminal(window, cx))),
            )
            .when_some(status, |bar, status| {
                let tooltip = status.clone();
                bar.child(
                    div()
                        .id("terminal-status")
                        .min_w_0()
                        .max_w(relative(0.3))
                        .truncate()
                        .role(Role::Status)
                        .text_color(colors.muted)
                        .child(status)
                        .managed_tooltip(move |window, cx| {
                            Tooltip::new(tooltip.clone()).build(window, cx)
                        }),
                )
            })
            .child(div().flex_1())
            .child(
                icon_button(
                    "terminal-terminate",
                    IconName::Trash,
                    t!("terminal.terminate"),
                )
                .size(px(22.))
                .text_color(colors.muted)
                .custom(
                    ButtonCustomVariant::new(cx)
                        .foreground(cx.theme().danger)
                        .hover(cx.theme().danger.opacity(0.13))
                        .active(cx.theme().danger.opacity(0.20)),
                )
                .on_click(
                    cx.listener(|this, _, window, cx| this.close_active_terminal(window, cx)),
                ),
            )
            .child(
                icon_button(
                    "terminal-maximize",
                    IconName::Maximize,
                    if panel.maximized {
                        t!("terminal.restore")
                    } else {
                        t!("terminal.maximize")
                    },
                )
                .debug_selector(|| "terminal-maximize".into())
                .size(px(22.))
                .selected(panel.maximized)
                .toggled(panel.maximized)
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(panel) = this.current_terminal_mut() {
                        panel.maximized = !panel.maximized;
                        cx.notify();
                    }
                })),
            )
            .child(
                icon_button("terminal-hide", IconName::ChevronDown, t!("terminal.hide"))
                    .size(px(22.))
                    .rounded(px(11.))
                    .on_click(cx.listener(|this, _, window, cx| this.hide_terminal(window, cx))),
            );

        Some(
            v_flex()
                .id("terminal-panel")
                .relative()
                .w_full()
                .min_w_0()
                .track_focus(&panel.focus)
                .key_context("Terminal")
                .on_action(cx.listener(|this, _: &NextTab, window, cx| {
                    this.cycle_terminal_tabs(true, window, cx);
                }))
                .on_action(cx.listener(|this, _: &PreviousTab, window, cx| {
                    this.cycle_terminal_tabs(false, window, cx);
                }))
                .role(Role::Group)
                .aria_label(t!("common.terminal"))
                .bg(background)
                .when(panel.maximized, |el| {
                    let shadow: Hsla = rgb(if dark { 0x000000 } else { 0x181a28 }).into();
                    el.absolute()
                        .inset_0()
                        .rounded(px(12.))
                        .overflow_hidden()
                        .block_mouse_except_scroll()
                        .shadow(vec![BoxShadow {
                            color: shadow.opacity(if dark { 0.42 } else { 0.12 }),
                            offset: point(px(0.), px(0.)),
                            blur_radius: px(9.),
                            spread_radius: px(0.),
                            inset: false,
                        }])
                })
                .when(!panel.maximized, |el| {
                    el.flex_shrink_0()
                        .h(height)
                        .border_t_1()
                        .border_color(colors.border)
                })
                .child(toolbar)
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .pt(px(2.))
                        .font_family(self.editor_font_family(cx))
                        .text_size(px(self.config.current().editor.font_size as f32))
                        .child(active.session.clone()),
                )
                .when(!panel.maximized, |el| el.child(resizer))
                .into_any_element(),
        )
    }

    pub(super) fn terminal_column(
        &self,
        column: Div,
        id: &'static str,
        maximum: Pixels,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        column
            .id(id)
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<ResizeDrag>, _, cx| {
                    if let Some(resize) = &this.terminals.resizing {
                        this.set_terminal_height(
                            resize.start + resize.pointer - event.event.position.y,
                            maximum,
                            false,
                            cx,
                        );
                    }
                }),
            )
            .child(
                canvas(|_, _, _| (), {
                    let view = cx.weak_entity();
                    move |_, (), window, _| {
                        let moved = view.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                            if phase.capture() && !event.dragging() {
                                _ = moved.update(cx, |this, cx| {
                                    this.end_terminal_resize(maximum, cx);
                                });
                            }
                        });
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if phase.capture() && event.button == MouseButton::Left {
                                _ = view.update(cx, |this, cx| {
                                    this.end_terminal_resize(maximum, cx);
                                });
                            }
                        });
                    }
                })
                .absolute()
                .size_0(),
            )
    }

    fn terminal_tab_strip(
        &self,
        panel: &Panel,
        colors: Palette,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let scroll = panel.scroll.clone();
        let active = panel.active;
        let len = panel.tabs.len();
        let indicator_transition = Transition::new(Duration::from_millis(150))
            .easing(Easing::cubic_bezier(0.2, 0., 0., 1.).expect("valid tab easing"));
        let key = self.repository.as_ref().unwrap().storage_key().to_vec();
        let status_error: Hsla = rgb(if cx.theme().mode == gpui_component::ThemeMode::Dark {
            0xe68a8c
        } else {
            0xb43b43
        })
        .into();
        let strip = h_flex()
            .id("terminal-tabs")
            .h_full()
            .min_w_0()
            .gap(px(4.))
            .role(Role::TabList)
            .aria_label(t!("terminal.sessions"))
            .overflow_x_scroll()
            .lock_scroll_axis()
            .track_scroll(&scroll)
            .on_prepaint(move |bounds, window, _| {
                let Some(item) = scroll.bounds_for_item(active) else {
                    return;
                };
                let offset = scroll.offset().x;
                let change = if item.left() < bounds.left() {
                    bounds.left() - item.left()
                } else if item.right() > bounds.right() {
                    bounds.right() - item.right()
                } else {
                    px(0.)
                };
                if change != px(0.) {
                    window.on_next_frame(move |window, _| {
                        scroll.set_offset(point(offset + change, px(0.)));
                        window.refresh();
                    });
                }
            })
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                let index = match event.keystroke.key.as_str() {
                    "left" => (active + len - 1) % len,
                    "right" => (active + 1) % len,
                    "home" => 0,
                    "end" => len - 1,
                    "delete" => {
                        this.close_active_terminal(window, cx);
                        cx.stop_propagation();
                        return;
                    }
                    _ => return,
                };
                this.select_terminal(index, false, window, cx);
                cx.stop_propagation();
            }))
            .children(panel.tabs.iter().enumerate().map(|(index, tab)| {
                let selected = index == active;
                let number = tab.number;
                let group: SharedString = format!("terminal-tab-{number}").into();
                let label = tab.session.read(cx).label.clone();
                let key = key.clone();
                let location = crate::paths::display(&panel.root);
                let error = matches!(
                    tab.session.read(cx).status,
                    Status::Exited(_) | Status::Failed(_)
                );
                let close_label = t!("terminal.close_session", number = number).into_owned();
                let indicator = tab.indicator.clone();
                let transition = indicator_transition.clone();
                h_flex()
                    .id(("terminal-tab", number as usize))
                    .relative()
                    .group(group.clone())
                    .h_full()
                    .max_w(px(144.))
                    .flex_shrink_0()
                    .text_color(if error {
                        status_error
                    } else if selected {
                        colors.ink
                    } else {
                        colors.muted
                    })
                    .hover(|style| style.bg(colors.soft))
                    .child(
                        div()
                            .id("select")
                            .h_full()
                            .flex()
                            .items_center()
                            .min_w_0()
                            .pl(px(8.))
                            .pr(px(4.))
                            .track_focus(&tab.focus)
                            .tab_stop(selected)
                            .role(Role::Tab)
                            .aria_selected(selected)
                            .aria_label(label.clone())
                            .cursor_pointer()
                            .child(div().truncate().child(label))
                            .managed_tooltip(move |window, cx| {
                                Tooltip::new(location.clone()).build(window, cx)
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_terminal(index, true, window, cx)
                            }))
                            .on_key_down(cx.listener(
                                move |this, event: &KeyDownEvent, window, cx| {
                                    if event.keystroke.key == "enter" {
                                        this.select_terminal(index, true, window, cx);
                                        cx.stop_propagation();
                                    }
                                },
                            )),
                    )
                    .child(
                        compact_icon_button("close", IconName::X, close_label)
                            .track_focus(&tab.close_focus)
                            .tab_stop(selected)
                            .size(px(14.))
                            .mr(px(2.))
                            .when(!selected, |el| {
                                el.opacity(0.).group_hover(group, |style| style.opacity(1.))
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_terminal(&key, number, true, window, cx)
                            })),
                    )
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |mut bounds, (), window, cx| {
                                let opacity = indicator
                                    .sample(if selected { 1. } else { 0. }, transition, window, cx)
                                    .value;
                                if opacity > 0. {
                                    let inset = bounds.size.width * (0.15 * (1. - opacity));
                                    bounds.origin.x += inset;
                                    bounds.size.width -= inset * 2.;
                                    let mut bar = fill(bounds, colors.accent.opacity(opacity));
                                    bar.corner_radii = px(1.).into();
                                    window.paint_quad(bar);
                                }
                            },
                        )
                        .absolute()
                        .left(px(6.))
                        .right(px(6.))
                        .bottom(px(1.))
                        .h(px(2.)),
                    )
            }));
        h_flex()
            .relative()
            .h_full()
            .min_w_0()
            .child(strip)
            .child(ScrollableMask::vertical_wheel(&panel.scroll))
    }
}

fn terminal_file_key(repo: &Repository, path: &Path) -> anyhow::Result<PathBuf> {
    if repo.ssh.is_none() {
        return repo.local_file_key(&repo.root.join(path));
    }
    let root = SshTarget::parse(&repo.root)?
        .ok_or_else(|| anyhow::anyhow!(t!("ssh.absolute_repo_path")))?;
    // The terminal runs on the SSH host. Interpret its paths as POSIX paths
    // even when the desktop app is running on Windows.
    let root = root
        .path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!(t!("ssh.path_encoding")))?
        .trim_end_matches('/');
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!(t!("ssh.path_encoding")))?;
    let relative = if path.starts_with('/') {
        path.strip_prefix(root)
            .and_then(|path| path.strip_prefix('/'))
            .ok_or_else(|| anyhow::anyhow!(t!("common.path_outside_repo")))?
    } else {
        path
    };
    Ok(RepoPath::from_local(Path::new(relative))?
        .as_path()
        .to_owned())
}

fn focus_session(session: &Entity<Session>, window: &mut Window, cx: &mut App) {
    let focus = session.read(cx).focus.clone();
    focus.focus(window, cx);
    // A menu can restore focus as its contents leave the dispatch tree. Apply
    // the focus again once the new session is mounted.
    let session = session.downgrade();
    window.on_next_frame(move |window, cx| {
        _ = session.update(cx, |session, cx| session.focus.focus(window, cx));
    });
}
