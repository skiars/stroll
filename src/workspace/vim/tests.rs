use std::{fs, path::PathBuf, process::Command, sync::Arc};

use gpui_component::Root;

use crate::{config::ConfigFile, repository::Store};

use super::*;

fn draw(visual: &mut VisualTestContext) {
    visual.run_until_parked();
    visual.update(|window, cx| {
        window.simulate_next_frame(cx);
        window.draw(cx).clear(cx);
    });
}

fn with_vim_workspace(
    contents: &str,
    test: impl FnOnce(&mut VisualTestContext, &Entity<Workspace>),
) -> anyhow::Result<()> {
    with_vim_repository(
        "a-long-file-name-for-vim-status-checks.txt",
        contents,
        &[],
        test,
    )
}

fn with_vim_repository(
    name: &str,
    contents: &str,
    files: &[(&str, &str)],
    test: impl FnOnce(&mut VisualTestContext, &Entity<Workspace>),
) -> anyhow::Result<()> {
    let fixture = tempfile::tempdir()?;
    let repository = fixture.path().join("repo");
    let config = fixture.path().join("config.toml");
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&repository)
            .status()?
            .success()
    );
    for (path, contents) in std::iter::once((name, contents)).chain(files.iter().copied()) {
        let path = repository.join(path);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, contents)?;
    }
    fs::write(
        &config,
        "[keyboard]\nvim_mode = true\n[lsp]\nenabled = false\n",
    )?;
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let config = ConfigFile::load(Some(config));
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        crate::test_support::text_system(true),
    );
    app.update(gpui_component::init);
    let mut workspace = None;
    let (_, visual) = app.add_window_view(|window, cx| {
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
        workspace = Some(view.clone());
        Root::new(view, window, cx)
    });
    let workspace = workspace.unwrap();
    visual.update(|window, cx| {
        workspace.update(cx, |this, cx| {
            this.open_repository(
                super::super::Location {
                    root: repository,
                    file: Some(PathBuf::from(name)),
                },
                window,
                cx,
            )
        });
    });
    draw(visual);
    test(visual, &workspace);
    Ok(())
}

#[::core::prelude::v1::test]
fn vim_k_requests_hover_at_the_caret_and_escape_preserves_visual_mode() -> anyhow::Result<()> {
    use gpui::Task;
    use gpui_component::input::{HoverDocumentation, HoverProvider, Rope};
    use lsp_types::{Hover, HoverContents, MarkedString};
    use std::{cell::RefCell, rc::Rc};

    struct Documentation(Rc<RefCell<Vec<usize>>>);

    impl HoverProvider for Documentation {
        fn hover(
            &self,
            _: &Rope,
            offset: usize,
            _: &mut Window,
            cx: &mut App,
        ) -> Task<anyhow::Result<Option<Hover>>> {
            self.0.borrow_mut().push(offset);
            let timer =
                (offset == 11).then(|| cx.background_executor().timer(Duration::from_millis(500)));
            cx.background_spawn(async move {
                if let Some(timer) = timer {
                    timer.await;
                }
                Ok((offset != 6).then(|| Hover {
                    contents: HoverContents::Scalar(MarkedString::String("Symbol docs".into())),
                    range: None,
                }))
            })
        }

        fn enrich_hover(
            &self,
            _: &Hover,
            _: usize,
            _: &mut Window,
            cx: &mut App,
        ) -> Task<Option<HoverDocumentation>> {
            let timer = cx.background_executor().timer(Duration::from_millis(500));
            cx.background_spawn(async move {
                timer.await;
                Some(HoverDocumentation {
                    plaintext: Some("Translated documentation".into()),
                    ..Default::default()
                })
            })
        }
    }

    with_vim_workspace("alpha beta slow\n", |visual, workspace| {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let editor = visual.update(|_, cx| {
            cx.set_reduce_motion(true);
            let editor = workspace.read(cx).active_file().unwrap().editor.clone();
            editor.update(cx, |editor, _| {
                editor.lsp_mut().hover_provider = Some(Rc::new(Documentation(calls.clone())));
            });
            editor
        });
        let hover = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| {
                editor
                    .read(cx)
                    .hover_popover()
                    .map(|hover| hover.symbol_range.clone())
            })
        };

        visual.simulate_keystrokes("shift-k");
        draw(visual);
        assert_eq!(hover(visual), Some(0..5));
        assert!(visual.debug_bounds("hover-markdown").is_some());
        visual.update(|window, cx| {
            assert_eq!(editor.read(cx).cursor(), 0);
            assert!(editor.read(cx).focus_handle(cx).is_focused(window));
        });

        visual.simulate_mouse_move(point(px(2.), px(2.)), None, Modifiers::default());
        visual.simulate_keystrokes("shift-k");
        draw(visual);
        assert_eq!(hover(visual), Some(0..5));
        assert_eq!(*calls.borrow(), [0]);

        visual.simulate_keystrokes("l");
        draw(visual);
        assert!(hover(visual).is_none());
        visual.simulate_keystrokes("4 l shift-k");
        draw(visual);
        assert!(hover(visual).is_none(), "whitespace should have no popup");
        assert_eq!(*calls.borrow(), [0]);

        visual.simulate_keystrokes("w shift-k");
        draw(visual);
        assert!(
            hover(visual).is_none(),
            "an empty response should have no popup"
        );
        visual.simulate_keystrokes("w shift-k");
        draw(visual);
        assert_eq!(*calls.borrow(), [0, 6, 11]);
        visual.simulate_keystrokes("escape");
        visual.executor().advance_clock(Duration::from_secs(1));
        draw(visual);
        assert!(hover(visual).is_none());

        visual.simulate_keystrokes("0 shift-k");
        draw(visual);
        assert_eq!(hover(visual), Some(0..5));
        visual.simulate_keystrokes("v");
        draw(visual);
        assert!(visual.debug_bounds("status-vim-mode: VISUAL").is_some());
        let selection = visual.update(|_, cx| editor.read(cx).selected_range());
        visual.simulate_keystrokes("escape");
        visual.executor().advance_clock(Duration::from_secs(1));
        draw(visual);
        assert!(hover(visual).is_none());
        assert!(visual.debug_bounds("status-vim-mode: VISUAL").is_some());
        assert_eq!(
            visual.update(|_, cx| editor.read(cx).selected_range()),
            selection
        );
        visual.simulate_keystrokes("escape");
        draw(visual);
        assert!(visual.debug_bounds("status-vim-mode: NORMAL").is_some());
    })
}

#[::core::prelude::v1::test]
fn vim_insert_escape_closes_hover_and_returns_to_normal() -> anyhow::Result<()> {
    use lsp_types::{CompletionItem, Hover, HoverContents, MarkedString};

    with_vim_workspace("symbol next\n", |visual, workspace| {
        visual.simulate_keystrokes("a");
        draw(visual);
        let editor =
            visual.update(|_, cx| workspace.read(cx).active_file().unwrap().editor.clone());
        visual.update(|_, cx| {
            editor.update(cx, |editor, cx| {
                editor.present_hover(
                    0..6,
                    Hover {
                        contents: HoverContents::Scalar(MarkedString::String("Symbol docs".into())),
                        range: None,
                    },
                    cx,
                );
                editor.present_completion_items(
                    0,
                    "s",
                    vec![CompletionItem::new_simple("symbol".into(), "Item".into())],
                    cx,
                );
            });
        });
        draw(visual);
        assert!(visual.debug_bounds("status-vim-mode: INSERT").is_some());
        visual.update(|_, cx| {
            assert!(editor.read(cx).has_language_hover());
            assert!(editor.read(cx).completion_menu_state().open);
        });

        visual.simulate_keystrokes("escape");
        draw(visual);
        assert!(visual.debug_bounds("status-vim-mode: INSERT").is_some());
        visual.update(|_, cx| {
            assert!(editor.read(cx).has_language_hover());
            assert!(!editor.read(cx).completion_menu_state().open);
        });

        visual.simulate_keystrokes("escape");
        draw(visual);
        assert!(visual.debug_bounds("status-vim-mode: NORMAL").is_some());
        visual.update(|_, cx| assert!(!editor.read(cx).has_language_hover()));

        visual.simulate_keystrokes("l");
        visual.update(|_, cx| {
            let editor = editor.read(cx);
            assert_eq!(editor.text().to_string(), "symbol next\n");
            assert_eq!(editor.cursor(), 1);
        });
    })
}

#[::core::prelude::v1::test]
fn vim_status_toggle_renders_open_corners_and_a_short_disabled_slash() -> anyhow::Result<()> {
    let renderer = SvgRenderer::new(Arc::new(()));
    let side = (STATUS_ICON_SIZE.as_f32() * 2.) as usize;
    let extent = DevicePixels(side as i32);
    let render = |art: &[u8]| -> anyhow::Result<_> {
        let svg = renderer.parse_svg(art)?;
        let image = renderer.render_parsed(&svg, SvgSize::ExactSize(size(extent, extent)))?;
        Ok(image.as_bytes(0).unwrap().to_vec())
    };
    let enabled = render(include_bytes!("../icons/vim-on.svg"))?;
    let disabled = render(include_bytes!("../icons/vim-off.svg"))?;
    let alpha = |image: &[u8], x, y| image[(y * side + x) * 4 + 3];
    for (x, y) in [(17, 3), (31, 17), (17, 31), (3, 17)] {
        assert!(alpha(&enabled, x, y) > 128);
        assert!(alpha(&disabled, x, y) > 128);
    }
    for (x, y) in [(6, 9), (28, 8), (26, 26), (9, 25), (17, 10)] {
        assert!(alpha(&enabled, x, y) < 16, "expected clearance at {x}, {y}");
    }
    for (x, y) in [(10, 11), (26, 11), (17, 26)] {
        assert!(alpha(&enabled, x, y) > 200, "expected V ink at {x}, {y}");
    }
    assert!(alpha(&enabled, 19, 28) < 128);
    for index in [4, 30] {
        assert_eq!(alpha(&enabled, index, index), 0);
        assert!(alpha(&disabled, index, index) > 128);
    }
    for index in [1, 33] {
        assert_eq!(alpha(&disabled, index, index), 0);
    }
    Ok(())
}

#[::core::prelude::v1::test]
fn vim_status_keeps_geometry_and_leader_does_not_take_focus() -> anyhow::Result<()> {
    with_vim_workspace(
        "one two three four five six seven eight\n",
        |visual, workspace| {
            let focus = visual.update(|window, cx| window.focused(cx).unwrap());
            for width in [500., 1080.] {
                visual.simulate_resize(size(px(width), px(480.)));
                draw(visual);
                let stable = |visual: &mut VisualTestContext| {
                    [
                        "status-file-path",
                        "status-file-position: Ln 1:1",
                        "status-file-controls",
                    ]
                    .map(|selector| visual.debug_bounds(selector).unwrap())
                };
                let geometry = stable(visual);
                let status = visual.debug_bounds("workspace-status").unwrap();
                let normal = visual.debug_bounds("status-vim-mode: NORMAL").unwrap();
                let icon = visual.debug_bounds("status-vim-icon-on").unwrap();
                assert_eq!(normal.left(), status.left());
                assert_eq!(normal.size.height, MODE_HEIGHT);
                assert_eq!(normal.center().y, status.center().y);
                assert_eq!(icon.left() - normal.right(), STATUS_ITEM_GAP);
                assert_eq!(icon.center().y, status.center().y);
                visual.update(|_, cx| assert_eq!(workspace.read(cx).vim.wide_status, width > 500.));
                for (keys, selector) in [
                    ("i", "status-vim-mode: INSERT"),
                    ("v", "status-vim-mode: VISUAL"),
                    ("shift-v", "status-vim-mode: V-LINE"),
                    ("ctrl-v", "status-vim-mode: V-BLOCK"),
                ] {
                    visual.simulate_keystrokes(keys);
                    draw(visual);
                    let mode = visual.debug_bounds(selector).unwrap();
                    assert_eq!(mode.left(), normal.left());
                    assert_eq!(mode.size.height, normal.size.height);
                    assert_eq!(
                        mode.size.width > normal.size.width,
                        selector == "status-vim-mode: V-BLOCK"
                    );
                    assert_eq!(stable(visual), geometry);
                    visual.simulate_keystrokes("escape");
                    draw(visual);
                }
                for key in ["2", "d", "3", "escape"] {
                    visual.simulate_keystrokes(key);
                    draw(visual);
                    assert_eq!(stable(visual), geometry, "after {key} at {width}");
                }
                visual.simulate_keystrokes("space");
                visual.simulate_event(KeyDownEvent {
                    keystroke: Keystroke::parse("space").unwrap(),
                    is_held: true,
                    prefer_character_input: false,
                });
                draw(visual);
                assert_eq!(stable(visual), geometry);
                assert!(visual.debug_bounds("status-vim-pending: ␣").is_some());
                assert!(visual.debug_bounds("vim-leader").is_none());
                visual.executor().advance_clock(HINT_DELAY);
                draw(visual);
                let leader = visual.debug_bounds("vim-leader").unwrap();
                assert_eq!(status.right() - leader.right(), px(12.));
                assert_eq!(status.top() - leader.bottom(), px(8.));
                visual.simulate_keystrokes("g");
                draw(visual);
                assert!(visual.debug_bounds("status-vim-pending: ␣ g").is_some());
                assert!(visual.debug_bounds("vim-leader").is_some());
                visual.simulate_keystrokes("escape");
                draw(visual);
                assert!(visual.debug_bounds("vim-leader").is_none());
                visual.update(|window, _| assert!(focus.is_focused(window)));
            }
            visual.simulate_keystrokes("space space");
            draw(visual);
            visual.update(|_, cx| assert!(workspace.read(cx).locator.open));
            assert!(visual.debug_bounds("vim-leader").is_none());
            visual.update(|_, cx| assert!(workspace.read(cx).vim.hint.is_none()));
            visual.simulate_keystrokes("escape v l");
            draw(visual);
            let selected = visual.update(|_, cx| {
                workspace
                    .read(cx)
                    .active_file()
                    .unwrap()
                    .editor
                    .read(cx)
                    .selected_range()
            });
            assert_eq!(selected, 0..2);
            visual.simulate_keystrokes("space escape");
            draw(visual);
            visual.update(|_, cx| {
                let file = workspace.read(cx).active_file().unwrap();
                assert_eq!(file.editor.read(cx).selected_range(), selected);
                assert!(file.vim.mode() == source::Mode::Visual);
            });
            let mode = visual.debug_bounds("status-vim-mode: VISUAL").unwrap();
            visual.update(|window, cx| workspace.read(cx).focus.clone().focus(window, cx));
            draw(visual);
            assert_eq!(visual.debug_bounds("status-vim-mode: VISUAL"), Some(mode));
            visual.update(|window, cx| focus.focus(window, cx));
            draw(visual);
            visual.simulate_keystrokes("escape c i w");
            visual.simulate_input("new");
            visual.simulate_keystrokes("escape");
            draw(visual);
            visual.update(|_, cx| {
                let file = workspace.read(cx).active_file().unwrap();
                assert_eq!(
                    file.editor.read(cx).value(),
                    "new two three four five six seven eight\n"
                );
            });
            visual.simulate_keystrokes("u");
            draw(visual);
            visual.update(|_, cx| {
                let file = workspace.read(cx).active_file().unwrap();
                assert_eq!(
                    file.editor.read(cx).value(),
                    "one two three four five six seven eight\n"
                );
            });
        },
    )
}
fn reset_editor(visual: &mut VisualTestContext, workspace: &Entity<Workspace>, value: &str) {
    visual.update(|window, cx| {
        let editor = workspace.read(cx).active_file().unwrap().editor.clone();
        workspace.update(cx, |workspace, cx| {
            let path = workspace.tabs.active().unwrap().to_path_buf();
            workspace.files.get_mut(&path).unwrap().vim = source::Source::default();
            cx.notify();
        });
        editor.update(cx, |editor, cx| {
            editor.set_value(value.to_owned(), window, cx);
            editor.set_modal_editing(true, cx);
        });
    });
    draw(visual);
}

fn assert_source(
    visual: &mut VisualTestContext,
    workspace: &Entity<Workspace>,
    keys: &str,
    selected_text: &[&str],
    cursor: (u32, u32),
    contents: &str,
) {
    visual.simulate_keystrokes(keys);
    draw(visual);
    visual.update(|_, cx| {
        let editor = workspace.read(cx).active_file().unwrap().editor.read(cx);
        let mut ranges = editor
            .selection_endpoints()
            .filter_map(|(a, b)| (a != b).then_some(a.min(b)..a.max(b)))
            .collect::<Vec<_>>();
        ranges.sort_unstable_by_key(|range| range.start);
        let actual = ranges
            .into_iter()
            .map(|range| editor.text().slice(range).to_string())
            .collect::<Vec<_>>();
        assert_eq!(actual, selected_text, "{keys} selection");
        let position = editor.cursor_position();
        assert_eq!((position.line, position.character), cursor, "{keys} caret");
        assert_eq!(editor.value().as_str(), contents, "{keys} edit");
    });
}

#[::core::prelude::v1::test]
fn vim_go_hint_uses_the_leader_layout_and_preserves_modal_commands() -> anyhow::Result<()> {
    let text = "one\n two\nthree\nfour\n";
    with_vim_repository(
        "source.rs",
        text,
        &[("other.rs", "other\n")],
        |visual, workspace| {
            let focus = visual.update(|window, cx| window.focused(cx).unwrap());
            visual.simulate_keystrokes("G g g");
            draw(visual);
            assert!(visual.debug_bounds("vim-go").is_none());
            visual.update(|_, cx| {
                assert_eq!(
                    workspace
                        .read(cx)
                        .active_file()
                        .unwrap()
                        .editor
                        .read(cx)
                        .cursor(),
                    0
                )
            });

            visual.simulate_keystrokes("v l g");
            visual.simulate_event(KeyDownEvent {
                keystroke: Keystroke::parse("g").unwrap(),
                is_held: true,
                prefer_character_input: false,
            });
            draw(visual);
            assert!(visual.debug_bounds("vim-go").is_none());
            visual.executor().advance_clock(HINT_DELAY);
            draw(visual);
            let menu = visual.debug_bounds("vim-go").unwrap();
            let status = visual.debug_bounds("workspace-status").unwrap();
            assert_eq!(status.right() - menu.right(), px(12.));
            assert_eq!(status.top() - menu.bottom(), px(8.));
            assert!(visual.debug_bounds("vim-go-key-g").is_some());
            assert!(visual.debug_bounds("vim-go-key-d").is_none());
            assert!(visual.debug_bounds("vim-go-key-r").is_none());
            assert!(visual.debug_bounds("vim-go-key-t").is_none());
            visual.update(|window, _| assert!(focus.is_focused(window)));
            visual.simulate_keystrokes("escape");
            draw(visual);
            assert!(visual.debug_bounds("vim-go").is_none());
            visual.update(|_, cx| {
                let this = workspace.read(cx);
                let file = this.active_file().unwrap();
                assert!(this.vim.hint.is_none());
                assert!(file.vim.mode() == source::Mode::Visual);
                assert_eq!(file.editor.read(cx).selected_range(), 0..2);
            });

            visual.simulate_keystrokes("escape g");
            visual.executor().advance_clock(HINT_DELAY);
            draw(visual);
            visual.simulate_keystrokes("c");
            draw(visual);
            assert!(visual.debug_bounds("vim-go-key-c").is_some());
            assert!(visual.debug_bounds("vim-go-key-g").is_none());
            assert!(visual.debug_bounds("status-vim-pending: g c").is_some());
            visual.simulate_keystrokes("c");
            draw(visual);
            assert!(visual.debug_bounds("vim-go").is_none());
            visual.update(|_, cx| {
                assert_eq!(
                    workspace
                        .read(cx)
                        .active_file()
                        .unwrap()
                        .editor
                        .read(cx)
                        .text(),
                    "// one\n two\nthree\nfour\n"
                );
            });

            visual.simulate_keystrokes("u 3 g g d g g");
            draw(visual);
            visual.update(|_, cx| {
                assert_eq!(
                    workspace
                        .read(cx)
                        .active_file()
                        .unwrap()
                        .editor
                        .read(cx)
                        .text(),
                    "four\n"
                );
            });
            visual.simulate_keystrokes("i g");
            visual.executor().advance_clock(HINT_DELAY);
            draw(visual);
            assert!(visual.debug_bounds("vim-go").is_none());
            visual.update(|_, cx| {
                assert_eq!(
                    workspace
                        .read(cx)
                        .active_file()
                        .unwrap()
                        .editor
                        .read(cx)
                        .text(),
                    "gfour\n"
                );
            });

            visual.simulate_keystrokes("escape g");
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    this.open_file("other.rs".into(), false, window, cx);
                });
            });
            visual.executor().advance_clock(HINT_DELAY);
            draw(visual);
            assert!(visual.debug_bounds("vim-go").is_none());
            for (keys, path) in [("g t", "source.rs"), ("g shift-t", "other.rs")] {
                visual.simulate_keystrokes(keys);
                draw(visual);
                visual.update(|_, cx| {
                    assert_eq!(
                        workspace.read(cx).tabs.active(),
                        Some(std::path::Path::new(path))
                    );
                });
            }
        },
    )
}

fn wait_for_preview_key_state(
    visual: &mut VisualTestContext,
    mut ready: impl FnMut(&mut Window, &mut App) -> bool,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        draw(visual);
        if visual.update(|window, cx| ready(window, cx)) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Preview keyboard state did not settle"
        );
        visual.executor().advance_clock(Duration::from_millis(25));
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn click_preview_control(visual: &mut VisualTestContext, selector: &'static str) {
    let button = visual.debug_bounds(selector).unwrap();
    visual.simulate_click(button.center(), Modifiers::none());
    draw(visual);
}

#[::core::prelude::v1::test]
fn editor_preview_tab_commands_follow_focus_and_preserve_find_input() -> anyhow::Result<()> {
    use crate::session::EditorPreviewMode;

    let document = format!(
        "# First hit\n\n{}## Last hit\n",
        "Read this.\n\n".repeat(50)
    );
    with_vim_repository(
        "guide.md",
        &document,
        &[("other.txt", "Other")],
        |visual, workspace| {
            visual.simulate_resize(size(px(1050.), px(550.)));
            visual.update(|window, cx| {
                cx.set_reduce_motion(true);
                workspace.update(cx, |this, cx| {
                    this.keep_tab(std::path::Path::new("guide.md"), cx);
                    this.open_file("other.txt".into(), false, window, cx);
                });
            });
            let on_tab = |visual: &mut VisualTestContext, name: &str| {
                wait_for_preview_key_state(visual, |_, cx| {
                    workspace.read(cx).tabs.active() == Some(std::path::Path::new(name))
                });
            };
            on_tab(visual, "other.txt");
            visual.simulate_keystrokes("shift-h");
            on_tab(visual, "guide.md");

            click_preview_control(visual, "editor-preview-icon-source");
            wait_for_preview_key_state(visual, |window, cx| {
                let this = workspace.read(cx);
                this.active_editor_preview_mode() == EditorPreviewMode::Full
                    && this.vim_reader(window, cx).is_some()
            });
            for (keys, name) in [
                ("shift-l", "other.txt"),
                ("shift-h", "guide.md"),
                ("g t", "other.txt"),
                ("g shift-t", "guide.md"),
            ] {
                visual.simulate_keystrokes(keys);
                on_tab(visual, name);
            }

            visual.simulate_keystrokes("/");
            draw(visual);
            assert!(visual.debug_bounds("find-query-field").is_some());
            visual.simulate_keystrokes("shift-h shift-l");
            draw(visual);
            assert!(visual.debug_bounds("find-query-field").is_some());
            on_tab(visual, "guide.md");
            visual.simulate_keystrokes("escape /");
            visual.simulate_input("hit");
            visual.simulate_keystrokes("enter");
            wait_for_preview_key_state(visual, |window, cx| {
                workspace
                    .read(cx)
                    .vim_reader(window, cx)
                    .is_some_and(|reader| reader.read(cx).session_position(cx).is_some())
            });
            let top = visual.update(|_, cx| {
                workspace
                    .read(cx)
                    .active_markdown_preview()
                    .unwrap()
                    .read(cx)
                    .session_position(cx)
                    .unwrap()
                    .block
            });
            visual.simulate_keystrokes("f3");
            wait_for_preview_key_state(visual, |_, cx| {
                workspace
                    .read(cx)
                    .active_markdown_preview()
                    .unwrap()
                    .read(cx)
                    .session_position(cx)
                    .is_some_and(|position| position.block > top)
            });
            visual.simulate_keystrokes("shift-f3");
            wait_for_preview_key_state(visual, |_, cx| {
                workspace
                    .read(cx)
                    .active_markdown_preview()
                    .unwrap()
                    .read(cx)
                    .session_position(cx)
                    .is_some_and(|position| position.block == top)
            });
            visual.simulate_keystrokes("escape");

            click_preview_control(visual, "editor-preview-icon-full");
            visual.update(|window, cx| {
                let this = workspace.read(cx);
                assert!(this.vim_source(window, cx).is_some());
            });
            click_preview_control(visual, "editor-preview-pane");
            visual.simulate_keystrokes("shift-l");
            on_tab(visual, "other.txt");
            visual.simulate_keystrokes("g shift-t");
            on_tab(visual, "guide.md");
            click_preview_control(visual, "editor-preview-pane");
            visual.simulate_keystrokes(": escape shift-x");
            on_tab(visual, "other.txt");
            visual.update(|_, cx| {
                assert!(
                    workspace
                        .read(cx)
                        .tabs
                        .index(std::path::Path::new("guide.md"))
                        .is_none()
                );
            });
        },
    )
}

#[::core::prelude::v1::test]
fn editor_preview_history_restores_full_reading_and_replays_source() -> anyhow::Result<()> {
    use crate::session::EditorPreviewMode;

    let document = (0..45)
        .map(|section| format!("## Section {section}\n\nParagraph {section}.\n\n"))
        .collect::<String>();
    with_vim_repository("guide.md", &document, &[], |visual, workspace| {
        visual.simulate_resize(size(px(1050.), px(500.)));
        visual.update(|_, cx| cx.set_reduce_motion(true));
        click_preview_control(visual, "editor-preview-icon-source");
        wait_for_preview_key_state(visual, |window, cx| {
            workspace.read(cx).vim_reader(window, cx).is_some()
        });
        visual.simulate_keystrokes("shift-g");
        wait_for_preview_key_state(visual, |window, cx| {
            workspace
                .read(cx)
                .editor_preview_history_location(window, cx)
                .and_then(|location| location.editor_preview)
                .and_then(|preview| preview.document.position)
                .is_some_and(|position| position.block > 5)
        });
        let position = visual.update(|window, cx| {
            workspace
                .read(cx)
                .editor_preview_history_location(window, cx)
                .unwrap()
                .editor_preview
                .unwrap()
                .document
                .position
                .unwrap()
        });

        visual.simulate_keystrokes("g f");
        draw(visual);
        let caret = visual.update(|window, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.active_editor_preview_mode(), EditorPreviewMode::Split);
            let caret = this
                .vim_source(window, cx)
                .unwrap()
                .editor
                .read(cx)
                .cursor();
            assert!(caret > 0);
            caret
        });
        visual.simulate_keystrokes("ctrl-o");
        wait_for_preview_key_state(visual, |window, cx| {
            let this = workspace.read(cx);
            this.active_editor_preview_mode() == EditorPreviewMode::Full
                && this
                    .editor_preview_history_location(window, cx)
                    .and_then(|location| location.editor_preview)
                    .and_then(|preview| preview.document.position)
                    .is_some_and(|restored| restored.line == position.line)
        });
        visual.simulate_keystrokes("ctrl-i");
        draw(visual);
        visual.update(|window, cx| {
            let this = workspace.read(cx);
            assert_eq!(this.active_editor_preview_mode(), EditorPreviewMode::Split);
            assert_eq!(
                this.vim_source(window, cx)
                    .unwrap()
                    .editor
                    .read(cx)
                    .cursor(),
                caret
            );
        });
    })
}

#[::core::prelude::v1::test]
fn editor_preview_file_commands_reuse_save_and_close_paths() -> anyhow::Result<()> {
    with_vim_repository(
        "guide.md",
        "# Guide\n",
        &[("other.txt", "Other")],
        |visual, workspace| {
            let path = visual.update(|_, cx| {
                workspace
                    .read(cx)
                    .repository
                    .as_ref()
                    .unwrap()
                    .root
                    .join("guide.md")
            });
            visual.simulate_keystrokes("0 i");
            visual.simulate_input("Draft ");
            visual.simulate_keystrokes("escape");
            click_preview_control(visual, "editor-preview-icon-source");
            wait_for_preview_key_state(visual, |window, cx| {
                workspace.read(cx).vim_reader(window, cx).is_some()
            });
            visual.simulate_keystrokes(": q enter");
            draw(visual);
            visual.update(|_, cx| assert!(workspace.read(cx).confirming_close));
            assert_eq!(fs::read_to_string(&path).unwrap(), "# Guide\n");
            visual.simulate_keystrokes("escape");
            wait_for_preview_key_state(visual, |window, cx| {
                !workspace.read(cx).confirming_close
                    && workspace.read(cx).vim_reader(window, cx).is_some()
            });
            visual.simulate_keystrokes(": w enter");
            wait_for_preview_key_state(visual, |_, cx| {
                !workspace
                    .read(cx)
                    .saving
                    .contains(std::path::Path::new("guide.md"))
                    && fs::read_to_string(&path).is_ok_and(|text| text == "Draft # Guide\n")
            });

            visual.simulate_keystrokes(": w q enter");
            wait_for_preview_key_state(visual, |_, cx| workspace.read(cx).tabs.active().is_none());
            assert_eq!(fs::read_to_string(path).unwrap(), "Draft # Guide\n");
        },
    )
}

#[::core::prelude::v1::test]
fn image_preview_has_tab_navigation_and_the_close_leader() -> anyhow::Result<()> {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="48" height="48"><rect width="48" height="48" fill="teal"/></svg>"#;
    with_vim_repository(
        "picture.svg",
        svg,
        &[("other.txt", "Other")],
        |visual, workspace| {
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    this.keep_tab(std::path::Path::new("picture.svg"), cx);
                    this.open_file("other.txt".into(), false, window, cx);
                });
            });
            wait_for_preview_key_state(visual, |_, cx| {
                workspace.read(cx).tabs.active() == Some(std::path::Path::new("other.txt"))
            });
            visual.simulate_keystrokes("shift-h");
            draw(visual);
            click_preview_control(visual, "editor-preview-icon-source");
            wait_for_preview_key_state(visual, |window, cx| {
                workspace.read(cx).vim_image_focused(window)
            });
            visual.simulate_keystrokes("g t");
            draw(visual);
            visual.update(|_, cx| {
                assert_eq!(
                    workspace.read(cx).tabs.active(),
                    Some(std::path::Path::new("other.txt"))
                );
            });
            visual.simulate_keystrokes("shift-h");
            draw(visual);
            visual.simulate_keystrokes("space b d");
            draw(visual);
            visual.update(|_, cx| {
                assert!(
                    workspace
                        .read(cx)
                        .tabs
                        .index(std::path::Path::new("picture.svg"))
                        .is_none()
                );
            });
        },
    )
}

#[::core::prelude::v1::test]
fn vim_go_from_docs_opens_source_at_the_reading_position() -> anyhow::Result<()> {
    use gpui_component::input::RopeExt as _;

    let document = (0..45)
        .map(|section| format!("## Section {section}\n\nParagraph {section}.\n\n"))
        .collect::<String>();
    with_vim_repository("guide.md", &document, &[], |visual, workspace| {
        visual.simulate_resize(size(px(1000.), px(460.)));
        visual.update(|_, cx| cx.set_reduce_motion(true));
        visual.simulate_keystrokes("space 1");
        draw(visual);
        visual.simulate_keystrokes("G g");
        visual.executor().advance_clock(HINT_DELAY);
        draw(visual);
        assert!(visual.debug_bounds("vim-go-key-g").is_some());
        assert!(visual.debug_bounds("vim-go-key-f").is_some());
        assert!(visual.debug_bounds("vim-go-key-r").is_none());
        let (origin, row) = visual.update(|_, cx| {
            let this = workspace.read(cx);
            let reader = this.doc.as_ref().unwrap().read(cx);
            let position = reader.session_position(cx).unwrap();
            let offset = position
                .line
                .map(|line| line.source_offset)
                .or(position.source_offset)
                .unwrap();
            let row = document.as_bytes()[..offset]
                .iter()
                .filter(|&&byte| byte == b'\n')
                .count();
            assert!(row > 0);
            (this.document_history_location(cx), row as u32)
        });
        visual.simulate_keystrokes("f");
        draw(visual);
        visual.update(|window, cx| {
            let this = workspace.read(cx);
            let file = this.vim_source(window, cx).unwrap();
            let editor = file.editor.read(cx);
            assert!(file.vim.mode() == source::Mode::Normal);
            assert_eq!(editor.text().offset_to_position(editor.cursor()).line, row);
        });
        visual.simulate_keystrokes("ctrl-o");
        draw(visual);
        visual.update(|window, cx| {
            let this = workspace.read(cx);
            assert!(this.vim_reader(window, cx).is_some());
            assert_eq!(this.document_history_location(cx), origin);
        });
        visual.simulate_keystrokes("ctrl-i");
        draw(visual);
        visual.update(|window, cx| {
            let file = workspace.read(cx).vim_source(window, cx).unwrap();
            let editor = file.editor.read(cx);
            assert_eq!(editor.text().offset_to_position(editor.cursor()).line, row);
        });
    })
}

#[::core::prelude::v1::test]
fn vim_find_keeps_match_count_visible_and_returns_focus() -> anyhow::Result<()> {
    with_vim_workspace("first hit\nsecond hit\nthird hit\n", |visual, workspace| {
        let editor =
            visual.update(|_, cx| workspace.read(cx).active_file().unwrap().editor.clone());
        let assert_match = |visual: &mut VisualTestContext, label, row| {
            draw(visual);
            assert!(visual.debug_bounds("find-query-field").is_some());
            visual.update(|window, cx| {
                let state = editor.read(cx);
                assert!(state.focus_handle(cx).is_focused(window));
                assert!(state.selected_range().is_empty());
                assert!(state.search_session().open);
                assert_eq!(state.search_session().matcher.label(), label);
                assert_eq!(state.cursor_position().line, row);
                assert!(
                    workspace.read(cx).active_file().unwrap().vim.mode() == source::Mode::Normal
                );
            });
        };
        visual.simulate_keystrokes("/");
        draw(visual);
        visual.simulate_input("hit");
        visual.simulate_keystrokes("enter");
        assert_match(visual, "1/3", 0);
        visual.simulate_keystrokes("2 n");
        assert_match(visual, "3/3", 2);
        visual.simulate_keystrokes("n");
        assert_match(visual, "1/3", 0);
        visual.simulate_keystrokes("shift-n");
        assert_match(visual, "3/3", 2);
        visual.simulate_keystrokes("?");
        draw(visual);
        visual.simulate_keystrokes("enter");
        assert_match(visual, "2/3", 1);
        visual.simulate_keystrokes("/");
        draw(visual);
        visual.simulate_input("missing");
        visual.simulate_keystrokes("escape");
        draw(visual);
        assert!(visual.debug_bounds("find-query-field").is_none());
        visual.update(|window, cx| {
            let state = editor.read(cx);
            assert!(state.focus_handle(cx).is_focused(window));
            assert_eq!(state.search_session().query, "hit");
            assert_eq!(state.cursor_position().line, 1);
        });
        visual.simulate_keystrokes("/");
        draw(visual);
        visual.simulate_keystrokes("enter escape");
        draw(visual);
        assert!(visual.debug_bounds("find-query-field").is_none());
        visual.update(|_, cx| assert_eq!(editor.read(cx).cursor_position().line, 2));
    })
}

#[::core::prelude::v1::test]
fn vim_word_search_uses_whole_words_and_the_source_history() -> anyhow::Result<()> {
    with_vim_workspace(
        "alpha_beta xalpha_beta
ALPHA_BETA alpha_beta2
alpha_beta
name $item
name $item
",
        |visual, workspace| {
            let check = |visual: &mut VisualTestContext, keys, row, col, query, label| {
                visual.simulate_keystrokes(keys);
                draw(visual);
                visual.update(|window, cx| {
                    let workspace = workspace.read(cx);
                    assert!(!workspace.repository_overlay_open());
                    let file = workspace.active_file().unwrap();
                    let editor = file.editor.read(cx);
                    assert!(editor.focus_handle(cx).is_focused(window));
                    assert!(file.vim.mode() == source::Mode::Normal);
                    assert!(editor.selected_range().is_empty());
                    let position = editor.cursor_position();
                    assert_eq!((position.line, position.character), (row, col));
                    assert_eq!(editor.search_session().query, query);
                    assert_eq!(editor.search_session().matcher.label(), label);
                });
            };
            check(visual, "l l #", 2, 0, "alpha_beta", "3/3");
            check(visual, "ctrl-o", 0, 2, "alpha_beta", "3/3");
            check(visual, "ctrl-i", 2, 0, "alpha_beta", "3/3");
            check(visual, "* n", 1, 0, "alpha_beta", "2/3");
            check(visual, "2 #", 2, 0, "ALPHA_BETA", "3/3");
            check(visual, "4 g g 6 l *", 4, 5, "$item", "2/2");
        },
    )
}

#[::core::prelude::v1::test]
fn vim_document_find_returns_focus_without_dismissing() -> anyhow::Result<()> {
    let document = (0..40)
        .map(|index| {
            if index == 12 || index == 28 {
                "target\n\n".to_owned()
            } else {
                format!("Paragraph {index}.\n\n")
            }
        })
        .collect::<String>();
    with_vim_repository("README.md", &document, &[], |visual, workspace| {
        visual.simulate_keystrokes("space 1");
        draw(visual);
        for _ in 0..40 {
            if visual.update(|_, cx| workspace.read(cx).doc.is_some()) {
                break;
            }
            visual.executor().advance_clock(Duration::from_millis(25));
            draw(visual);
        }
        let reader = visual.update(|_, cx| workspace.read(cx).doc.as_ref().unwrap().clone());
        visual.simulate_keystrokes("/");
        draw(visual);
        visual.simulate_input("target");
        visual.simulate_keystrokes("enter");
        draw(visual);
        assert!(visual.debug_bounds("find-query-field").is_some());
        visual.update(|window, cx| assert!(reader.read(cx).content_focused(window, cx)));
        crate::workspace::finish_scroll_navigation(visual);
        let first = visual.update(|_, cx| reader.read(cx).session_position(cx).unwrap());
        visual.simulate_keystrokes("n");
        crate::workspace::finish_scroll_navigation(visual);
        let next = visual.update(|_, cx| reader.read(cx).session_position(cx).unwrap());
        assert!(next.block > first.block);
        visual.simulate_keystrokes("/");
        draw(visual);
        visual.simulate_input("Paragraph 2.");
        visual.simulate_keystrokes("escape");
        draw(visual);
        assert!(visual.debug_bounds("find-query-field").is_none());
        visual.update(|window, cx| {
            assert!(reader.read(cx).content_focused(window, cx));
            assert_eq!(
                reader.read(cx).session_position(cx).unwrap().block,
                next.block
            );
        });
    })
}

#[::core::prelude::v1::test]
fn vim_yank_highlights_the_copied_range_until_its_own_timer_expires() -> anyhow::Result<()> {
    with_vim_workspace(
        "one
two
three
",
        |visual, workspace| {
            let ranges = |visual: &mut VisualTestContext| {
                visual.update(|_, cx| {
                    workspace
                        .read(cx)
                        .active_file()
                        .unwrap()
                        .vim
                        .yank_highlight_ranges(cx)
                })
            };
            visual.simulate_keystrokes("y");
            draw(visual);
            assert!(ranges(visual).is_empty());
            visual.simulate_keystrokes("y");
            draw(visual);
            assert_eq!(ranges(visual), vec![0..4]);

            visual.executor().advance_clock(Duration::from_millis(100));
            visual.simulate_keystrokes("j 2 y y");
            draw(visual);
            assert_eq!(ranges(visual), vec![4..14]);
            visual.executor().advance_clock(Duration::from_millis(100));
            draw(visual);
            assert_eq!(ranges(visual), vec![4..14]);
            visual.executor().advance_clock(Duration::from_millis(90));
            draw(visual);
            assert!(ranges(visual).is_empty());

            visual.simulate_keystrokes("v l y");
            draw(visual);
            assert_eq!(ranges(visual), vec![4..6]);
        },
    )
}

#[::core::prelude::v1::test]
fn vim_indent_supports_operators_visual_blocks_and_undo() -> anyhow::Result<()> {
    let text = "a\n b\nc\n\nd\n";
    with_vim_workspace(text, |visual, workspace| {
        let width = visual.update(|_, cx| {
            workspace
                .read(cx)
                .active_file()
                .unwrap()
                .editor
                .read(cx)
                .tab_width()
        });
        let indent = " ".repeat(width);
        let shifted = format!("{indent}a\n{indent} b\nc\n\nd\n");
        assert_source(visual, workspace, "2 > >", &[], (0, width as u32), &shifted);
        assert_source(visual, workspace, "u", &[], (0, 0), text);
        assert_source(visual, workspace, "> j", &[], (0, width as u32), &shifted);
        assert_source(
            visual,
            workspace,
            "< <",
            &[],
            (0, 0),
            &format!("a\n{indent} b\nc\n\nd\n"),
        );
        assert_source(visual, workspace, "j .", &[], (1, 1), text);
        assert_source(
            visual,
            workspace,
            "g g shift-v 2 j 2 >",
            &[],
            (0, (2 * width) as u32),
            &format!("{indent}{indent}a\n{indent}{indent} b\n{indent}{indent}c\n\nd\n"),
        );
        visual.simulate_keystrokes("u");
        draw(visual);
        visual.update(|_, cx| {
            assert_eq!(
                workspace
                    .read(cx)
                    .active_file()
                    .unwrap()
                    .editor
                    .read(cx)
                    .value(),
                text
            );
        });
        reset_editor(visual, workspace, "abcd\nab cd\na\n  ab\n");
        let shifted = format!("ab{indent}cd\nab{indent} cd\na\n  {indent}ab\n");
        assert_source(
            visual,
            workspace,
            "g g 0 2 l ctrl-v 3 j >",
            &[],
            (0, 2),
            &shifted,
        );
        assert_source(
            visual,
            workspace,
            "ctrl-v 3 j <",
            &[],
            (0, 2),
            "abcd\nab cd\na\n  ab\n",
        );
    })
}

#[::core::prelude::v1::test]
fn vim_visual_head_and_repeated_edits_follow_inclusive_ranges() -> anyhow::Result<()> {
    let text = "one two three four\n";
    with_vim_workspace(text, |visual, workspace| {
        assert_source(visual, workspace, "v e", &["one"], (0, 2), text);
        assert_source(visual, workspace, "o", &["one"], (0, 0), text);
        assert_source(visual, workspace, "x", &[], (0, 0), " two three four\n");
        assert_source(visual, workspace, "u", &[], (0, 0), text);
        assert_source(visual, workspace, "0 v w", &["one t"], (0, 4), text);
        assert!(
            visual
                .debug_bounds("status-file-position: Ln 1:5")
                .is_some()
        );
        assert_source(
            visual,
            workspace,
            "escape 0 shift-v",
            &["one two three four\n"],
            (0, 0),
            text,
        );
        visual.simulate_keystrokes("escape");
        visual
            .update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("PASTE".into())));
        assert_source(
            visual,
            workspace,
            "0 v e p",
            &[],
            (0, 4),
            "PASTE two three four\n",
        );
        reset_editor(visual, workspace, text);
        assert_source(
            visual,
            workspace,
            "v e d w .",
            &[],
            (0, 1),
            "  three four\n",
        );
        reset_editor(visual, workspace, text);
        assert_source(
            visual,
            workspace,
            "2 d i w",
            &[],
            (0, 0),
            "two three four\n",
        );
        reset_editor(visual, workspace, "one\ntwo\n");
        assert_source(visual, workspace, "d w", &[], (0, 0), "\ntwo\n");
        reset_editor(visual, workspace, text);
        visual.simulate_keystrokes("l l c w");
        visual.simulate_input("a");
        assert_source(
            visual,
            workspace,
            "escape",
            &[],
            (0, 2),
            "ona two three four\n",
        );
        reset_editor(visual, workspace, "  one\n  two\nthree\n");
        visual.simulate_keystrokes("c c");
        visual.simulate_input("a");
        assert_source(
            visual,
            workspace,
            "escape j .",
            &[],
            (1, 2),
            "  a\n  a\nthree\n",
        );
        reset_editor(visual, workspace, text);
        visual.simulate_keystrokes("2 i");
        visual.simulate_input("x");
        assert_source(
            visual,
            workspace,
            "escape",
            &[],
            (0, 1),
            "xxone two three four\n",
        );
        reset_editor(visual, workspace, text);
        visual.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("P".into())));
        assert_source(
            visual,
            workspace,
            "2 p",
            &[],
            (0, 2),
            "oPPne two three four\n",
        );
        reset_editor(visual, workspace, "123456\nx\n123456\n");
        assert_source(
            visual,
            workspace,
            "4 l 1 j 1 j",
            &[],
            (2, 4),
            "123456\nx\n123456\n",
        );
        reset_editor(visual, workspace, "a:x:y:z\n");
        assert_source(visual, workspace, "t : ;", &[], (0, 2), "a:x:y:z\n");
    })
}

#[::core::prelude::v1::test]
fn vim_ctrl_v_selects_and_edits_rectangles() -> anyhow::Result<()> {
    let text = "abcd\nxy\n1234\nlonger\n";
    with_vim_workspace(text, |visual, workspace| {
        visual
            .update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("PASTE".into())));
        assert_source(
            visual,
            workspace,
            "l ctrl-v l j j",
            &["bc", "y", "23"],
            (2, 2),
            text,
        );
        assert!(visual.debug_bounds("status-vim-mode: V-BLOCK").is_some());
        assert_source(visual, workspace, "o", &["bc", "y", "23"], (0, 1), text);
        assert_source(visual, workspace, "d", &[], (0, 1), "ad\nx\n14\nlonger\n");
        assert_source(
            visual,
            workspace,
            "p",
            &[],
            (0, 2),
            "adbc\nx y\n1423\nlonger\n",
        );

        reset_editor(visual, workspace, text);
        visual.simulate_keystrokes("l ctrl-v l 2 j shift-i");
        visual.simulate_input("!");
        assert_source(
            visual,
            workspace,
            "escape",
            &[],
            (0, 1),
            "a!bcd\nx!y\n1!234\nlonger\n",
        );
        assert_source(visual, workspace, "u", &[], (0, 1), text);
        reset_editor(visual, workspace, text);
        visual.simulate_keystrokes("l ctrl-v l 2 j shift-a");
        visual.simulate_input("!");
        assert_source(
            visual,
            workspace,
            "escape",
            &[],
            (0, 1),
            "abc!d\nxy !\n123!4\nlonger\n",
        );
        reset_editor(visual, workspace, text);
        visual.simulate_keystrokes("l ctrl-v $ 2 j shift-a");
        visual.simulate_input("!");
        assert_source(
            visual,
            workspace,
            "escape",
            &[],
            (0, 1),
            "abcd!\nxy!\n1234!\nlonger\n",
        );
        reset_editor(visual, workspace, text);
        visual.simulate_keystrokes("l ctrl-v l 2 j c");
        visual.simulate_input("!");
        assert_source(
            visual,
            workspace,
            "escape",
            &[],
            (0, 1),
            "a!d\nx!\n1!4\nlonger\n",
        );
        assert_source(visual, workspace, "u", &[], (0, 1), text);

        reset_editor(visual, workspace, "abcd\nx\n1234\n");
        assert_source(
            visual,
            workspace,
            "2 l ctrl-v l 2 j shift-a escape",
            &[],
            (0, 2),
            "abcd\nx\n1234\n",
        );
        visual.simulate_keystrokes("ctrl-v l 2 j shift-i");
        visual.simulate_input("!");
        assert_source(
            visual,
            workspace,
            "escape",
            &[],
            (0, 2),
            "ab!cd\nx\n12!34\n",
        );
        reset_editor(visual, workspace, text);
        visual.simulate_keystrokes("l ctrl-v l 2 j y");
        assert_source(
            visual,
            workspace,
            "4 shift-g p",
            &[],
            (3, 1),
            "abcd\nxy\n1234\nlbconger\n y\n 23\n",
        );

        let display_columns = "abcd\n\tX\n界ab\n";
        reset_editor(visual, workspace, display_columns);
        visual.update(|_, cx| {
            let editor = workspace.read(cx).active_file().unwrap().editor.clone();
            editor.update(cx, |editor, cx| {
                editor.set_tab_size(
                    gpui_base::input::TabSize {
                        tab_size: 4,
                        hard_tabs: true,
                    },
                    cx,
                )
            });
        });
        assert_source(
            visual,
            workspace,
            "l ctrl-v l 2 j",
            &["bc", "\t", "界a"],
            (2, 1),
            display_columns,
        );
        assert_source(visual, workspace, "y", &[], (0, 1), display_columns);
        visual.update(|_, cx| {
            assert_eq!(
                cx.read_from_clipboard().unwrap().text().unwrap(),
                "bc\n  \n a"
            )
        });
        assert_source(
            visual,
            workspace,
            "l ctrl-v h 2 j d",
            &[],
            (0, 1),
            "ad\n  X\n b\n",
        );

        reset_editor(visual, workspace, text);
        visual.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string("!".into())));
        assert_source(
            visual,
            workspace,
            "l ctrl-v l 2 j p",
            &[],
            (0, 1),
            "a!d\nx!\n1!4\nlonger\n",
        );
        reset_editor(visual, workspace, "a\n\n");
        visual.simulate_keystrokes("i");
        visual.simulate_keystrokes("ctrl-v");
        draw(visual);
        visual.update(|_, cx| {
            assert!(workspace.read(cx).active_file().unwrap().vim.mode() == source::Mode::Insert);
        });
    })
}

#[::core::prelude::v1::test]
fn vim_substitute_uses_change_ranges_and_history() -> anyhow::Result<()> {
    let text = "one two\n  three\nfour\n";
    with_vim_workspace(text, |visual, workspace| {
        visual.simulate_keystrokes("3 s");
        visual.simulate_input("X");
        assert_source(
            visual,
            workspace,
            "escape",
            &[],
            (0, 0),
            "X two\n  three\nfour\n",
        );
        assert_source(
            visual,
            workspace,
            "w .",
            &[],
            (0, 2),
            "X X\n  three\nfour\n",
        );
        assert_source(visual, workspace, "u u 0", &[], (0, 0), text);

        visual.simulate_keystrokes("v e s");
        visual.simulate_input("Y");
        assert_source(
            visual,
            workspace,
            "escape",
            &[],
            (0, 0),
            "Y two\n  three\nfour\n",
        );

        reset_editor(visual, workspace, "  one\n  two\n  three\n  four\n");
        visual.simulate_keystrokes("2 shift-s");
        visual.simulate_input("Z");
        assert_source(
            visual,
            workspace,
            "escape",
            &[],
            (0, 2),
            "  Z\n  three\n  four\n",
        );
        assert_source(visual, workspace, "j .", &[], (1, 2), "  Z\n  Z\n");
        assert_source(
            visual,
            workspace,
            "u u 0",
            &[],
            (0, 0),
            "  one\n  two\n  three\n  four\n",
        );
    })
}

#[::core::prelude::v1::test]
fn vim_toggle_case_uses_grapheme_counts_and_repeats_at_the_caret() -> anyhow::Result<()> {
    let text = "aB--cd\né\u{301}ßz\n";
    let toggled = "Ab--cd\né\u{301}ßz\n";
    with_vim_workspace(text, |visual, workspace| {
        assert_source(visual, workspace, "2 ~", &[], (0, 2), toggled);
        assert_source(visual, workspace, ".", &[], (0, 4), toggled);
        assert_source(visual, workspace, ".", &[], (0, 5), "Ab--CD\né\u{301}ßz\n");
        assert_source(visual, workspace, "u 0", &[], (0, 0), toggled);
        assert_source(
            visual,
            workspace,
            "j 0 9 ~",
            &[],
            (1, 4),
            "Ab--cd\nÉ\u{301}SSZ\n",
        );
        assert_source(visual, workspace, "u 0", &[], (1, 0), toggled);

        reset_editor(visual, workspace, "a-bcd\n");
        assert_source(visual, workspace, "x ~ .", &[], (0, 2), "-Bcd\n");
    })
}

#[::core::prelude::v1::test]
fn vim_surround_join_replace_and_match_integrate_with_undo_and_repeat() -> anyhow::Result<()> {
    let text = "one two\n  three\nfour\n";
    with_vim_workspace(text, |visual, workspace| {
        assert_source(
            visual,
            workspace,
            "y s i w )",
            &[],
            (0, 0),
            "(one) two\n  three\nfour\n",
        );
        assert_source(
            visual,
            workspace,
            "3 w .",
            &[],
            (0, 6),
            "(one) (two)\n  three\nfour\n",
        );
        assert_source(
            visual,
            workspace,
            "c s ) ]",
            &[],
            (0, 6),
            "(one) [two]\n  three\nfour\n",
        );
        assert_source(
            visual,
            workspace,
            "d s ]",
            &[],
            (0, 6),
            "(one) two\n  three\nfour\n",
        );
        assert_source(
            visual,
            workspace,
            "u",
            &[],
            (0, 6),
            "(one) [two]\n  three\nfour\n",
        );
        reset_editor(visual, workspace, text);
        assert_source(
            visual,
            workspace,
            "j y s s (",
            &[],
            (1, 2),
            "one two\n  ( three )\nfour\n",
        );
        reset_editor(visual, workspace, text);
        assert_source(
            visual,
            workspace,
            "v e shift-s \"",
            &[],
            (0, 0),
            "\"one\" two\n  three\nfour\n",
        );
        reset_editor(visual, workspace, text);
        assert_source(
            visual,
            workspace,
            "shift-j",
            &[],
            (0, 7),
            "one two three\nfour\n",
        );
        assert_source(visual, workspace, "u", &[], (0, 0), text);
        assert_source(
            visual,
            workspace,
            "g shift-j",
            &[],
            (0, 7),
            "one two  three\nfour\n",
        );
        reset_editor(visual, workspace, text);
        assert_source(
            visual,
            workspace,
            "shift-v j shift-j",
            &[],
            (0, 7),
            "one two three\nfour\n",
        );
        reset_editor(visual, workspace, text);
        assert_source(
            visual,
            workspace,
            "2 r x",
            &[],
            (0, 1),
            "xxe two\n  three\nfour\n",
        );
        reset_editor(visual, workspace, "(one [two])\n");
        assert_source(visual, workspace, "%", &[], (0, 10), "(one [two])\n");
        assert_source(visual, workspace, "%", &[], (0, 0), "(one [two])\n");
        assert_source(visual, workspace, "d %", &[], (0, 0), "\n");
    })
}

#[::core::prelude::v1::test]
fn vim_navigation_reaches_the_final_empty_line() -> anyhow::Result<()> {
    with_vim_workspace("abc\n", |visual, workspace| {
        for wrap in [true, false] {
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    let file = this.files.get_mut(this.tabs.active().unwrap()).unwrap();
                    file.apply_wrap(wrap, window, cx);
                });
            });
            let text = "abc\n";
            reset_editor(visual, workspace, text);
            assert_source(visual, workspace, "l l j", &[], (1, 0), text);
            assert_source(visual, workspace, "j", &[], (1, 0), text);
            assert_source(visual, workspace, "k 0", &[], (0, 0), text);
            assert_source(visual, workspace, "g g shift-g", &[], (1, 0), text);
            assert_source(visual, workspace, "d j", &[], (1, 0), text);
            assert_source(visual, workspace, "k d j", &[], (0, 0), text);

            let text = "abc\n\n";
            reset_editor(visual, workspace, text);
            assert_source(visual, workspace, "j", &[], (1, 0), text);
            assert_source(visual, workspace, "j", &[], (2, 0), text);
            assert_source(visual, workspace, "g g 2 j", &[], (2, 0), text);
            assert_source(visual, workspace, "g g 3 shift-g", &[], (2, 0), text);
            assert_source(visual, workspace, "g g v shift-g", &[text], (2, 0), text);
            assert_source(visual, workspace, "escape", &[], (2, 0), text);

            let text = "abc\n  def";
            reset_editor(visual, workspace, text);
            assert_source(visual, workspace, "2 j", &[], (1, 0), text);
            assert_source(visual, workspace, "g g shift-g", &[], (1, 2), text);
        }
    })
}

#[::core::prelude::v1::test]
fn vim_line_navigation_uses_the_editor_scroll_animation() -> anyhow::Result<()> {
    let text = (0..180)
        .map(|row| format!("line {row:03}\n"))
        .collect::<String>();
    with_vim_workspace(&text, |visual, workspace| {
        visual.simulate_resize(size(px(900.), px(420.)));
        visual.update(|_, cx| {
            cx.set_smooth_scrolling(true);
            cx.set_reduce_motion(false);
        });
        draw(visual);
        let read = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| {
                let editor = workspace.read(cx).active_file().unwrap().editor.read(cx);
                (
                    editor.cursor_position().line,
                    editor.scroll_offset().y,
                    editor.is_scroll_navigating(),
                )
            })
        };
        let settle = |visual: &mut VisualTestContext| {
            for _ in 0..35 {
                visual.executor().advance_clock(Duration::from_millis(16));
                draw(visual);
            }
        };
        for (keys, down) in [
            ("ctrl-d", true),
            ("ctrl-u", false),
            ("ctrl-f", true),
            ("ctrl-b", false),
            ("8 0 g g", true),
            ("g g", false),
            ("shift-g", true),
            ("g g", false),
            ("5 0 j", true),
            ("pageup", false),
        ] {
            let before = read(visual);
            visual.simulate_keystrokes(keys);
            draw(visual);
            let queued = read(visual);
            if matches!(keys, "ctrl-d" | "ctrl-u" | "ctrl-f" | "ctrl-b" | "pageup") {
                assert_eq!(queued.1, before.1, "{keys} starts from the current offset");
            }
            assert!(if down {
                queued.1 <= before.1
            } else {
                queued.1 >= before.1
            });
            assert!(queued.2, "{keys} scheduled animation");
            assert_eq!(queued.0 > before.0, down, "{keys} caret moves first");
            settle(visual);
            let end = read(visual);
            assert_eq!(end.0, queued.0, "{keys} caret remains at the target");
            assert_eq!(end.1 < before.1, down, "{keys} viewport reached the target");
            assert_ne!(end.1, before.1, "{keys} moved viewport");
            assert_ne!(end.1, queued.1, "{keys} animated the arrival");
            assert!(!end.2, "{keys} animation settled");
        }
        visual.update(|_, cx| cx.set_reduce_motion(true));
        visual.simulate_keystrokes("shift-g");
        draw(visual);
        let end = read(visual);
        assert!(!end.2);
        settle(visual);
        assert_eq!(read(visual), end);
    })
}

#[::core::prelude::v1::test]
fn vim_mouse_places_the_caret_on_source_glyphs_around_inlay_hints() -> anyhow::Result<()> {
    use gpui_component::input::InlayHint;

    let text = "ABe\u{301}\n\nQ界Z\n";
    with_vim_workspace(text, |visual, workspace| {
        visual.simulate_resize(size(px(740.), px(620.)));
        let editor = visual.update(|window, cx| {
            cx.set_reduce_motion(true);
            let state = workspace.update(cx, |this, cx| {
                let file = this.files.get_mut(this.tabs.active().unwrap()).unwrap();
                file.apply_wrap(false, window, cx);
                file.editor.clone()
            });
            state.update(cx, |state, cx| {
                state.set_inlay_hints(
                    [(1, "middle hint"), (5, "end hint"), (6, "empty row hint")]
                        .map(|(offset, label)| InlayHint {
                            offset,
                            label: label.into(),
                        })
                        .to_vec(),
                    cx,
                );
            });
            state
        });
        let caret_at = |visual: &mut VisualTestContext, offset| {
            visual.update(|_, cx| {
                editor.update(cx, |state, cx| {
                    state.set_modal_selection(&[(offset, offset)], offset, cx);
                })
            });
            draw(visual);
            visual.update(|_, cx| editor.read(cx).cursor_layout().unwrap().0)
        };
        let check_click = |visual: &mut VisualTestContext, position, offset, caret, status| {
            visual.simulate_click(position, Modifiers::default());
            draw(visual);
            visual.update(|_, cx| {
                let state = editor.read(cx);
                assert_eq!(state.cursor(), offset);
                assert_eq!(state.selected_range(), offset..offset);
                assert_eq!(state.cursor_layout().unwrap().0, caret);
            });
            assert!(visual.debug_bounds(status).is_some());
        };

        let a = caret_at(visual, 0);
        let b = caret_at(visual, 1);
        let end = caret_at(visual, 2);
        let empty = caret_at(visual, 6);
        assert!(b.left() > a.right() + a.size.width);
        check_click(
            visual,
            point(a.right() - px(1.), a.center().y),
            0,
            a,
            "status-file-position: Ln 1:1",
        );
        check_click(
            visual,
            point((a.right() + b.left()) / 2., b.center().y),
            1,
            b,
            "status-file-position: Ln 1:2",
        );
        check_click(
            visual,
            point(end.right() + px(10.), end.center().y),
            2,
            end,
            "status-file-position: Ln 1:3",
        );
        check_click(
            visual,
            point(end.right() + px(170.), end.center().y),
            2,
            end,
            "status-file-position: Ln 1:3",
        );
        check_click(
            visual,
            point(empty.right() + px(8.), empty.center().y),
            6,
            empty,
            "status-file-position: Ln 2:1",
        );
        check_click(
            visual,
            point(end.right() - px(1.), end.center().y),
            2,
            end,
            "status-file-position: Ln 1:3",
        );

        visual.simulate_keystrokes("i");
        draw(visual);
        visual.simulate_click(
            point(end.right() + px(170.), end.center().y),
            Modifiers::default(),
        );
        visual.update(|_, cx| {
            assert_eq!(
                editor.read(cx).cursor(),
                5,
                "Insert clicks use the insertion boundary"
            )
        });
        visual.simulate_keystrokes("escape");
        assert_source(visual, workspace, "x", &[], (0, 1), "AB\n\nQ界Z\n");

        reset_editor(visual, workspace, text);
        visual.update(|window, cx| {
            workspace.update(cx, |this, cx| {
                let file = this.files.get_mut(this.tabs.active().unwrap()).unwrap();
                file.apply_wrap(true, window, cx);
            });
            editor.update(cx, |state, cx| {
                state.set_inlay_hints(
                    vec![InlayHint {
                        offset: 8,
                        label: "type ".repeat(40).into(),
                    }],
                    cx,
                );
            })
        });
        let q = caret_at(visual, 7);
        let wide = caret_at(visual, 8);
        let inset = visual.update(|_, cx| editor.read(cx).input_bounds().right() - px(26.));
        assert!(
            wide.top() > q.top(),
            "the long inlay wraps before the source glyph"
        );
        check_click(
            visual,
            point(inset, q.center().y),
            7,
            q,
            "status-file-position: Ln 3:1",
        );
        assert_source(visual, workspace, "j", &[], (2, 1), text);
        visual.update(|_, cx| assert_eq!(editor.read(cx).cursor_layout().unwrap().0, wide));
        assert_source(visual, workspace, "k", &[], (2, 0), text);
        visual.update(|_, cx| assert_eq!(editor.read(cx).cursor_layout().unwrap().0, q));
        check_click(
            visual,
            wide.center(),
            8,
            wide,
            "status-file-position: Ln 3:2",
        );
        assert_source(visual, workspace, "x", &[], (2, 1), "ABe\u{301}\n\nQZ\n");
    })
}

#[::core::prelude::v1::test]
fn vim_panes_keep_document_focus_and_reveal_the_active_buffer() -> anyhow::Result<()> {
    let document = (0..48)
        .map(|section| format!("# Section {section}\n\nParagraph {section}.\n\n"))
        .collect::<String>();
    let target = "guide/nested/current.md";
    with_vim_repository(
        target,
        &document,
        &[("a.txt", "first"), ("guide/nested/other.md", "# Other")],
        |visual, workspace| {
            visual.simulate_resize(size(px(1000.), px(460.)));
            visual.update(|_, cx| cx.set_reduce_motion(true));
            let wait = |visual: &mut VisualTestContext,
                        ready: &dyn Fn(&Workspace, &App) -> bool| {
                for _ in 0..100 {
                    draw(visual);
                    if visual.update(|_, cx| ready(workspace.read(cx), cx)) {
                        return;
                    }
                    visual.executor().advance_clock(Duration::from_millis(25));
                }
                panic!("workspace did not settle");
            };

            wait(visual, &|this, _| this.active_file().is_some());
            let assert_tree = |visual: &mut VisualTestContext, path: &str| {
                draw(visual);
                visual.update(|window, cx| {
                    let this = workspace.read(cx);
                    assert!(this.tree_focus.is_focused(window));
                    assert_eq!(this.tree_keyboard_path, PathBuf::from(path));
                });
            };
            let assert_editor = |visual: &mut VisualTestContext, step| {
                draw(visual);
                visual.update(|window, cx| {
                    let this = workspace.read(cx);
                    assert!(
                        this.vim_source(window, cx).is_some(),
                        "{step}: source should have focus, tree focus = {}",
                        this.tree_focus.is_focused(window),
                    );
                });
            };
            let assert_doc = |visual: &mut VisualTestContext| {
                draw(visual);
                assert!(visual.debug_bounds("workspace-status").is_none());
                assert!(visual.debug_bounds("status-vim-toggle").is_none());
                visual.update(|window, cx| {
                    let this = workspace.read(cx);
                    assert_eq!(this.view, WorkspaceView::Docs);
                    assert!(
                        this.doc
                            .as_ref()
                            .unwrap()
                            .read(cx)
                            .content_focused(window, cx)
                    );
                });
            };
            let hide_tree = |visual: &mut VisualTestContext| {
                visual.update(|_, cx| {
                    workspace.update(cx, |this, cx| {
                        this.set_file_tree_visible(false);
                        this.tree_keyboard_path = "a.txt".into();
                        cx.notify();
                    });
                });
                draw(visual);
            };
            let toggle = |visual: &mut VisualTestContext, enabled: bool| {
                let bounds = visual.debug_bounds("status-vim-toggle").unwrap();
                visual.simulate_click(bounds.center(), Modifiers::default());
                draw(visual);
                assert_eq!(
                    visual.debug_bounds("status-vim-mode: NORMAL").is_some(),
                    enabled
                );
                assert_eq!(visual.debug_bounds("status-vim-icon-on").is_some(), enabled);
                assert_eq!(
                    visual.debug_bounds("status-vim-icon-off").is_some(),
                    !enabled
                );
                if !enabled {
                    let icon = visual.debug_bounds("status-vim-icon-off").unwrap();
                    let status = visual.debug_bounds("workspace-status").unwrap();
                    assert_eq!(icon.left() - status.left(), STATUS_ITEM_GAP);
                }
                visual.update(|_, cx| {
                    let this = workspace.read(cx);
                    assert_eq!(this.config.current().keyboard.vim_mode, enabled);
                    let saved = ConfigFile::load(Some(this.config.path().unwrap().to_owned()));
                    assert_eq!(saved.current().keyboard.vim_mode, enabled);
                    assert!(!saved.current().lsp.enabled);
                });
            };
            let switch = |visual: &mut VisualTestContext, view| {
                visual.update(|window, cx| {
                    workspace.update(cx, |this, cx| {
                        this.switch_view_from_topbar(view, window, cx)
                    });
                });
                draw(visual);
            };

            assert_editor(visual, "startup");
            hide_tree(visual);
            visual.simulate_keystrokes("space e");
            assert_tree(visual, target);
            wait(visual, &|this, _| {
                this.rows
                    .iter()
                    .any(|row| row.entry.path == PathBuf::from(target))
            });
            visual.simulate_keystrokes("j");
            assert_tree(visual, "guide/nested/other.md");
            visual.simulate_keystrokes("space e");
            assert_editor(visual, "Space e return");

            visual.simulate_keystrokes("ctrl-h");
            assert_tree(visual, target);
            visual.simulate_keystrokes("ctrl-l");
            assert_editor(visual, "Ctrl+l return");

            toggle(visual, false);
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    this.focus_tree(target.into(), window, cx);
                })
            });
            visual.simulate_keystrokes("j k l h");
            assert_tree(visual, target);
            visual.simulate_keystrokes("down");
            assert_tree(visual, "guide/nested/other.md");

            switch(visual, WorkspaceView::Docs);
            wait(visual, &|this, _| {
                this.doc.is_some() && this.docs_tree_complete
            });
            assert_doc(visual);
            visual.simulate_keystrokes("j j j");
            wait(visual, &|this, cx| {
                this.doc
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .session_position(cx)
                    .is_some_and(|position| position.block > 0 || position.inset > 0.)
            });
            visual.simulate_keystrokes("g g");
            wait(visual, &|this, cx| {
                this.doc
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .session_position(cx)
                    .is_some_and(|position| position.block == 0 && position.inset == 0.)
            });

            let doc_path = visual.update(|_, cx| {
                workspace
                    .read(cx)
                    .doc
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .path
                    .clone()
            });
            visual.update(|window, cx| {
                workspace.update(cx, |this, cx| {
                    this.focus_tree(doc_path.clone(), window, cx);
                })
            });
            visual.simulate_keystrokes("j k h l");
            assert_tree(visual, doc_path.to_str().unwrap());

            switch(visual, WorkspaceView::Editor);
            assert_editor(visual, "switching to Editor");
            toggle(visual, true);
            visual.simulate_keystrokes("space 1");
            assert_doc(visual);
            visual.simulate_keystrokes("space 3");
            draw(visual);
            visual.update(|window, cx| {
                let this = workspace.read(cx);
                assert_eq!(this.view, WorkspaceView::Git);
                assert!(this.git.as_ref().unwrap().read(cx).vim_focused(window, cx));
            });
            visual.simulate_keystrokes("space 2");
            assert_editor(visual, "Space 2");
            visual.simulate_keystrokes("space 1");
            assert_doc(visual);
            hide_tree(visual);
            visual.simulate_keystrokes("ctrl-h");
            assert_tree(visual, doc_path.to_str().unwrap());
            visual.simulate_keystrokes("ctrl-l");
            assert_doc(visual);
            visual.simulate_keystrokes("space e");
            assert_tree(visual, doc_path.to_str().unwrap());
        },
    )
}
