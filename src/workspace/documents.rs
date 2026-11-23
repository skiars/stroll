use rust_i18n::t;

use std::path::{Path, PathBuf};

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, StyleSized as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    popover::Popover,
    scroll::popup_scrollbar,
    v_flex,
};
use gpui_kit_assets::IconName;

use crate::{paths, session::OpenDocument};

use super::{
    Palette, Workspace, compact_icon_button, file_name, file_type_icon, icon, quiet_button,
    titlebar_actions,
};

pub(super) struct CachedDocs {
    pub key: Vec<u8>,
    pub root: PathBuf,
    pub tree: std::rc::Rc<crate::docs::DocumentTree>,
    pub complete: bool,
    pub scroll: UniformListScrollHandle,
    pub reader: Option<Entity<crate::docs::DocReader>>,
}

impl Workspace {
    pub(super) fn cache_docs(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = &self.repository else {
            return;
        };
        if !self.docs_tree_seeded {
            return;
        }
        if let Some(reader) = &self.doc {
            reader.update(cx, |reader, cx| reader.release_online_images(cx));
        }
        let key = repo.checkout_key().to_vec();
        self.recent_docs.retain(|cached| cached.key != key);
        self.recent_docs.push_front(CachedDocs {
            key,
            root: repo.root.clone(),
            tree: self.docs_tree.clone(),
            complete: self.docs_tree_complete,
            scroll: if self.view == super::WorkspaceView::Docs {
                self.tree_scroll.clone()
            } else {
                UniformListScrollHandle::new()
            },
            reader: self.doc.clone(),
        });
        self.recent_docs.truncate(super::RECENT_REPOSITORY_LIMIT);
    }

    pub(super) fn initial_document(&self) -> Option<PathBuf> {
        if self.session.open_documents.is_some() {
            return None;
        }
        if !self.docs_tree_complete
            && let Some(repo) = self
                .repository_snapshot
                .as_ref()
                .or(self.repository.as_ref())
            && let Some(entries) = repo.children.get(Path::new(""))
            && let Some(readme) = entries.iter().find(|entry| {
                !entry.directory
                    && !entry.symlink
                    && crate::docs::is_markdown(&entry.path)
                    && crate::docs::is_root_readme(&entry.path)
            })
        {
            return Some(readme.path.clone());
        }
        self.docs_tree.initial.clone()
    }

    fn open_documents(&self) -> &[OpenDocument] {
        self.session.open_documents.as_deref().unwrap_or(&[])
    }

    fn close_open_document(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if self.repository_ui.opening.is_some() {
            return;
        }
        let active = self.session.document.as_deref() == Some(path);
        if active {
            self.capture_document_position(cx);
        }
        let Some(index) = self.session.close_document(path) else {
            return;
        };
        self.session_revision += 1;
        if active {
            self.pause_document_images(true, cx);
            self.doc = None;
            self.doc_task = Task::ready(());
            self.doc_loading = None;
            self.doc_error = None;
            if let Some(next) = self.session.document.clone() {
                self.open_doc(next, None, window, cx);
            }
        }
        if self.open_documents().is_empty() || !self.shows_document_list() {
            self.dismiss_document_list(window, cx);
        } else {
            self.select_document_row(index.min(self.open_documents().len() - 1), cx);
            self.document_list_focus.focus(window, cx);
        }
    }

    fn dismiss_document_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_documents_list = false;
        if self.shows_document_list() {
            self.document_list_trigger.focus(window, cx);
        } else {
            self.focus.focus(window, cx);
        }
        cx.notify();
    }

    fn focus_active_document_row(&mut self, cx: &mut Context<Self>) {
        let index = self
            .open_documents()
            .iter()
            .position(|document| Some(&document.path) == self.session.document.as_ref())
            .unwrap_or(0);
        self.select_document_row(index, cx);
    }

    fn select_document_row(&mut self, index: usize, cx: &mut Context<Self>) {
        self.document_list_selection = index;
        self.open_documents_scroll
            .scroll_to_item(index, ScrollStrategy::Center);
        cx.notify();
    }

    pub(super) fn document_toolbar(
        &self,
        window: &Window,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement {
        let path = self
            .session
            .document
            .as_ref()
            .map(|path| paths::display(path));
        let title = path
            .clone()
            .unwrap_or_else(|| t!("docs.open_documents").into_owned());
        let trigger = quiet_button("open-documents-trigger")
            .debug_selector(|| "open-documents-trigger".into())
            .track_focus(&self.document_list_trigger)
            .input_h(gpui_component::Size::default())
            .min_w_0()
            .px(px(6.))
            .content_gap(px(4.))
            .label_size(cx.theme().control_font_size)
            .icon_size(px(14.))
            .icon(icon(IconName::ChevronDown, colors.muted, 14.))
            .label(title.clone())
            .accessibility_label(format!("{} · {title}", t!("docs.open_documents")))
            .tooltip(title);

        h_flex()
            .h(super::FILE_TOOLBAR_HEIGHT)
            .flex_shrink_0()
            .px_2()
            .gap_1()
            .children(self.show_file_tree_button(cx))
            .child(
                h_flex()
                    .id("document-path-controls")
                    .min_w_0()
                    .gap(px(2.))
                    .child(self.document_list_popover(trigger, Anchor::TopLeft, cx, colors))
                    .when_some(path, |bar, path| {
                        bar.child(
                            compact_icon_button(
                                "copy-document-path",
                                IconName::Copy,
                                t!("docs.copy_path"),
                            )
                            .debug_selector(|| "copy-document-path".into())
                            .flex_shrink_0()
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                            }),
                        )
                    }),
            )
            .child(div().flex_1().min_w_0())
            .when_some(self.doc.as_ref(), |bar, reader| {
                bar.child(reader.read(cx).toolbar_actions(reader, window, cx))
            })
    }

    pub(super) fn shows_document_list(&self) -> bool {
        self.standalone.is_none() || self.open_documents().len() > 1
    }

    pub(super) fn titlebar_document_list(
        &self,
        path: &Path,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement {
        let title = file_name(path).to_owned();
        let trigger = Button::new("open-documents-trigger")
            .text()
            .debug_selector(|| "open-documents-trigger".into())
            .track_focus(&self.document_list_trigger)
            .h(px(28.))
            .min_w_0()
            .px_0()
            .content_gap(px(8.))
            .label_size(px(13.5))
            .icon(file_type_icon(
                self.repository.as_deref(),
                &self.config.file_associations,
                path,
                colors,
                15.,
            ))
            .icon_size(px(15.))
            .child(
                div()
                    .id("standalone-file-title")
                    .debug_selector(|| "standalone-file-title".into())
                    .min_w_0()
                    .truncate()
                    .text_color(colors.ink)
                    .child(title.clone()),
            )
            .child(
                div()
                    .id("standalone-document-caret")
                    .debug_selector(|| "standalone-document-caret".into())
                    .flex_shrink_0()
                    .child(icon(IconName::ChevronDown, colors.muted, 14.)),
            )
            .accessibility_label(format!("{} · {title}", t!("docs.open_documents")));
        titlebar_actions()
            .id("standalone-document-list")
            .relative()
            .group("standalone-document-list")
            .min_w_0()
            .when(cfg!(target_os = "windows"), |title| title.occlude())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(self.document_list_popover(trigger, Anchor::TopLeft, cx, colors))
            .child(
                div()
                    .id("standalone-document-hover-line")
                    .debug_selector(|| "standalone-document-hover-line".into())
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom(px(1.))
                    .h(px(2.))
                    .rounded(px(1.))
                    .group_hover("standalone-document-list", |style| style.bg(colors.accent)),
            )
    }

    fn document_list_popover(
        &self,
        trigger: Button,
        anchor: Anchor,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement {
        let workspace = cx.weak_entity();
        let list = Popover::new("open-documents")
            .dropdown_motion()
            .anchor(anchor)
            .scrollable(false)
            .offset(px(6.))
            .p_2()
            .min_w_0()
            .open(self.open_documents_list)
            .track_focus(&self.document_list_focus)
            .on_open_change(cx.listener(|this, &open, _, cx| {
                this.open_documents_list = open;
                if open {
                    this.focus_active_document_row(cx);
                } else {
                    cx.notify();
                }
            }))
            .trigger(trigger)
            .content(move |_, window, cx| {
                workspace
                    .read_with(cx, |this, _| {
                        this.document_list_content(workspace.clone(), window, colors)
                            .into_any_element()
                    })
                    .unwrap_or_else(|_| div().into_any_element())
            });

        div()
            .id("open-documents-control")
            .min_w_0()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "down" && this.document_list_trigger.is_focused(window) {
                    cx.stop_propagation();
                    this.open_documents_list = true;
                    this.focus_active_document_row(cx);
                }
            }))
            .child(list)
    }

    fn document_list_content(
        &self,
        workspace: WeakEntity<Self>,
        window: &Window,
        colors: Palette,
    ) -> impl IntoElement + use<> {
        let width = px(440.).min((window.viewport_size().width - px(32.)).max(px(0.)));
        let documents = self.open_documents();
        let length = documents.len();
        let height = (px(46.) * length as f32)
            .min(px(400.))
            .min(window.viewport_size().height * 0.55);
        let scroll = self.open_documents_scroll.clone();
        let keys_workspace = workspace.clone();
        let focus = self.document_list_focus.clone();
        let rows = uniform_list("open-documents-list", length, move |range, _, cx| {
            workspace
                .read_with(cx, |this, _| {
                    range
                        .filter_map(|index| {
                            let document = this.open_documents().get(index)?;
                            Some(this.document_list_row(document, index, workspace.clone(), colors))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .w_full()
        .h(height)
        .track_scroll(&scroll);

        v_flex()
            .id("open-documents-content")
            .aria_label(t!("docs.open_documents"))
            .track_focus(&focus)
            .w(width)
            .on_key_down(move |event, window, cx| {
                if !focus.is_focused(window) {
                    return;
                }
                _ = keys_workspace.update(cx, |this, cx| {
                    let count = this.open_documents().len();
                    if !this.open_documents_list || count == 0 {
                        return;
                    }
                    let index = this.document_list_selection;
                    match event.keystroke.key.as_str() {
                        "up" => this.select_document_row((index + count - 1) % count, cx),
                        "down" => this.select_document_row((index + 1) % count, cx),
                        "home" => this.select_document_row(0, cx),
                        "end" => this.select_document_row(count - 1, cx),
                        "enter" => {
                            let path = this.open_documents()[index].path.clone();
                            this.activate_open_document(path, window, cx);
                        }
                        _ => return,
                    }
                    cx.stop_propagation();
                });
            })
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_size(px(12.))
                    .text_color(colors.muted)
                    .child(t!("docs.open_documents")),
            )
            .when(length == 0, |list| {
                list.child(
                    div()
                        .px_2()
                        .py_2()
                        .text_size(px(12.))
                        .text_color(colors.muted)
                        .child(t!("docs.no_open_documents")),
                )
            })
            .when(length > 0, |list| {
                list.child(
                    div()
                        .relative()
                        .h(height)
                        .child(rows)
                        .child(div().absolute().inset_0().child(popup_scrollbar(&scroll))),
                )
            })
    }

    fn activate_open_document(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_document_list(window, cx);
        self.navigate_document(path, None, window, cx);
    }

    fn document_list_row(
        &self,
        document: &OpenDocument,
        index: usize,
        workspace: WeakEntity<Self>,
        colors: Palette,
    ) -> impl IntoElement + use<> {
        let path = document.path.clone();
        let active = self.session.document.as_ref() == Some(&path);
        let path_text = paths::display(&path);
        let title = document
            .title
            .as_deref()
            .unwrap_or_else(|| file_name(&path))
            .to_owned();
        h_flex()
            .id(("open-document", index))
            .w_full()
            .h(px(46.))
            .px_2()
            .gap(px(4.))
            .rounded(px(6.))
            .hover(|row| row.bg(colors.hover))
            .when(index == self.document_list_selection, |row| {
                row.bg(colors.hover)
            })
            .when(active, |row| row.bg(colors.accent.opacity(0.15)))
            .child(
                v_flex()
                    .id("select-open-document")
                    .role(Role::Button)
                    .aria_selected(active)
                    .aria_label(path_text.clone())
                    .tab_index(0)
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .cursor_pointer()
                    .child(
                        div()
                            .truncate()
                            .text_size(px(13.))
                            .when(active, |title| title.font_semibold())
                            .child(title),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(colors.muted)
                            .child(path_text.clone()),
                    )
                    .on_click({
                        let path = path.clone();
                        let workspace = workspace.clone();
                        move |_, window, cx| {
                            _ = workspace.update(cx, |this, cx| {
                                this.activate_open_document(path.clone(), window, cx);
                            });
                        }
                    }),
            )
            .child(
                compact_icon_button(
                    "close-open-document",
                    IconName::X,
                    t!("docs.close_document", path = path_text),
                )
                .debug_selector(move || format!("close-open-document-{index}"))
                .flex_shrink_0()
                .on_click(move |_, window, cx| {
                    _ = workspace.update(cx, |this, cx| {
                        this.close_open_document(&path, window, cx);
                    });
                }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::ConfigFile,
        repository::{Repository, Store},
        session::{WorkspaceView, WorktreeSession},
    };
    use core::prelude::v1::test;
    use gpui_component::Root;
    use std::{fs, process::Command, sync::Arc};

    #[test]
    fn markdown_link_context_menu_opens_links_and_images() -> anyhow::Result<()> {
        const WEB: &str = "https://example.test/screenshot";
        const GUIDE: &str = "./guide.md#details";
        const IMAGE: &str = "./shot.svg";

        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()?
                .success()
        );
        fs::write(
            root.join("README.md"),
            format!("[Guide]({GUIDE})\n\n[![Screenshot]({IMAGE})][site]\n\n[site]: {WEB}\n"),
        )?;
        fs::write(root.join("guide.md"), "# Guide\n\n## Details\n")?;
        fs::write(
            root.join("shot.svg"),
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="96" height="64"><rect width="96" height="64" fill="blue"/></svg>"#,
        )?;

        let mut app = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        app.update(|cx| {
            gpui_component::init(cx);
            cx.set_reduce_motion(true);
        });
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let repo = Repository::open(&root)?;
        let mut workspace = None;
        let (_, visual) = app.add_window_view(|window, cx| {
            let view =
                cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Docs, window, cx));
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(900.), px(620.)));
        visual.update(|window, cx| {
            window.activate_window();
            workspace.update(cx, |this, cx| {
                let session = WorktreeSession {
                    view: Some(WorkspaceView::Docs),
                    document: Some("README.md".into()),
                    ..Default::default()
                };
                this.apply_repository(repo, session, None, None, window, cx);
            });
        });

        use std::time::{Duration, Instant};

        let frame = crate::workspace::finish_scroll_navigation;
        let wait_for_doc = |visual: &mut VisualTestContext, path: &str| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                frame(visual);
                let (ready, history) = visual.update(|_, cx| {
                    let this = workspace.read(cx);
                    let ready = this.view == WorkspaceView::Docs
                        && this
                            .doc
                            .as_ref()
                            .is_some_and(|doc| doc.read(cx).path == Path::new(path));
                    (ready, this.session.source_history.clone())
                });
                if ready {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "Timed out opening {path}: {history:?}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let doc_path = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| {
                workspace
                    .read(cx)
                    .doc
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .path
                    .clone()
            })
        };
        let image_position = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| {
                workspace
                    .read(cx)
                    .doc
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .block_bounds(1, cx)
                    .center()
            })
        };
        let open_menu = |visual: &mut VisualTestContext, position| {
            visual.simulate_mouse_down(position, MouseButton::Right, Modifiers::none());
            visual.simulate_mouse_up(position, MouseButton::Right, Modifiers::none());
            frame(visual);
            assert!(visual.debug_bounds("popup-menu-surface").is_some());
        };
        let assert_clipboard = |visual: &VisualTestContext, expected: &str| {
            assert_eq!(
                visual.read_from_clipboard().and_then(|item| item.text()),
                Some(expected.to_owned())
            );
        };
        let back = |visual: &mut VisualTestContext, position| {
            let button = MouseButton::Navigate(NavigationDirection::Back);
            visual.simulate_mouse_down(position, button, Modifiers::none());
            visual.simulate_mouse_up(position, button, Modifiers::none());
            wait_for_doc(visual, "README.md");
        };

        wait_for_doc(visual, "README.md");
        let text_link = visual.update(|window, cx| {
            let doc = workspace.read(cx).doc.as_ref().unwrap().read(cx);
            let block = doc.block_bounds(0, cx);
            let underline = window
                .painted_underlines()
                .into_iter()
                .map(|line| line.bounds.map(|value| px(value.0 / window.scale_factor())))
                .find(|bounds| block.contains(&bounds.center()))
                .unwrap();
            point(underline.center().x, block.center().y)
        });
        open_menu(visual, text_link);
        assert_eq!(doc_path(visual), Path::new("README.md"));
        visual.simulate_keystrokes("down down enter");
        assert_clipboard(visual, GUIDE);
        frame(visual);
        open_menu(visual, text_link);
        visual.simulate_keystrokes("down enter");
        wait_for_doc(visual, "guide.md");
        visual.update(|_, cx| {
            let doc = workspace.read(cx).doc.as_ref().unwrap().read(cx);
            assert_eq!(
                doc.history_location(cx).fragment.as_deref(),
                Some("details")
            );
        });
        back(visual, text_link);

        let image = image_position(visual);
        visual.simulate_click(image, Modifiers::none());
        frame(visual);
        assert!(visual.debug_bounds("image-preview-close").is_some());
        assert!(visual.debug_bounds("popup-menu-surface").is_none());
        assert!(visual.opened_url().is_none());
        visual.simulate_keystrokes("escape");
        frame(visual);

        open_menu(visual, image);
        assert!(visual.debug_bounds("popup-menu-separator").is_some());
        assert!(visual.debug_bounds("image-preview-close").is_none());
        assert!(visual.opened_url().is_none());
        visual.simulate_keystrokes("down down enter");
        assert_clipboard(visual, WEB);
        frame(visual);

        open_menu(visual, image);
        visual.simulate_keystrokes("down down down down down enter");
        assert_clipboard(visual, IMAGE);
        frame(visual);

        open_menu(visual, image);
        visual.simulate_keystrokes("down enter");
        assert_eq!(visual.opened_url().as_deref(), Some(WEB));
        frame(visual);

        let focus = visual.update(|window, cx| window.focused(cx));
        open_menu(visual, image);
        visual.simulate_keystrokes("down down down enter");
        frame(visual);
        assert!(visual.debug_bounds("image-preview-close").is_some());
        visual.simulate_keystrokes("escape");
        frame(visual);
        assert_eq!(visual.update(|window, cx| window.focused(cx)), focus);

        open_menu(visual, image);
        visual.simulate_keystrokes("down down down down enter");
        frame(visual);
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.view, WorkspaceView::Editor);
            assert_eq!(
                this.active_image_tab().map(|(path, _)| path),
                Some(Path::new("shot.svg"))
            );
        });
        back(visual, image);
        visual.update(|_, cx| assert_eq!(workspace.read(cx).view, WorkspaceView::Docs));
        Ok(())
    }

    #[test]
    fn document_tree_publishes_in_stages_and_restores_cached_reader() -> anyhow::Result<()> {
        use crate::repository::Location;
        use std::time::{Duration, Instant};

        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        let other = fixture.path().join("other");
        for path in [&root, &other] {
            assert!(
                Command::new("git")
                    .args(["init", "-q"])
                    .arg(path)
                    .status()?
                    .success()
            );
            fs::write(
                path.join("README.md"),
                (0..50)
                    .map(|index| format!("# Section {index}\n\nParagraph {index}.\n\n"))
                    .collect::<String>(),
            )?;
        }
        fs::write(root.join(".gitignore"), "ignored/\n")?;
        fs::write(root.join("visible.md"), "# Visible")?;
        fs::create_dir_all(root.join("ignored/wiki/.git"))?;
        fs::write(root.join("ignored/wiki/hidden.md"), "# Hidden")?;
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        store.save_session(
            &Repository::open(&other)?,
            &WorktreeSession {
                view: Some(WorkspaceView::Docs),
                ..Default::default()
            },
        )?;
        let config = ConfigFile::load(Some(fixture.path().join("settings.toml")));
        let dispatcher = TestDispatcher::new(0);
        let mut cx = TestAppContext::build_with_text_system(
            dispatcher.clone(),
            None,
            crate::test_support::text_system(true),
        );
        cx.update(gpui_component::init);
        let mut workspace = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view =
                cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Docs, window, cx));
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1120.), px(760.)));

        let open = |visual: &mut VisualTestContext, root: PathBuf| {
            visual.update(|window, cx| {
                window.activate_window();
                workspace.update(cx, |this, cx| {
                    this.open_repository(Location { root, file: None }, window, cx);
                });
            });
        };
        let draw = |visual: &mut VisualTestContext| {
            visual.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
            });
        };
        let document_count = |this: &Workspace| {
            this.docs_tree
                .children
                .values()
                .flatten()
                .filter(|entry| !entry.directory)
                .count()
        };
        let start = Instant::now();
        open(visual, root.clone());
        let mut body = None;
        let mut partial = None;
        let mut complete = None;
        while body.is_none() || partial.is_none() || complete.is_none() {
            let ran = dispatcher.tick(false);
            let (has_reader, seed_count, done) = visual.update(|_, cx| {
                let this = workspace.read(cx);
                (
                    this.doc.is_some(),
                    document_count(this),
                    this.docs_tree_complete,
                )
            });
            if !ran
                || (has_reader && body.is_none())
                || (seed_count > 0 && partial.is_none())
                || (done && complete.is_none())
            {
                draw(visual);
            }
            if seed_count > 0 && partial.is_none() {
                assert!(
                    !done,
                    "the Git listing must draw before the directory walk finishes"
                );
                let readme_row = visual.debug_bounds("workspace-tree-row:README.md").unwrap();
                partial = Some(seed_count);
                visual.update(|window, cx| {
                    use gpui::InputEvent as _;
                    let position = readme_row.center();
                    window.dispatch_event(
                        MouseDownEvent {
                            position,
                            button: MouseButton::Left,
                            click_count: 1,
                            ..Default::default()
                        }
                        .to_platform_input(),
                        cx,
                    );
                    window.dispatch_event(
                        MouseUpEvent {
                            position,
                            button: MouseButton::Left,
                            click_count: 1,
                            ..Default::default()
                        }
                        .to_platform_input(),
                        cx,
                    );
                });
            }
            if has_reader && body.is_none() {
                assert!(visual.debug_bounds("open-documents-trigger").is_some());
                body = Some(());
            }
            if done && complete.is_none() {
                complete = Some(seed_count);
            }
            anyhow::ensure!(
                start.elapsed() < Duration::from_secs(20),
                "Docs did not finish"
            );
        }
        assert_eq!((partial, complete), (Some(2), Some(3)));

        let original_reader = visual.update(|_, cx| workspace.read(cx).doc.clone().unwrap());
        visual.update(|_, cx| {
            original_reader.update(cx, |reader, cx| {
                assert!(reader.jump_to_fragment("section-20", cx));
            })
        });
        crate::workspace::finish_scroll_navigation(visual);
        let position =
            visual.update(|_, cx| original_reader.read(cx).session_position(cx).unwrap());
        assert!(position.block > 0);
        // Drain background work before recording the switch-back interval.
        visual.run_until_parked();
        open(visual, other.clone());
        visual.run_until_parked();
        draw(visual);
        visual.run_until_parked();
        let other = other.canonicalize()?;
        visual.update(|_, cx| {
            assert_eq!(workspace.read(cx).repository.as_ref().unwrap().root, other)
        });

        let back = Instant::now();
        open(visual, root);
        loop {
            let ran = dispatcher.tick(false);
            let ready = visual.update(|_, cx| {
                let this = workspace.read(cx);
                this.doc.as_ref() == Some(&original_reader) && this.docs_tree_complete
            });
            if ready {
                draw(visual);
                assert!(
                    visual
                        .debug_bounds("workspace-tree-row:README.md")
                        .is_some()
                );
                visual.update(|_, cx| {
                    assert_eq!(
                        original_reader.read(cx).session_position(cx),
                        Some(position)
                    )
                });
                break;
            }
            if !ran {
                draw(visual);
            }
            anyhow::ensure!(
                back.elapsed() < Duration::from_secs(20),
                "cached Docs did not restore"
            );
        }
        Ok(())
    }

    #[test]
    fn document_list_switches_and_restores_by_worktree() -> anyhow::Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        let other = fixture.path().join("other");
        fs::create_dir(&root)?;
        let git = |args: &[&str]| -> anyhow::Result<()> {
            let output = Command::new("git").current_dir(&root).args(args).output()?;
            anyhow::ensure!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(())
        };
        git(&["init", "-q"])?;
        fs::write(
            root.join("README.md"),
            (0..50)
                .map(|i| format!("# Section {i}\n\nParagraph {i}.\n\n"))
                .collect::<String>(),
        )?;
        fs::write(root.join("notes.md"), "# Notes\n")?;
        git(&["add", "."])?;
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "Docs",
        ])?;
        git(&["worktree", "add", "-qb", "other", other.to_str().unwrap()])?;

        let repo = Repository::open(&root)?;
        let mut other_repo = Repository::open(&other)?;
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        store.bind_worktree(&mut other_repo)?;
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        cx.update(gpui_component::init);
        let mut workspace = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| {
                Workspace::with_config(store.clone(), config, WorkspaceView::Docs, window, cx)
            });
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        let readme = PathBuf::from("README.md");
        let notes = PathBuf::from("notes.md");
        visual.simulate_resize(size(px(1120.), px(760.)));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                let session = WorktreeSession {
                    document: Some(readme.clone()),
                    ..Default::default()
                };
                this.apply_repository(repo.clone(), session, None, None, window, cx);
            });
        });
        visual.run_until_parked();
        let draw = |visual: &mut VisualTestContext| {
            visual.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
            });
            visual.run_until_parked();
        };
        draw(visual);
        visual.update(|_, cx| {
            let reader = workspace.read(cx).doc.clone().unwrap();
            reader.update(cx, |reader, cx| {
                assert!(reader.jump_to_fragment("section-20", cx))
            });
        });
        crate::workspace::finish_scroll_navigation(visual);

        let position = visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                let reader = this.doc.clone();
                this.open_doc(readme.clone(), None, window, cx);
                assert_eq!(this.doc, reader);
                this.open_doc(notes.clone(), None, window, cx);
                let revision = this.session_revision;
                this.open_doc(notes.clone(), None, window, cx);
                assert_eq!(this.session_revision, revision);
                let position = this.session.documents[&readme];
                assert!(position.block > 0);
                assert_eq!(this.open_documents().len(), 2);
                position
            })
        });
        visual.run_until_parked();
        draw(visual);
        let trigger = visual.debug_bounds("open-documents-trigger").unwrap();
        assert_eq!(trigger.size.height, px(26.));
        let copy = visual.debug_bounds("copy-document-path").unwrap();
        assert!((copy.left() - trigger.right()).abs() < px(5.));
        visual.simulate_click(copy.center(), Modifiers::default());
        visual.update(|_, cx| {
            assert_eq!(
                cx.read_from_clipboard()
                    .and_then(|item| item.text())
                    .as_deref(),
                Some("notes.md")
            );
        });
        visual.update(|window, cx| {
            workspace
                .read(cx)
                .document_list_trigger
                .clone()
                .focus(window, cx);
        });
        draw(visual);
        visual.simulate_keystrokes("down");
        draw(visual);
        visual.update(|window, cx| {
            assert!(workspace.read(cx).open_documents_list);
            assert!(workspace.read(cx).document_list_focus.is_focused(window));
        });
        visual.simulate_keystrokes("up enter");
        visual.run_until_parked();
        draw(visual);
        draw(visual);
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert!(!this.open_documents_list);
            let reader = this.doc.as_ref().unwrap().read(cx);
            assert_eq!(reader.path, readme);
            assert_eq!(reader.session_position(cx), Some(position));
            let history = &this.session.source_history.entries;
            assert_eq!(history.len(), 2);
            assert_eq!(history[0].path.as_ref(), Some(&notes));
            assert_eq!(history[1].path.as_ref(), Some(&readme));
            assert!(history.iter().all(|location| location.document.is_some()));
        });

        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.flush_session(cx).unwrap();
                this.apply_repository(other_repo, Default::default(), None, None, window, cx);
            });
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                assert_eq!(this.open_documents().len(), 1);
                this.apply_repository(
                    repo.clone(),
                    store.session(&repo).unwrap(),
                    None,
                    None,
                    window,
                    cx,
                );
            });
        });
        visual.run_until_parked();
        draw(visual);
        draw(visual);
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.open_documents().len(), 2);
            assert_eq!(
                this.doc.as_ref().unwrap().read(cx).session_position(cx),
                Some(position)
            );
        });
        let trigger = visual.debug_bounds("open-documents-trigger").unwrap();
        visual.simulate_click(trigger.center(), Modifiers::default());
        draw(visual);
        let close = visual.debug_bounds("close-open-document-0").unwrap();
        visual.simulate_click(close.center(), Modifiers::default());
        draw(visual);
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert!(this.open_documents_list);
            assert_eq!(this.session.document.as_ref(), Some(&notes));
        });
        let close = visual.debug_bounds("close-open-document-0").unwrap();
        visual.simulate_click(close.center(), Modifiers::default());
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                assert!(this.session.document.is_none());
                this.flush_session(cx).unwrap();
                this.apply_repository(
                    repo.clone(),
                    store.session(&repo).unwrap(),
                    None,
                    None,
                    window,
                    cx,
                );
            });
        });
        visual.run_until_parked();
        visual.update(|_, cx| {
            let this = workspace.read(cx);
            assert!(this.open_documents().is_empty());
            assert!(this.doc.is_none());
            assert!(this.session.documents.contains_key(&readme));
        });
        Ok(())
    }
}
