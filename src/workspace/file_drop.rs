use gpui_component::notification::Notification;
use rust_i18n::t;

use super::*;

enum DroppedFile {
    Editor(Option<(FileBuffer, Option<ServerProject>)>),
    Document(Option<docs::PreparedDocument>),
}

impl Workspace {
    pub(super) fn accepts_file_drop(&self) -> bool {
        self.repository_ui.opening.is_none()
            && !self.repository_overlay_open()
            && !self.confirming_close
            && !self.locator.open
            && self.image_preview.is_none()
    }

    pub(super) fn file_drop_area(
        &self,
        area: Stateful<Div>,
        cx: &Context<Self>,
        colors: Palette,
    ) -> Stateful<Div> {
        area.debug_selector(|| "file-drop-content".into())
            .when(self.accepts_file_drop(), |area| {
                area.drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(colors.soft))
                    .on_drop(cx.listener(Self::drop_files))
            })
    }

    pub(super) fn local_document_text(&self, path: &Path) -> Option<Rope> {
        self.files
            .get(path)
            .filter(|_| path.is_absolute())
            .map(|file| {
                if self.standalone.is_some() {
                    file.saved.clone()
                } else {
                    file.committed.clone()
                }
            })
    }

    pub(super) fn prepare_document(
        repo: Option<&Repository>,
        path: &Path,
        store: &Store,
        local_text: Option<Rope>,
    ) -> Result<docs::PreparedDocument> {
        match (local_text, repo) {
            (Some(text), _) => docs::PreparedDocument::new(text.to_string()),
            (None, Some(repo)) if path.is_absolute() => Document::open(repo, path, store)
                .and_then(|document| docs::PreparedDocument::new(document.text)),
            (None, Some(repo)) => docs::load(repo, path),
            (None, None) => docs::load_local(path),
        }
    }

    fn drop_files(&mut self, paths: &ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        if !self.accepts_file_drop() || paths.paths().is_empty() {
            return;
        }
        let paths = paths.paths().to_vec();
        let repo = self.repository.clone();
        let welcome = repo.is_none() && self.standalone.is_none();
        let read_markdown = !welcome && self.view != WorkspaceView::Editor;
        let store = self.store.clone();
        let loaded = self.files.keys().cloned().collect::<HashSet<_>>();
        let texts = if read_markdown {
            loaded
                .iter()
                .filter_map(|path| {
                    self.local_document_text(path)
                        .map(|text| (path.clone(), text))
                })
                .collect::<HashMap<_, _>>()
        } else {
            HashMap::new()
        };
        let current_doc = self.doc.as_ref().map(|doc| doc.read(cx).path.clone());
        let load = cx.background_spawn({
            let repo = repo.clone();
            async move {
                let mut seen = HashSet::new();
                paths
                    .into_iter()
                    .filter_map(|original| {
                        let result = (|| {
                            let path = match &repo {
                                Some(repo) => repo.local_file_key(&original)?,
                                None => crate::repository::canonical_local_file(&original)?,
                            };
                            if !seen.insert(path.clone()) {
                                return Ok(None);
                            }
                            let file = if read_markdown && docs::is_markdown(&path) {
                                let prepared = if current_doc.as_ref() == Some(&path) {
                                    None
                                } else {
                                    Some(Self::prepare_document(
                                        repo.as_deref(),
                                        &path,
                                        &store,
                                        texts.get(&path).cloned(),
                                    )?)
                                };
                                DroppedFile::Document(prepared)
                            } else {
                                let buffer = if loaded.contains(&path) {
                                    None
                                } else {
                                    Some(match &repo {
                                        Some(repo) => {
                                            FileBuffer::new(Document::open(repo, &path, &store)?)
                                                .with_project(repo, &path)
                                        }
                                        None => (
                                            FileBuffer::new(Document::open_local(&path, &store)?),
                                            ServerProject::for_local_file(&path),
                                        ),
                                    })
                                };
                                DroppedFile::Editor(buffer)
                            };
                            Ok(Some((path, file)))
                        })();
                        result.transpose().map(|result: Result<_>| {
                            result.map_err(|error| {
                                format!("{} — {error:#}", crate::paths::display(&original))
                            })
                        })
                    })
                    .collect::<Vec<_>>()
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let files = load.await;
            _ = this.update_in(cx, move |this, window, cx| {
                let same_repository = match (&this.repository, &repo) {
                    (Some(current), Some(repo)) => Arc::ptr_eq(current, repo),
                    (None, None) => true,
                    _ => false,
                };
                if !same_repository || !this.accepts_file_drop() {
                    return;
                }
                let mut errors = Vec::new();
                let mut editor_paths = Vec::new();
                let mut document = None;
                let mut view = None;
                for result in files {
                    match result {
                        Ok((path, DroppedFile::Editor(_))) if welcome => {
                            if let Err(error) = Self::open_standalone_in_view(
                                this.store.clone(),
                                this.config.clone(),
                                path.clone(),
                                None,
                                Some(WorkspaceView::Editor),
                                cx,
                            ) {
                                errors.push(format!("{} — {error:#}", paths::display(&path)));
                            }
                        }
                        Ok((path, DroppedFile::Editor(buffer))) => {
                            if !this.files.contains_key(&path) {
                                let Some((buffer, project)) = buffer else {
                                    continue;
                                };
                                this.install_file(&path, buffer, project, window, cx);
                            }
                            editor_paths.push(path);
                            view = Some(WorkspaceView::Editor);
                        }
                        Ok((path, DroppedFile::Document(prepared))) => {
                            let entry = this.session.remember_document(&path);
                            if let Some(prepared) = prepared.as_ref() {
                                entry.title = prepared.title().map(str::to_owned);
                            }
                            this.session_revision += 1;
                            document = Some((path, prepared.map(|doc| Task::ready(Ok(doc)))));
                            view = Some(WorkspaceView::Docs);
                        }
                        Err(error) => errors.push(error),
                    }
                }
                if view == Some(WorkspaceView::Editor) {
                    this.switch_view(WorkspaceView::Editor, window, cx);
                    this.cancel_file_open();
                    this.restore_terminal_split();
                }
                if let Some(active) = editor_paths.last().cloned() {
                    this.capture_active_file_position(cx);
                    let previous = this.tabs.active().map(ToOwned::to_owned);
                    for path in editor_paths {
                        this.remember_editor_preview(&path);
                        this.tabs.open(path, false);
                    }
                    this.finish_tab_selection(previous.as_deref(), window, cx);
                    this.locator_visit(&active);
                }
                if let Some((path, prepared)) = document {
                    if view == Some(WorkspaceView::Docs) && this.view != WorkspaceView::Docs {
                        this.switch_view_to(
                            WorkspaceView::Docs,
                            Some((path, prepared)),
                            window,
                            cx,
                        );
                    } else {
                        this.open_doc_at(path, None, None, prepared, window, cx);
                    }
                }
                if !errors.is_empty() {
                    window.push_notification(
                        Notification::error(errors.join("\n"))
                            .title(t!("workspace.drop_failed"))
                            .autohide(false),
                        cx,
                    );
                }
            });
        })
        .detach();
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::{config::ConfigFile, repository::Store, workspace::SaveFile};
    use anyhow::Result;
    use gpui::{FileDropEvent, TestAppContext, TestDispatcher, VisualTestContext, px, size};
    use gpui_component::Root;
    use std::{fs, process::Command};

    fn drop_files(visual: &mut VisualTestContext, paths: &[PathBuf]) {
        visual.update(|window, cx| window.draw(cx).clear(cx));
        let position = visual.debug_bounds("file-drop-content").unwrap().center();
        visual.simulate_event(FileDropEvent::Entered {
            position,
            paths: ExternalPaths(paths.iter().cloned().collect()),
        });
        visual.simulate_event(FileDropEvent::Submit { position });
        visual.run_until_parked();
    }

    #[::core::prelude::v1::test]
    fn repository_content_routes_dropped_files_by_page() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir(&root)?;
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()?
                .success()
        );
        let repo_file = root.join("existing.txt");
        let notes = fixture.path().canonicalize()?.join("notes.md");
        let guide = notes.with_file_name("guide.md");
        let binary = notes.with_file_name("binary.md");
        fs::write(&repo_file, "Repository file\n")?;
        fs::write(&notes, "# Notes\n")?;
        fs::write(&guide, "# Guide\n")?;
        fs::write(&binary, [0, 255])?;
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
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
        visual.simulate_resize(size(px(1150.), px(760.)));

        drop_files(visual, std::slice::from_ref(&notes));
        visual.update(|_, cx| assert_eq!(cx.windows().len(), 2));

        let repo = Repository::open(&root)?;
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.apply_repository(repo, Default::default(), None, None, window, cx);
            });
        });
        drop_files(visual, &[repo_file.clone(), notes.clone(), binary.clone()]);
        let editor = visual.update(|window, cx| {
            let this = workspace.read(cx);
            assert_eq!(
                this.tabs.paths(),
                &[PathBuf::from("existing.txt"), notes.clone()]
            );
            assert_eq!(this.view, WorkspaceView::Editor);
            assert!(this.doc.is_none());
            assert_eq!(this.tabs.active(), Some(notes.as_path()));
            assert_eq!(window.notifications(cx).len(), 1);
            let editor = workspace.read(cx).active_file().unwrap().editor.clone();
            editor.update(cx, |editor, cx| editor.select_all(window, cx));
            window.draw(cx).clear(cx);
            editor
        });
        visual.simulate_input("# Edited");
        visual.simulate_keystrokes("enter");
        drop_files(visual, std::slice::from_ref(&notes));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                assert_eq!(this.active_file().unwrap().editor, editor);
                assert!(this.doc.is_none());
                assert_eq!(editor.read(cx).text().to_string(), "# Edited\n");
                this.switch_view(WorkspaceView::Git, window, cx);
            });
        });

        drop_files(visual, &[guide.clone(), notes.clone(), binary.clone()]);
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.view, WorkspaceView::Docs);
            assert_eq!(this.tabs.paths().len(), 2);
            assert!(!this.files.contains_key(&guide));
            assert_eq!(this.doc.as_ref().unwrap().read(cx).path, notes);
            assert_eq!(this.session.open_documents.as_ref().unwrap().len(), 2);
        });
        drop_files(visual, std::slice::from_ref(&guide));
        drop_files(visual, std::slice::from_ref(&binary));
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.view, WorkspaceView::Docs);
            assert_eq!(this.tabs.paths().len(), 2);
            assert_eq!(this.doc.as_ref().unwrap().read(cx).path, guide);
        });
        drop_files(visual, &[guide, repo_file]);
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.view, WorkspaceView::Editor);
            assert_eq!(this.tabs.active(), Some(Path::new("existing.txt")));
        });
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn standalone_drop_reuses_document_and_editor_controls() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().canonicalize()?;
        let notes = root.join("notes.md");
        let guide = root.join("guide.md");
        let text = root.join("log.txt");
        let extras =
            ["scratch-a.txt", "scratch-b.txt", "scratch-c.txt"].map(|name| root.join(name));
        fs::write(&notes, "# Notes\n")?;
        fs::write(&guide, "# Guide\n")?;
        fs::write(&text, "Log\n")?;
        for extra in &extras {
            fs::write(extra, "Another file\n")?;
        }
        let store = Arc::new(Store::new(root.join("state"))?);
        let config = ConfigFile::load(Some(root.join("config.toml")));
        let mut app = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        app.update(|cx| {
            gpui_component::init(cx);
            cx.bind_keys([KeyBinding::new(
                "ctrl-tab",
                NextTab,
                Some("WorkspaceEditor && !Terminal && !Locator"),
            )]);
        });
        app.update(|cx| Workspace::open_standalone(store, config, notes.clone(), None, cx))?;
        let handle = app.windows()[0];
        let workspace = app.update_window(handle, |root, _, cx| {
            root.downcast::<Root>()
                .unwrap()
                .read(cx)
                .view()
                .clone()
                .downcast::<Workspace>()
                .unwrap()
        })?;
        let mut visual = VisualTestContext::from_window(handle, &app);
        visual.simulate_resize(size(px(900.), px(650.)));
        visual.run_until_parked();
        let draw = |visual: &mut VisualTestContext| {
            visual.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
            });
        };
        draw(&mut visual);
        assert!(visual.debug_bounds("standalone-file-title").is_some());
        assert!(visual.debug_bounds("open-documents-trigger").is_none());
        assert!(visual.debug_bounds("copy-document-path").is_none());

        drop_files(&mut visual, std::slice::from_ref(&guide));
        draw(&mut visual);
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.view, WorkspaceView::Docs);
            assert_eq!(this.doc.as_ref().unwrap().read(cx).path, guide);
            assert!(this.tabs.paths().is_empty());
            assert_eq!(cx.windows().len(), 1);
        });
        let trigger = visual.debug_bounds("open-documents-trigger").unwrap();
        let title = visual.debug_bounds("standalone-file-title").unwrap();
        let caret = visual.debug_bounds("standalone-document-caret").unwrap();
        assert!(trigger.contains(&title.center()));
        assert!(trigger.contains(&caret.center()));
        assert!(caret.left() >= title.right());
        assert!(visual.debug_bounds("copy-document-path").is_none());

        let paints_hover_line = |visual: &mut VisualTestContext| {
            draw(visual);
            let line = visual
                .debug_bounds("standalone-document-hover-line")
                .unwrap();
            visual.update(|window, cx| {
                let accent = Palette::for_mode(cx.theme().mode).accent;
                let scale = window.scale_factor();
                window.painted_quads().iter().any(|quad| {
                    quad.background.as_solid() == Some(accent)
                        && quad.bounds.size.height.0 == 2. * scale
                        && quad.bounds.bottom().0 == (line.bottom().as_f32() * scale).round()
                })
            })
        };
        assert!(!paints_hover_line(&mut visual));
        visual.simulate_mouse_move(title.center(), None, Default::default());
        assert!(paints_hover_line(&mut visual));
        visual.simulate_mouse_move(
            point(trigger.right() + px(20.), title.center().y),
            None,
            Default::default(),
        );
        assert!(!paints_hover_line(&mut visual));

        visual.simulate_click(title.center(), Default::default());
        draw(&mut visual);
        visual.simulate_keystrokes("home enter");
        visual.run_until_parked();
        visual
            .update(|_, cx| assert_eq!(workspace.read(cx).session.document.as_ref(), Some(&notes)));
        draw(&mut visual);
        let caret = visual.debug_bounds("standalone-document-caret").unwrap();
        visual.simulate_click(caret.center(), Default::default());
        draw(&mut visual);
        let close_document = visual
            .debug_bounds("close-open-document-1")
            .unwrap()
            .center();
        visual.simulate_click(close_document, Default::default());
        draw(&mut visual);
        assert!(visual.debug_bounds("open-documents-trigger").is_none());

        drop_files(&mut visual, std::slice::from_ref(&text));
        draw(&mut visual);
        assert!(visual.debug_bounds("standalone-file-title").is_some());
        assert!(visual.debug_bounds("standalone-title-tabs").is_none());
        drop_files(&mut visual, std::slice::from_ref(&guide));
        draw(&mut visual);
        assert!(visual.debug_bounds("standalone-title-tabs").is_some());
        assert!(visual.debug_bounds("standalone-file-title").is_none());
        assert!(visual.debug_bounds("workspace-tab-bar").is_none());

        let lane = visual.debug_bounds("standalone-title-tabs").unwrap();
        let first_close = format!("close-tab-slot:{}", paths::display(&text));
        let first_close = visual.debug_bounds(first_close.leak()).unwrap();
        let last_close = format!("close-tab-slot:{}", paths::display(&guide));
        let last_close = visual.debug_bounds(last_close.leak()).unwrap();
        let gaps = [
            lane.left() - px(2.),
            first_close.right() + px(5.),
            (lane.right() + last_close.right()) / 2.,
        ];
        for x in gaps {
            visual.update(|window, cx| {
                let position = point(x, lane.center().y);
                let button = MouseButton::Left;
                let modifiers = Modifiers::default();
                let press = window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        button,
                        position,
                        modifiers,
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
                assert!(
                    press.propagate,
                    "empty title bar at {position:?} must accept a window press"
                );
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent {
                        button,
                        position,
                        modifiers,
                        click_count: 1,
                    }),
                    cx,
                );
            });
        }

        let select_tab = |visual: &mut VisualTestContext, path: &Path| {
            let selector = format!("select-tab:{}", paths::display(path));
            let tab = visual.debug_bounds(selector.leak()).unwrap();
            visual.simulate_click(tab.center(), Default::default());
            draw(visual);
        };
        select_tab(&mut visual, &text);
        visual.update(|_, cx| assert_eq!(workspace.read(cx).tabs.active(), Some(text.as_path())));
        select_tab(&mut visual, &guide);

        drop_files(&mut visual, &extras);
        visual.simulate_resize(size(px(420.), px(650.)));
        for _ in 0..3 {
            draw(&mut visual);
            visual.run_until_parked();
        }
        visual.update(|window, cx| {
            let this = workspace.read(cx);
            assert!(this.tab_viewport_width < window.viewport_size().width);
            assert!(
                this.tabs_overflow,
                "tab viewport: {}",
                this.tab_viewport_width
            );
            assert!(this.tab_scroll.offset().x < px(0.));
        });
        visual.simulate_keystrokes("ctrl-tab");
        for _ in 0..3 {
            draw(&mut visual);
            visual.run_until_parked();
        }
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.tabs.active(), Some(text.as_path()));
            assert_eq!(this.tab_scroll.offset().x, px(0.));
        });
        visual.simulate_resize(size(px(900.), px(650.)));
        draw(&mut visual);
        let close_tab = |visual: &mut VisualTestContext, path: &Path| {
            let selector = format!("close-tab-slot:{}", paths::display(path));
            let close = visual.debug_bounds(selector.leak()).unwrap();
            visual.simulate_click(close.center(), Default::default());
            draw(visual);
        };
        for extra in &extras {
            close_tab(&mut visual, extra);
        }
        select_tab(&mut visual, &guide);
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                assert_eq!(this.view, WorkspaceView::Editor);
                assert_eq!(this.tabs.paths(), &[text.clone(), guide.clone()]);
                assert_eq!(this.session.document.as_ref(), Some(&notes));
                assert_eq!(this.standalone_active_path(), Some(guide.as_path()));
                let editor = this.active_file().unwrap().editor.clone();
                editor.update(cx, |editor, cx| editor.set_value("# Updated\n", window, cx));
                this.editor_changed(editor, window, cx);
                this.save(&SaveFile, window, cx);
            });
        });
        visual.run_until_parked();
        assert_eq!(fs::read_to_string(&guide)?, "# Updated\n");

        close_tab(&mut visual, &guide);
        assert!(visual.debug_bounds("standalone-title-tabs").is_none());
        assert!(visual.debug_bounds("standalone-file-title").is_some());
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                assert_eq!(this.tabs.active(), Some(text.as_path()));
                let editor = this.active_file().unwrap().editor.clone();
                editor.update(cx, |editor, cx| editor.set_value("Unsaved", window, cx));
                this.editor_changed(editor, window, cx);
            });
        });
        drop_files(&mut visual, std::slice::from_ref(&guide));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                let editor = this.active_file().unwrap().editor.clone();
                editor.update(cx, |editor, cx| {
                    editor.set_value("Also unsaved", window, cx)
                });
                this.editor_changed(editor, window, cx);
                assert!(!this.request_window_close(window, cx));
                assert!(this.confirming_close);
                this.resolve_tab_close(&text, false, window, cx);
                assert!(this.confirming_close);
                assert_eq!(this.tabs.paths(), std::slice::from_ref(&guide));
                this.resolve_tab_close(&guide, false, window, cx);
            });
        });
        visual.run_until_parked();
        assert!(app.windows().is_empty());
        Ok(())
    }
}
