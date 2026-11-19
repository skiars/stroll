use super::*;
use crate::session::StandaloneSession;

impl Workspace {
    pub fn open_standalone(
        store: Arc<Store>,
        config: ConfigFile,
        path: PathBuf,
        fragment: Option<String>,
        cx: &mut App,
    ) -> Result<()> {
        Self::open_standalone_in_view(store, config, path, fragment, None, cx)
    }

    pub(super) fn open_standalone_in_view(
        store: Arc<Store>,
        config: ConfigFile,
        path: PathBuf,
        fragment: Option<String>,
        view: Option<WorkspaceView>,
        cx: &mut App,
    ) -> Result<()> {
        let path = path.canonicalize()?;
        anyhow::ensure!(path.is_file(), t!("storage.local_regular_file"));
        for handle in cx.windows() {
            let activated = handle
                .update(cx, |root, window, cx| {
                    let Some(workspace) = root
                        .downcast::<gpui_component::Root>()
                        .ok()
                        .and_then(|root| root.read(cx).view().clone().downcast::<Self>().ok())
                    else {
                        return false;
                    };
                    if workspace.read(cx).standalone.as_ref() != Some(&path) {
                        return false;
                    }
                    workspace.update(cx, |this, cx| {
                        this.activate_standalone(path.clone(), fragment.clone(), window, cx);
                        if view == Some(WorkspaceView::Editor) {
                            this.open_document_source(path.clone(), window, cx);
                        }
                    });
                    true
                })
                .unwrap_or(false);
            if activated {
                return Ok(());
            }
        }
        let session = match Store::read_session_file::<StandaloneSession>(
            &store.standalone_session_path(&path),
        ) {
            Ok(session) => session.unwrap_or_default(),
            Err(error) => {
                eprintln!("Could not load standalone session: {error:#}");
                StandaloneSession::default()
            }
        };
        let view = view.unwrap_or_else(|| {
            if docs::is_markdown(&path) {
                if fragment.is_none() && session.view == Some(WorkspaceView::Editor) {
                    WorkspaceView::Editor
                } else {
                    WorkspaceView::Docs
                }
            } else {
                WorkspaceView::Editor
            }
        });
        let prepared = (view == WorkspaceView::Docs).then(|| {
            let path = path.clone();
            cx.background_spawn(async move { docs::load_local(&path) })
        });
        let mut options = TitleBar::window_options();
        if let Some(titlebar) = options.titlebar.as_mut() {
            titlebar.title = Some(file_name(&path).to_owned().into());
        }
        options.app_id = Some("io.stroll.Stroll".to_owned());
        options.window_decorations = Some(WindowDecorations::Client);
        options.window_background = WindowBackgroundAppearance::Transparent;
        options.window_min_size = Some(size(px(400.), px(360.)));
        options.window_bounds = Some(standalone_window_bounds(
            &store,
            view,
            size(px(400.), px(360.)),
            cx,
        ));
        cx.open_window(options, move |window, cx| {
            let workspace = cx.new(|cx| {
                Self::with_window(store, config, Some(view), Some(path.clone()), window, cx)
            });
            let weak = workspace.downgrade();
            window.on_window_should_close(cx, move |window, cx| {
                weak.update(cx, |workspace, cx| {
                    workspace.request_window_close(window, cx)
                })
                .unwrap_or(true)
            });
            workspace.update(cx, |this, cx| {
                this.session.view = session.view;
                this.session.panels = session.panels;
                if let Some(position) = session.file_position {
                    this.session.files.insert(path.clone(), position);
                }
                if let Some(position) = session.document_position {
                    this.session.documents.insert(path.clone(), position);
                }
                this.restore_panel_preferences();
                match view {
                    WorkspaceView::Docs => {
                        this.open_doc_at(path, fragment, None, prepared, window, cx)
                    }
                    _ => this.request_file(path, FileOpen::Startup, window, cx),
                }
            });
            cx.new(|cx| gpui_component::Root::new(workspace, window, cx))
        })?;
        Ok(())
    }

    fn activate_standalone(
        &mut self,
        path: PathBuf,
        fragment: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.activate_window();
        if fragment.is_some() && docs::is_markdown(&path) {
            self.switch_view_to(WorkspaceView::Docs, Some((path.clone(), None)), window, cx);
            if self.view == WorkspaceView::Docs {
                self.open_doc(path, fragment, window, cx);
            }
        }
    }

    pub(super) fn open_local_window(
        &mut self,
        path: PathBuf,
        fragment: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(current) = &self.standalone
            && path.canonicalize().is_ok_and(|path| &path == current)
        {
            self.activate_standalone(current.clone(), fragment, window, cx);
            return;
        }
        if let Err(error) =
            Self::open_standalone(self.store.clone(), self.config.clone(), path, fragment, cx)
        {
            self.show_error(error, window, cx);
        }
    }

    pub(super) fn choose_local_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.repository_ui.menu_open = false;
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t!("workspace.open_local_file").into_owned().into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(result) = selection.await {
                _ = this.update_in(cx, |this, window, cx| match result {
                    Ok(Some(paths)) => {
                        if let Some(path) = paths.into_iter().next() {
                            this.open_chosen_local_file(path, window, cx);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => this.show_error(error, window, cx),
                });
            }
        })
        .detach();
        cx.notify();
    }

    pub(super) fn open_chosen_local_file(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(repo) = &self.repository
            && let Ok(key) = repo.local_file_key(&path)
            && (!key.is_absolute() || self.tabs.index(&key).is_some())
        {
            self.switch_view(WorkspaceView::Editor, window, cx);
            if self.view == WorkspaceView::Editor {
                if self.tabs.index(&key).is_some() {
                    self.activate_tab(&key, window, cx);
                } else {
                    self.open_file(key, false, window, cx);
                }
            }
            return;
        }
        self.open_local_window(path, None, window, cx);
    }

    pub(super) fn standalone_active_path(&self) -> Option<&Path> {
        if self.view == WorkspaceView::Docs {
            self.session.document.as_deref()
        } else {
            self.tabs
                .active()
                .or_else(|| self.loading_file.as_ref().map(|(path, _)| path.as_path()))
        }
    }

    pub(super) fn standalone_topbar(
        &self,
        window: &Window,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement {
        let path = self.standalone_active_path();
        let editing = self.view == WorkspaceView::Editor;
        let dirty = path.is_some_and(|path| self.tab_is_dirty(path));
        let show_tabs = editing && self.tabs.paths().len() > 1;
        let document_list_title = path.filter(|_| !editing && self.shows_document_list());
        let actions = titlebar_actions()
            // Keep Win32 hit testing from reaching the draggable title bar behind the buttons.
            .when(cfg!(target_os = "windows"), |actions| actions.occlude())
            .flex_shrink_0()
            .gap_2()
            .when(!editing, |bar| {
                bar.when_some(self.doc.as_ref(), |bar, doc| {
                    bar.child(doc.read(cx).toolbar_actions(doc, window, cx))
                })
            })
            .when(editing && path.is_some(), |bar| {
                bar.child(
                    icon_button("standalone-find", IconName::Search, t!("common.find"))
                        .on_click(cx.listener(|this, _, window, cx| this.find(window, cx))),
                )
                .child(
                    icon_button("standalone-save", IconName::Save, t!("common.save"))
                        .disabled(!dirty || path.is_some_and(|path| self.saving.contains(path)))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.save(&SaveFile, window, cx)),
                        ),
                )
            })
            .when(
                editing && (path.is_some_and(docs::is_markdown) || self.session.document.is_some()),
                |bar| {
                    bar.child(
                        icon_button(
                            "standalone-read",
                            Icon::default().path("icons/book-open.svg"),
                            t!("docs.read_markdown"),
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            let document = this
                                .tabs
                                .active()
                                .filter(|path| docs::is_markdown(path))
                                .map(|path| (path.to_owned(), None));
                            this.switch_view_to(WorkspaceView::Docs, document, window, cx);
                        })),
                    )
                },
            )
            .child(self.theme_button(cx));
        TitleBar::new()
            .h(px(36.))
            .bg(colors.bg)
            .border_0()
            .on_close_window(cx.listener(|this, _, window, cx| {
                if this.request_window_close(window, cx) {
                    window.remove_window();
                }
            }))
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .pl_1()
                    .pr_2()
                    .when(!show_tabs && document_list_title.is_none(), |bar| {
                        bar.when_some(path, |bar, path| {
                            bar.child(file_type_icon(
                                self.repository.as_deref(),
                                path,
                                colors,
                                15.,
                            ))
                        })
                        .child(
                            div()
                                .id("standalone-file-title")
                                .debug_selector(|| "standalone-file-title".into())
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .child(path.map(file_name).unwrap_or("Stroll").to_owned()),
                        )
                        .when(dirty, |bar| {
                            bar.child(div().size(px(6.)).rounded_full().bg(colors.warning))
                        })
                    })
                    .when_some(document_list_title, |bar, path| {
                        bar.child(self.titlebar_document_list(path, cx, colors))
                            .child(div().flex_1().min_w_0())
                    })
                    .when(show_tabs, |bar| {
                        bar.h_full().pl_0().child(self.titlebar_tabs(cx, colors))
                    })
                    .child(actions),
            )
    }
}

#[cfg(all(test, target_os = "linux"))]
#[::core::prelude::v1::test]
fn standalone_markdown_keeps_the_editor_buffer_and_reads_saved_text() -> Result<()> {
    use gpui::{TestAppContext, TestDispatcher, VisualTestContext};
    use gpui_component::Root;

    let fixture = tempfile::tempdir()?;
    let path = fixture.path().canonicalize()?.join("notes.md");
    fs::write(&path, "# Original\n")?;
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
    let platform = gpui_platform::current_platform(true);
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    app.update(gpui_component::init);
    app.update(|cx| {
        Workspace::open_standalone(
            store.clone(),
            config.clone(),
            path.clone(),
            Some("original".into()),
            cx,
        )
    })?;
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
    visual.simulate_resize(size(px(530.), px(440.)));
    visual.run_until_parked();
    visual.update(|window, cx| {
        workspace.update(cx, |this, cx| {
            assert!(this.repository.is_none());
            assert!(this.doc.is_some());
            this.open_document_source(path.clone(), window, cx);
        });
    });
    visual.run_until_parked();
    let editor = visual.update(|window, cx| {
        let editor = workspace.read(cx).active_file().unwrap().editor.clone();
        editor.update(cx, |editor, cx| editor.select_all(window, cx));
        window.draw(cx).clear(cx);
        editor
    });
    visual.simulate_input("# Edited");
    visual.simulate_keystrokes("enter");
    visual.update(|window, cx| {
        workspace.update(cx, |this, cx| {
            assert!(this.tab_is_dirty(&path));
            assert!(!this.request_window_close(window, cx));
            assert!(this.confirming_close);
            this.cancel_tab_close(window, cx);
            window.close_dialog(cx);

            this.switch_view(WorkspaceView::Docs, window, cx);
            let doc = this.doc.as_ref().unwrap();
            assert!(doc.update(cx, |doc, cx| doc.jump_to_fragment("original", cx)));
            assert!(!doc.update(cx, |doc, cx| doc.jump_to_fragment("edited", cx)));
            this.open_document_source(path.clone(), window, cx);
            assert_eq!(this.active_file().unwrap().editor, editor);
            this.save(&SaveFile, window, cx);
        });
    });
    visual.run_until_parked();
    assert_eq!(fs::read_to_string(&path)?, "# Edited\n");
    visual.update(|window, cx| {
        workspace.update(cx, |this, cx| {
            assert!(!this.tab_is_dirty(&path));
            this.switch_view(WorkspaceView::Docs, window, cx);
        });
    });
    visual.run_until_parked();
    visual.update(|_, cx| {
        let doc = workspace.read(cx).doc.clone().unwrap();
        assert!(doc.update(cx, |doc, cx| doc.jump_to_fragment("edited", cx)));
    });
    visual.run_until_parked();
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
        workspace.update(cx, |this, cx| {
            this.outline_visibility = Some(false);
            this.outline_width = px(235.);
            this.open_document_source(path.clone(), window, cx);
            this.active_file().unwrap().editor.update(cx, |editor, cx| {
                editor.set_selected_range(2..8, cx);
            });
        });
        window.draw(cx).clear(cx);
        assert!(workspace.update(cx, |this, cx| this.request_window_close(window, cx)));
        window.remove_window();
    });
    let saved: StandaloneSession =
        Store::read_session_file(&store.standalone_session_path(&path))?.unwrap();
    assert!(saved.document_position.is_some());
    app.update(|cx| Workspace::open_standalone(store, config, path.clone(), None, cx))?;
    app.run_until_parked();
    let reopened = app.windows()[0];
    app.update_window(reopened, |root, _, cx| {
        let root = root.downcast::<Root>().unwrap();
        let workspace = root
            .read(cx)
            .view()
            .clone()
            .downcast::<Workspace>()
            .unwrap();
        let view = workspace.read(cx);
        assert_eq!(view.view, WorkspaceView::Editor);
        assert_eq!(view.outline_visibility, Some(false));
        assert_eq!(view.outline_width, px(235.));
        let editor = &view.active_file().unwrap().editor;
        assert_eq!(editor.read(cx).selected_range(), 2..8);
    })?;
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
#[::core::prelude::v1::test]
fn standalone_text_opens_and_saves_without_a_repository() -> Result<()> {
    use gpui::{TestAppContext, TestDispatcher};
    use gpui_component::Root;

    let fixture = tempfile::tempdir()?;
    let path = fixture.path().canonicalize()?.join("notes.txt");
    fs::write(&path, "Original\r\n")?;
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    app.update(gpui_component::init);
    app.update(|cx| {
        Workspace::open_standalone(store.clone(), config.clone(), path.clone(), None, cx)
    })?;
    let handle = app.windows()[0];
    app.update(|cx| Workspace::open_standalone(store, config, path.clone(), None, cx))?;
    assert_eq!(app.windows(), [handle]);
    app.run_until_parked();
    app.update(|cx| assert_eq!(cx.active_window(), Some(handle)));
    let workspace = app.update_window(handle, |root, window, cx| {
        assert_eq!(window.viewport_size(), size(px(1280.), px(850.)));
        let root = root.downcast::<Root>().unwrap();
        let workspace = root
            .read(cx)
            .view()
            .clone()
            .downcast::<Workspace>()
            .unwrap();
        let this = workspace.read(cx);
        assert!(this.repository.is_none());
        assert_eq!(this.view, WorkspaceView::Editor);
        let file = this.active_file().unwrap();
        assert_eq!(file.path, path);
        assert_eq!(file.editor.read(cx).text().to_string(), "Original\n");
        file.editor
            .clone()
            .update(cx, |editor, cx| editor.select_all(window, cx));
        window.draw(cx).clear(cx);
        workspace
    })?;
    app.simulate_input(handle, "Updated");
    app.simulate_keystrokes(handle, "enter");
    app.update_window(handle, |_, window, cx| {
        workspace.update(cx, |this, cx| this.save(&SaveFile, window, cx));
    })?;
    app.run_until_parked();
    assert_eq!(fs::read_to_string(&path)?, "Updated\r\n");

    let other = fixture.path().join("other.txt");
    fs::write(&other, "Other\n")?;
    app.update_window(handle, |_, window, cx| {
        workspace.update(cx, |this, cx| {
            this.open_local_window(other.clone(), None, window, cx);
            this.open_local_window(other, None, window, cx);
        });
    })?;
    assert_eq!(app.windows().len(), 2);
    app.update(|cx| assert_ne!(cx.active_window(), Some(handle)));
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
#[::core::prelude::v1::test]
fn standalone_embedded_images_open_the_original_in_the_preview() -> Result<()> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use gpui::{TestAppContext, TestDispatcher, VisualTestContext};
    use image::{ImageFormat, Rgb, RgbImage};
    use std::io::Cursor;

    let fixture = tempfile::tempdir()?;
    let path = fixture.path().canonicalize()?.join("embedded.md");
    let mut png = Cursor::new(Vec::new());
    RgbImage::from_pixel(192, 128, Rgb([170, 30, 190])).write_to(&mut png, ImageFormat::Png)?;
    let uri = format!(
        "data:image/png;base64,{}",
        STANDARD.encode(png.into_inner())
    );
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    app.update(gpui_component::init);
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let config = ConfigFile::load(Some(fixture.path().join("config.toml")));

    for source in [
        format!("![Example]({uri})\n"),
        format!("<img src=\"{uri}\" width=\"96\" height=\"64\" alt=\"Example\">\n"),
    ] {
        fs::write(&path, source)?;
        app.update(|cx| {
            Workspace::open_standalone(store.clone(), config.clone(), path.clone(), None, cx)
        })?;
        let handle = *app.windows().last().unwrap();
        let workspace = app.update_window(handle, |root, _, cx| {
            root.downcast::<gpui_component::Root>()
                .unwrap()
                .read(cx)
                .view()
                .clone()
                .downcast::<Workspace>()
                .unwrap()
        })?;
        let mut visual = VisualTestContext::from_window(handle, &app);
        visual.simulate_resize(size(px(530.), px(440.)));
        for _ in 0..3 {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        }
        visual.simulate_click(point(px(54.), px(60.)), Default::default());
        visual.update(|_, cx| {
            let preview = workspace
                .read(cx)
                .image_preview
                .as_ref()
                .expect("click should open the image preview");
            let ImageSource::Image(original) = &preview.image else {
                panic!("expected the original embedded image");
            };
            assert_eq!(original.format(), gpui::ImageFormat::Png);
            assert_eq!(
                image::load_from_memory(original.bytes()).unwrap().width(),
                192
            );
        });
        visual.simulate_keystrokes("escape");
        visual.update(|window, cx| {
            assert!(workspace.read(cx).image_preview.is_none());
            window.remove_window();
        });
    }

    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
#[::core::prelude::v1::test]
fn standalone_editor_syncs_external_replacements_and_retains_draft_conflicts() -> Result<()> {
    use std::time::{Duration, Instant};

    use gpui::{TestAppContext, TestDispatcher, VisualTestContext};
    use gpui_component::Root;

    let fixture = tempfile::tempdir()?;
    let path = fixture.path().canonicalize()?.join("notes.txt");
    fs::write(&path, "Original\n")?;
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    app.update(gpui_component::init);
    let open = |app: &mut TestAppContext| -> Result<_> {
        app.update(|cx| {
            Workspace::open_standalone(store.clone(), config.clone(), path.clone(), None, cx)
        })?;
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
        Ok((workspace, VisualTestContext::from_window(handle, app)))
    };
    let settle = |visual: &mut VisualTestContext,
                  workspace: &Entity<Workspace>,
                  ready: &dyn Fn(&Workspace, &App) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            visual.executor().advance_clock(Duration::from_millis(30));
            visual.run_until_parked();
            if visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                ready(workspace.read(cx), cx)
            }) {
                break;
            }
            assert!(Instant::now() < deadline, "Timed out waiting for file sync");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let replace = |text: &str| -> Result<()> {
        let staged = path.with_extension("tmp");
        fs::write(&staged, text)?;
        fs::rename(staged, &path)?;
        Ok(())
    };
    let (workspace, mut visual) = open(&mut app)?;
    settle(&mut visual, &workspace, &|this, _| {
        this.repository_watch_active && this.active_file().is_some()
    });
    replace("External edit\n")?;
    settle(&mut visual, &workspace, &|this, cx| {
        this.active_file()
            .is_some_and(|file| file.editor.read(cx).text() == "External edit\n")
    });
    visual.simulate_input("Draft: ");
    replace("Different disk contents\n")?;
    let conflict = |this: &Workspace, cx: &App| {
        this.active_file().is_some_and(|file| {
            file.document.conflict && file.editor.read(cx).text() == "Draft: External edit\n"
        })
    };
    settle(&mut visual, &workspace, &conflict);
    assert_eq!(fs::read_to_string(&path)?, "Different disk contents\n");
    visual.update(|window, _| window.remove_window());

    let (workspace, mut visual) = open(&mut app)?;
    settle(&mut visual, &workspace, &|this, cx| {
        this.repository_watch_active && conflict(this, cx)
    });
    let revision = visual.update(|_, cx| workspace.read(cx).active_file().unwrap().sync_revision);
    replace("Different disk contents\n")?;
    settle(&mut visual, &workspace, &|this, _| {
        this.active_file()
            .is_some_and(|file| file.sync_revision > revision)
    });
    visual.run_until_parked();
    assert!(visual.update(|_, cx| conflict(workspace.read(cx), cx)));
    Ok(())
}
