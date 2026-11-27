use std::sync::{
    atomic::Ordering,
    mpsc::{self, RecvTimeoutError},
};

use super::*;
use crate::ai::{DocumentTranslations, Snapshot};

const TRANSLATION_ICON: &[u8] = include_bytes!("icons/translation.svg");

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum TranslationView {
    Document,
    Editor,
}

impl TranslationView {
    fn enabled(self, config: &crate::config::Config) -> bool {
        match self {
            Self::Document => config.translation.document,
            Self::Editor => config.translation.editor,
        }
    }

    fn toggle(self, config: &mut crate::config::Config) {
        match self {
            Self::Document => config.translation.document = !config.translation.document,
            Self::Editor => config.translation.editor = !config.translation.editor,
        }
    }

    fn icon_size(self) -> Pixels {
        match self {
            Self::Document => px(19.),
            Self::Editor => px(14.),
        }
    }
}

#[derive(Default)]
pub(super) struct DocumentTranslation {
    active: Option<ActiveTranslation>,
}

#[derive(Clone, PartialEq)]
enum TranslationSource {
    Document(SharedString),
    Editor(Rope),
}

struct ActiveTranslation {
    view: TranslationView,
    path: PathBuf,
    source: TranslationSource,
    snapshot: Snapshot,
    translations: Arc<DocumentTranslations>,
    status: TranslationStatus,
    _task: Task<()>,
}

impl ActiveTranslation {
    fn matches(&self, view: TranslationView, path: &Path, source: &TranslationSource) -> bool {
        self.view == view
            && self.path == path
            && &self.source == source
            && self.snapshot.is_current()
    }
}

#[derive(PartialEq)]
enum TranslationStatus {
    Loading,
    Ready,
    Failed,
}

impl Workspace {
    pub(crate) fn document_translation_button(
        &self,
        workspace: Entity<Self>,
        view: TranslationView,
        path: &Path,
        colors: Palette,
    ) -> Div {
        let enabled = view.enabled(self.config.current());
        let available = self.ai.snapshot(false).is_some();
        let status = self
            .document_translation
            .active
            .as_ref()
            .filter(|active| active.view == view && active.path == path)
            .map(|active| &active.status);
        let label = match (enabled, available, status) {
            (false, false, _) => t!("docs.translation_unavailable"),
            (false, true, _) => t!("docs.translation_on"),
            (true, _, Some(TranslationStatus::Loading)) => t!("docs.translation_loading"),
            (true, _, Some(TranslationStatus::Failed)) => t!("docs.translation_failed"),
            (true, _, None) if view == TranslationView::Editor => t!("docs.translation_paused"),
            (true, _, _) => t!("docs.translation_off"),
        };
        let (id, size) = match view {
            TranslationView::Document => ("document-translation-toggle", px(28.)),
            TranslationView::Editor => ("status-translation-toggle", px(22.)),
        };
        let color = if enabled { colors.accent } else { colors.muted };
        let glyph = Icon::default()
            .data(TRANSLATION_ICON)
            .when(available || enabled, |glyph| glyph.text_color(color));
        div()
            .flex_shrink_0()
            .debug_selector(move || id.into())
            .child(
                icon_button(id, glyph, label)
                    .size(size)
                    .icon_size(view.icon_size())
                    .toggled(enabled)
                    .disabled(!available && !enabled)
                    .on_click(move |_, window, cx| {
                        workspace.update(cx, |this, cx| {
                            let mut config = this.config.current().clone();
                            view.toggle(&mut config);
                            if let Err(error) = this.commit_settings(config, window, cx) {
                                this.show_error(error, window, cx);
                            }
                        });
                    }),
            )
    }

    fn visible_translation_target(
        &self,
        cx: &App,
    ) -> Option<(TranslationView, Entity<DocReader>, TranslationSource)> {
        match self.view {
            WorkspaceView::Docs => {
                let reader = self.doc.as_ref()?;
                let doc = reader.read(cx);
                docs::is_markdown(&doc.path).then(|| {
                    (
                        TranslationView::Document,
                        reader.clone(),
                        TranslationSource::Document(doc.source_text()),
                    )
                })
            }
            WorkspaceView::Editor
                if !self.terminal_maximized()
                    && self.active_editor_preview_mode()
                        != crate::session::EditorPreviewMode::Source =>
            {
                let reader = self.active_markdown_preview()?;
                let file = self.files.get(&reader.read(cx).path)?;
                Some((
                    TranslationView::Editor,
                    reader.clone(),
                    TranslationSource::Editor(file.saved.clone()),
                ))
            }
            _ => None,
        }
    }

    pub(super) fn sync_document_translation(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some((view, reader, source)) = self.visible_translation_target(cx) else {
            self.document_translation.active = None;
            return;
        };
        let path = &reader.read(cx).path;
        let snapshot = view
            .enabled(self.config.current())
            .then(|| self.ai.snapshot(false))
            .flatten();
        let Some(snapshot) = snapshot else {
            self.document_translation.active = None;
            reader.update(cx, |reader, cx| reader.set_translation(None, cx));
            return;
        };
        let path = path.clone();
        let active = &mut self.document_translation.active;
        if !active
            .as_ref()
            .is_some_and(|active| active.matches(view, &path, &source))
        {
            let translations = active
                .take()
                .filter(|active| {
                    active.view == view && active.path == path && active.snapshot.is_current()
                })
                .map_or_else(Arc::default, |active| active.translations);
            let repo = self.repository.clone();
            let request = snapshot.clone();
            let saved = source.clone();
            let request_path = path.clone();
            let task = cx.spawn_in(window, async move |workspace, cx| {
                let (send, updates) = async_channel::unbounded();
                let work = cx.background_executor().spawn(request.translate_document(
                    repo,
                    request_path,
                    move || match saved {
                        TranslationSource::Document(text) => text.to_string(),
                        TranslationSource::Editor(text) => text.to_string(),
                    },
                    move |batch, cancelled| {
                        // Wait for the visible reader to accept each batch before
                        // the worker can send another request.
                        let (permit, wait) = mpsc::sync_channel(1);
                        if send.try_send((batch, permit)).is_err() {
                            return false;
                        }
                        while !cancelled.load(Ordering::Relaxed) {
                            match wait.recv_timeout(Duration::from_millis(50)) {
                                Ok(permit) => return permit,
                                Err(RecvTimeoutError::Disconnected) => break,
                                Err(RecvTimeoutError::Timeout) => {}
                            }
                        }
                        false
                    },
                ));
                let mut completed = HashSet::new();
                while let Ok((batch, permit)) = updates.recv().await {
                    let visible = workspace
                        .update_in(cx, |this, _, cx| {
                            let Some((view, reader, source)) = this.visible_translation_target(cx)
                            else {
                                return false;
                            };
                            let Some(active) = &mut this.document_translation.active else {
                                return false;
                            };
                            if !active.matches(view, &reader.read(cx).path, &source) {
                                return false;
                            }
                            if !batch.is_empty() {
                                completed.extend(batch.keys().cloned());
                                Arc::make_mut(&mut active.translations).extend(batch);
                                cx.notify();
                            }
                            true
                        })
                        .unwrap_or(false);
                    _ = permit.send(visible);
                    if !visible {
                        return;
                    }
                }
                let result = work.await;
                _ = workspace.update_in(cx, |this, _, cx| {
                    if let Some(active) = &mut this.document_translation.active {
                        if result.is_ok() && completed.len() < active.translations.len() {
                            Arc::make_mut(&mut active.translations)
                                .retain(|source, _| completed.contains(source));
                        }
                        active.status = if result.is_ok() {
                            TranslationStatus::Ready
                        } else {
                            TranslationStatus::Failed
                        };
                        cx.notify();
                    }
                });
            });
            *active = Some(ActiveTranslation {
                view,
                path,
                source,
                snapshot,
                translations,
                status: TranslationStatus::Loading,
                _task: task,
            });
        }
        let translations = active
            .as_ref()
            .map(|active| &active.translations)
            .filter(|translations| !translations.is_empty())
            .cloned();
        reader.update(cx, |reader, cx| reader.set_translation(translations, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{
        install_test_keyring, mock_endpoint, mock_endpoint_before_reply, save_provider,
    };
    use crate::session::EditorPreviewMode;
    use std::{sync::atomic::AtomicUsize, time::Instant};

    const INITIAL: &str = "Stable paragraph.\n\nInitial paragraph.\n";
    const REVISED: &str = "Stable paragraph.\n\nRevised paragraph.\n";
    const FIRST: &str = "document-translated-prose:0";
    const SECOND: &str = "document-translated-prose:19";
    const FILE: &str = "README.md";

    #[::core::prelude::v1::test]
    fn translation_icon_fits_both_toolbar_and_status_bar() -> Result<()> {
        let renderer = SvgRenderer::new(Arc::new(()));
        let wrap = include_bytes!("icons/wrap-on.svg");
        let measure = |source, size_px: Pixels| -> Result<_> {
            let side = (size_px.as_f32() * 2.) as usize;
            let svg = renderer.parse_svg(source)?;
            let extent = DevicePixels(side as i32);
            let image = renderer.render_parsed(&svg, SvgSize::ExactSize(size(extent, extent)))?;
            let mut ink = None::<Bounds<usize>>;
            for (index, pixel) in image.as_bytes(0).unwrap().chunks_exact(4).enumerate() {
                if pixel[3] >= 128 {
                    let dot = Bounds {
                        origin: point(index % side, index / side),
                        size: size(1, 1),
                    };
                    ink = Some(ink.map_or(dot, |bounds| bounds.union(&dot)));
                }
            }
            Ok(ink.expect("visible icon"))
        };
        let toolbar_size = TranslationView::Document.icon_size();
        let status_size = TranslationView::Editor.icon_size();
        assert_eq!(toolbar_size, px(19.));
        assert_eq!(status_size, px(14.));
        let toolbar_ink = measure(TRANSLATION_ICON, toolbar_size)?;
        let status_ink = measure(TRANSLATION_ICON, status_size)?;
        let wrap_ink = measure(wrap, status_size)?;
        assert!(status_ink.size.width.abs_diff(wrap_ink.size.width) <= 2);
        assert!(status_ink.size.height.abs_diff(wrap_ink.size.height) <= 2);
        for (ink, size_px) in [(toolbar_ink, toolbar_size), (status_ink, status_size)] {
            let side = (size_px.as_f32() * 2.) as usize;
            assert!((ink.left() + ink.right()).abs_diff(side) <= 2);
            assert!((ink.top() + ink.bottom()).abs_diff(side) <= 2);
        }
        Ok(())
    }

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
            assert!(
                Instant::now() < deadline,
                "document translation did not settle"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn test_app() -> TestAppContext {
        let app = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(cfg!(target_os = "linux")),
        );
        app.update(gpui_component::init);
        app
    }

    fn fixture<'a>(
        app: &'a mut TestAppContext,
        directory: &Path,
        base: &str,
        source: &str,
        view: WorkspaceView,
    ) -> Result<(Entity<Workspace>, &'a mut VisualTestContext)> {
        install_test_keyring();
        let root = directory.join("repo");
        fs::create_dir(&root)?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "-qb", "main"])
                .arg(&root)
                .status()?
                .success()
        );
        fs::write(root.join(FILE), source)?;
        let repo = Repository::open(&root)?;
        let store = Arc::new(Store::new(directory.join("state"))?);
        store.save_editor_preview_preference("markdown", EditorPreviewMode::Source)?;
        let config_path = directory.join("config.toml");
        let mut ai = save_provider(&config_path, &Default::default(), base, "model", "mock-key")?;
        let config = ConfigFile::load(Some(config_path));
        ai.auto_translate = false;
        let mut workspace = None;
        let (_, visual) = app.add_window_view(|window, cx| {
            let view = cx.new(|cx| Workspace::with_config(store, config, view, window, cx));
            workspace = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1100.), px(750.)));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.apply_repository(repo, Default::default(), None, None, window, cx);
                this.ai.configure(ai);
                if view == WorkspaceView::Editor {
                    this.open_file(FILE.into(), false, window, cx);
                }
            });
        });
        settle(visual, &workspace, |this, _| match view {
            WorkspaceView::Editor => this.active_file().is_some(),
            _ => this.doc.is_some(),
        });
        Ok((workspace, visual))
    }

    fn translation_ready(this: &Workspace) -> bool {
        this.document_translation
            .active
            .as_ref()
            .is_some_and(|active| active.status == TranslationStatus::Ready)
    }

    #[track_caller]
    fn click(visual: &mut VisualTestContext, selector: &'static str) {
        let bounds = visual.debug_bounds(selector).expect(selector);
        visual.simulate_click(bounds.center(), Modifiers::none());
    }

    #[::core::prelude::v1::test]
    fn document_translation_respects_the_preview_and_reuses_cached_paragraphs_after_save()
    -> Result<()> {
        let (base, server) = mock_endpoint(vec![
            (
                200,
                r#"{"choices":[{"message":{"content":"保留段落。\n\n原始段落。"},"finish_reason":"stop"}]}"#,
            ),
            (
                200,
                r#"{"choices":[{"message":{"content":"更新段落。"},"finish_reason":"stop"}]}"#,
            ),
        ]);
        let directory = tempfile::tempdir()?;
        let mut app = test_app();
        let (workspace, visual) = fixture(
            &mut app,
            directory.path(),
            &base,
            INITIAL,
            WorkspaceView::Editor,
        )?;

        assert!(visual.debug_bounds(FIRST).is_none());
        let wrap = visual.debug_bounds("status-editor-wrap").unwrap();
        let translation = visual.debug_bounds("status-translation-toggle").unwrap();
        let actions = visual.debug_bounds("status-file-actions").unwrap();
        let position = visual.debug_bounds("status-file-position: Ln 1:1").unwrap();
        assert_eq!(translation.size, size(px(22.), px(22.)));
        assert_eq!(translation.center().y, wrap.center().y);
        assert_eq!(actions, translation);
        assert!(actions.right() < position.left());
        assert!(position.right() < wrap.left());
        click(visual, "status-translation-toggle");
        settle(visual, &workspace, |this, _| {
            assert_eq!(this.active_editor_preview_mode(), EditorPreviewMode::Source);
            assert!(this.document_translation.active.is_none());
            TranslationView::Editor.enabled(this.config.current())
        });
        assert!(visual.debug_bounds("editor-preview-full").is_none());

        click(visual, "editor-preview-icon-source");
        settle(visual, &workspace, |this, _| translation_ready(this));
        assert!(visual.debug_bounds(FIRST).is_some());
        assert!(visual.debug_bounds(SECOND).is_some());
        click(visual, "editor-preview-icon-full");
        settle(visual, &workspace, |this, _| {
            this.active_editor_preview_mode() == EditorPreviewMode::Split
        });
        let divider = visual
            .debug_bounds("editor-preview-divider-anchor")
            .unwrap();

        visual.update(|window, cx| {
            let editor = workspace.read(cx).active_file().unwrap().editor.clone();
            editor.update(cx, |editor, cx| {
                editor.replace_all(REVISED.to_owned(), window, cx)
            });
        });
        settle(visual, &workspace, |this, cx| {
            this.active_markdown_preview()
                .is_some_and(|reader| reader.read(cx).source_text().as_str() == REVISED)
        });
        assert!(visual.debug_bounds(FIRST).is_some());
        assert!(visual.debug_bounds(SECOND).is_none());

        visual
            .update(|window, cx| workspace.update(cx, |this, cx| this.save(&SaveFile, window, cx)));
        settle(visual, &workspace, |this, _| {
            translation_ready(this)
                && this
                    .document_translation
                    .active
                    .as_ref()
                    .unwrap()
                    .translations
                    .values()
                    .any(|value| value.as_str() == "更新段落。")
        });
        assert!(visual.debug_bounds(SECOND).is_some());
        assert_eq!(
            visual.debug_bounds("editor-preview-divider-anchor"),
            Some(divider)
        );
        let requests = server.join().unwrap();
        assert_eq!(
            requests[0]["messages"][1]["content"],
            "Stable paragraph.\n\nInitial paragraph."
        );
        assert_eq!(requests[1]["messages"][1]["content"], "Revised paragraph.");

        click(visual, "status-translation-toggle");
        settle(visual, &workspace, |this, _| {
            this.document_translation.active.is_none()
        });
        assert!(visual.debug_bounds(FIRST).is_none());
        assert_eq!(
            visual.debug_bounds("editor-preview-divider-anchor"),
            Some(divider)
        );

        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.switch_view(WorkspaceView::Docs, window, cx);
            })
        });
        settle(visual, &workspace, |this, _| this.doc.is_some());
        assert!(visual.debug_bounds(FIRST).is_none());
        let search = visual.debug_bounds("document-find").unwrap();
        let translation = visual.debug_bounds("document-translation-toggle").unwrap();
        assert_eq!(translation.size, size(px(28.), px(28.)));
        assert_eq!(translation.center().y, search.center().y);
        assert!((px(0.)..px(8.)).contains(&(translation.left() - search.right())));
        click(visual, "document-translation-toggle");
        settle(visual, &workspace, |this, _| translation_ready(this));
        assert!(visual.debug_bounds(FIRST).is_some());
        assert!(visual.debug_bounds(SECOND).is_some());

        let reopened_config = ConfigFile::load(Some(directory.path().join("config.toml")));
        assert!(TranslationView::Document.enabled(reopened_config.current()));
        assert!(!TranslationView::Editor.enabled(reopened_config.current()));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.close_open_document(Path::new(FILE), window, cx);
                this.navigate_document(FILE.into(), None, window, cx);
            });
        });
        settle(visual, &workspace, |this, _| translation_ready(this));
        assert!(visual.debug_bounds(FIRST).is_some());
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn document_translation_renders_batches_and_ignores_a_reply_after_disable() -> Result<()> {
        let (resume, wait) = mpsc::channel();
        let second_request = Arc::new(AtomicUsize::new(0));
        let (base, server) = mock_endpoint_before_reply(
            vec![
                (
                    200,
                    r#"{"choices":[{"message":{"content":"第一段。"},"finish_reason":"stop"}]}"#,
                ),
                (
                    200,
                    r#"{"choices":[{"message":{"content":"第二段。"},"finish_reason":"stop"}]}"#,
                ),
            ],
            {
                let second_request = second_request.clone();
                move || {
                    if second_request.fetch_add(1, Ordering::Relaxed) == 1 {
                        wait.recv_timeout(Duration::from_secs(15)).unwrap();
                    }
                }
            },
        );
        let directory = tempfile::tempdir()?;
        let source = format!(
            "# Translation guide\n\n{}\n\n{}\n",
            "Second sentence ".repeat(375),
            "Third sentence ".repeat(400),
        );
        let mut app = test_app();
        let (workspace, visual) = fixture(
            &mut app,
            directory.path(),
            &base,
            &source,
            WorkspaceView::Docs,
        )?;
        assert!(visual.debug_bounds(FIRST).is_none());
        click(visual, "document-translation-toggle");
        settle(visual, &workspace, |this, _| {
            second_request.load(Ordering::Relaxed) >= 2
                && this
                    .document_translation
                    .active
                    .as_ref()
                    .is_some_and(|active| {
                        active.status == TranslationStatus::Loading
                            && active.translations.len() == 1
                    })
        });
        assert!(visual.debug_bounds(FIRST).is_some());

        click(visual, "document-translation-toggle");
        settle(visual, &workspace, |this, _| {
            this.document_translation.active.is_none()
        });
        assert!(visual.debug_bounds(FIRST).is_none());
        resume.send(()).unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        settle(visual, &workspace, |this, _| {
            this.document_translation.active.is_none()
        });
        assert!(visual.debug_bounds(FIRST).is_none());
        Ok(())
    }
}
