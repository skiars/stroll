use rust_i18n::t;

use std::{
    cell::RefCell,
    ffi::OsString,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Sender},
    },
    time::{Duration, Instant},
};

use gpui::*;
use gpui_component::{ActiveTheme, ThemeMode, animation::ease_in_cubic, menu::ContextMenuExt as _};

use crate::session::TerminalTab;
use crate::terminal::{
    Command, Emulator, Event, Frame, Link, LinkTarget, MouseAction, Process, PtySize,
    SelectionAction, bounded_scrollback,
};

#[cfg(windows)]
mod directory;
mod links;
mod paint;

use links::link_modifier;
use paint::Appearance;

#[derive(Clone)]
pub(super) enum Status {
    Starting,
    Running,
    Exited(u32),
    Failed(String),
}

pub(super) enum SessionEvent {
    Exited,
    StatusChanged,
    OpenFile {
        path: PathBuf,
        position: Option<lsp_types::Position>,
    },
}

pub(super) struct Session {
    pub(super) focus: FocusHandle,
    pub(super) label: SharedString,
    pub(super) status: Status,
    root: PathBuf,
    shell: OsString,
    last_directory: RefCell<Option<PathBuf>>,
    last_scrollback: RefCell<Arc<str>>,
    #[cfg(windows)]
    directory_report: directory::DirectoryReport,
    ssh: Option<crate::remote::SshSession>,
    process: Option<Process>,
    commands: Sender<Command>,
    pending: Option<(mpsc::Receiver<Command>, async_channel::Sender<Event>)>,
    emulator: Option<Emulator>,
    frame: Frame,
    dirty: bool,
    appearance: Option<Appearance>,
    geometry: Geometry,
    scroll_remainder: f32,
    dragging_selection: bool,
    selection_pointer: Point<Pixels>,
    pressed_link: Option<(Link, Point<Pixels>)>,
    link_hover: Option<Point<Pixels>>,
    context_link: Option<LinkTarget>,
    right_mouse_reported: bool,
    scroll_drag: Option<Pixels>,
    scrollbar_fade: Fade,
    scrollbar_last_activity: Option<Instant>,
    scrollbar_hovered: bool,
    scrollbar_idle_task: Option<Task<()>>,
    marked: String,
    marked_selection: Range<usize>,
    ime_anchor: Option<Bounds<Pixels>>,
    sync_task: Option<Task<()>>,
    blink: Instant,
    blink_task: Option<Task<()>>,
    selection_task: Option<Task<()>>,
    _task: Task<()>,
}

impl EventEmitter<SessionEvent> for Session {}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Geometry {
    bounds: Bounds<Pixels>,
    cell: Size<Pixels>,
    cols: u16,
    rows: u16,
}

impl Geometry {
    fn origin(self, x: usize, y: usize) -> Point<Pixels> {
        self.bounds.origin + point(self.cell.width * x as f32, self.cell.height * y as f32)
    }

    fn cell_at(self, point: Point<Pixels>) -> (u16, u16) {
        let offset = point - self.bounds.origin;
        let x = (offset.x / self.cell.width).floor() as u16;
        let y = (offset.y / self.cell.height).floor() as u16;
        (x.min(self.cols - 1), y.min(self.rows - 1))
    }

    fn relative(self, point: Point<Pixels>) -> (f32, f32) {
        let offset = point - self.bounds.origin;
        // The bridge uses integral logical cell sizes for input and PTY sizing.
        (
            offset.x.as_f32() / self.cell.width.as_f32() * self.cell.width.as_f32().round(),
            offset.y.as_f32() / self.cell.height.as_f32() * self.cell.height.as_f32().round(),
        )
    }

    fn size(self) -> PtySize {
        PtySize {
            cols: self.cols,
            rows: self.rows,
            cell_width: self.cell.width.as_f32().round().max(1.) as u16,
            cell_height: self.cell.height.as_f32().round().max(1.) as u16,
        }
    }
}

/// A fade-only opacity transition for the terminal's hand-painted scrollbar.
#[derive(Clone, Copy, Debug)]
struct Fade {
    from: f32,
    target: f32,
    started_at: Instant,
    duration: Duration,
}

impl Fade {
    fn settled(value: f32, now: Instant) -> Self {
        Self {
            from: value,
            target: value,
            started_at: now,
            duration: Duration::ZERO,
        }
    }

    fn sample(&self, now: Instant) -> (f32, bool) {
        if self.from == self.target || self.duration.is_zero() {
            return (self.target, false);
        }
        let linear = now.saturating_duration_since(self.started_at).as_secs_f32()
            / self.duration.as_secs_f32();
        if linear >= 1.0 {
            (self.target, false)
        } else {
            let factor = if self.target > self.from {
                linear
            } else {
                ease_in_cubic(linear)
            };
            (self.from + (self.target - self.from) * factor, true)
        }
    }

    fn set_target(&mut self, target: f32, duration: Duration, now: Instant) -> (f32, bool) {
        if duration.is_zero() {
            *self = Self::settled(target, now);
            return (target, false);
        }
        if self.target != target {
            let current = self.sample(now).0;
            self.from = current;
            self.target = target;
            self.started_at = now;
            self.duration = duration;
        }
        self.sample(now)
    }
}

impl Session {
    pub(super) fn new(
        tab: TerminalTab,
        root: &Path,
        shell: OsString,
        ssh: Option<crate::remote::SshSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let TerminalTab {
            number,
            directory,
            scrollback,
        } = tab;
        let root = Self::directory_root(root);
        let directory = if ssh.is_some() {
            directory
        } else {
            directory.map(|path| root.join(path))
        };
        let name = Path::new(&shell)
            .file_stem()
            .unwrap_or(&shell)
            .to_string_lossy();
        let label = if ssh.is_some() {
            format!("SSH {number}")
        } else {
            format!("{name} {number}")
        }
        .into();
        let (commands, requests) = mpsc::channel();
        let (output, events) = async_channel::bounded(16);
        let task = cx.spawn(async move |view, cx| {
            while let Ok(event) = events.recv().await {
                let mut batch = vec![event];
                for _ in 0..15 {
                    let Ok(event) = events.try_recv() else { break };
                    batch.push(event);
                }
                if view.update(cx, |this, cx| this.output(batch, cx)).is_err() {
                    return;
                }
                // Yield to layout and input during large bursts of PTY output.
                cx.background_executor()
                    .timer(Duration::from_millis(4))
                    .await;
            }
        });
        let focus = cx.focus_handle().tab_index(0).tab_stop(true);
        cx.on_focus(&focus, window, |this, _, cx| {
            this.blink = Instant::now();
            if let Some(emulator) = &mut this.emulator {
                emulator.focus(true);
            }
            cx.notify();
        })
        .detach();
        cx.on_blur(&focus, window, |this, _, cx| {
            if let Some(emulator) = &mut this.emulator {
                emulator.focus(false);
            }
            this.marked.clear();
            this.marked_selection = 0..0;
            this.end_mouse_selection(cx);
            this.scroll_drag = None;
            cx.notify();
        })
        .detach();
        cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.end_mouse_selection(cx);
                this.set_link_hover(None, cx);
                this.scroll_drag = None;
            }
        })
        .detach();
        Self {
            focus,
            label,
            root,
            shell,
            last_directory: RefCell::new(directory),
            last_scrollback: RefCell::new(
                if bounded_scrollback(&scrollback).len() == scrollback.len() {
                    scrollback
                } else {
                    bounded_scrollback(&scrollback).into()
                },
            ),
            #[cfg(windows)]
            directory_report: directory::DirectoryReport::default(),
            ssh,
            commands,
            pending: Some((requests, output)),
            process: None,
            emulator: None,
            status: Status::Starting,
            frame: Frame::default(),
            dirty: true,
            appearance: None,
            geometry: Geometry::default(),
            scroll_remainder: 0.,
            dragging_selection: false,
            selection_pointer: Point::default(),
            pressed_link: None,
            link_hover: None,
            context_link: None,
            right_mouse_reported: false,
            scroll_drag: None,
            scrollbar_fade: Fade::settled(0.0, Instant::now()),
            scrollbar_last_activity: None,
            scrollbar_hovered: false,
            scrollbar_idle_task: None,
            marked: String::new(),
            marked_selection: 0..0,
            ime_anchor: None,
            sync_task: None,
            blink: Instant::now(),
            blink_task: None,
            selection_task: None,
            _task: task,
        }
    }

    pub(super) fn relocate_root(&mut self, root: &Path) {
        let root = Self::directory_root(root);
        let directory = self.last_directory.get_mut();
        if let Some(relative) = directory
            .as_deref()
            .and_then(|path| path.strip_prefix(&self.root).ok())
        {
            *directory = Some(root.join(relative));
        }
        self.root = root;
    }

    fn directory_root(root: &Path) -> PathBuf {
        crate::remote::SshTarget::parse(root)
            .ok()
            .flatten()
            .map_or_else(|| root.to_owned(), |target| target.path)
    }

    pub(super) fn snapshot_directory(&self) -> Option<PathBuf> {
        if self.ssh.is_some() {
            if let Some(path) = self.process.as_ref().and_then(Process::working_directory) {
                let path = path
                    .strip_prefix(&self.root)
                    .map_or(path.clone(), Path::to_owned);
                self.last_directory.replace(Some(path));
            }
            return self.last_directory.borrow().clone();
        }
        #[cfg(unix)]
        let reported = self.process.as_ref().and_then(Process::working_directory);
        #[cfg(windows)]
        let reported = self.directory_report.path().cloned();
        if let Some(path) = reported.filter(|path| path.is_absolute() && path.to_str().is_some()) {
            self.last_directory.replace(Some(path));
        }
        self.last_directory.borrow().as_ref().map(|path| {
            path.strip_prefix(&self.root)
                .map_or_else(|_| path.clone(), Path::to_owned)
        })
    }

    pub(super) fn scrollback_snapshot(&self, enabled: bool) -> Arc<str> {
        if !enabled {
            self.last_scrollback.replace(Arc::default());
        } else if let Some(history) = self
            .emulator
            .as_ref()
            .and_then(Emulator::scrollback_snapshot)
        {
            self.last_scrollback.replace(history);
        }
        self.last_scrollback.borrow().clone()
    }

    pub(super) fn terminate(&mut self) -> Option<Process> {
        let mut process = self.process.take()?;
        process.terminate();
        Some(process)
    }

    pub(super) fn composing(&self) -> bool {
        !self.marked.is_empty()
    }

    fn set_status(&mut self, status: Status, cx: &mut Context<Self>) {
        self.status = status;
        cx.emit(SessionEvent::StatusChanged);
        cx.notify();
    }

    fn output(&mut self, events: Vec<Event>, cx: &mut Context<Self>) {
        for event in events {
            match event {
                Event::Output(bytes) => {
                    #[cfg(windows)]
                    if self.ssh.is_none() {
                        self.directory_report.advance(&bytes);
                    }
                    if let Some(emulator) = &mut self.emulator {
                        emulator.write(&bytes);
                        self.dirty = true;
                    }
                    if matches!(self.status, Status::Starting) {
                        self.set_status(Status::Running, cx);
                    }
                }
                Event::Exit(0) => cx.emit(SessionEvent::Exited),
                Event::Exit(code) => {
                    if let Some(emulator) = &mut self.emulator {
                        emulator.finish_sync();
                        self.dirty = true;
                    }
                    self.set_status(Status::Exited(code), cx);
                }
                Event::Error(error) => self.set_status(Status::Failed(error), cx),
            }
        }
        self.schedule_sync(cx);
        cx.notify();
    }

    fn schedule_sync(&mut self, cx: &Context<Self>) {
        if self.sync_task.is_some() {
            return;
        }
        let Some(deadline) = self.emulator.as_ref().and_then(Emulator::sync_deadline) else {
            return;
        };
        self.sync_task = Some(cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(deadline.saturating_duration_since(Instant::now()))
                .await;
            _ = view.update(cx, |this, cx| {
                this.sync_task = None;
                if let Some(emulator) = &mut this.emulator
                    && emulator
                        .sync_deadline()
                        .is_some_and(|deadline| deadline <= Instant::now())
                {
                    emulator.finish_sync();
                    this.dirty = true;
                    cx.notify();
                }
                this.schedule_sync(cx);
            });
        }));
    }

    fn grid_font(window: &Window) -> (Font, Pixels) {
        let style = window.text_style();
        let mut font = style.font();
        font.features = FontFeatures::disable_ligatures();
        (font, style.font_size.to_pixels(window.rem_size()))
    }

    fn layout_grid(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let theme = cx.theme();
        let appearance = Appearance::new(theme.mode == ThemeMode::Dark);
        let (font, size) = Self::grid_font(window);
        let text = window.text_system();
        let font_id = text.resolve_font(&font);
        let scale = window.scale_factor();
        let line_height =
            (text.ascent(font_id, size) + text.descent(font_id, size).abs()).max(size);
        let cell = gpui::size(
            text.layout_width(font_id, size, 'W').max(px(1.)),
            px((line_height.as_f32() * scale).ceil() / scale),
        );
        if bounds.size.width < cell.width * 2. || bounds.size.height < cell.height {
            self.pressed_link = None;
            self.geometry = Geometry::default();
            return;
        }
        let geometry = Geometry {
            cols: ((bounds.size.width / cell.width).floor() as u16).clamp(2, 500),
            rows: ((bounds.size.height / cell.height).floor() as u16).clamp(1, 300),
            bounds,
            cell,
        };
        let size = geometry.size();
        if let Some((commands, output)) = self.pending.take() {
            let mut emulator = Emulator::new(size, self.commands.clone());
            emulator.restore_scrollback(
                &self.last_scrollback.borrow(),
                &t!("terminal.scrollback_restored"),
                cfg!(windows) && self.ssh.is_none(),
            );
            self.emulator = Some(emulator);
            let process = match &self.ssh {
                Some(ssh) => Process::start_remote(
                    ssh.clone(),
                    self.last_directory.borrow().clone(),
                    size,
                    commands,
                    output,
                ),
                None => Process::start_local(
                    self.shell.clone(),
                    &self.root,
                    self.last_directory.borrow().as_deref(),
                    size,
                    commands,
                    output,
                ),
            };
            match process {
                Ok(process) => self.process = Some(process),
                Err(error) => self.set_status(Status::Failed(error.to_string()), cx),
            }
        } else if self.geometry.size() != size
            && let Some(emulator) = &mut self.emulator
        {
            emulator.resize(size, cfg!(windows) && self.ssh.is_none());
            _ = self.commands.send(Command::Resize(size));
            self.dirty = true;
        }
        self.geometry = geometry;
        if self.appearance != Some(appearance) {
            if let Some(emulator) = &mut self.emulator {
                emulator.theme(
                    appearance.foreground,
                    appearance.background,
                    &appearance.ansi,
                );
                self.dirty = true;
            }
            self.appearance = Some(appearance);
        }
        if self.dirty {
            if let Some(emulator) = &mut self.emulator {
                emulator.frame(&mut self.frame);
            }
            self.dirty = false;
        }
    }

    fn ime_bounds(&self) -> Option<Bounds<Pixels>> {
        let cursor = self.frame.cursor?;
        let geometry = self.geometry;
        (cursor.x < usize::from(geometry.cols) && cursor.y < usize::from(geometry.rows))
            .then(|| Bounds::new(geometry.origin(cursor.x, cursor.y), geometry.cell))
    }

    fn focused_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let stroke = &event.keystroke;
        let mods = stroke.modifiers;
        let key = stroke.key.as_str();
        if self.emulator.is_none() {
            cx.stop_propagation();
            return;
        }
        let clipboard_shortcut = if cfg!(target_os = "macos") {
            mods.platform && !mods.control && !mods.alt
        } else {
            mods == Modifiers {
                control: true,
                shift: true,
                ..Modifiers::none()
            }
        };
        if clipboard_shortcut {
            match key {
                "c" => {
                    self.copy_selection(cx);
                    cx.stop_propagation();
                    return;
                }
                "v" => {
                    self.paste_clipboard(cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }
        if key == "insert" && mods.shift && !mods.control && !mods.alt && !mods.platform {
            self.paste_clipboard(cx);
            cx.stop_propagation();
            return;
        }
        if self.composing() {
            return;
        }
        // Plain text and IME commits reach EntityInputHandler once, through GPUI.
        if event.prefer_character_input && stroke.key_char.is_some()
            || !mods.control
                && !mods.alt
                && !mods.platform
                && stroke.key_char.is_some()
                && !matches!(key, "enter" | "tab" | "escape" | "backspace" | "delete")
        {
            return;
        }
        self.begin_input(cx);
        if let Some(emulator) = &mut self.emulator {
            emulator.key(
                key,
                modifier_bits(mods),
                stroke.key_char.as_deref().unwrap_or(""),
            );
        }
        cx.stop_propagation();
    }

    fn begin_input(&mut self, cx: &mut Context<Self>) {
        self.end_mouse_selection(cx);
        if let Some(emulator) = &mut self.emulator {
            emulator.scroll_to(usize::MAX);
            emulator.clear_selection();
            self.dirty = true;
        }
        self.blink = Instant::now();
        cx.notify();
    }

    fn selection_text(&self) -> Option<String> {
        self.emulator
            .as_ref()?
            .copy()
            .filter(|text| !text.is_empty())
    }

    fn copy_selection(&self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.selection_text() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    fn can_paste(&self) -> bool {
        self.emulator.is_some() && matches!(self.status, Status::Starting | Status::Running)
    }

    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        if self.can_paste()
            && let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
        {
            self.paste_text(&text, cx);
        }
    }

    fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() || !self.can_paste() {
            return;
        }
        self.begin_input(cx);
        if let Some(emulator) = &mut self.emulator {
            emulator.paste(text);
        }
    }

    fn show_scrollbar(&mut self, cx: &mut Context<Self>) {
        self.scrollbar_last_activity = Some(Instant::now());
        if self.scrollbar_idle_task.is_none() {
            let idle = cx.theme().scrollbar_motion().idle();
            self.scrollbar_idle_task = Some(cx.spawn(async move |view, cx| {
                loop {
                    let remaining = view
                        .update(cx, |this, _| {
                            this.scrollbar_last_activity
                                .map(|last| idle.saturating_sub(last.elapsed()))
                                .unwrap_or(Duration::ZERO)
                        })
                        .unwrap_or(Duration::ZERO);
                    if remaining.is_zero() {
                        break;
                    }
                    cx.background_executor().timer(remaining).await;
                }
                _ = view.update(cx, |this, cx| {
                    this.scrollbar_idle_task = None;
                    cx.notify();
                });
            }));
        }
        cx.notify();
    }

    fn update_scrollbar_opacity(&mut self, window: &mut Window, cx: &mut Context<Self>) -> f32 {
        let motion = cx.theme().scrollbar_motion();
        let now = Instant::now();
        let visible = self.frame.scroll_maximum() != 0
            && (self.scroll_drag.is_some()
                || self.scrollbar_hovered
                || self
                    .scrollbar_last_activity
                    .is_some_and(|last| now.saturating_duration_since(last) < motion.idle()));
        let duration = if cx.reduce_motion() {
            Duration::ZERO
        } else if visible {
            motion.enter()
        } else {
            motion.exit()
        };
        let (opacity, running) =
            self.scrollbar_fade
                .set_target(if visible { 1.0 } else { 0.0 }, duration, now);
        if running {
            window.request_animation_frame();
        }
        opacity
    }

    fn scroll(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.pressed_link = None;
        if let Some(emulator) = &mut self.emulator {
            emulator.scroll(delta);
            self.dirty = true;
            self.show_scrollbar(cx);
        }
    }

    fn wheel(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.geometry.rows == 0 || event.modifiers.control || event.modifiers.platform {
            return;
        }
        let delta = match event.delta {
            ScrollDelta::Lines(delta) => delta.y * 3.,
            ScrollDelta::Pixels(delta) => delta.y / self.geometry.cell.height,
        };
        self.scroll_remainder -= delta;
        let lines = self.scroll_remainder.trunc() as isize;
        self.scroll_remainder -= lines as f32;
        if lines != 0 {
            let tracking = event.source == ScrollSource::Pointer
                && !event.modifiers.shift
                && (0..lines.unsigned_abs().min(30)).fold(false, |_, _| {
                    self.mouse(
                        MouseAction::Press,
                        if lines < 0 { 4 } else { 5 },
                        event.modifiers,
                        false,
                        event.position,
                    )
                });
            if !tracking
                && (event.modifiers.shift
                    || !self
                        .emulator
                        .as_ref()
                        .is_some_and(|emulator| emulator.alternate_scroll(lines)))
            {
                self.scroll(lines, cx);
            }
        }
        cx.stop_propagation();
    }

    fn mouse(
        &mut self,
        action: MouseAction,
        button: u8,
        mods: Modifiers,
        pressed: bool,
        pos: Point<Pixels>,
    ) -> bool {
        let point = self.geometry.relative(pos);
        self.geometry.rows != 0
            && self.emulator.as_mut().is_some_and(|emulator| {
                emulator.mouse(action, button, modifier_bits(mods), pressed, point)
            })
    }

    fn select(
        &mut self,
        action: SelectionAction,
        pos: Point<Pixels>,
        clicks: usize,
        cx: &mut Context<Self>,
    ) {
        if self.geometry.rows != 0
            && let Some(emulator) = &mut self.emulator
        {
            emulator.select(
                action,
                self.geometry.cell_at(pos),
                self.geometry.relative(pos),
                clicks,
            );
            self.dirty = true;
            cx.notify();
        }
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        let reported = !event.modifiers.shift
            && self.mouse(
                MouseAction::Press,
                mouse_button(event.button),
                event.modifiers,
                true,
                event.position,
            );
        if event.button == MouseButton::Right {
            self.right_mouse_reported = reported;
            if !reported {
                self.context_link = self.link_at(event.position).map(|link| link.target.clone());
                self.set_link_hover(None, cx);
                return;
            }
        }
        if reported {
            cx.stop_propagation();
            return;
        }
        if event.button == MouseButton::Left {
            self.pressed_link = (event.click_count == 1 && link_modifier(event.modifiers, cx))
                .then(|| self.link_at(event.position).cloned())
                .flatten()
                .map(|link| (link, event.position));
            self.dragging_selection = true;
            self.selection_pointer = event.position;
            self.select(
                SelectionAction::Press,
                event.position,
                event.click_count,
                cx,
            );
        }
        cx.stop_propagation();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.dragging_selection && event.dragging() {
            if let Some((_, origin)) = &self.pressed_link {
                if (event.position - *origin).magnitude() <= 2. {
                    return;
                }
                self.pressed_link = None;
            }
            self.selection_pointer = event.position;
            self.select(SelectionAction::Drag, event.position, 0, cx);
        } else if !event.modifiers.shift {
            self.mouse(
                MouseAction::Move,
                event.pressed_button.map_or(0, mouse_button),
                event.modifiers,
                event.pressed_button.is_some(),
                event.position,
            );
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.button == MouseButton::Left && self.dragging_selection {
            if let Some((link, origin)) = self.pressed_link.take()
                && (event.position - origin).magnitude() <= 2.
                && link_modifier(event.modifiers, cx)
                && self.link_at(event.position) == Some(&link)
                && !self.reports_mouse()
            {
                Self::open_link(&link.target, cx);
            }
            self.end_mouse_selection(cx);
            return;
        }
        let report = match event.button {
            MouseButton::Right => std::mem::take(&mut self.right_mouse_reported),
            _ => !event.modifiers.shift,
        };
        if report {
            self.mouse(
                MouseAction::Release,
                mouse_button(event.button),
                event.modifiers,
                false,
                event.position,
            );
        }
    }

    fn end_mouse_selection(&mut self, cx: &mut Context<Self>) {
        self.pressed_link = None;
        if self.dragging_selection {
            self.dragging_selection = false;
            self.selection_task = None;
            self.select(SelectionAction::Release, self.selection_pointer, 0, cx);
        }
    }

    fn scrollbar_thumb(&self, track: Bounds<Pixels>) -> Option<Bounds<Pixels>> {
        let max = self.frame.scroll_maximum();
        if max == 0 || track.size.height <= px(0.) {
            return None;
        }
        let height = (track.size.height
            * (self.frame.scroll_visible as f32 / self.frame.scroll_total as f32))
            .clamp(px(16.).min(track.size.height), track.size.height);
        let offset = (track.size.height - height) * (self.frame.scroll_offset as f32 / max as f32);
        Some(Bounds::new(
            point(track.left() + px(3.), track.top() + offset),
            size(px(4.), height),
        ))
    }

    fn scroll_to_pointer(
        &mut self,
        position: Pixels,
        track: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if let Some(thumb) = self.scrollbar_thumb(track) {
            let travel = track.size.height - thumb.size.height;
            let offset =
                position - track.top() - self.scroll_drag.unwrap_or(thumb.size.height / 2.);
            if travel > px(0.) {
                let row = (offset / travel).clamp(0., 1.) * self.frame.scroll_maximum() as f32;
                self.scroll_to(row.round() as usize, cx);
            }
        }
    }

    fn scroll_to(&mut self, row: usize, cx: &mut Context<Self>) {
        self.pressed_link = None;
        if let Some(emulator) = &mut self.emulator {
            emulator.scroll_to(row);
            self.dirty = true;
            self.show_scrollbar(cx);
        }
    }
}

impl Render for Session {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scrollbar_opacity = self.update_scrollbar_opacity(window, cx);
        let surface = cx.entity();
        let weak = cx.weak_entity();
        let painting = weak.clone();
        let scrollbar = weak.clone();
        let menu_session = weak.clone();
        let active_scrollbar = self.frame.scroll_maximum() != 0;
        div()
            .id("terminal-screen")
            .size_full()
            .min_h_0()
            .flex()
            .overflow_hidden()
            .key_context("Terminal")
            .child(
                div()
                    .id("terminal-input")
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .ml(px(4.))
                    .mr(px(2.))
                    .track_focus(&self.focus)
                    .key_context("TerminalInput")
                    .role(Role::Terminal)
                    .aria_label(self.label.clone())
                    .cursor_text()
                    .on_key_down(cx.listener(Self::focused_key))
                    .on_any_mouse_down(cx.listener(Self::mouse_down))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
                    .on_mouse_up(MouseButton::Middle, cx.listener(Self::mouse_up))
                    .on_mouse_up(MouseButton::Right, cx.listener(Self::mouse_up))
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, event, window, cx| {
                            this.pressed_link = None;
                            this.mouse_up(event, window, cx);
                        }),
                    )
                    .on_mouse_up_out(MouseButton::Right, cx.listener(Self::mouse_up))
                    .on_modifiers_changed(cx.listener(|_, _, _, cx| cx.notify()))
                    .on_scroll(cx.listener(Self::wheel))
                    .child(
                        canvas(
                            move |bounds, window, cx| {
                                let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
                                let tooltips = surface.update(cx, |this, cx| {
                                    this.layout_grid(bounds, window, cx);
                                    this.link_tooltips(window, cx)
                                });
                                (surface, hitbox, tooltips)
                            },
                            move |bounds, (surface, hitbox, mut tooltips), window, cx| {
                                surface.update(cx, |this, cx| this.paint_grid(bounds, window, cx));
                                if let Some(tooltips) = &mut tooltips {
                                    tooltips.paint(window, cx);
                                }
                                let exit = painting.clone();
                                window.on_mouse_event(move |_: &MouseExitEvent, phase, _, cx| {
                                    if phase.bubble() {
                                        _ = exit.update(cx, |this, cx| {
                                            this.set_link_hover(None, cx);
                                        });
                                    }
                                });
                                window.on_mouse_event(
                                    move |event: &MouseMoveEvent, phase, window, cx| {
                                        if phase.capture() && !event.dragging() {
                                            _ = painting.update(cx, |this, cx| {
                                                this.end_mouse_selection(cx);
                                            });
                                        } else if phase.bubble() {
                                            _ = painting.update(cx, |this, cx| {
                                                let hovered = hitbox.is_hovered(window);
                                                this.set_link_hover(
                                                    (hovered && !event.dragging())
                                                        .then_some(event.position),
                                                    cx,
                                                );
                                                if this.dragging_selection || hovered {
                                                    this.mouse_move(event, window, cx);
                                                }
                                            });
                                        }
                                    },
                                );
                            },
                        )
                        .size_full(),
                    )
                    .context_menu(move |menu, _, cx| {
                        let Some(session) = menu_session.upgrade() else {
                            return menu;
                        };
                        Session::context_menu(menu, &session, cx)
                    }),
            )
            .child(
                div()
                    .id("terminal-scrollback")
                    .relative()
                    .w(px(10.))
                    .h_full()
                    .flex_shrink_0()
                    .role(Role::ScrollBar)
                    .aria_orientation(Orientation::Vertical)
                    .aria_label(t!("terminal.history"))
                    .aria_min_numeric_value(0.)
                    .aria_max_numeric_value(self.frame.scroll_maximum() as f64)
                    .aria_numeric_value(self.frame.scroll_offset as f64)
                    .tab_index(if active_scrollbar { 0 } else { -1 })
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        let page = this.geometry.rows as isize;
                        let delta = match event.keystroke.key.as_str() {
                            "up" => -1,
                            "down" => 1,
                            "pageup" => -page,
                            "pagedown" => page,
                            "home" => -(this.frame.scroll_total as isize),
                            "end" => this.frame.scroll_total as isize,
                            _ => return,
                        };
                        this.scroll(delta, cx);
                        cx.stop_propagation();
                    }))
                    .on_scroll(cx.listener(Self::wheel))
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, (), window, cx| {
                                let thumb = scrollbar
                                    .read_with(cx, |this, _| this.scrollbar_thumb(bounds))
                                    .ok()
                                    .flatten();
                                if scrollbar_opacity > 0.0
                                    && let Some(thumb) = thumb
                                {
                                    let color = if cx.theme().mode == ThemeMode::Dark {
                                        0x666a76
                                    } else {
                                        0xb1b4bf
                                    };
                                    window.paint_quad(
                                        fill(thumb, rgb(color).opacity(scrollbar_opacity))
                                            .corner_radii(px(2.)),
                                    );
                                }

                                // Hovering the scrollbar keeps it visible, and leaving
                                // starts the shared idle hold.
                                let hover = scrollbar.clone();
                                window.on_mouse_event(
                                    move |event: &MouseMoveEvent, phase, _, cx| {
                                        if !phase.bubble() {
                                            return;
                                        }
                                        let hovered = bounds.contains(&event.position);
                                        _ = hover.update(cx, |this, cx| {
                                            if this.scrollbar_hovered != hovered {
                                                this.scrollbar_hovered = hovered;
                                                this.show_scrollbar(cx);
                                            }
                                        });
                                    },
                                );

                                let click = scrollbar.clone();
                                window.on_mouse_event(
                                    move |event: &MouseDownEvent, phase, _, cx| {
                                        if phase == DispatchPhase::Bubble
                                            && event.button == MouseButton::Left
                                            && scrollbar_opacity > 0.0
                                            && bounds.contains(&event.position)
                                        {
                                            _ = click.update(cx, |this, cx| {
                                                if let Some(thumb) = this.scrollbar_thumb(bounds) {
                                                    this.scroll_drag =
                                                        Some(if thumb.contains(&event.position) {
                                                            event.position.y - thumb.top()
                                                        } else {
                                                            thumb.size.height / 2.
                                                        });
                                                    this.scroll_to_pointer(
                                                        event.position.y,
                                                        bounds,
                                                        cx,
                                                    );
                                                }
                                            });
                                        }
                                    },
                                );
                                let release = scrollbar.clone();
                                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                                    if phase.capture() && event.button == MouseButton::Left {
                                        _ = release.update(cx, |this, cx| {
                                            this.scroll_drag = None;
                                            this.scrollbar_hovered =
                                                bounds.contains(&event.position);
                                            this.show_scrollbar(cx);
                                        });
                                    }
                                });
                                window.on_mouse_event(
                                    move |event: &MouseMoveEvent, phase, _, cx| {
                                        if phase.capture() && !event.dragging() {
                                            _ = scrollbar.update(cx, |this, cx| {
                                                this.scroll_drag = None;
                                                this.scrollbar_hovered =
                                                    bounds.contains(&event.position);
                                                this.show_scrollbar(cx);
                                            });
                                        } else if phase.bubble() && event.dragging() {
                                            _ = scrollbar.update(cx, |this, cx| {
                                                if this.scroll_drag.is_some() {
                                                    this.scroll_to_pointer(
                                                        event.position.y,
                                                        bounds,
                                                        cx,
                                                    );
                                                }
                                            });
                                        }
                                    },
                                );
                            },
                        )
                        .size_full(),
                    ),
            )
    }
}

impl EntityInputHandler for Session {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let utf16: Vec<_> = self.marked.encode_utf16().collect();
        let range = range.start.min(utf16.len())..range.end.min(utf16.len());
        *adjusted = Some(range.clone());
        Some(String::from_utf16_lossy(&utf16[range]))
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.marked_selection.clone(),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.composing()
            .then(|| 0..self.marked.encode_utf16().count())
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked.clear();
        self.marked_selection = 0..0;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.unmark_text(window, cx);
        if !text.is_empty() {
            self.begin_input(cx);
            _ = self.commands.send(Command::Input(text.as_bytes().to_vec()));
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        selection: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked.clear();
        self.marked.push_str(text);
        let end = text.encode_utf16().count();
        self.marked_selection = selection.unwrap_or(end..end);
        cx.notify();
    }

    fn paste(&mut self, item: ClipboardItem, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = item.text() {
            self.paste_text(&text, cx);
        }
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.ime_bounds()
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

fn mouse_button(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 3,
        _ => 0,
    }
}

fn modifier_bits(mods: Modifiers) -> u16 {
    u16::from(mods.shift)
        | (u16::from(mods.control) << 1)
        | (u16::from(mods.alt) << 2)
        | (u16::from(mods.platform) << 3)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        sync::{Arc, mpsc},
        time::{Duration, Instant},
    };

    use gpui::{
        AppContext as _, ClipboardItem, Context, Entity, EntityInputHandler as _, FocusHandle,
        Focusable as _, Hsla, InteractiveElement as _, IntoElement, KeyBinding, Keystroke,
        Modifiers, MouseButton, ParentElement as _, Pixels, Render, Styled, TestAppContext,
        TestDispatcher, VisualTestContext, Window, div, point, px, rgb, size,
    };
    use gpui_component::{ActiveTheme as _, Root};

    use super::{Command, Emulator, Event, Fade, PtySize, Session};
    use crate::{
        config::ConfigFile,
        repository::{Repository, Store},
        session::EditorPreviewMode,
        workspace::{NextTab, PreviousTab, Workspace, WorkspaceView},
    };

    #[test]
    fn scrollbar_fade_reaches_both_targets() {
        let start = Instant::now();
        let mut fade = Fade::settled(0., start);

        let (opacity, running) = fade.set_target(1., Duration::from_millis(300), start);
        assert_eq!(opacity, 0.);
        assert!(running);
        let halfway = fade.sample(start + Duration::from_millis(150)).0;
        assert!((halfway - 0.5).abs() < 0.001, "fade in is linear");
        assert_eq!(fade.sample(start + Duration::from_millis(300)), (1., false));

        let (opacity, running) = fade.set_target(
            0.,
            Duration::from_millis(300),
            start + Duration::from_millis(300),
        );
        assert_eq!(opacity, 1.);
        assert!(running);
        let (opacity, running) = fade.sample(start + Duration::from_millis(600));
        assert_eq!(opacity, 0.);
        assert!(!running);
    }

    struct TerminalWithToolbar {
        toolbar: FocusHandle,
        session: Entity<Session>,
    }

    impl Render for TerminalWithToolbar {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_col()
                .key_context("Terminal")
                .child(
                    div()
                        .id("toolbar")
                        .track_focus(&self.toolbar)
                        .tab_index(0)
                        .child("Toolbar"),
                )
                .child(div().flex_1().min_h_0().child(self.session.clone()))
        }
    }

    #[gpui::test]
    fn terminal_input_keeps_shell_keys_and_toolbar_tab_navigation(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            Workspace::bind_terminal_keys(cx);
        });
        let (commands, input) = mpsc::channel();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let session = cx.new(|cx| {
                let mut session = Session::new(
                    crate::session::TerminalTab {
                        number: 1,
                        ..Default::default()
                    },
                    Path::new("."),
                    "shell".into(),
                    None,
                    window,
                    cx,
                );
                session.pending = None;
                session.commands = commands;
                session.emulator = Some(Emulator::new(
                    PtySize {
                        cols: 80,
                        rows: 24,
                        cell_width: 8,
                        cell_height: 16,
                    },
                    session.commands.clone(),
                ));
                session
            });
            let content = cx.new(|cx| TerminalWithToolbar {
                toolbar: cx.focus_handle().tab_stop(true),
                session,
            });
            Root::new(content, window, cx)
        });
        let terminal_focus = cx.update(|window, cx| {
            let content = Root::read(window, cx)
                .view()
                .clone()
                .downcast::<TerminalWithToolbar>()
                .unwrap();
            let content = content.read(cx);
            let terminal_focus = content.session.read(cx).focus.clone();
            content.toolbar.clone().focus(window, cx);
            terminal_focus
        });

        cx.simulate_keystrokes("tab");
        cx.update(|window, _| assert!(terminal_focus.is_focused(window)));
        for (key, expected) in [
            ("tab", &b"\t"[..]),
            ("shift-tab", &b"\x1b[Z"[..]),
            ("ctrl-c", &b"\x03"[..]),
            ("ctrl-space", &b"\x00"[..]),
            ("escape", &b"\x1b"[..]),
            ("ctrl-[", &b"\x1b"[..]),
        ] {
            cx.simulate_keystrokes(key);
            cx.update(|window, _| assert!(terminal_focus.is_focused(window)));
            let mut writes = input.try_iter().filter_map(|command| match command {
                Command::Input(bytes) => Some(bytes),
                _ => None,
            });
            assert_eq!(writes.next().as_deref(), Some(expected), "{key}");
            assert_eq!(writes.next(), None, "{key}");
        }
    }

    #[gpui::test]
    fn terminal_divider_and_active_indicator_follow_theme_and_motion(cx: &mut TestAppContext) {
        use gpui::{linear_color_stop, linear_gradient};
        use gpui_component::ThemeMode;

        use crate::workspace::{Palette, apply_theme};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()
                .unwrap()
                .success()
        );
        let repo = Arc::new(Repository::open(&root).unwrap());
        let store = Arc::new(Store::new(temp.path().join("state")).unwrap());
        let config = ConfigFile::load(Some(temp.path().join("config.toml")));
        cx.update(gpui_component::init);
        let mut workspace = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| {
                let mut view =
                    Workspace::with_config(store, config, WorkspaceView::Editor, window, cx);
                view.repository = Some(repo);
                view
            });
            view.update(cx, |view, cx| {
                apply_theme(ThemeMode::Light, cx);
                cx.set_reduce_motion(false);
                view.show_terminal(window, cx);
                view.create_terminal(window, cx);
                for tab in &view.current_terminal().unwrap().tabs {
                    tab.session.update(cx, |session, _| session.pending = None);
                }
            });
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1000.), px(760.)));

        let paint_indicators = |visual: &mut VisualTestContext| {
            visual.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
            });
            let toolbar = visual.debug_bounds("terminal-toolbar").unwrap();
            visual.update(|window, cx| {
                let accent = Palette::for_mode(cx.theme().mode).accent;
                let scale = window.scale_factor();
                let mut bars: Vec<_> = window
                    .painted_quads()
                    .iter()
                    .filter_map(|quad| {
                        let color = quad.background.as_solid()?;
                        (color == accent.opacity(color.a)
                            && quad.bounds.size.height.0 == 2. * scale
                            && quad.bounds.bottom().0
                                == ((toolbar.bottom() - px(1.)).as_f32() * scale).round())
                        .then_some((quad.bounds, color.a))
                    })
                    .collect();
                bars.sort_by(|(a, _), (b, _)| a.left().0.total_cmp(&b.left().0));
                bars
            })
        };
        let settled = paint_indicators(visual);
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].1, 1.);
        let select = |visual: &mut VisualTestContext, index| {
            visual.update(|window, cx| {
                workspace.update(cx, |view, cx| {
                    view.select_terminal(index, false, window, cx)
                });
            });
        };
        select(visual, 0);
        paint_indicators(visual);
        visual.executor().advance_clock(Duration::from_millis(75));
        let switching = paint_indicators(visual);
        assert_eq!(switching.len(), 2);
        assert!(switching.iter().all(|(_, alpha)| (0. ..1.).contains(alpha)));
        assert!(switching[1].0.size.width < settled[0].0.size.width);
        visual.executor().advance_clock(Duration::from_millis(75));
        let settled = paint_indicators(visual);
        assert_eq!(settled.len(), 1);
        assert!(switching[0].0.size.width < settled[0].0.size.width);

        let black = Hsla::black();
        let shadow = linear_gradient(
            180.,
            linear_color_stop(black.opacity(0.04), 0.),
            linear_color_stop(black.opacity(0.), 1.),
        );
        let panel_bounds = visual.update(|window, cx| {
            let quads = window.painted_quads();
            let shadow = quads.iter().find(|quad| quad.background == shadow).unwrap();
            let scale = window.scale_factor();
            assert_eq!(shadow.bounds.size.height.0, 3. * scale);
            assert!(shadow.bounds.bottom() < settled[0].0.top());
            let light_border = Palette::for_mode(ThemeMode::Light).border;
            let top_border = quads
                .iter()
                .find(|quad| {
                    quad.border_color == light_border
                        && quad.border_widths.top.0 == scale
                        && quad.bounds.left() == shadow.bounds.left()
                        && quad.bounds.right() == shadow.bounds.right()
                        && quad.bounds.top() + quad.border_widths.top == shadow.bounds.top()
                })
                .expect("the light shadow starts just below the terminal divider");
            apply_theme(ThemeMode::Dark, cx);
            cx.set_reduce_motion(true);
            top_border.bounds
        });
        select(visual, 1);
        assert_eq!(paint_indicators(visual).len(), 1);
        visual.update(|window, cx| {
            let quads = window.painted_quads();
            assert!(quads.iter().all(|quad| quad.background != shadow));
            let divider = Palette::for_mode(ThemeMode::Dark).border;
            let scale = window.scale_factor();
            assert!(quads.iter().any(|quad| {
                quad.border_color == divider
                    && quad.border_widths.top.0 == scale
                    && quad.bounds == panel_bounds
            }));
            workspace.update(cx, |view, cx| {
                view.current_terminal_mut().unwrap().maximized = true;
                cx.notify();
            });
            window.draw(cx).clear(cx);
            assert!(!window.painted_quads().iter().any(|quad| {
                quad.border_color == divider
                    && quad.border_widths.top.0 == scale
                    && quad.bounds.size.width == panel_bounds.size.width
            }));
            apply_theme(ThemeMode::Light, cx);
            window.draw(cx).clear(cx);
            assert!(
                window
                    .painted_quads()
                    .iter()
                    .all(|quad| quad.background != shadow)
            );
        });
    }

    #[gpui::test]
    fn docs_shortcut_opens_workspace_terminal_and_restores_worktree_tabs(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()
                .unwrap()
                .success()
        );
        let doc_path = Path::new("README.md");
        let snippet = "printf terminal-doc-marker";
        fs::write(
            root.join(doc_path),
            format!("# Commands\n\n```sh\n{snippet}\n```\n"),
        )
        .unwrap();
        let workspace_path = Path::new("guide.md");
        fs::write(root.join(workspace_path), "# Workspace guide\n").unwrap();
        fs::create_dir(root.join("examples")).unwrap();
        let store = Arc::new(Store::new(temp.path().join("state")).unwrap());
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "[terminal]\nrestore_scrollback = true\n").unwrap();
        let open_workspace = |cx: &mut TestAppContext, saved: crate::session::WorktreeSession| {
            let repo = Repository::open(&root).unwrap();
            let mut workspace = None;
            let (_, visual) = cx.add_window_view(|window, cx| {
                let config = ConfigFile::load(Some(config_path.clone()));
                let page = saved.view.unwrap_or(WorkspaceView::Docs);
                let file = (page == WorkspaceView::Docs).then(|| doc_path.into());
                let view =
                    cx.new(|cx| Workspace::with_config(store.clone(), config, page, window, cx));
                view.update(cx, |view, cx| {
                    view.apply_repository(repo, saved, file, None, window, cx);
                    // Keep native filesystem events out of the deterministic UI scheduler.
                    view.repository_watch_pending = false;
                    view.repository_watch = gpui::Task::ready(());
                    if let Some(panel) = view.current_terminal() {
                        for tab in &panel.tabs {
                            tab.session.update(cx, |session, _| {
                                let (_, events) = session.pending.as_mut().unwrap();
                                // Exercise lazy layout without waking the deterministic UI scheduler from the PTY thread.
                                *events = async_channel::bounded(1).0;
                            });
                        }
                    }
                });
                workspace = Some(view.clone());
                Root::new(view, window, cx)
            });
            visual.simulate_resize(size(px(1000.), px(760.)));
            workspace.unwrap()
        };
        cx.update(|cx| {
            gpui_component::init(cx);
            Workspace::bind_terminal_keys(cx);
            cx.bind_keys([
                KeyBinding::new(
                    "ctrl-1",
                    crate::workspace::SwitchWorkspaceView(WorkspaceView::Docs),
                    None,
                ),
                KeyBinding::new(
                    "ctrl-2",
                    crate::workspace::SwitchWorkspaceView(WorkspaceView::Editor),
                    None,
                ),
            ]);
        });
        let mut initial = crate::session::WorktreeSession::default();
        initial.tabs.open(workspace_path.into(), false);
        initial
            .editor_previews
            .insert(workspace_path.into(), EditorPreviewMode::Full);
        let workspace = open_workspace(cx, initial);
        let mut visual = VisualTestContext::from_window(*cx.windows().last().unwrap(), cx);
        visual.run_until_parked();
        let repo = Repository::open(&root).unwrap();
        assert!(visual.debug_bounds("terminal-toolbar").is_none());
        let copy = visual
            .debug_bounds("docs-copy-code")
            .expect("document code block is visible");
        visual.simulate_click(copy.center(), Modifiers::none());
        let copied = visual.update(|_, cx| cx.read_from_clipboard().unwrap().text().unwrap());
        assert_eq!(copied.trim_end(), snippet);

        let (session, commands) = visual.update(|window, cx| {
            window.dispatch_keystroke(Keystroke::parse("ctrl-`").unwrap(), cx);
            workspace.update(cx, |view, cx| {
                assert_eq!(view.view, WorkspaceView::Editor);
                assert_eq!(view.tabs.active(), Some(workspace_path));
                assert!(view.terminal_focused(window, cx));
                let session = view.current_terminal().unwrap().tabs[0].session.clone();
                let (command_tx, commands) = mpsc::channel();
                session.update(cx, |session, _| {
                    session.pending = None;
                    let mut terminal = Emulator::new(
                        PtySize {
                            cols: 80,
                            rows: 24,
                            cell_width: 8,
                            cell_height: 16,
                        },
                        command_tx.clone(),
                    );
                    terminal.write(b"\x1b[?2004hprevious output\r\n");
                    session.commands = command_tx;
                    session.emulator = Some(terminal);
                    #[cfg(unix)]
                    session.last_directory.replace(Some(root.join("examples")));
                });
                (session, commands)
            })
        });
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert!(visual.debug_bounds("terminal-toolbar").is_some());
        visual.update(|window, cx| {
            let view = workspace.read(cx);
            assert!(view.terminal_focused(window, cx));
        });
        #[cfg(target_os = "macos")]
        visual.simulate_keystrokes("cmd-v");
        #[cfg(not(target_os = "macos"))]
        visual.simulate_keystrokes("ctrl-shift-v");
        let pasted = commands
            .try_iter()
            .find_map(|command| match command {
                Command::Input(bytes) => Some(bytes),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            String::from_utf8(pasted).unwrap(),
            format!("\x1b[200~{}\x1b[201~", copied.replace("\r\n", "\n")),
        );

        visual.update(|window, cx| {
            workspace.update(cx, |view, cx| {
                assert!(view.terminal_focused(window, cx));
                view.current_terminal_mut().unwrap().maximized = true;
            });
        });
        visual.simulate_keystrokes("ctrl-1");
        visual.update(|window, cx| window.draw(cx).clear(cx));
        assert!(visual.debug_bounds("terminal-toolbar").is_none());
        assert!(visual.debug_bounds("docs-copy-code").is_some());
        assert!(
            !commands
                .try_iter()
                .any(|command| matches!(command, Command::Input(_)))
        );
        visual.update(|window, cx| {
            let view = workspace.read(cx);
            assert_eq!(view.view, WorkspaceView::Docs);
            assert!(!view.terminal_focused(window, cx));
            assert_eq!(view.current_terminal().unwrap().tabs[0].session, session);
        });
        visual.simulate_keystrokes("ctrl-2");
        visual.run_until_parked();
        let reader_focus = visual.update(|window, cx| {
            let view = workspace.read(cx);
            assert_eq!(view.view, WorkspaceView::Editor);
            assert!(!view.terminal_maximized());
            assert!(view.terminal_open());
            assert_eq!(view.current_terminal().unwrap().tabs[0].session, session);
            let reader = view.active_markdown_preview().unwrap().clone();
            reader.update(cx, |reader, cx| reader.focus_content(window, cx));
            window.focused(cx).unwrap()
        });
        visual.update(|window, cx| {
            workspace.update(cx, |view, cx| {
                let mut config = view.config.current().clone();
                config.keyboard.vim_mode = true;
                view.commit_settings(config, window, cx).unwrap();
            });
        });
        visual.simulate_keystrokes("ctrl-j");
        visual.update(|window, cx| assert!(workspace.read(cx).terminal_focused(window, cx)));
        visual.simulate_keystrokes("ctrl-k");
        visual.update(|window, cx| {
            assert!(workspace.read(cx).terminal_open());
            assert!(reader_focus.is_focused(window));
        });
        assert!(
            !commands
                .try_iter()
                .any(|command| matches!(command, Command::Input(_)))
        );
        visual.simulate_keystrokes("ctrl-` ctrl-`");
        visual.update(|window, cx| {
            assert!(!workspace.read(cx).terminal_open());
            assert!(reader_focus.is_focused(window));
        });

        visual.simulate_keystrokes("ctrl-`");
        visual.update(|window, cx| {
            workspace.update(cx, |view, cx| {
                view.create_terminal(window, cx);
                view.close_active_terminal(window, cx);
                view.create_terminal(window, cx);
                assert_eq!(view.current_terminal().unwrap().tabs[1].number, 3);
                view.switch_view(WorkspaceView::Docs, window, cx);
                view.flush_session(cx).unwrap();
            });
        });
        let mut saved = store.session(&repo).unwrap();
        assert_eq!(saved.view, Some(WorkspaceView::Docs));
        let terminal = saved.terminal.as_ref().unwrap();
        assert!(terminal.open);
        assert_eq!(terminal.active, Some(3));
        assert_eq!(
            terminal
                .tabs
                .iter()
                .map(|tab| tab.number)
                .collect::<Vec<_>>(),
            [1, 3]
        );
        assert!(terminal.tabs[0].scrollback.contains("previous output"));
        #[cfg(unix)]
        assert_eq!(
            terminal.tabs[0].directory.as_deref(),
            Some(Path::new("examples"))
        );

        let mut legacy = serde_json::to_value(terminal).unwrap();
        legacy["docs_open"] = true.into();
        saved.terminal = Some(serde_json::from_value(legacy).unwrap());
        let reopened = open_workspace(cx, saved);
        let mut visual = VisualTestContext::from_window(*cx.windows().last().unwrap(), cx);
        visual.run_until_parked();
        assert!(visual.debug_bounds("terminal-toolbar").is_none());
        visual.update(|_, cx| {
            let view = reopened.read(cx);
            assert!(!view.terminal_open());
            let panel = view.current_terminal().unwrap();
            assert_eq!(panel.tabs[panel.active].number, 3);
            assert!(
                panel
                    .tabs
                    .iter()
                    .all(|tab| tab.session.read(cx).pending.is_some())
            );
        });
        visual.simulate_keystrokes("ctrl-`");
        visual.update(|window, cx| {
            let view = reopened.read(cx);
            assert_eq!(view.view, WorkspaceView::Editor);
            assert_eq!(view.tabs.active(), Some(workspace_path));
            assert!(view.terminal_focused(window, cx));
            reopened.update(cx, |view, cx| view.flush_session(cx).unwrap());
        });

        let visible = open_workspace(cx, store.session(&repo).unwrap());
        let mut visual = VisualTestContext::from_window(*cx.windows().last().unwrap(), cx);
        visual.run_until_parked();
        visual.update(|window, cx| {
            let view = visible.read(cx);
            assert_eq!(view.view, WorkspaceView::Editor);
            assert!(view.terminal_open());
            assert!(!view.terminal_focused(window, cx));
            let panel = view.current_terminal().unwrap();
            assert!(panel.tabs[0].session.read(cx).pending.is_some());
            assert!(panel.tabs[1].session.read(cx).pending.is_none());
        });
        assert!(visual.debug_bounds("terminal-toolbar").is_some());
    }

    #[test]
    fn ctrl_tab_cycles_only_the_focused_terminal_tabs() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()?
                .success()
        );
        let first = Path::new("first.txt");
        let second = Path::new("second.txt");
        fs::write(root.join(first), "first\n")?;
        fs::write(root.join(second), "second\n")?;
        let repo = Repository::open(&root)?;
        let store = Arc::new(Store::new(temp.path().join("state"))?);
        let config = ConfigFile::load(Some(temp.path().join("config.toml")));
        let mut app = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        app.update(|cx| {
            gpui_component::init(cx);
            Workspace::bind_terminal_keys(cx);
            cx.bind_keys([
                KeyBinding::new(
                    "ctrl-tab",
                    NextTab,
                    Some("WorkspaceEditor && !Terminal && !Locator"),
                ),
                KeyBinding::new(
                    "ctrl-shift-tab",
                    PreviousTab,
                    Some("WorkspaceEditor && !Terminal && !Locator"),
                ),
            ]);
        });
        let mut workspace = None;
        let (_, visual) = app.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            view.update(cx, |view, cx| {
                view.apply_repository(repo, Default::default(), None, None, window, cx);
            });
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1000.), px(760.)));
        for path in [first, second] {
            visual.update(|window, cx| {
                workspace.update(cx, |view, cx| {
                    view.open_file(path.to_owned(), false, window, cx);
                });
            });
            visual.run_until_parked();
        }
        let add_terminal = |visual: &mut VisualTestContext| {
            visual.update(|window, cx| {
                workspace.update(cx, |view, cx| {
                    if view.current_terminal().is_some() {
                        view.create_terminal(window, cx);
                    } else {
                        view.show_terminal(window, cx);
                    }
                    let panel = view.current_terminal().unwrap();
                    panel.tabs[panel.active].session.update(cx, |session, _| {
                        let (commands, input) = mpsc::channel();
                        session.pending = None;
                        session.emulator = Some(Emulator::new(
                            PtySize {
                                cols: 80,
                                rows: 24,
                                cell_width: 8,
                                cell_height: 16,
                            },
                            commands.clone(),
                        ));
                        session.commands = commands;
                        input
                    })
                })
            })
        };
        let assert_terminal_focus = |visual: &mut VisualTestContext, expected: usize| {
            visual.update(|window, cx| {
                let view = workspace.read(cx);
                let panel = view.current_terminal().unwrap();
                assert_eq!(panel.active, expected);
                assert!(
                    panel.tabs[expected]
                        .session
                        .read(cx)
                        .focus
                        .is_focused(window)
                );
                assert_eq!(view.tabs.active(), Some(second));
            });
        };

        let single = add_terminal(visual);
        visual.simulate_keystrokes("ctrl-tab ctrl-shift-tab");
        assert_terminal_focus(visual, 0);
        assert!(
            !single
                .try_iter()
                .any(|cmd| matches!(cmd, Command::Input(_)))
        );

        let second_terminal = add_terminal(visual);
        let third_terminal = add_terminal(visual);
        assert_terminal_focus(visual, 2);
        for (keys, expected) in [
            ("ctrl-tab", 0),
            ("ctrl-tab", 1),
            ("ctrl-shift-tab", 0),
            ("ctrl-shift-tab", 2),
        ] {
            visual.simulate_keystrokes(keys);
            assert_terminal_focus(visual, expected);
        }
        visual.simulate_keystrokes("tab shift-tab");
        let writes = third_terminal
            .try_iter()
            .filter_map(|cmd| match cmd {
                Command::Input(bytes) => Some(bytes),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(writes, [b"\t".to_vec(), b"\x1b[Z".to_vec()]);
        for input in [single, second_terminal] {
            assert!(!input.try_iter().any(|cmd| matches!(cmd, Command::Input(_))));
        }

        visual.update(|window, cx| {
            workspace
                .read(cx)
                .active_file()
                .unwrap()
                .editor
                .clone()
                .update(cx, |editor, cx| editor.focus(window, cx));
        });
        for (keys, expected) in [("ctrl-tab", first), ("ctrl-shift-tab", second)] {
            visual.simulate_keystrokes(keys);
            visual.update(|_, cx| {
                let view = workspace.read(cx);
                assert_eq!(view.tabs.active(), Some(expected));
                assert_eq!(view.current_terminal().unwrap().active, 2);
            });
        }
        Ok(())
    }

    fn terminal_fixture<'a>(
        cx: &'a mut TestAppContext,
        output: &str,
    ) -> (
        Entity<Session>,
        mpsc::Receiver<Command>,
        &'a mut VisualTestContext,
    ) {
        let (commands, input) = mpsc::channel();
        let mut terminal = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            gpui_component::init(cx);
            Workspace::bind_terminal_keys(cx);
            let session = cx.new(|cx| {
                let mut session = Session::new(
                    crate::session::TerminalTab {
                        number: 1,
                        ..Default::default()
                    },
                    Path::new("."),
                    "shell".into(),
                    None,
                    window,
                    cx,
                );
                session.pending = None;
                session.commands = commands;
                session.emulator = Some(Emulator::new(
                    PtySize {
                        cols: 2,
                        rows: 1,
                        cell_width: 8,
                        cell_height: 16,
                    },
                    session.commands.clone(),
                ));
                session
            });
            terminal = Some(session.clone());
            Root::new(session, window, cx)
        });
        let terminal = terminal.unwrap();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            terminal.update(cx, |this, cx| {
                this.focus.focus(window, cx);
                this.output(vec![Event::Output(output.as_bytes().to_vec())], cx);
            });
            window.draw(cx).clear(cx);
        });
        (terminal, input, cx)
    }

    #[gpui::test]
    fn terminal_ime_follows_a_hidden_tui_cursor(cx: &mut TestAppContext) {
        let (terminal, input, cx) = terminal_fixture(cx, "\x1b[2;5H\x1b[?25l");
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(window.is_window_active());
            terminal.update(cx, |this, cx| {
                assert!(!this.frame.cursor.unwrap().visible);
                let geometry = this.geometry;
                let initial = gpui::Bounds::new(geometry.origin(4, 1), geometry.cell);
                assert_eq!(
                    this.bounds_for_range(0..0, geometry.bounds, window, cx),
                    Some(initial)
                );
                this.replace_and_mark_text_in_range(None, "中文", None, window, cx);
                this.output(
                    vec![Event::Output(
                        b"\x1b[?2026h\x1b[3;7H\x1b[?25l\x1b[?2026l".to_vec(),
                    )],
                    cx,
                );
            });
            window.draw(cx).clear(cx);
            terminal.update(cx, |this, cx| {
                assert!(!this.frame.cursor.unwrap().visible);
                let geometry = this.geometry;
                let moved = gpui::Bounds::new(geometry.origin(6, 2), geometry.cell);
                assert_eq!(this.ime_anchor, Some(moved));
                assert_eq!(
                    this.bounds_for_range(2..2, geometry.bounds, window, cx),
                    Some(moved)
                );
                assert_eq!(this.marked_text_range(window, cx), Some(0..2));
                this.replace_text_in_range(None, "中文", window, cx);
                assert_eq!(this.marked_text_range(window, cx), None);
            });
        });
        let writes: Vec<_> = input
            .try_iter()
            .filter_map(|command| match command {
                Command::Input(bytes) => Some(bytes),
                _ => None,
            })
            .collect();
        assert_eq!(writes, ["中文".as_bytes()]);
    }

    #[gpui::test]
    fn terminal_clipboard_shortcuts_preserve_shell_keys_and_copy_unicode(cx: &mut TestAppContext) {
        let (terminal, input, cx) = terminal_fixture(cx, "中e\u{301}字\r\nnext");
        let geometry = cx.update(|_, cx| terminal.read(cx).geometry);
        let start = geometry.origin(0, 0) + point(px(1.), px(1.));
        let end = geometry.origin(3, 1) + point(geometry.cell.width - px(1.), px(1.));
        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
        let (copy, paste) = if cfg!(target_os = "macos") {
            ("cmd-c", "cmd-v")
        } else {
            ("ctrl-shift-c", "ctrl-shift-v")
        };
        let selected = "中e\u{301}字\nnext";
        cx.simulate_keystrokes(copy);
        cx.update(|_, cx| {
            assert_eq!(
                cx.read_from_clipboard()
                    .and_then(|item| item.text())
                    .as_deref(),
                Some(selected)
            );
            assert_eq!(
                terminal.read(cx).selection_text().as_deref(),
                Some(selected)
            );
        });
        let writes = || {
            input
                .try_iter()
                .filter_map(|command| match command {
                    Command::Input(bytes) => Some(bytes),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert!(writes().is_empty());
        cx.simulate_keystrokes("ctrl-c ctrl-v insert");
        assert_eq!(
            writes(),
            [b"\x03".to_vec(), b"\x16".to_vec(), b"\x1b[2~".to_vec()]
        );
        cx.simulate_keystrokes(copy);
        assert!(
            writes().is_empty(),
            "copy with no selection cannot interrupt the shell"
        );

        cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("中\r\nsecond".into())));
        cx.simulate_keystrokes(paste);
        assert_eq!(writes(), ["中\rsecond".as_bytes().to_vec()]);
        cx.update(|_, cx| {
            terminal.update(cx, |this, cx| {
                this.output(vec![Event::Output(b"\x1b[?2004h".to_vec())], cx)
            });
        });
        cx.simulate_keystrokes("shift-insert");
        assert_eq!(
            writes(),
            ["\x1b[200~中\nsecond\x1b[201~".as_bytes().to_vec()]
        );
        cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string(String::new())));
        cx.simulate_keystrokes("shift-insert");
        assert!(writes().is_empty());
    }

    #[gpui::test]
    fn terminal_link_click_drag_and_context_menu_respect_mouse_reporting(cx: &mut TestAppContext) {
        let app = cx.clone();
        let url = "https://target.test/real";
        let (terminal, input, cx) =
            terminal_fixture(cx, &format!("\x1b]8;;{url}\x1b\\link label\x1b]8;;\x1b\\"));
        let (geometry, primary, focus) = cx.update(|_, cx| {
            let terminal = terminal.read(cx);
            (
                terminal.geometry,
                Modifiers::secondary_key(cx),
                terminal.focus.clone(),
            )
        });
        let first = geometry.origin(0, 0) + point(px(1.), px(1.));
        let last = geometry.origin(9, 0) + point(geometry.cell.width - px(1.), px(1.));
        let shift = Modifiers {
            shift: true,
            ..Modifiers::none()
        };

        cx.simulate_mouse_move(first, None, Modifiers::none());
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                window
                    .painted_quads()
                    .iter()
                    .any(|quad| quad.background == cx.theme().link.into())
            );
        });
        cx.simulate_click(first, Modifiers::none());
        assert!(app.opened_url().is_none());
        cx.simulate_mouse_down(first, MouseButton::Left, primary);
        cx.simulate_mouse_move(last, MouseButton::Left, primary);
        cx.simulate_mouse_up(last, MouseButton::Left, primary);
        assert!(app.opened_url().is_none(), "a modified drag selects text");
        cx.update(|_, cx| {
            assert_eq!(
                terminal.read(cx).selection_text().as_deref(),
                Some("link label")
            )
        });

        cx.simulate_mouse_down(first, MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(first, MouseButton::Right, Modifiers::none());
        cx.update(|window, _| assert!(!focus.is_focused(window)));
        cx.simulate_keystrokes("down enter");
        cx.update(|window, cx| {
            assert!(focus.is_focused(window));
            assert_eq!(
                cx.read_from_clipboard()
                    .and_then(|item| item.text())
                    .as_deref(),
                Some("link label")
            );
        });

        cx.simulate_mouse_down(first, MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(first, MouseButton::Right, Modifiers::none());
        cx.simulate_keystrokes("up enter");
        cx.update(|_, cx| {
            assert_eq!(
                cx.read_from_clipboard()
                    .and_then(|item| item.text())
                    .as_deref(),
                Some(url)
            )
        });

        cx.update(|_, cx| {
            terminal.update(cx, |this, cx| {
                this.output(vec![Event::Output(b"\x1b[?1000h\x1b[?1006h".to_vec())], cx)
            });
        });
        cx.simulate_click(first, primary);
        assert!(app.opened_url().is_none());
        cx.simulate_mouse_down(first, MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(first, MouseButton::Right, Modifiers::none());
        cx.update(|window, _| assert!(focus.is_focused(window)));
        let writes: Vec<_> = input
            .try_iter()
            .filter_map(|command| match command {
                Command::Input(bytes) => Some(bytes),
                _ => None,
            })
            .collect();
        let modifier = if primary.control { 16 } else { 0 };
        assert_eq!(
            writes,
            [
                format!("\x1b[<{modifier};1;1M").into_bytes(),
                format!("\x1b[<{modifier};1;1m").into_bytes(),
                b"\x1b[<2;1;1M".to_vec(),
                b"\x1b[<2;1;1m".to_vec(),
            ]
        );

        cx.simulate_mouse_down(first, MouseButton::Left, shift);
        cx.simulate_mouse_move(last, MouseButton::Left, shift);
        cx.simulate_mouse_up(last, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_down(first, MouseButton::Right, shift);
        cx.simulate_mouse_up(first, MouseButton::Right, Modifiers::none());
        cx.update(|window, _| assert!(!focus.is_focused(window)));
        cx.simulate_keystrokes("down down enter");
        assert!(matches!(input.try_recv(), Ok(Command::Input(bytes)) if bytes == url.as_bytes()));
        assert!(input.try_recv().is_err());
        cx.update(|window, _| assert!(focus.is_focused(window)));

        cx.update(|_, cx| {
            terminal.update(cx, |this, cx| {
                this.output(vec![Event::Output(b"\x1b[?1000l".to_vec())], cx)
            });
        });
        cx.simulate_click(first, primary);
        assert_eq!(app.opened_url().as_deref(), Some(url));
    }

    #[test]
    fn terminal_file_links_open_and_position_the_workspace_editor() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()?
                .success()
        );
        let path = Path::new("src/sample.txt");
        fs::create_dir(root.join("src"))?;
        let source = "first\n😀中target\nlast\n";
        fs::write(root.join(path), source)?;
        let repo = Repository::open(&root)?;
        let store = Arc::new(Store::new(temp.path().join("state"))?);
        let config = ConfigFile::load(Some(temp.path().join("config.toml")));
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
            view.update(cx, |view, cx| {
                view.apply_repository(repo, Default::default(), None, None, window, cx);
            });
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1000.), px(760.)));
        let terminal = visual.update(|window, cx| {
            workspace.update(cx, |view, cx| {
                view.show_terminal(window, cx);
                let panel = view.current_terminal_mut().unwrap();
                panel.maximized = true;
                let terminal = panel.tabs[0].session.clone();
                terminal.update(cx, |session, _| {
                    session.pending = None;
                    session.emulator = Some(Emulator::new(
                        PtySize {
                            cols: 80,
                            rows: 24,
                            cell_width: 8,
                            cell_height: 16,
                        },
                        session.commands.clone(),
                    ));
                });
                terminal
            })
        });
        let (first, primary) = visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            terminal.update(cx, |session, cx| {
                session.output(
                    vec![Event::Output(
                        b"src/sample.txt:2:3\r\n./src/sample.txt:3".to_vec(),
                    )],
                    cx,
                );
            });
            window.draw(cx).clear(cx);
            (
                terminal.read(cx).geometry.origin(0, 0) + point(px(1.), px(1.)),
                Modifiers::secondary_key(cx),
            )
        });
        visual.simulate_click(first, Modifiers::none());
        assert!(visual.update(|_, cx| workspace.read(cx).terminal_maximized()));
        visual.simulate_click(first, primary);
        visual.run_until_parked();
        let second = visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            let view = workspace.read(cx);
            assert!(!view.terminal_maximized());
            assert_eq!(view.tabs.active(), Some(path));
            assert!(!view.tabs.is_preview(path));
            let editor = view.files[path].editor.read(cx);
            assert_eq!(editor.cursor(), source.find("target").unwrap());
            assert!(editor.focus_handle(cx).is_focused(window));
            terminal.read(cx).geometry.origin(0, 1) + point(px(1.), px(1.))
        });

        visual.simulate_mouse_down(second, MouseButton::Right, Modifiers::none());
        visual.simulate_mouse_up(second, MouseButton::Right, Modifiers::none());
        visual.simulate_keystrokes("up up enter");
        visual.run_until_parked();
        visual.update(|window, cx| {
            let editor = workspace.read(cx).files[path].editor.read(cx);
            assert_eq!(editor.cursor(), source.find("last").unwrap());
            assert!(editor.focus_handle(cx).is_focused(window));
        });
        Ok(())
    }

    #[gpui::test]
    fn fractional_terminal_columns_align_cursor_selection_mouse_and_pty(cx: &mut TestAppContext) {
        struct TerminalView {
            session: Entity<Session>,
            font_size: Pixels,
        }

        impl Render for TerminalView {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div()
                    .size_full()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(self.font_size)
                    .child(self.session.clone())
            }
        }

        cx.update(gpui_component::init);
        let mut terminal_view = None;
        let (commands, requests) = mpsc::channel();
        let (_, cx) = cx.add_window_view(|window, cx| {
            let session = cx.new(|cx| {
                let mut session = Session::new(
                    crate::session::TerminalTab {
                        number: 1,
                        ..Default::default()
                    },
                    Path::new("."),
                    "shell".into(),
                    None,
                    window,
                    cx,
                );
                session.pending = None;
                session.commands = commands;
                session.emulator = Some(Emulator::new(
                    PtySize {
                        cols: 80,
                        rows: 24,
                        cell_width: 8,
                        cell_height: 16,
                    },
                    session.commands.clone(),
                ));
                session
            });
            let view = cx.new(|_| TerminalView {
                session,
                font_size: px(13.5),
            });
            terminal_view = Some(view.clone());
            Root::new(view, window, cx)
        });
        let terminal_view = terminal_view.unwrap();
        let terminal = cx.update(|_, cx| terminal_view.read(cx).session.clone());
        while requests.try_recv().is_ok() {}
        cx.simulate_scale_factor_change(2.);
        cx.simulate_resize(size(px(694.), px(140.)));
        let geometry = cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let terminal = terminal.read(cx);
            assert_eq!(window.scale_factor(), 2.);
            assert!((terminal.geometry.cell.width.as_f32() - 8.1).abs() < 0.001);
            assert_eq!(terminal.geometry.cols, 83);
            assert_eq!(terminal.frame.rows[0].len(), 83);
            terminal.geometry
        });
        let resize = requests
            .try_iter()
            .filter_map(|command| match command {
                Command::Resize(size) => Some(size),
                Command::Input(_) => None,
            })
            .last();
        assert_eq!(resize, Some(geometry.size()));

        cx.update(|window, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.output(
                    vec![Event::Output(
                        b"\x1b]12;#17da75\x07\x1b[2;70HABC\x1b[2;71H".to_vec(),
                    )],
                    cx,
                );
            });
            window.draw(cx).clear(cx);
            let color: Hsla = rgb(0x17da75).into();
            let left = geometry.bounds.left() + px(70. * 8.1);
            let right = geometry.bounds.left() + px(71. * 8.1);
            let top = geometry.origin(0, 1).y;
            assert!(window.painted_quads().iter().any(|quad| {
                quad.border_color == color
                    && quad.bounds.left() == window.pixel_snap(left).scale(2.)
                    && quad.bounds.right() == window.pixel_snap(right).scale(2.)
                    && quad.bounds.top() == window.pixel_snap(top).scale(2.)
            }));
        });

        let at_column = |column: f32| {
            point(
                geometry.bounds.left() + px(column * 8.1),
                geometry.origin(0, 1).y + geometry.cell.height / 2.,
            )
        };
        let start = at_column(69.25);
        let end = at_column(71.75);
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(end, MouseButton::Left, Default::default());
        cx.simulate_mouse_up(end, MouseButton::Left, Default::default());
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let terminal = terminal.read(cx);
            assert_eq!(
                terminal.emulator.as_ref().unwrap().copy().as_deref(),
                Some("ABC")
            );
            let row = &terminal.frame.rows[1];
            assert!(row[69..72].iter().all(|(cell, _)| cell.selected));
            assert!(!row[68].0.selected && !row[72].0.selected);
        });

        cx.update(|_, cx| {
            terminal.update(cx, |terminal, cx| {
                terminal.output(vec![Event::Output(b"\x1b[?1000h\x1b[?1006h".to_vec())], cx);
            });
        });
        for (column, press, release) in [
            (69.99, &b"\x1b[<0;70;2M"[..], &b"\x1b[<0;70;2m"[..]),
            (70.01, &b"\x1b[<0;71;2M"[..], &b"\x1b[<0;71;2m"[..]),
        ] {
            let position = at_column(column);
            cx.simulate_mouse_down(position, MouseButton::Left, Default::default());
            cx.simulate_mouse_up(position, MouseButton::Left, Default::default());
            assert!(matches!(requests.try_recv(), Ok(Command::Input(bytes)) if bytes == press));
            assert!(matches!(requests.try_recv(), Ok(Command::Input(bytes)) if bytes == release));
        }

        let resized = cx.update(|window, cx| {
            terminal_view.update(cx, |view, cx| {
                view.font_size = px(20.);
                cx.notify();
            });
            window.draw(cx).clear(cx);
            let resized = terminal.read(cx).geometry;
            assert!((resized.cell.width.as_f32() - 12.).abs() < 0.001);
            assert!(resized.cols < geometry.cols && resized.rows < geometry.rows);
            resized
        });
        assert!(matches!(requests.try_recv(), Ok(Command::Resize(size)) if size == resized.size()));
    }

    #[gpui::test]
    fn terminal_selection_autoscroll_stops_after_a_missing_mouse_up(cx: &mut TestAppContext) {
        let mut terminal = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            gpui_component::init(cx);
            let session = cx.new(|cx| {
                let mut session = Session::new(
                    crate::session::TerminalTab {
                        number: 1,
                        ..Default::default()
                    },
                    Path::new("."),
                    "shell".into(),
                    None,
                    window,
                    cx,
                );
                session.pending = None;
                let mut emulator = Emulator::new(
                    PtySize {
                        cols: 80,
                        rows: 24,
                        cell_width: 8,
                        cell_height: 16,
                    },
                    session.commands.clone(),
                );
                emulator.write("some terminal output\r\n".repeat(200).as_bytes());
                session.emulator = Some(emulator);
                session
            });
            terminal = Some(session.clone());
            Root::new(session, window, cx)
        });
        let terminal = terminal.unwrap();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let bounds = cx.update(|_, cx| terminal.read(cx).geometry.bounds);
        let start = point(bounds.left() + px(8.), bounds.top() + px(24.));
        let edge = point(start.x, bounds.top() - px(16.));
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(edge, MouseButton::Left, Default::default());
        let selected = cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let terminal = terminal.read(cx);
            let emulator = terminal.emulator.as_ref().unwrap();
            assert!(terminal.selection_task.is_some() && emulator.selection_autoscroll());
            emulator.copy().filter(|text| !text.is_empty()).unwrap()
        });

        cx.simulate_mouse_move(start, None, Default::default());
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(100));
        cx.simulate_mouse_move(edge, MouseButton::Left, Default::default());
        cx.update(|_, cx| {
            let terminal = terminal.read(cx);
            let emulator = terminal.emulator.as_ref().unwrap();
            assert!(!terminal.dragging_selection && terminal.selection_task.is_none());
            assert!(!emulator.selection_autoscroll());
            assert_eq!(emulator.copy().as_deref(), Some(selected.as_str()));
        });
    }
}
