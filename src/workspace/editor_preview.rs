use super::*;
use crate::session::EditorPreviewMode;
use gpui_base::InteractiveElementExt as _;
use std::borrow::Cow;

const DEFAULT_SPLIT_FRACTION: f32 = 0.5;
const MIN_SPLIT_FRACTION: f32 = 0.25;
const MAX_SPLIT_FRACTION: f32 = 0.75;
const DIVIDER_HIT_PADDING: Pixels = px(5.);

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
}

#[derive(Default)]
pub(super) struct EditorPreviews {
    markdown: HashMap<PathBuf, MarkdownPreview>,
    pub(super) svg_source_errors: HashMap<PathBuf, SharedString>,
    pub(super) split_drag: Option<(PreviewKind, f32)>,
}

struct MarkdownPreview {
    source: Option<Rope>,
    reader: Option<Result<Entity<docs::DocReader>, SharedString>>,
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
        let dark = cx.theme().mode == ThemeMode::Dark;
        let color: Hsla = match (mode, dark) {
            (EditorPreviewMode::Source, false) => rgb(0x1756a8),
            (EditorPreviewMode::Source, true) => rgb(0x8bb8ff),
            (EditorPreviewMode::Split, false) => rgb(0x9a4d08),
            (EditorPreviewMode::Split, true) => rgb(0xffb366),
            (EditorPreviewMode::Full, false) => rgb(0x713ab0),
            (EditorPreviewMode::Full, true) => rgb(0xcc9bff),
        }
        .into();
        let path = path.to_owned();
        let (art, selector) = mode.icon();
        let glyph = Icon::default().data(art).text_color(color);
        Some(
            icon_button("editor-preview-mode", glyph, mode.label())
                .icon_size(px(16.))
                .tooltip_placement(Placement::Bottom)
                .debug_selector(move || selector.into())
                .on_click(cx.listener(move |this, _, window, cx| {
                    if this.tabs.active() != Some(&path) {
                        return;
                    }
                    let current = this.editor_preview_mode(&path);
                    let next = current.next();
                    if next == EditorPreviewMode::Full {
                        this.capture_active_file_position(cx);
                        if let Some(file) = this.files.get(&path) {
                            file.editor
                                .update(cx, |editor, cx| editor.dismiss_lsp_overlays(cx));
                        }
                    }
                    this.set_editor_preview_mode(&path, next);
                    if let Err(error) = this
                        .store
                        .save_editor_preview_preference(kind.preference_key(), next)
                    {
                        this.show_error(error, window, cx);
                    }
                    if kind == PreviewKind::Svg
                        && next != EditorPreviewMode::Full
                        && !this.files.contains_key(&path)
                    {
                        this.request_file(
                            path.clone(),
                            FileOpen::Select {
                                preview: true,
                                position: None,
                            },
                            window,
                            cx,
                        );
                    }
                    cx.notify();
                })),
        )
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
        let existing = self.editor_previews.markdown.get_mut(&path);
        if existing
            .as_ref()
            .is_some_and(|preview| preview.source.as_ref() == Some(&source))
        {
            return;
        }
        let updating = existing.is_some();
        let reader = existing.and_then(|preview| preview.reader.take());
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
                if this
                    .files
                    .get(&key)
                    .is_none_or(|file| file.editor.read(cx).text() != &snapshot)
                {
                    preview.source = None;
                    cx.notify();
                    return;
                }
                let typography = this.config.current().docs;
                reader.update(cx, |reader, cx| {
                    reader.set_typography(typography, cx);
                    if let Some(Ok(previous)) = &preview.reader {
                        reader.inherit_editor_preview(previous, window, cx);
                    }
                });
                preview.reader = Some(Ok(reader));
                cx.notify();
            });
        });
        self.editor_previews.markdown.insert(
            path,
            MarkdownPreview {
                source: Some(source),
                reader,
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
        if mode == EditorPreviewMode::Full {
            return v_flex()
                .debug_selector(|| "editor-preview-full".into())
                .flex_1()
                .min_h_0()
                .min_w_0()
                .overflow_hidden()
                .child(preview)
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
                    .w(relative(fraction))
                    .flex_none()
                    .h_full()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .child(source),
            )
            .child(
                v_flex()
                    .debug_selector(|| "editor-preview-pane".into())
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .child(preview),
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
                assert_eq!(button.size, size(px(28.), px(28.)));
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
        let icon = visual.debug_bounds("editor-preview-icon-source").unwrap();
        let actions = visual.debug_bounds("workspace-actions-trigger").unwrap();
        assert!(icon.right() <= actions.left());
        assert!(visual.debug_bounds("editor-preview-pane").is_none());
        assert!(
            visual
                .debug_bounds("editor-preview-divider-anchor")
                .is_none()
        );
        click_mode(&mut visual, &workspace, "editor-preview-icon-source");
        assert!(visual.debug_bounds("editor-preview-full").is_some());
        assert!(
            visual
                .debug_bounds("editor-preview-divider-anchor")
                .is_none()
        );
        assert!(visual.debug_bounds("workspace-editor").is_none());
        click_mode(&mut visual, &workspace, "editor-preview-icon-full");
        assert_split_layout(&mut visual, 0.5);
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
        click_mode(&mut visual, &workspace, "editor-preview-icon-full");
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

        open(&mut visual, &workspace, plain);
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
