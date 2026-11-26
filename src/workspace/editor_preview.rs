use super::*;
use crate::session::EditorPreviewMode;
use gpui_base::{InteractiveElementExt as _, Popup};
use gpui_component::{Side, menu::PopupMenu};
use std::borrow::Cow;

mod markdown_scroll;

const DEFAULT_SPLIT_FRACTION: f32 = 0.5;
const MIN_SPLIT_FRACTION: f32 = 0.25;
const MAX_SPLIT_FRACTION: f32 = 0.75;
const DIVIDER_HIT_PADDING: Pixels = px(5.);
const MODE_MENU_CLOSE_DELAY: Duration = Duration::from_millis(180);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PreviewKind {
    Markdown,
    Svg,
}

impl PreviewKind {
    pub(super) fn for_path(path: &Path) -> Option<Self> {
        if docs::is_markdown(path) {
            Some(Self::Markdown)
        } else if crate::image_viewer::format(path) == Some(gpui::ImageFormat::Svg) {
            Some(Self::Svg)
        } else {
            None
        }
    }

    fn preference_key(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Svg => "svg",
        }
    }
}

impl EditorPreviewMode {
    fn next(self) -> Self {
        match self {
            Self::Full => Self::Split,
            Self::Split => Self::Source,
            Self::Source => Self::Full,
        }
    }

    fn label(self) -> Cow<'static, str> {
        match self {
            Self::Source => t!("editor_preview.source"),
            Self::Split => t!("editor_preview.split"),
            Self::Full => t!("editor_preview.full"),
        }
    }

    fn icon(self) -> (&'static [u8], &'static str) {
        // Draw the frame and symbol together so they scale from the same origin.
        match self {
            Self::Source => (
                include_bytes!("icons/editor-preview-source.svg"),
                "editor-preview-icon-source",
            ),
            Self::Split => (
                include_bytes!("icons/editor-preview-split.svg"),
                "editor-preview-icon-split",
            ),
            Self::Full => (
                include_bytes!("icons/editor-preview-full.svg"),
                "editor-preview-icon-full",
            ),
        }
    }

    fn glyph(self, dark: bool) -> (Icon, &'static str) {
        let (art, selector) = self.icon();
        let color = match (self, dark) {
            (Self::Source, false) => rgb(0x1756a8),
            (Self::Source, true) => rgb(0x8bb8ff),
            (Self::Split, false) => rgb(0x9a4d08),
            (Self::Split, true) => rgb(0xffb366),
            (Self::Full, false) => rgb(0x713ab0),
            (Self::Full, true) => rgb(0xcc9bff),
        };
        (Icon::default().data(art).text_color(color), selector)
    }
}

#[derive(Default)]
pub(super) struct EditorPreviews {
    markdown: HashMap<PathBuf, MarkdownPreview>,
    pub(super) svg_source_errors: HashMap<PathBuf, SharedString>,
    pub(super) split_drag: Option<(PreviewKind, f32)>,
    scroll_follow: markdown_scroll::ScrollFollow,
    mode_menu: Option<PreviewModeMenu>,
    pending_history: Option<(PathBuf, EditorPreviewLocation, Option<FocusHandle>)>,
}

struct PreviewModeMenu {
    path: PathBuf,
    menu: Entity<PopupMenu>,
    bounds: Rc<Cell<Option<(Bounds<Pixels>, Bounds<Pixels>)>>>,
    close_task: Option<Task<()>>,
    _dismiss: Subscription,
    _escape: Subscription,
}

struct MarkdownPreview {
    source: Option<Rope>,
    reader: Option<Result<Entity<docs::DocReader>, SharedString>>,
    reader_revision: Option<u64>,
    _task: Task<()>,
}

#[derive(Clone)]
struct PreviewDivider {
    kind: PreviewKind,
    offset: Rc<Cell<Pixels>>,
}

impl Render for PreviewDivider {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl Workspace {
    fn editor_preview_split_fraction(&self, kind: PreviewKind) -> f32 {
        let saved = match kind {
            PreviewKind::Markdown => self.session.editor_markdown_split,
            PreviewKind::Svg => self.session.editor_svg_split,
        };
        self.editor_previews
            .split_drag
            .filter(|(dragged, _)| *dragged == kind)
            .map(|(_, fraction)| fraction)
            .or(saved)
            .unwrap_or(DEFAULT_SPLIT_FRACTION)
            .clamp(MIN_SPLIT_FRACTION, MAX_SPLIT_FRACTION)
    }

    fn set_editor_preview_split_fraction(&mut self, kind: PreviewKind, fraction: f32) {
        let split = match kind {
            PreviewKind::Markdown => &mut self.session.editor_markdown_split,
            PreviewKind::Svg => &mut self.session.editor_svg_split,
        };
        let value = (fraction != DEFAULT_SPLIT_FRACTION).then_some(fraction);
        if *split != value {
            *split = value;
            self.session_revision += 1;
        }
    }

    pub(super) fn finish_editor_preview_resize(
        &mut self,
        event: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button == MouseButton::Left
            && let Some((kind, fraction)) = self.editor_previews.split_drag.take()
        {
            self.set_editor_preview_split_fraction(kind, fraction);
            cx.notify();
        }
    }

    pub(super) fn editor_preview_mode(&self, path: &Path) -> EditorPreviewMode {
        self.session
            .editor_previews
            .get(path)
            .copied()
            .unwrap_or_default()
    }

    pub(super) fn remember_editor_preview(&mut self, path: &Path) {
        let Some(kind) = PreviewKind::for_path(path) else {
            return;
        };
        if !self.session.editor_previews.contains_key(path) {
            let mode = self
                .store
                .editor_preview_preference(kind.preference_key())
                .unwrap_or_else(|error| {
                    eprintln!("Could not load editor preview preference: {error:#}");
                    EditorPreviewMode::default()
                });
            self.set_editor_preview_mode(path, mode);
        }
    }

    pub(super) fn set_editor_preview_mode(&mut self, path: &Path, mode: EditorPreviewMode) {
        if self.session.editor_previews.insert(path.to_owned(), mode) != Some(mode) {
            self.session_revision += 1;
        }
    }

    pub(super) fn forget_editor_preview(&mut self, path: &Path) {
        self.document_translation
            .forget(TranslationView::Editor, path);
        self.editor_previews.markdown.remove(path);
        self.editor_previews.svg_source_errors.remove(path);
        if self.session.editor_previews.remove(path).is_some() {
            self.session_revision += 1;
        }
    }

    pub(super) fn remap_editor_previews(&mut self, map: impl Fn(&Path) -> PathBuf) {
        self.session.editor_previews = std::mem::take(&mut self.session.editor_previews)
            .into_iter()
            .filter_map(|(path, mode)| {
                let target = map(&path);
                (PreviewKind::for_path(&path) == PreviewKind::for_path(&target))
                    .then_some((target, mode))
            })
            .collect();
        // A reader resolves relative resources against its original path.
        self.editor_previews
            .markdown
            .retain(|path, _| map(path) == *path);
        self.editor_previews.svg_source_errors =
            std::mem::take(&mut self.editor_previews.svg_source_errors)
                .into_iter()
                .filter_map(|(path, error)| {
                    let target = map(&path);
                    (PreviewKind::for_path(&target) == Some(PreviewKind::Svg))
                        .then_some((target, error))
                })
                .collect();
    }

    pub(super) fn active_editor_preview_mode(&self) -> EditorPreviewMode {
        self.tabs
            .active()
            .map_or(EditorPreviewMode::Source, |path| {
                self.editor_preview_mode(path)
            })
    }

    pub(super) fn active_markdown_preview(&self) -> Option<&Entity<docs::DocReader>> {
        self.editor_previews
            .markdown
            .get(self.tabs.active()?)?
            .reader
            .as_ref()?
            .as_ref()
            .ok()
    }

    pub(super) fn update_preview_typography(&self, typography: crate::config::Docs, cx: &mut App) {
        for preview in self.editor_previews.markdown.values() {
            if let Some(Ok(reader)) = &preview.reader {
                reader.update(cx, |reader, cx| reader.set_typography(typography, cx));
            }
        }
    }

    pub(super) fn editor_preview_button(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let path = self.tabs.active()?;
        let kind = PreviewKind::for_path(path)?;
        let mode = self.editor_preview_mode(path);
        let (glyph, selector) = mode.glyph(cx.theme().mode == ThemeMode::Dark);
        let path = path.to_owned();
        let standalone = self.standalone.is_some();
        let button = if standalone {
            icon_button("editor-preview-mode", glyph, mode.label()).icon_size(px(16.))
        } else {
            tab_bar_icon_control("editor-preview-mode", glyph, mode.label()).tab_stop(false)
        };
        let click_path = path.clone();
        let button = button
            .tooltip_placement(Placement::Bottom)
            .debug_selector(move || selector.into())
            .on_click(cx.listener(move |this, _, window, cx| {
                this.close_editor_preview_menu(cx);
                let next = this.editor_preview_mode(&click_path).next();
                this.change_editor_preview_mode(&click_path, kind, next, window, cx);
            }));
        Some(if standalone {
            button.into_any_element()
        } else {
            self.editor_preview_menu_trigger(button, path, kind, cx)
                .into_any_element()
        })
    }

    pub(super) fn cancel_editor_preview_history(&mut self) {
        self.editor_previews.pending_history = None;
    }

    pub(super) fn restore_editor_preview_history(
        &mut self,
        path: PathBuf,
        location: EditorPreviewLocation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_editor_preview_history();
        if let Some(reader) = self.active_markdown_preview() {
            reader.update(cx, |reader, cx| {
                reader.replay_history_location(location.document, false, cx);
                reader.focus_content(window, cx);
            });
        } else {
            self.editor_previews.pending_history = Some((path, location, window.focused(cx)));
        }
    }

    fn finish_markdown_preview(
        &mut self,
        path: &Path,
        reader: &Entity<docs::DocReader>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let history = self
            .editor_previews
            .pending_history
            .take_if(|(key, _, _)| key == path);
        let mode = self.editor_preview_mode(path);
        if self.view != WorkspaceView::Editor
            || self.tabs.active() != Some(path)
            || mode == EditorPreviewMode::Source
        {
            return;
        }
        if let Some((_, location, focus)) = history.filter(|(_, location, _)| location.mode == mode)
        {
            reader.update(cx, |reader, cx| {
                reader.replay_history_location(location.document, false, cx);
                if window.focused(cx) == focus {
                    reader.focus_content(window, cx);
                }
            });
        } else if mode == EditorPreviewMode::Full && self.focus.is_focused(window) {
            reader.update(cx, |reader, cx| reader.focus_content(window, cx));
        }
    }

    fn change_editor_preview_mode(
        &mut self,
        path: &Path,
        kind: PreviewKind,
        mode: EditorPreviewMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.editor_preview_mode(path);
        if self.tabs.active() != Some(path) || current == mode {
            return;
        }
        if current == EditorPreviewMode::Split {
            self.reset_markdown_scroll_follow(cx);
        }
        if mode == EditorPreviewMode::Full {
            self.capture_active_file_position(cx);
            if let Some(file) = self.files.get(path) {
                file.editor
                    .update(cx, |editor, cx| editor.dismiss_lsp_overlays(cx));
            }
        }
        self.cancel_editor_preview_history();
        self.set_editor_preview_mode(path, mode);
        if let Err(error) = self
            .store
            .save_editor_preview_preference(kind.preference_key(), mode)
        {
            self.show_error(error, window, cx);
        }
        if kind == PreviewKind::Svg
            && mode != EditorPreviewMode::Full
            && !self.files.contains_key(path)
        {
            self.request_file(
                path.to_owned(),
                FileOpen::Select {
                    preview: true,
                    position: None,
                },
                window,
                cx,
            );
        } else if mode == EditorPreviewMode::Full
            || current == EditorPreviewMode::Full
            || (mode == EditorPreviewMode::Source
                && (self.image_viewer_focus.is_focused(window)
                    || self
                        .active_markdown_preview()
                        .is_some_and(|reader| reader.read(cx).content_focused(window, cx))))
        {
            self.focus_workspace_content(None, window, cx);
        }
        cx.notify();
    }

    pub(super) fn sync_editor_preview_menu(&mut self) {
        if self
            .editor_previews
            .mode_menu
            .as_ref()
            .is_some_and(|state| {
                self.view != WorkspaceView::Editor
                    || self.repository.is_none()
                    || self.tabs.active() != Some(state.path.as_path())
            })
        {
            self.editor_previews.mode_menu = None;
        }
    }

    fn editor_preview_menu_trigger(
        &self,
        button: Button,
        path: PathBuf,
        kind: PreviewKind,
        cx: &Context<Self>,
    ) -> Popup {
        let open = self
            .editor_previews
            .mode_menu
            .as_ref()
            .filter(|state| state.path == path);
        let trigger = Popup::new("editor-preview-menu-trigger", button)
            .flex_shrink_0()
            .anchor(Anchor::TopRight)
            .offset(px(6.))
            .hover_listener_mode(HoverListenerMode::InputModalityIndependent)
            .on_hover(cx.listener(move |this, &hovered, window, cx| {
                if hovered {
                    this.show_editor_preview_menu(&path, kind, window, cx);
                } else if this
                    .editor_previews
                    .mode_menu
                    .as_ref()
                    .is_some_and(|state| state.bounds.get().is_none())
                {
                    // The pointer can leave before the popup paints its move listener.
                    this.hover_editor_preview_menu(false, window, cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close_editor_preview_menu(cx)),
            );

        let Some(open) = open else {
            return trigger;
        };
        let bounds = open.bounds.clone();
        let position_bounds = bounds.clone();
        let close_pending = open.close_task.is_some();
        let workspace = cx.weak_entity();
        trigger
            .on_position(move |popup, trigger| position_bounds.set(Some((popup.bounds, trigger))))
            .content(
                div().relative().child(open.menu.clone()).child(
                    canvas(
                        |_, _, _| (),
                        move |_, (), window, cx| {
                            if close_pending
                                && bounds.get().is_some_and(|(menu, _)| {
                                    menu.contains(&window.mouse_position())
                                })
                            {
                                let workspace = workspace.clone();
                                window.defer(cx, move |window, cx| {
                                    _ = workspace.update(cx, |this, cx| {
                                        this.hover_editor_preview_menu(true, window, cx)
                                    });
                                });
                            }
                            // Treat the trigger and popup as one hover region.
                            let bounds = bounds.clone();
                            let workspace = workspace.clone();
                            let exited = workspace.clone();
                            window.on_mouse_event(
                                move |event: &MouseMoveEvent, phase, window, cx| {
                                    if phase.capture() {
                                        let inside = bounds.get().is_some_and(|(menu, button)| {
                                            menu.contains(&event.position)
                                                || button.contains(&event.position)
                                        });
                                        _ = workspace.update(cx, |this, cx| {
                                            this.hover_editor_preview_menu(inside, window, cx)
                                        });
                                    }
                                },
                            );
                            window.on_mouse_event(move |_: &MouseExitEvent, phase, window, cx| {
                                if phase.capture() {
                                    _ = exited.update(cx, |this, cx| {
                                        this.hover_editor_preview_menu(false, window, cx)
                                    });
                                }
                            });
                        },
                    )
                    .absolute()
                    .inset_0(),
                ),
            )
    }

    fn show_editor_preview_menu(
        &mut self,
        path: &Path,
        kind: PreviewKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view != WorkspaceView::Editor
            || self.repository.is_none()
            || self.tabs.active() != Some(path)
            || self.editor_previews.mode_menu.is_some()
        {
            return;
        }

        let mode = self.editor_preview_mode(path);
        let dark = cx.theme().mode == ThemeMode::Dark;
        let workspace = cx.weak_entity();
        let menu = PopupMenu::build(window, cx, |mut menu, _, _| {
            menu = menu.min_w(Pixels::ZERO).check_side(Side::Right);
            for choice in [
                EditorPreviewMode::Source,
                EditorPreviewMode::Split,
                EditorPreviewMode::Full,
            ] {
                let path = path.to_owned();
                let workspace = workspace.clone();
                let (icon, _) = choice.glyph(dark);
                let item = PopupMenuItem::new(choice.label())
                    .icon(icon.with_size(px(15.)))
                    .on_click(move |_, window, cx| {
                        _ = workspace.update(cx, |this, cx| {
                            this.change_editor_preview_mode(&path, kind, choice, window, cx);
                        });
                    });
                menu = menu.item_with_check(item, choice == mode);
            }
            menu
        });
        let dismiss = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.close_editor_preview_menu(cx);
        });
        let window_id = window.window_handle().window_id();
        // The editor retains focus, so Escape must work outside the menu's key context.
        let escape = cx.intercept_keystrokes(move |event, window, cx| {
            if event.keystroke.key == "escape" && window.window_handle().window_id() == window_id {
                _ = workspace.update(cx, |this, cx| {
                    this.close_editor_preview_menu(cx);
                    cx.stop_propagation();
                });
            }
        });
        self.editor_previews.mode_menu = Some(PreviewModeMenu {
            path: path.to_owned(),
            menu,
            bounds: Rc::new(Cell::new(None)),
            close_task: None,
            _dismiss: dismiss,
            _escape: escape,
        });
        cx.notify();
    }

    fn hover_editor_preview_menu(
        &mut self,
        inside: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.editor_previews.mode_menu.as_mut() else {
            return;
        };
        if inside {
            state.close_task = None;
        } else if state.close_task.is_none() {
            state.close_task = Some(cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(MODE_MENU_CLOSE_DELAY).await;
                _ = this.update(cx, |this, cx| this.close_editor_preview_menu(cx));
            }));
        }
    }

    fn close_editor_preview_menu(&mut self, cx: &mut Context<Self>) {
        if self.editor_previews.mode_menu.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn refresh_active_editor_preview(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.view != WorkspaceView::Editor || self.terminal_maximized() {
            return;
        }
        let Some(path) = self.tabs.active().map(Path::to_owned) else {
            return;
        };
        self.remember_editor_preview(&path);
        if self.editor_preview_mode(&path) == EditorPreviewMode::Source {
            return;
        }
        let source = self
            .files
            .get(&path)
            .map(|file| file.editor.read(cx).text().clone());
        match PreviewKind::for_path(&path) {
            Some(PreviewKind::Markdown) => {
                if let Some(source) = source {
                    self.refresh_markdown_preview(path, source, window, cx);
                }
            }
            Some(PreviewKind::Svg) => {
                if let Some(source) = source {
                    self.refresh_svg_image_tab(path, source, window, cx);
                } else if !self.images.contains_key(&path)
                    && self.editor_previews.svg_source_errors.contains_key(&path)
                {
                    self.load_image_tab(path, window, cx);
                }
            }
            None => {}
        }
    }

    fn refresh_markdown_preview(
        &mut self,
        path: PathBuf,
        source: Rope,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let (updating, reader) = match self.editor_previews.markdown.get_mut(&path) {
            Some(preview) if preview.source.as_ref() == Some(&source) => {
                if preview.reader_revision.is_some() {
                    preview.reader_revision = self
                        .files
                        .get(&path)
                        .map(|file| file.editor.read(cx).document_revision());
                }
                return;
            }
            Some(preview) => (true, preview.reader.take()),
            None => (false, None),
        };
        let repo = self.repository.clone();
        let remote = repo
            .as_ref()
            .filter(|_| !path.is_absolute())
            .and_then(|repo| repo.remote_resources());
        let root = repo.as_ref().map_or_else(
            || path.parent().unwrap_or(Path::new("")).to_owned(),
            |repo| repo.root.clone(),
        );
        let snapshot = source.clone();
        let workspace = cx.weak_entity();
        // Drop the previous waiter on edits, so only the latest source can replace the reader.
        let key = path.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            if updating {
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;
            }
            let text = snapshot.clone();
            let result = cx
                .background_executor()
                .spawn(async move { docs::PreparedDocument::from_editor(text.to_string(), remote) })
                .await;
            let Ok(Some((reader, pending_parse))) = this.update_in(cx, |this, window, cx| {
                let preview = this.editor_previews.markdown.get_mut(&key)?;
                let document = match result {
                    Ok(document) => document,
                    Err(error) => {
                        if !matches!(preview.reader, Some(Ok(_))) {
                            preview.reader = Some(Err(format!("{error:#}").into()));
                            cx.notify();
                        }
                        return None;
                    }
                };
                let standalone = this.standalone.is_some();
                let reader = cx.new(|cx| {
                    let mut reader = docs::DocReader::new(
                        root,
                        key.clone(),
                        document,
                        None,
                        workspace,
                        window,
                        cx,
                    );
                    reader.set_editor_preview(standalone);
                    reader
                });
                let pending_parse =
                    reader.update(cx, |reader, cx| reader.wait_for_editor_preview_parse(cx));
                Some((reader, pending_parse))
            }) else {
                return;
            };
            // A new text view can still be parsing after its document has been prepared.
            if let Some((_subscription, parsed)) = pending_parse
                && parsed.await.is_err()
            {
                return;
            }
            _ = this.update_in(cx, |this, window, cx| {
                let Some(preview) = this.editor_previews.markdown.get_mut(&key) else {
                    return;
                };
                let Some(editor) = this
                    .files
                    .get(&key)
                    .map(|file| file.editor.read(cx))
                    .filter(|editor| editor.text() == &snapshot)
                else {
                    preview.source = None;
                    cx.notify();
                    return;
                };
                let revision = editor.document_revision();
                let typography = this.config.current().docs;
                reader.update(cx, |reader, cx| {
                    reader.set_typography(typography, cx);
                    if let Some(Ok(previous)) = &preview.reader {
                        reader.inherit_editor_preview(previous, window, cx);
                    }
                });
                preview.reader = Some(Ok(reader.clone()));
                preview.reader_revision = Some(revision);
                this.finish_markdown_preview(&key, &reader, window, cx);
                cx.notify();
            });
        });
        self.editor_previews.markdown.insert(
            path,
            MarkdownPreview {
                source: Some(source),
                reader,
                reader_revision: None,
                _task: task,
            },
        );
    }

    pub(super) fn editor_tab_body(
        &self,
        path: &Path,
        source: AnyElement,
        cx: &Context<Self>,
        colors: Palette,
    ) -> AnyElement {
        let mode = self.editor_preview_mode(path);
        if mode == EditorPreviewMode::Source {
            return source;
        }
        let Some(kind) = PreviewKind::for_path(path) else {
            return source;
        };
        let preview = match kind {
            PreviewKind::Markdown => {
                match self
                    .editor_previews
                    .markdown
                    .get(path)
                    .and_then(|item| item.reader.as_ref())
                {
                    Some(Ok(reader)) => reader.clone().into_any_element(),
                    Some(Err(error)) => Self::preview_status(error.clone(), colors.warning),
                    None => Self::preview_status(
                        t!("editor_preview.rendering").into_owned().into(),
                        colors.muted,
                    ),
                }
            }
            PreviewKind::Svg => self.images.get(path).map_or_else(
                || {
                    Self::preview_status(
                        t!("editor_preview.rendering").into_owned().into(),
                        colors.muted,
                    )
                },
                |image| self.image_tab_content(path, image, cx, colors),
            ),
        };
        let preview = v_flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .child(preview)
            .when(kind == PreviewKind::Markdown, |pane| {
                pane.key_context("Reader").on_action(cx.listener(
                    move |this, _: &FindInDocument, window, cx| {
                        if let Some(reader) = this.active_markdown_preview().cloned() {
                            if mode == EditorPreviewMode::Split {
                                this.reset_markdown_scroll_follow(cx);
                            }
                            reader.update(cx, |reader, cx| reader.find(window, cx));
                        }
                    },
                ))
            });
        if mode == EditorPreviewMode::Full {
            return preview
                .debug_selector(|| "editor-preview-full".into())
                .into_any_element();
        }
        let fraction = self.editor_preview_split_fraction(kind);
        h_flex()
            .id("editor-preview-split")
            .debug_selector(|| "editor-preview-split".into())
            .relative()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<PreviewDivider>, _, cx| {
                    let drag = event.drag(cx);
                    let width = f32::from(event.bounds.size.width);
                    if drag.kind == kind && width > 0. {
                        let offset =
                            event.event.position.x - event.bounds.left() - drag.offset.get();
                        let fraction = (f32::from(offset) / width)
                            .clamp(MIN_SPLIT_FRACTION, MAX_SPLIT_FRACTION);
                        this.editor_previews.split_drag = Some((kind, fraction));
                        cx.notify();
                    }
                }),
            )
            .child(
                v_flex()
                    .relative()
                    .w(relative(fraction))
                    .flex_none()
                    .h_full()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .child(source)
                    .when(kind == PreviewKind::Markdown, |pane| {
                        pane.child(Self::markdown_scroll_input(true, cx))
                            .capture_key_down(cx.listener(
                                |this, event: &KeyDownEvent, window, cx| {
                                    if matches!(event.keystroke.key.as_str(), "pageup" | "pagedown")
                                    {
                                        this.begin_markdown_scroll_follow(window, cx);
                                    }
                                },
                            ))
                    }),
            )
            .child(
                preview
                    .relative()
                    .debug_selector(|| "editor-preview-pane".into())
                    .h_full()
                    .when(kind == PreviewKind::Markdown, |pane| {
                        pane.child(Self::markdown_scroll_input(false, cx))
                            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                if matches!(
                                    event.keystroke.key.as_str(),
                                    "up" | "down"
                                        | "pageup"
                                        | "pagedown"
                                        | "home"
                                        | "end"
                                        | "space"
                                ) {
                                    this.pause_markdown_scroll_follow(cx);
                                }
                            }))
                    }),
            )
            .child(Self::editor_preview_divider(kind, fraction, cx))
            .into_any_element()
    }

    fn editor_preview_divider(
        kind: PreviewKind,
        fraction: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        // Anchor the resize handle at the boundary, above both panes.
        div()
            .debug_selector(|| "editor-preview-divider-anchor".into())
            .absolute()
            .left(relative(fraction))
            .top_0()
            .bottom_0()
            .w(px(1.))
            .child(
                pane_resize_handle("editor-preview-divider", Axis::Horizontal, true)
                    .with_hit_padding(DIVIDER_HIT_PADDING, DIVIDER_HIT_PADDING)
                    .on_drag_from_press(
                        PreviewDivider {
                            kind,
                            offset: Rc::default(),
                        },
                        |drag, position, _, cx| {
                            drag.offset.set(position.x - DIVIDER_HIT_PADDING);
                            cx.new(|_| drag.clone())
                        },
                    )
                    .on_double_click(cx.listener(move |this, _, _, cx| {
                        this.set_editor_preview_split_fraction(kind, DEFAULT_SPLIT_FRACTION);
                        cx.notify();
                    })),
            )
    }

    pub(super) fn preview_status(message: SharedString, color: Hsla) -> AnyElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .items_center()
            .justify_center()
            .px_4()
            .text_color(color)
            .child(message)
            .into_any_element()
    }

    pub(crate) fn follow_editor_preview_link(
        &mut self,
        from: &Path,
        target: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view != WorkspaceView::Editor || self.tabs.active() != Some(from) {
            return;
        }
        match docs::destination(from, target) {
            Some(Destination::External(url)) => cx.open_url(url.as_str()),
            Some(Destination::Local { path, fragment }) => {
                let path = if !path.is_absolute() {
                    Ok(path)
                } else if let Some(repo) = &self.repository {
                    repo.local_file_key(&path)
                } else {
                    crate::repository::canonical_local_file(&path)
                };
                match path {
                    Ok(path) => {
                        if PreviewKind::for_path(&path) == Some(PreviewKind::Svg)
                            && fragment.is_none()
                        {
                            self.request_file(path, FileOpen::ImageBrowse, window, cx);
                            return;
                        }
                        let position = fragment.and_then(|fragment| {
                            if docs::is_markdown(&path) && docs::line_fragment(&fragment).is_none()
                            {
                                Some(SourcePosition::MarkdownFragment(fragment))
                            } else {
                                docs::line_fragment(&fragment).map(|line| {
                                    SourcePosition::Lsp(lsp_types::Position::new(line, 0))
                                })
                            }
                        });
                        self.request_file(
                            path,
                            FileOpen::Select {
                                preview: true,
                                position,
                            },
                            window,
                            cx,
                        );
                    }
                    Err(error) => self.show_error(error, window, cx),
                }
            }
            None => toast::info(window, cx, t!("workspace.link_failed")),
        }
    }

    pub(crate) fn preview_fragment_missing(
        &self,
        fragment: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        toast::info(window, cx, t!("docs.fragment_missing", fragment = fragment));
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::session::DocumentPosition;
    use std::time::Instant;

    #[track_caller]
    fn settle(
        visual: &mut VisualTestContext,
        workspace: &Entity<Workspace>,
        ready: impl Fn(&Workspace, &App) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            visual.executor().advance_clock(Duration::from_millis(150));
            visual.run_until_parked();
            if visual.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
                ready(workspace.read(cx), cx)
            }) {
                return;
            }
            assert!(Instant::now() < deadline, "editor preview did not settle");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[track_caller]
    fn assert_split_layout(visual: &mut VisualTestContext, fraction: f32) {
        let split = visual.debug_bounds("editor-preview-split").unwrap();
        let preview = visual.debug_bounds("editor-preview-pane").unwrap();
        let line = visual
            .debug_bounds("editor-preview-divider-anchor")
            .unwrap();
        assert_eq!(line.left(), preview.left());
        assert_eq!(line.size.height, split.size.height);
        let expected = split.left() + split.size.width * fraction;
        assert!(
            (line.left() - expected).abs() <= px(1.),
            "divider at {}, expected {expected} for fraction {fraction}",
            line.left()
        );
    }

    fn markdown_scroll_frames(
        visual: &mut VisualTestContext,
        workspace: &Entity<Workspace>,
    ) -> Vec<Option<DocumentPosition>> {
        (0..8)
            .map(|_| {
                visual.executor().advance_clock(Duration::from_millis(16));
                visual.run_until_parked();
                visual.update(|window, cx| {
                    window.simulate_next_frame(cx);
                    window.draw(cx).clear(cx);
                    workspace
                        .read(cx)
                        .active_markdown_preview()
                        .and_then(|reader| reader.read(cx).session_position(cx))
                })
            })
            .collect()
    }

    #[track_caller]
    fn send_source_scroll_to(
        visual: &mut VisualTestContext,
        workspace: &Entity<Workspace>,
        source: &str,
        offset: usize,
    ) -> usize {
        let row = source[..offset]
            .bytes()
            .filter(|&byte| byte == b'\n')
            .count();
        let position = visual.debug_bounds("workspace-editor").unwrap().center();
        let delta = visual.update(|_, cx| {
            let editor = workspace.read(cx).active_file().unwrap().editor.read(cx);
            let (top, inset) = editor.viewport_anchor().unwrap();
            let line_height = editor.line_height().unwrap();
            line_height * (row as f32 - top.line as f32 + 0.65) - inset
        });
        visual.simulate_mouse_move(position, None, Modifiers::default());
        visual.simulate_event(ScrollEvent {
            position,
            delta: ScrollDelta::Pixels(point(px(0.), -delta)),
            touch_phase: TouchPhase::Started,
            ..Default::default()
        });
        row
    }

    #[track_caller]
    fn scroll_source_to(
        visual: &mut VisualTestContext,
        workspace: &Entity<Workspace>,
        source: &str,
        offset: usize,
    ) -> Vec<Option<DocumentPosition>> {
        let row = send_source_scroll_to(visual, workspace, source, offset);
        let frames = markdown_scroll_frames(visual, workspace);
        let source_row = visual.update(|_, cx| {
            let editor = workspace.read(cx).active_file().unwrap().editor.read(cx);
            editor.viewport_anchor().unwrap().0.line as usize
        });
        assert_eq!(
            source_row,
            row,
            "source scroll to {}",
            &source[offset..].lines().next().unwrap()
        );
        frames
    }

    const MARKDOWN_SCROLL_FIXTURE: &str =
        include_str!("../../tests/fixtures/markdown-scroll/complex-layout.md");

    fn open_markdown_scroll_fixture<'a>(
        cx: &'a mut TestAppContext,
        fixture: &Path,
    ) -> Result<(Entity<Workspace>, &'a mut VisualTestContext)> {
        open_markdown_preview_fixture(cx, fixture, MARKDOWN_SCROLL_FIXTURE)
    }

    fn open_markdown_preview_fixture<'a>(
        cx: &'a mut TestAppContext,
        fixture: &Path,
        source: &str,
    ) -> Result<(Entity<Workspace>, &'a mut VisualTestContext)> {
        let image = include_bytes!("../../tests/fixtures/markdown-scroll/tall-image.svg");
        let path = fixture.join("complex-layout.md");
        fs::write(&path, source)?;
        fs::write(fixture.join("tall-image.svg"), image)?;
        let config_path = fixture.join("config.toml");
        fs::write(&config_path, "[editor]\nsoft_wrap = false\n")?;
        let config = ConfigFile::load(Some(config_path));
        let store = Arc::new(Store::new(fixture.join("state"))?);
        store.save_editor_preview_preference("markdown", EditorPreviewMode::Split)?;
        cx.update(gpui_component::init);
        let mut workspace = None;
        let (_, mut visual) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| {
                Workspace::with_window(
                    store,
                    config,
                    Some(WorkspaceView::Editor),
                    Some(path.clone()),
                    window,
                    cx,
                )
            });
            view.update(cx, |this, cx| {
                this.request_file(path, FileOpen::Startup, window, cx)
            });
            workspace = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(950.), px(330.)));
        settle(&mut visual, &workspace, |this, cx| {
            this.active_markdown_preview()
                .is_some_and(|reader| reader.read(cx).session_position(cx).is_some())
        });
        Ok((workspace, visual))
    }

    #[::core::prelude::v1::test]
    fn markdown_preview_find_follows_focus_in_split_and_full_modes() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(cfg!(target_os = "linux")),
        );
        let (find, copy) = if cfg!(target_os = "macos") {
            ("cmd-f", "cmd-c")
        } else {
            ("ctrl-f", "ctrl-c")
        };
        cx.update(|cx| {
            cx.bind_keys([KeyBinding::new(
                find,
                FindInDocument,
                Some("Reader && !Locator"),
            )]);
        });
        let markdown = format!(
            "> [!NOTE] **Split preview title**\n> An alert in split view.\n\n{MARKDOWN_SCROLL_FIXTURE}"
        );
        let (workspace, visual) =
            open_markdown_preview_fixture(&mut cx, fixture.path(), &markdown)?;
        visual.update(|window, _| window.activate_window());

        let preview = visual.debug_bounds("editor-preview-pane").unwrap();
        visual.update(|window, cx| {
            let note = rgb(if cx.theme().mode.is_dark() {
                0x8ea2ff
            } else {
                0x365ac3
            })
            .into();
            let scale = window.scale_factor();
            assert!(window.painted_quads().iter().any(|quad| {
                let width = quad.bounds.size.width.as_f32() / scale;
                let center = quad.bounds.center().map(|value| px(value.as_f32() / scale));
                quad.background.as_solid() == Some(note)
                    && (width - 3.).abs() <= 0.5
                    && preview.contains(&center)
            }));
        });

        let source = visual.debug_bounds("workspace-editor").unwrap();
        visual.simulate_click(source.center(), Modifiers::none());
        visual.simulate_keystrokes(find);
        settle(visual, &workspace, |this, cx| {
            this.active_file()
                .unwrap()
                .editor
                .read(cx)
                .search_session()
                .open
        });
        assert!(source.contains(&visual.debug_bounds("find-bar").unwrap().center()));
        visual.simulate_keystrokes("escape");
        settle(visual, &workspace, |this, cx| {
            !this
                .active_file()
                .unwrap()
                .editor
                .read(cx)
                .search_session()
                .open
        });

        visual.simulate_click(preview.center(), Modifiers::none());
        visual.simulate_keystrokes(find);
        settle(visual, &workspace, |_, _| true);
        assert!(preview.contains(&visual.debug_bounds("find-bar").unwrap().center()));

        visual.simulate_input("Final section");
        let final_section = markdown.find("## Final section").unwrap();
        settle(visual, &workspace, |this, cx| {
            let reader = this.active_markdown_preview().unwrap().read(cx);
            reader
                .editor_scroll_anchor(final_section, cx)
                .is_some_and(|anchor| reader.editor_scroll_delta(anchor, 0., cx).is_some())
        });
        visual.simulate_keystrokes(find);
        visual.simulate_keystrokes(copy);
        visual.update(|_, cx| {
            assert_eq!(
                cx.read_from_clipboard()
                    .and_then(|item| item.text())
                    .as_deref(),
                Some("Final section")
            );
        });
        visual.simulate_keystrokes("escape");
        settle(visual, &workspace, |_, _| true);
        assert!(visual.debug_bounds("find-bar").is_none());
        visual.simulate_keystrokes(find);
        settle(visual, &workspace, |_, _| true);
        assert!(preview.contains(&visual.debug_bounds("find-bar").unwrap().center()));
        visual.simulate_input("Split preview title");
        settle(visual, &workspace, |_, _| true);
        visual.simulate_keystrokes(find);
        visual.simulate_keystrokes(copy);
        visual.update(|_, cx| {
            assert_eq!(
                cx.read_from_clipboard()
                    .and_then(|item| item.text())
                    .as_deref(),
                Some("Split preview title")
            );
        });
        visual.simulate_keystrokes("escape");

        for (icon, mode) in [
            ("editor-preview-icon-split", EditorPreviewMode::Source),
            ("editor-preview-icon-source", EditorPreviewMode::Full),
        ] {
            let button = visual.debug_bounds(icon).unwrap();
            visual.simulate_click(button.center(), Modifiers::none());
            settle(visual, &workspace, |this, _| {
                this.active_editor_preview_mode() == mode
            });
        }
        let full = visual.debug_bounds("editor-preview-full").unwrap();
        visual.simulate_click(full.center(), Modifiers::none());
        visual.simulate_keystrokes(find);
        settle(visual, &workspace, |_, _| true);
        assert!(full.contains(&visual.debug_bounds("find-bar").unwrap().center()));
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn markdown_split_scroll_holds_through_complex_layout_and_rejoins_the_right_heading()
    -> Result<()> {
        let source = MARKDOWN_SCROLL_FIXTURE;
        let fixture = tempfile::tempdir()?;
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(cfg!(target_os = "linux")),
        );
        let (workspace, mut visual) = open_markdown_scroll_fixture(&mut cx, fixture.path())?;
        visual.update(|_, cx| cx.set_smooth_scrolling(false));

        let checkpoints: Vec<_> = source
            .match_indices("## Checkpoint\n")
            .map(|(offset, _)| offset)
            .collect();
        let (before, after) = (checkpoints[0], checkpoints[1]);
        let reach = |visual: &mut VisualTestContext, at| {
            let frames = scroll_source_to(visual, &workspace, source, at);
            assert_eq!(
                frames
                    .last()
                    .and_then(|frame| *frame)
                    .and_then(|position| position.source_offset),
                Some(at)
            );
            frames
        };
        reach(&mut visual, before);
        let plain_line = source
            .find("the same paragraph with visible words")
            .unwrap();
        let ordinary = scroll_source_to(&mut visual, &workspace, source, plain_line)
            .pop()
            .unwrap()
            .unwrap();
        assert_eq!(ordinary.source_offset, source.find("This plain paragraph"));
        let visible_line = ordinary.line.unwrap().source_offset;
        assert!(visible_line <= plain_line && plain_line - visible_line < 90);

        let start = reach(&mut visual, before).pop().unwrap().unwrap();
        assert_eq!(start.inset, 0.);
        for landmark in [
            "| Delta",
            "    B -- No",
            "![Tall calibration",
            "# Example heading inside a literal block",
            "\\sum_",
        ] {
            let frames = scroll_source_to(
                &mut visual,
                &workspace,
                source,
                source.find(landmark).unwrap(),
            );
            for position in frames {
                let position = position.expect("the held preview is visible");
                assert_eq!(
                    position.source_offset,
                    Some(before),
                    "preview moved while crossing {landmark}"
                );
                assert_eq!(
                    position.inset, start.inset,
                    "preview shook while crossing {landmark}"
                );
            }
        }
        visual.simulate_resize(size(px(850.), px(330.)));
        assert!(
            markdown_scroll_frames(&mut visual, &workspace)
                .iter()
                .all(|frame| {
                    frame.is_some_and(|position| position.source_offset == Some(before))
                })
        );
        let frames = reach(&mut visual, after);
        let visited: Vec<_> = frames
            .iter()
            .flatten()
            .filter_map(|frame| frame.source_offset)
            .collect();
        assert!(
            visited
                .iter()
                .all(|offset| *offset == before || *offset == after)
        );
        assert!(visited.windows(2).all(|pair| pair[0] <= pair[1]));

        let source_y = visual.update(|_, cx| {
            workspace
                .read(cx)
                .active_file()
                .unwrap()
                .editor
                .read(cx)
                .scroll_offset()
                .y
        });
        let preview = visual.debug_bounds("editor-preview-pane").unwrap().center();
        visual.simulate_mouse_move(preview, None, Modifiers::default());
        visual.simulate_event(ScrollEvent {
            position: preview,
            delta: ScrollDelta::Pixels(point(px(0.), px(180.))),
            ..Default::default()
        });
        let manual = markdown_scroll_frames(&mut visual, &workspace)
            .pop()
            .unwrap()
            .unwrap();
        assert_ne!(manual.source_offset, Some(after));
        assert_eq!(
            visual.update(|_, cx| workspace
                .read(cx)
                .active_file()
                .unwrap()
                .editor
                .read(cx)
                .scroll_offset()
                .y),
            source_y
        );

        let source_pane = visual.debug_bounds("workspace-editor").unwrap().center();
        visual.simulate_mouse_move(source_pane, None, Modifiers::default());
        visual.simulate_event(ScrollEvent {
            position: source_pane,
            delta: ScrollDelta::Pixels(point(px(0.), px(-18.))),
            touch_phase: TouchPhase::Moved,
            ..Default::default()
        });
        assert!(
            markdown_scroll_frames(&mut visual, &workspace)
                .iter()
                .all(|frame| {
                    frame.is_some_and(|position| {
                        position.source_offset == manual.source_offset
                            && position.inset == manual.inset
                    })
                }),
            "an earlier source gesture reclaimed the manually scrolled preview"
        );

        let closing = source.find("## Final section").unwrap();
        reach(&mut visual, closing);
        reach(&mut visual, after);
        let frames = scroll_source_to(
            &mut visual,
            &workspace,
            source,
            source.find("| Mu").unwrap(),
        );
        assert!(
            frames
                .iter()
                .all(|frame| frame.and_then(|position| position.source_offset) == Some(after))
        );
        reach(&mut visual, before);

        let draft = source.replacen(
            "## Checkpoint",
            "## Draft-only heading\n\nAdded before the first checkpoint.\n\n## Checkpoint",
            1,
        );
        visual.update(|window, cx| {
            let editor = workspace.read(cx).active_file().unwrap().editor.clone();
            editor.update(cx, |editor, cx| {
                editor.replace_all(draft.clone(), window, cx)
            });
            window.draw(cx).clear(cx);
        });
        let draft_only = draft.find("## Draft-only heading").unwrap();
        let frames = scroll_source_to(&mut visual, &workspace, &draft, draft_only);
        assert!(
            frames[..4].iter().all(|frame| {
                frame.is_some_and(|position| position.source_offset == Some(before))
            }),
            "the earlier rendered buffer followed newly edited source"
        );
        settle(&mut visual, &workspace, |this, cx| {
            this.active_markdown_preview().is_some_and(|reader| {
                reader.read(cx).source_text().as_ref() == draft
                    && reader.read(cx).session_position(cx).is_some()
            })
        });
        let draft_checkpoint = draft.rfind("## Checkpoint").unwrap();
        assert_eq!(
            scroll_source_to(&mut visual, &workspace, &draft, draft_checkpoint)
                .last()
                .and_then(|frame| *frame)
                .and_then(|position| position.source_offset),
            Some(draft_checkpoint)
        );
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn markdown_split_scroll_interpolates_text_and_animates_safe_reentry() -> Result<()> {
        let source = MARKDOWN_SCROLL_FIXTURE;
        let fixture = tempfile::tempdir()?;
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(cfg!(target_os = "linux")),
        );
        let (workspace, mut visual) = open_markdown_scroll_fixture(&mut cx, fixture.path())?;
        visual.update(|_, cx| {
            cx.set_smooth_scrolling(true);
            cx.set_reduce_motion(false);
        });
        let reader_position = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| {
                let reader = workspace
                    .read(cx)
                    .active_markdown_preview()
                    .unwrap()
                    .read(cx);
                (
                    reader.editor_scroll_pixels(cx),
                    reader.editor_scroll_is_navigating(cx),
                )
            })
        };
        let frame = |visual: &mut VisualTestContext| {
            visual.executor().advance_clock(Duration::from_millis(16));
            visual.run_until_parked();
            visual.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
            });
            reader_position(visual).0
        };
        let plain = source.find("This plain paragraph").unwrap();
        send_source_scroll_to(&mut visual, &workspace, source, plain);
        settle(&mut visual, &workspace, |this, cx| {
            this.active_markdown_preview().is_some_and(|reader| {
                reader
                    .read(cx)
                    .session_position(cx)
                    .is_some_and(|position| position.source_offset == Some(plain))
            })
        });
        let editor_row = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| {
                workspace
                    .read(cx)
                    .active_file()
                    .unwrap()
                    .editor
                    .read(cx)
                    .viewport_anchor()
                    .unwrap()
                    .0
            })
        };
        let top = editor_row(&mut visual);
        let pane = visual.debug_bounds("workspace-editor").unwrap().center();
        let mut previous = reader_position(&mut visual).0;
        for _ in 0..3 {
            visual.simulate_event(ScrollEvent {
                position: pane,
                delta: ScrollDelta::Pixels(point(px(0.), px(-1.25))),
                ..Default::default()
            });
            let next = frame(&mut visual);
            assert!(
                next > previous,
                "each sub-line scroll advances the measured preview"
            );
            assert_eq!(editor_row(&mut visual), top);
            assert!(!reader_position(&mut visual).1);
            previous = next;
        }

        let table = source.find("| Mu").unwrap();
        let after = source.rfind("## Checkpoint").unwrap();
        send_source_scroll_to(&mut visual, &workspace, source, table);
        let held = reader_position(&mut visual).0;
        assert!((0..4).all(|_| frame(&mut visual) == held));
        send_source_scroll_to(&mut visual, &workspace, source, after);
        let moving: Vec<_> = (0..8).map(|_| frame(&mut visual)).collect();
        assert!(
            reader_position(&mut visual).1,
            "re-entry uses a live navigation animation"
        );
        assert!(moving.windows(2).all(|step| step[0] <= step[1]));
        assert!(moving.windows(2).filter(|step| step[0] < step[1]).count() >= 2);

        let reversal = reader_position(&mut visual).0;
        send_source_scroll_to(&mut visual, &workspace, source, table);
        assert!((0..6).all(|_| frame(&mut visual) == reversal));
        assert!(!reader_position(&mut visual).1);
        send_source_scroll_to(&mut visual, &workspace, source, after);
        frame(&mut visual);
        assert!(reader_position(&mut visual).1);
        let preview = visual.debug_bounds("editor-preview-pane").unwrap().center();
        visual.simulate_mouse_move(preview, None, Modifiers::default());
        visual.simulate_event(ScrollEvent {
            position: preview,
            delta: ScrollDelta::Pixels(point(px(0.), px(12.))),
            ..Default::default()
        });
        let manual = frame(&mut visual);
        assert!(!reader_position(&mut visual).1);
        assert!((0..5).all(|_| frame(&mut visual) == manual));

        send_source_scroll_to(&mut visual, &workspace, source, table);
        frame(&mut visual);
        send_source_scroll_to(&mut visual, &workspace, source, after);
        settle(&mut visual, &workspace, |this, cx| {
            this.active_markdown_preview().is_some_and(|reader| {
                reader
                    .read(cx)
                    .session_position(cx)
                    .is_some_and(|position| position.source_offset == Some(after))
            })
        });
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn markdown_split_scroll_retargets_before_the_previous_anchor_settles() -> Result<()> {
        let source = MARKDOWN_SCROLL_FIXTURE;
        let fixture = tempfile::tempdir()?;
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(cfg!(target_os = "linux")),
        );
        let (workspace, mut visual) = open_markdown_scroll_fixture(&mut cx, fixture.path())?;
        visual.update(|_, cx| {
            cx.set_smooth_scrolling(true);
            cx.set_reduce_motion(false);
        });
        let frame = |visual: &mut VisualTestContext| {
            visual.executor().advance_clock(Duration::from_millis(16));
            visual.run_until_parked();
            visual.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
                let reader = workspace
                    .read(cx)
                    .active_markdown_preview()
                    .unwrap()
                    .read(cx);
                (
                    reader.editor_scroll_pixels(cx),
                    reader.editor_scroll_destination(cx),
                )
            })
        };
        let table = source.find("| Mu").unwrap();
        let first = source.rfind("## Checkpoint").unwrap();
        let next = source.find("## Final section").unwrap();
        let gap = source.find("\n\nThis is the checkpoint after").unwrap() + 1;
        send_source_scroll_to(&mut visual, &workspace, source, table);
        frame(&mut visual);
        send_source_scroll_to(&mut visual, &workspace, source, first);
        frame(&mut visual);
        let (previous, _) = frame(&mut visual);
        send_source_scroll_to(&mut visual, &workspace, source, gap);
        let (before, destination) = frame(&mut visual);
        assert_eq!(
            destination,
            Some(first),
            "a blank source line keeps the accepted goal"
        );
        let step = before - previous;
        assert!(step > px(1.), "the first animation is still moving");

        let next_row = send_source_scroll_to(&mut visual, &workspace, source, next);
        let (retargeted, destination) = frame(&mut visual);
        let source_row = visual.update(|_, cx| {
            workspace
                .read(cx)
                .active_file()
                .unwrap()
                .editor
                .read(cx)
                .viewport_anchor()
                .unwrap()
                .0
                .line as usize
        });
        assert_eq!(source_row, next_row);
        assert_eq!(
            destination,
            Some(next),
            "the new anchor should replace the old target in flight"
        );
        let (continued, destination) = frame(&mut visual);
        assert_eq!(destination, Some(next));
        assert!(before < retargeted && retargeted < continued);
        assert!(
            continued - retargeted < step * 2.,
            "retargeting restarted the animation at launch speed"
        );
        settle(&mut visual, &workspace, |this, cx| {
            this.active_markdown_preview().is_some_and(|reader| {
                reader
                    .read(cx)
                    .session_position(cx)
                    .is_some_and(|position| position.source_offset == Some(next))
            })
        });
        Ok(())
    }

    fn drag_split(visual: &mut VisualTestContext, workspace: &Entity<Workspace>, fraction: f32) {
        let split = visual.debug_bounds("editor-preview-split").unwrap();
        let line = visual
            .debug_bounds("editor-preview-divider-anchor")
            .unwrap();
        // Grab just beside the line, away from the middle grip.
        let from = point(line.left() + px(4.), split.top() + px(30.));
        let to = point(split.left() + split.size.width * fraction + px(4.), from.y);
        let saved = visual.update(|_, cx| {
            let session = &workspace.read(cx).session;
            (session.editor_markdown_split, session.editor_svg_split)
        });
        visual.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: from,
            click_count: 1,
            ..Default::default()
        });
        for position in [point(from.x + (to.x - from.x) * 0.5, from.y), to] {
            visual.simulate_event(MouseMoveEvent {
                position,
                pressed_button: Some(MouseButton::Left),
                ..Default::default()
            });
        }
        settle(visual, workspace, |_, _| true);
        assert_split_layout(
            visual,
            fraction.clamp(MIN_SPLIT_FRACTION, MAX_SPLIT_FRACTION),
        );
        visual.update(|_, cx| {
            let session = &workspace.read(cx).session;
            assert_eq!(
                (session.editor_markdown_split, session.editor_svg_split),
                saved
            );
        });
        visual.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: to,
            click_count: 1,
            ..Default::default()
        });
        settle(visual, workspace, |this, _| {
            this.editor_previews.split_drag.is_none()
        });
        assert_split_layout(
            visual,
            fraction.clamp(MIN_SPLIT_FRACTION, MAX_SPLIT_FRACTION),
        );
    }

    fn preview_menu_after(
        visual: &mut VisualTestContext,
        elapsed: Duration,
    ) -> Option<Bounds<Pixels>> {
        visual.run_until_parked();
        visual.executor().advance_clock(elapsed);
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
        visual.debug_bounds("popup-menu-surface")
    }

    fn move_to(visual: &mut VisualTestContext, position: Point<Pixels>) {
        visual.simulate_mouse_move(position, None, Modifiers::none());
    }

    fn assert_editor_preview_menu(visual: &mut VisualTestContext, workspace: &Entity<Workspace>) {
        let focus = visual.update(|window, cx| {
            let focus = workspace
                .read(cx)
                .active_file()
                .unwrap()
                .editor
                .read(cx)
                .focus_handle(cx);
            focus.focus(window, cx);
            focus
        });
        let button = visual.debug_bounds("editor-preview-icon-source").unwrap();
        let actions = visual.debug_bounds("workspace-actions-trigger").unwrap();
        let gap = point(button.center().x, button.bottom() + px(2.));
        let millisecond = Duration::from_millis(1);

        assert!(preview_menu_after(visual, Duration::ZERO).is_none());
        move_to(visual, button.center());
        let menu = preview_menu_after(visual, Duration::ZERO).expect("menu opens on pointer entry");
        assert!(menu.top() > button.bottom());
        assert_eq!(menu.right(), button.right());
        assert_eq!(
            visual.debug_bounds("editor-preview-icon-source"),
            Some(button)
        );
        assert_eq!(
            visual.debug_bounds("workspace-actions-trigger"),
            Some(actions)
        );
        assert!(visual.update(|window, _| focus.is_focused(window)));

        let font_size = visual.update(|_, cx| {
            let font_size = cx.theme().control_font_size;
            Theme::update(cx, |theme| theme.control_font_size = px(10.));
            font_size
        });
        let compact = preview_menu_after(visual, Duration::ZERO).unwrap();
        assert!(compact.size.width < menu.size.width);
        assert_eq!(compact.right(), menu.right());
        visual.update(|_, cx| Theme::update(cx, |theme| theme.control_font_size = font_size));
        assert_eq!(preview_menu_after(visual, Duration::ZERO), Some(menu));

        move_to(visual, gap);
        assert!(preview_menu_after(visual, MODE_MENU_CLOSE_DELAY - millisecond).is_some());
        move_to(visual, menu.center());
        assert!(preview_menu_after(visual, MODE_MENU_CLOSE_DELAY).is_some());
        move_to(visual, gap);
        assert!(preview_menu_after(visual, MODE_MENU_CLOSE_DELAY).is_none());

        // Pointer events may arrive back-to-back before the popup's first draw.
        visual.update(|window, cx| {
            window.simulate_mouse_move(button.center(), cx);
            window.simulate_mouse_move(gap, cx);
        });
        assert!(preview_menu_after(visual, MODE_MENU_CLOSE_DELAY).is_none());
        visual.update(|window, cx| {
            window.simulate_mouse_move(button.center(), cx);
            window.simulate_mouse_move(menu.center(), cx);
        });
        assert!(preview_menu_after(visual, Duration::ZERO).is_some());
        assert!(preview_menu_after(visual, MODE_MENU_CLOSE_DELAY).is_some());

        move_to(visual, button.center());
        assert!(preview_menu_after(visual, Duration::ZERO).is_some());
        visual.simulate_keystrokes("escape");
        assert!(preview_menu_after(visual, Duration::ZERO).is_none());
        move_to(visual, button.center() + point(px(1.), px(1.)));
        assert!(preview_menu_after(visual, Duration::ZERO).is_none());
        assert!(visual.update(|window, _| focus.is_focused(window)));

        move_to(visual, gap);
        move_to(visual, button.center());
        assert!(preview_menu_after(visual, Duration::ZERO).is_some());
        visual.simulate_click(gap, Modifiers::none());
        assert!(preview_menu_after(visual, Duration::ZERO).is_none());

        visual.update(|window, cx| focus.focus(window, cx));
        move_to(visual, button.center());
        let menu = preview_menu_after(visual, Duration::ZERO).unwrap();
        move_to(visual, menu.center());
        visual.simulate_click(menu.center(), Modifiers::none());
        assert!(preview_menu_after(visual, Duration::ZERO).is_none());
        assert!(visual.debug_bounds("editor-preview-icon-split").is_some());
        assert!(visual.update(|window, _| focus.is_focused(window)));

        move_to(visual, button.center());
        let menu = preview_menu_after(visual, Duration::ZERO).unwrap();
        visual.simulate_click(menu.center(), Modifiers::none());
        assert!(preview_menu_after(visual, Duration::ZERO).is_none());
        assert!(visual.debug_bounds("editor-preview-icon-split").is_some());

        move_to(visual, gap);
        move_to(visual, button.center());
        assert!(preview_menu_after(visual, Duration::ZERO).is_some());
        visual.simulate_click(button.center(), Modifiers::none());
        assert!(preview_menu_after(visual, Duration::ZERO).is_none());
        assert!(visual.debug_bounds("editor-preview-icon-source").is_some());
        assert!(visual.update(|window, _| focus.is_focused(window)));
    }

    #[::core::prelude::v1::test]
    fn editor_preview_cycles_renders_drafts_and_saves_modes_and_splits() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir(&root)?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "-qb", "main"])
                .arg(&root)
                .status()?
                .success()
        );
        let first = Path::new("first.md");
        let second = Path::new("second.markdown");
        let svg = Path::new("diagram.svg");
        let plain = Path::new("notes.txt");
        let body =
            "\n\nA paragraph that stays visible while the draft is being rendered.".repeat(100);
        let saved_markdown = format!("# First heading{body}\n");
        let draft_markdown = format!("# Unwritten heading{body}\n");
        fs::write(root.join(first), &saved_markdown)?;
        fs::write(root.join(second), "# Second heading\n")?;
        fs::write(root.join(plain), "Plain text\n")?;
        let valid_svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="48"><rect width="64" height="48" fill="#2864d6"/></svg>"##;
        fs::write(root.join(svg), valid_svg)?;
        let repo = Repository::open(&root)?;
        let state = fixture.path().join("state");
        let store = Arc::new(Store::new(state.clone())?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(cfg!(target_os = "linux")),
        );
        cx.update(gpui_component::init);
        let mut workspace = None;
        let (_, mut visual) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| {
                Workspace::with_config(store.clone(), config, WorkspaceView::Editor, window, cx)
            });
            workspace = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(960.), px(600.)));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.apply_repository(repo.clone(), Default::default(), None, None, window, cx);
            });
        });
        let open = |visual: &mut VisualTestContext, workspace: &Entity<Workspace>, path: &Path| {
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    this.open_file(path.to_owned(), false, window, cx);
                });
            });
            settle(visual, workspace, |this, _| {
                this.tabs.active() == Some(path)
            });
        };
        let click_mode =
            |visual: &mut VisualTestContext, workspace: &Entity<Workspace>, icon: &'static str| {
                let button = visual.debug_bounds(icon).expect("preview icon");
                let actions = visual.debug_bounds("workspace-actions-trigger").unwrap();
                assert_eq!(button.size, size(px(22.), px(22.)));
                assert_eq!(button.size, actions.size);
                assert_eq!(button.center().y, actions.center().y);
                visual.simulate_click(button.center(), Modifiers::default());
                settle(visual, workspace, |_, _| true);
            };
        let edit = |visual: &mut VisualTestContext, value: &str| {
            visual.update(|window, cx| {
                let editor = workspace.read(cx).active_file().unwrap().editor.clone();
                editor.update(cx, |editor, cx| {
                    editor.replace_all(value.to_owned(), window, cx)
                });
            });
        };

        open(&mut visual, &workspace, first);
        let status = visual.debug_bounds("status-file-path").unwrap();
        let icon = visual.debug_bounds("editor-preview-icon-source").unwrap();
        let actions = visual.debug_bounds("workspace-actions-trigger").unwrap();
        assert!(icon.right() <= actions.left());
        assert!(visual.debug_bounds("editor-preview-pane").is_none());
        assert!(
            visual
                .debug_bounds("editor-preview-divider-anchor")
                .is_none()
        );
        assert_editor_preview_menu(&mut visual, &workspace);
        click_mode(&mut visual, &workspace, "editor-preview-icon-source");
        assert!(visual.debug_bounds("editor-preview-full").is_some());
        assert_eq!(visual.debug_bounds("status-file-path"), Some(status));
        assert!(
            visual
                .debug_bounds("editor-preview-divider-anchor")
                .is_none()
        );
        assert!(visual.debug_bounds("workspace-editor").is_none());
        click_mode(&mut visual, &workspace, "editor-preview-icon-full");
        assert_split_layout(&mut visual, 0.5);
        assert_eq!(visual.debug_bounds("status-file-path"), Some(status));
        drag_split(&mut visual, &workspace, 0.60);
        let source = visual.debug_bounds("workspace-editor").unwrap();
        let preview = visual.debug_bounds("editor-preview-pane").unwrap();
        assert!(source.right() <= preview.left());
        assert_eq!(source.top(), preview.top());

        settle(&mut visual, &workspace, |this, cx| {
            this.active_markdown_preview()
                .is_some_and(|reader| reader.read(cx).editor_preview_parsed(cx))
        });
        visual.simulate_event(ScrollEvent {
            position: preview.center(),
            delta: ScrollDelta::Lines(point(0., -12.)),
            ..Default::default()
        });
        let preview_position = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| {
                workspace
                    .read(cx)
                    .active_markdown_preview()
                    .unwrap()
                    .read(cx)
                    .session_position(cx)
            })
        };
        settle(&mut visual, &workspace, |this, cx| {
            this.active_markdown_preview()
                .and_then(|reader| reader.read(cx).session_position(cx))
                .is_some_and(|position| position.block > 0)
        });
        let old_position = preview_position(&mut visual).unwrap();
        let blank_published = Rc::new(Cell::new(false));
        let observation = visual.update(|_, cx| {
            let blank_published = blank_published.clone();
            cx.observe(&workspace, move |workspace, cx| {
                let preview = workspace.read(cx).active_markdown_preview();
                if preview.is_none_or(|reader| !reader.read(cx).editor_preview_parsed(cx)) {
                    blank_published.set(true);
                }
            })
        });
        edit(&mut visual, &draft_markdown);
        settle(&mut visual, &workspace, |this, cx| {
            this.active_markdown_preview().is_some_and(|reader| {
                let reader = reader.read(cx);
                reader.source_text().as_ref() == draft_markdown
                    && reader.editor_preview_parsed(cx)
                    && reader.session_position(cx).is_some()
            })
        });
        assert!(
            !blank_published.get(),
            "a preview was replaced before parsing finished"
        );
        drop(observation);
        assert_eq!(
            preview_position(&mut visual).unwrap().block,
            old_position.block
        );
        assert_eq!(fs::read_to_string(root.join(first))?, saved_markdown);

        open(&mut visual, &workspace, second);
        assert!(visual.debug_bounds("editor-preview-icon-split").is_some());
        assert_split_layout(&mut visual, 0.60);
        open(&mut visual, &workspace, first);
        click_mode(&mut visual, &workspace, "editor-preview-icon-split");
        assert!(visual.debug_bounds("editor-preview-icon-source").is_some());
        assert!(visual.debug_bounds("workspace-editor").is_some());
        assert!(visual.debug_bounds("editor-preview-full").is_none());
        open(&mut visual, &workspace, second);
        assert!(visual.debug_bounds("editor-preview-icon-split").is_some());
        assert_split_layout(&mut visual, 0.60);

        open(&mut visual, &workspace, svg);
        assert!(visual.debug_bounds("editor-preview-icon-source").is_some());
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.remove_tab(svg, window, cx);
                this.request_file(svg.to_owned(), FileOpen::ImageBrowse, window, cx);
            });
        });
        settle(&mut visual, &workspace, |this, _| {
            this.tabs.active() == Some(svg)
                && this.active_editor_preview_mode() == EditorPreviewMode::Full
        });
        assert!(visual.update(|_, cx| workspace.read(cx).active_file().is_none()));
        let button = visual.debug_bounds("editor-preview-icon-full").unwrap();
        move_to(
            &mut visual,
            point(button.center().x, button.bottom() + px(2.)),
        );
        move_to(&mut visual, button.center());
        let menu = preview_menu_after(&mut visual, Duration::ZERO).unwrap();
        visual.simulate_click(menu.center(), Modifiers::none());
        settle(&mut visual, &workspace, |this, _| {
            this.active_file().is_some_and(|file| file.path == svg)
        });
        assert_split_layout(&mut visual, 0.5);
        drag_split(&mut visual, &workspace, 0.90);
        let grip = visual
            .debug_bounds("editor-preview-divider-anchor")
            .unwrap()
            .center();
        visual.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: grip,
            click_count: 2,
            ..Default::default()
        });
        visual.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: grip,
            click_count: 2,
            ..Default::default()
        });
        settle(&mut visual, &workspace, |_, _| true);
        assert_split_layout(&mut visual, 0.5);
        drag_split(&mut visual, &workspace, 0.35);
        settle(&mut visual, &workspace, |this, _| {
            this.active_image_tab()
                .is_some_and(|(_, image)| image.has_image())
        });
        assert!(visual.debug_bounds("workspace-image-viewport").is_some());
        edit(&mut visual, "<svg");
        settle(&mut visual, &workspace, |this, _| {
            this.active_image_tab()
                .is_some_and(|(_, image)| image.has_svg_error())
        });
        assert!(visual.debug_bounds("editor-svg-preview-error").is_some());
        assert!(visual.debug_bounds("workspace-image-viewport").is_some());
        edit(&mut visual, &valid_svg.replace("#2864d6", "#ca4a56"));
        settle(&mut visual, &workspace, |this, _| {
            this.active_image_tab()
                .is_some_and(|(_, image)| image.has_image() && !image.has_svg_error())
        });
        click_mode(&mut visual, &workspace, "editor-preview-icon-split");
        click_mode(&mut visual, &workspace, "editor-preview-icon-source");
        assert!(visual.debug_bounds("editor-preview-full").is_some());
        assert!(
            visual
                .debug_bounds("editor-preview-divider-anchor")
                .is_none()
        );

        move_to(
            &mut visual,
            point(button.center().x, button.bottom() + px(2.)),
        );
        move_to(&mut visual, button.center());
        assert!(preview_menu_after(&mut visual, Duration::ZERO).is_some());
        open(&mut visual, &workspace, plain);
        assert!(preview_menu_after(&mut visual, Duration::ZERO).is_none());
        assert!(visual.debug_bounds("editor-preview-icon-source").is_none());
        assert!(visual.debug_bounds("editor-preview-icon-split").is_none());
        assert!(visual.debug_bounds("editor-preview-icon-full").is_none());
        visual.update(|_, cx| workspace.update(cx, |this, cx| this.flush_session(cx)))?;
        let fresh_store = Arc::new(Store::new(state)?);
        let session = fresh_store.session(&repo)?;
        assert_eq!(session.editor_previews[first], EditorPreviewMode::Source);
        assert_eq!(session.editor_previews[second], EditorPreviewMode::Split);
        assert_eq!(session.editor_previews[svg], EditorPreviewMode::Full);
        assert_eq!(
            fresh_store.editor_preview_preference("markdown")?,
            EditorPreviewMode::Source
        );
        assert_eq!(
            fresh_store.editor_preview_preference("svg")?,
            EditorPreviewMode::Full
        );

        let mut reopened = None;
        let (_, mut visual) = cx.add_window_view(|window, cx| {
            let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
            let view = cx.new(|cx| {
                Workspace::with_config(fresh_store, config, WorkspaceView::Editor, window, cx)
            });
            view.update(cx, |this, cx| {
                this.apply_repository(repo, session, None, None, window, cx);
            });
            reopened = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let reopened = reopened.unwrap();
        visual.simulate_resize(size(px(1120.), px(600.)));
        open(&mut visual, &reopened, second);
        assert_split_layout(&mut visual, 0.60);
        open(&mut visual, &reopened, svg);
        click_mode(&mut visual, &reopened, "editor-preview-icon-full");
        assert_split_layout(&mut visual, 0.35);
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn editor_preview_symbols_are_centered_at_toolbar_scales() -> Result<()> {
        let renderer = SvgRenderer::new(Arc::new(()));
        for mode in [
            EditorPreviewMode::Source,
            EditorPreviewMode::Split,
            EditorPreviewMode::Full,
        ] {
            let (art, _) = mode.icon();
            let svg = renderer.parse_svg(art)?;
            for side in [16, 20, 24, 32] {
                let extent = DevicePixels(side as i32);
                let image =
                    renderer.render_parsed(&svg, SvgSize::ExactSize(size(extent, extent)))?;
                let pixels = image.as_bytes(0).unwrap();
                let center = side as f32 / 2.;
                let mut ink = 0.;
                let mut offset = point(0., 0.);
                // Sample the center of the image to measure the symbol without the frame.
                let inset = side / 5;
                for y in inset..side - inset {
                    for x in inset..side - inset {
                        let alpha = f32::from(pixels[(y * side + x) * 4 + 3]);
                        ink += alpha;
                        offset.x += (x as f32 + 0.5 - center) * alpha;
                        offset.y += (y as f32 + 0.5 - center) * alpha;
                    }
                }
                assert!(ink > 0., "{mode:?} at {side}px has no visible symbol");
                let offset = point(offset.x / ink, offset.y / ink);
                assert!(
                    offset.x.abs() < 0.35 && offset.y.abs() < 0.35,
                    "{mode:?} at {side}px is offset by {offset:?}"
                );
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[::core::prelude::v1::test]
    fn editor_preview_paints_svg_and_split_divider_offscreen() -> Result<()> {
        use gpui_wgpu::WgpuHeadlessRenderer;

        let fixture = tempfile::tempdir()?;
        let path = fixture.path().join("asset.svg");
        fs::write(
            &path,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="128" height="96"><rect width="128" height="96" fill="#cc2878"/></svg>"##,
        )?;
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        store.save_editor_preview_preference("svg", EditorPreviewMode::Full)?;
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let mut app = HeadlessAppContext::with_platform(
            crate::test_support::text_system(true),
            Arc::new(gpui_kit_assets::AllAssets),
            || Ok(Some(Box::new(WgpuHeadlessRenderer::new()?))),
        );
        app.update(gpui_component::init);
        let mut workspace = None;
        let window = app.open_window(size(px(720.), px(480.)), |window, cx| {
            let entity = cx.new(|cx| {
                Workspace::with_window(
                    store,
                    config,
                    Some(WorkspaceView::Editor),
                    Some(path.clone()),
                    window,
                    cx,
                )
            });
            entity.update(cx, |this, cx| {
                this.request_file(path, FileOpen::Startup, window, cx);
            });
            workspace = Some(entity.clone());
            cx.new(|cx| gpui_component::Root::new(entity, window, cx))
        })?;
        let workspace = workspace.unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            app.advance_clock(Duration::from_millis(150));
            app.run_until_parked();
            let ready = app.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                let this = workspace.read(cx);
                this.active_editor_preview_mode() == EditorPreviewMode::Full
                    && this
                        .active_image_tab()
                        .is_some_and(|(_, image)| image.has_image())
            })?;
            if ready {
                break;
            }
            assert!(Instant::now() < deadline, "SVG preview did not render");
            std::thread::sleep(Duration::from_millis(10));
        }
        let screenshot = app.capture_screenshot(window.into())?;
        let center = screenshot.get_pixel(screenshot.width() / 2, screenshot.height() / 2);
        assert_eq!(*center, image::Rgba([204, 40, 120, 255]));

        app.update(|cx| {
            workspace.update(cx, |this, cx| {
                let active = this.tabs.active().unwrap().to_owned();
                this.set_editor_preview_mode(&active, EditorPreviewMode::Split);
                cx.notify();
            });
        });
        for (mode, line_color) in [
            (ThemeMode::Light, [228, 228, 223, 255]),
            (ThemeMode::Dark, [46, 47, 54, 255]),
        ] {
            app.update(|cx| apply_theme(mode, cx));
            app.run_until_parked();
            let line = app.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                let scale = window.scale_factor();
                window
                    .painted_quads()
                    .into_iter()
                    .find(|quad| {
                        quad.background.as_solid() == Some(cx.theme().border)
                            && quad.bounds.size.width.0 == scale
                            && quad.bounds.size.height.0 > 300. * scale
                    })
                    .expect("the divider paints a full-height theme border at rest")
                    .bounds
            })?;
            let screenshot = app.capture_screenshot(window.into())?;
            let x = line.center().x.0 as u32;
            for y in [line.top().0 + 10., line.center().y.0, line.bottom().0 - 10.] {
                assert_eq!(*screenshot.get_pixel(x, y as u32), image::Rgba(line_color));
            }
        }
        Ok(())
    }
}
