use std::{collections::HashSet, path::PathBuf, sync::Arc};

use gpui::{AppContext as _, Context, ExternalPaths, Window};
use gpui_component::{WindowExt as _, notification::Notification};
use rust_i18n::t;

use super::{FileBuffer, ServerProject, Workspace, WorkspaceView};
use crate::repository::Document;

impl Workspace {
    pub(super) fn accepts_file_drop(&self) -> bool {
        self.repository.is_some()
            && self.repository_ui.opening.is_none()
            && !self.repository_overlay_open()
            && !self.confirming_close
            && !self.locator.open
            && self.image_preview.is_none()
    }

    pub(super) fn drop_files(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.accepts_file_drop() || paths.paths().is_empty() {
            return;
        }
        let paths = paths.paths().to_vec();
        let repo = self.repository.as_ref().unwrap().clone();
        let store = self.store.clone();
        let loaded = self.files.keys().cloned().collect::<HashSet<_>>();
        let load = cx.background_spawn({
            let repo = repo.clone();
            async move {
                let mut seen = HashSet::new();
                paths
                    .into_iter()
                    .filter_map(|original| {
                        let result = repo.local_file_key(&original).and_then(|path| {
                            if !seen.insert(path.clone()) {
                                return Ok(None);
                            }
                            let buffer = if loaded.contains(&path) {
                                None
                            } else {
                                let document = Document::open(&repo, &path, &store)?;
                                let project = ServerProject::for_file(&repo, &path);
                                Some((FileBuffer::new(document), project))
                            };
                            Ok(Some((path, buffer)))
                        });
                        result.transpose().map(|result| {
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
                if !this.accepts_file_drop()
                    || !this
                        .repository
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &repo))
                {
                    return;
                }
                let mut opened = Vec::<PathBuf>::new();
                let mut errors = Vec::new();
                for result in files {
                    match result {
                        Ok((path, buffer)) => {
                            if !this.files.contains_key(&path) {
                                let Some((buffer, project)) = buffer else {
                                    continue;
                                };
                                this.install_file(&path, buffer, project, window, cx);
                            }
                            opened.push(path);
                        }
                        Err(error) => errors.push(error),
                    }
                }
                if let Some(active) = opened.last().cloned() {
                    this.switch_view(WorkspaceView::Editor, window, cx);
                    this.cancel_file_open();
                    this.restore_terminal_split();
                    this.capture_active_file_position(cx);
                    let previous = this.tabs.active().map(ToOwned::to_owned);
                    for path in opened {
                        this.tabs.open(path, false);
                    }
                    this.finish_tab_selection(previous.as_deref(), window, cx);
                    this.locator_visit(&active);
                    this.open_doc_from_tab(active, window, cx);
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::{config::ConfigFile, repository::Store, workspace::SaveFile};
    use anyhow::Result;
    use gpui::{FileDropEvent, TestAppContext, TestDispatcher, VisualTestContext, point, px, size};
    use gpui_component::Root;
    use std::{fs, process::Command};

    #[test]
    fn drops_local_files_into_an_open_repository() -> Result<()> {
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
        let log = notes.with_file_name("LOG");
        let binary = notes.with_file_name("binary");
        fs::write(&repo_file, "Repository file\n")?;
        fs::write(&notes, "# Notes\n")?;
        fs::write(&log, "Log\n")?;
        fs::write(&binary, [0, 255])?;
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let platform = gpui_platform::current_platform(true);
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            platform.text_system(),
        );
        cx.update(gpui_component::init);
        let mut workspace = None;
        let (_, visual) = cx.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1150.), px(760.)));
        let drop = |visual: &mut VisualTestContext, paths: &[PathBuf]| {
            visual.update(|window, cx| window.draw(cx).clear(cx));
            let position = point(px(600.), px(400.));
            visual.simulate_event(FileDropEvent::Entered {
                position,
                paths: ExternalPaths(paths.iter().cloned().collect()),
            });
            visual.simulate_event(FileDropEvent::Submit { position });
            visual.run_until_parked();
        };

        drop(visual, std::slice::from_ref(&notes));
        visual.update(|_, cx| assert!(workspace.read(cx).tabs.paths().is_empty()));

        let repo = crate::repository::Repository::open(&root)?;
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                this.apply_repository(repo, Default::default(), None, None, window, cx);
            });
        });
        drop(visual, &[repo_file, log.clone(), notes.clone(), binary]);
        let editor = visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                assert_eq!(
                    this.tabs.paths(),
                    &[PathBuf::from("existing.txt"), log.clone(), notes.clone()]
                );
                assert_eq!(this.view, WorkspaceView::Docs);
                assert_eq!(this.doc.as_ref().unwrap().read(cx).path, notes);
                assert!(this.rows.iter().all(|row| !row.entry.path.is_absolute()));
                this.open_document_source(notes.clone(), window, cx);
                assert_eq!(this.view, WorkspaceView::Editor);
                let editor = this.active_file().unwrap().editor.clone();
                editor.update(cx, |editor, cx| editor.select_all(window, cx));
                editor
            })
        });
        visual.update(|window, cx| window.draw(cx).clear(cx));
        visual.simulate_input("# Edited");
        visual.simulate_keystrokes("enter");

        drop(visual, std::slice::from_ref(&notes));
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                assert_eq!(this.tabs.paths().len(), 3);
                assert_eq!(this.view, WorkspaceView::Docs);
                assert!(
                    this.doc
                        .as_ref()
                        .unwrap()
                        .update(cx, |doc, cx| { doc.jump_to_fragment("edited", cx) })
                );
                this.open_document_source(notes.clone(), window, cx);
                assert_eq!(this.active_file().unwrap().editor, editor);
                assert_eq!(editor.read(cx).text().to_string(), "# Edited\n");
                this.save(&SaveFile, window, cx);
            });
        });
        visual.run_until_parked();
        assert_eq!(fs::read_to_string(notes)?, "# Edited\n");

        drop(visual, std::slice::from_ref(&log));
        visual.update(|window, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.view, WorkspaceView::Editor);
            let file = this.active_file().unwrap();
            assert_eq!(file.path, log);
            file.editor
                .clone()
                .update(cx, |editor, cx| editor.select_all(window, cx));
            window.draw(cx).clear(cx);
        });
        visual.simulate_input("Updated log");
        visual
            .update(|window, cx| workspace.update(cx, |this, cx| this.save(&SaveFile, window, cx)));
        visual.run_until_parked();
        assert_eq!(fs::read_to_string(log)?, "Updated log");
        Ok(())
    }
}
