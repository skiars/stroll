#![cfg(any(target_os = "linux", target_os = "windows"))]

use gpui::{
    AppContext as _, ClipboardItem, Context, Entity, EntityInputHandler as _, Render, Styled as _,
    TestAppContext, TestDispatcher, VisualTestContext, Window, point, px, size,
};
use gpui_component::{
    Root,
    input::{Editor, EditorState, InlayHint, Position, RopeExt as _},
};

struct EditorView {
    editor: Entity<EditorState>,
    readonly: bool,
}

impl Render for EditorView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        Editor::new(&self.editor)
            .readonly(self.readonly)
            .size_full()
    }
}

fn with_editor(
    text: &str,
    soft_wrap: bool,
    line_clipboard: bool,
    readonly: bool,
    test: impl FnOnce(&mut VisualTestContext, &Entity<EditorState>),
) {
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        gpui_platform::current_platform(true).text_system(),
    );
    let mut editor = None;
    let (_, visual) = app.add_window_view(|window, cx| {
        gpui_component::init(cx);
        let state = cx.new(|cx| {
            let state = EditorState::new(window, cx)
                .progressive_wrap()
                .soft_wrap(soft_wrap)
                .default_value(text.to_owned());
            if line_clipboard {
                state.line_clipboard().line_commands()
            } else {
                state
            }
        });
        editor = Some(state.clone());
        let root = cx.new(|_| EditorView {
            editor: state,
            readonly,
        });
        Root::new(root, window, cx)
    });
    let editor = editor.unwrap();
    visual.simulate_resize(size(px(360.), px(520.)));
    visual.update(|window, cx| editor.update(cx, |editor, cx| editor.focus(window, cx)));
    draw(visual);
    test(visual, &editor);
}

fn draw(visual: &mut VisualTestContext) {
    visual.update(|window, cx| window.draw(cx).clear(cx));
}

fn text(visual: &mut VisualTestContext, editor: &Entity<EditorState>) -> String {
    visual.update(|_, cx| editor.read(cx).text().to_string())
}

fn clipboard(visual: &mut VisualTestContext) -> String {
    visual.update(|_, cx| {
        cx.read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap()
    })
}

fn select(
    visual: &mut VisualTestContext,
    editor: &Entity<EditorState>,
    range: std::ops::Range<usize>,
) {
    visual.update(|_, cx| editor.update(cx, |editor, cx| editor.set_selected_range(range, cx)));
    draw(visual);
}

#[test]
fn edits_track_surviving_diagnostics_until_the_next_reply() {
    let source = "edit me\n😀 error\n";
    with_editor(source, true, false, false, |visual, editor| {
        visual.update(|_, cx| {
            editor.update(cx, |editor, _| {
                let diagnostics = editor.diagnostics_mut().unwrap();
                for (line, start, end, message) in [(0, 0, 4, "edited"), (1, 2, 7, "untouched")] {
                    diagnostics.push(lsp_types::Diagnostic {
                        range: lsp_types::Range::new(
                            lsp_types::Position::new(line, start),
                            lsp_types::Position::new(line, end),
                        ),
                        message: message.into(),
                        ..Default::default()
                    });
                }
            });
        });
        select(visual, editor, 2..2);
        visual.simulate_input("\n");
        draw(visual);
        visual.update(|_, cx| {
            let state = editor.read(cx);
            let diagnostics = state.diagnostics().unwrap();
            assert_eq!(diagnostics.len(), 2);
            let mut entries = diagnostics.iter();
            let edited = entries.next().unwrap();
            assert_eq!(edited.message.as_str(), "edited");
            assert_eq!(state.text().slice(edited.range.clone()), "ed\nit");
            let untouched = entries.next().unwrap();
            assert_eq!(untouched.message.as_str(), "untouched");
            assert_eq!(
                untouched.diagnostic.range,
                Position::new(2, 2)..Position::new(2, 7)
            );
            assert_eq!(state.text().slice(untouched.range.clone()), "error");
        });
        visual.update(|window, cx| {
            editor.update(cx, |state, cx| state.replay_text_history(true, window, cx));
        });
        visual.update(|window, cx| {
            editor.update(cx, |state, cx| {
                let diagnostics = state.diagnostics().unwrap();
                let untouched = diagnostics.iter().nth(1).unwrap();
                assert_eq!(
                    untouched.diagnostic.range,
                    Position::new(1, 2)..Position::new(1, 7)
                );
                state.edit_ranges(&[(0..4, "new".into())], window, cx);
                let mut remaining = state.diagnostics().unwrap().iter();
                assert_eq!(remaining.next().unwrap().message.as_str(), "untouched");
                assert!(remaining.next().is_none());
            });
        });
    });
}

#[test]
fn inlay_hints_keep_their_wrap_rows_after_line_insertion_and_undo() {
    let source = "before\nstable line\nafter";
    let row_start = source.find("stable").unwrap();
    let offset = source.find("line").unwrap();
    with_editor(source, true, false, false, |visual, editor| {
        visual.update(|_, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_inlay_hints(
                    vec![InlayHint {
                        offset,
                        label: "long_type ".repeat(30).into(),
                    }],
                    cx,
                );
            })
        });
        select(visual, editor, row_start..row_start);
        let wrapped = visual.update(|_, cx| editor.read(cx).projected_wrap_rows(1));
        assert!(wrapped > 1);

        visual.simulate_keystrokes("enter");
        draw(visual);
        visual.update(|_, cx| {
            let state = editor.read(cx);
            assert_eq!(state.text().to_string(), "before\n\nstable line\nafter");
            assert_eq!(state.projected_wrap_rows(1), 1);
            assert_eq!(state.projected_wrap_rows(2), wrapped);
        });

        visual.simulate_keystrokes("ctrl-z");
        draw(visual);
        visual.update(|_, cx| {
            let state = editor.read(cx);
            assert_eq!(state.text().to_string(), source);
            assert_eq!(state.projected_wrap_rows(1), wrapped);
        });
    });
}

#[test]
fn line_clipboard_uses_logical_lines_and_plain_clipboard_uses_the_selection() {
    let long_line = "wrapped line with Unicode 🦀 ".repeat(20);
    let source = format!("before\n{long_line}\nafter");
    with_editor(&source, true, true, false, |visual, editor| {
        let caret = 7 + "wrapped line with Unicode 🦀 ".len() * 10;
        select(visual, editor, caret..caret);
        visual.simulate_keystrokes("ctrl-c");
        assert_eq!(clipboard(visual), format!("{long_line}\n"));
        assert_eq!(text(visual, editor), source);

        visual.simulate_keystrokes("ctrl-x");
        assert_eq!(text(visual, editor), "before\nafter");
        visual.simulate_keystrokes("ctrl-z");
        assert_eq!(text(visual, editor), source);
        visual.update(|_, cx| assert_eq!(editor.read(cx).selected_range(), caret..caret));

        select(visual, editor, 2..2);
        visual.simulate_keystrokes("ctrl-v");
        assert_eq!(text(visual, editor), format!("{long_line}\n{source}"));
        visual.update(|_, cx| {
            assert_eq!(
                editor.read(cx).cursor_position(),
                Position::new(1, 2),
                "whole-line paste keeps the cursor in the original line"
            );
        });
        visual.simulate_keystrokes("ctrl-z");
        assert_eq!(text(visual, editor), source);

        select(visual, editor, 0..6);
        visual.simulate_keystrokes("ctrl-v");
        assert_eq!(
            text(visual, editor),
            format!("{long_line}\n\n{long_line}\nafter")
        );
        visual.simulate_keystrokes("ctrl-z");
        select(visual, editor, 0..2);
        visual.simulate_keystrokes("ctrl-x");
        assert_eq!(clipboard(visual), "be");
        assert_eq!(text(visual, editor), format!("fore\n{long_line}\nafter"));

        visual.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string("plain\n".into())));
        select(visual, editor, 2..2);
        visual.simulate_keystrokes("ctrl-v");
        assert_eq!(
            text(visual, editor),
            format!("foplain\nre\n{long_line}\nafter")
        );
    });
}

#[test]
fn line_clipboard_deduplicates_cursors_and_cuts_a_final_crlf_line_atomically() {
    with_editor(
        "first\r\nmiddle\r\n末行",
        true,
        true,
        false,
        |visual, editor| {
            let (middle, last) = visual.update(|_, cx| {
                let text = editor.read(cx).text();
                (text.line_start_offset(1), text.line_start_offset(2))
            });
            select(visual, editor, middle..middle);
            visual.update(|_, cx| {
                editor.update(cx, |editor, cx| {
                    editor.add_selection(middle + 2..middle + 2, cx);
                    editor.add_selection(last..last, cx);
                });
            });
            visual.simulate_keystrokes("ctrl-c");
            assert_eq!(clipboard(visual), "middle\r\n末行\r\n");
            visual.simulate_keystrokes("ctrl-x");
            assert_eq!(clipboard(visual), "middle\r\n末行\r\n");
            assert_eq!(text(visual, editor), "first");
            visual.simulate_keystrokes("ctrl-z");
            assert_eq!(text(visual, editor), "first\r\nmiddle\r\n末行");
            visual.simulate_keystrokes("ctrl-c");
            assert_eq!(clipboard(visual), "middle\r\n末行\r\n");

            visual.simulate_keystrokes("ctrl-v");
            assert_eq!(
                text(visual, editor),
                "first\r\nmiddle\r\nmiddle\r\n末行\r\n末行"
            );
            visual.simulate_keystrokes("ctrl-z");
            assert_eq!(text(visual, editor), "first\r\nmiddle\r\n末行");
            select(visual, editor, last..last);
            visual.simulate_keystrokes("ctrl-x");
            assert_eq!(clipboard(visual), "末行\r\n");
            assert_eq!(text(visual, editor), "first\r\nmiddle");
        },
    );
}

#[test]
fn whole_line_clipboard_requires_an_editable_opted_in_editor() {
    for (enabled, readonly) in [(false, false), (true, true)] {
        with_editor("abc\ndef", true, enabled, readonly, |visual, editor| {
            visual.update(|_, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string("sentinel".into()))
            });
            select(visual, editor, 1..1);
            visual.simulate_keystrokes("ctrl-c ctrl-x ctrl-shift-k alt-down");
            assert_eq!(clipboard(visual), "sentinel");
            assert_eq!(text(visual, editor), "abc\ndef");
        });
    }
}

#[test]
fn typing_keeps_a_visible_caret_at_the_edge_and_reveals_an_offscreen_caret() {
    let line = format!("{}\n", "abcdefghi ".repeat(8));
    let source = line.repeat(100);
    with_editor(&source, true, false, false, |visual, editor| {
        let caret = 40 * line.len() + 62;
        select(visual, editor, caret..caret);
        visual.run_until_parked();
        draw(visual);
        let edge = visual.update(|_, cx| {
            editor.update(cx, |editor, cx| {
                let geometry = editor.prepaint_caret_geometry().unwrap();
                let offset = point(
                    px(0.),
                    geometry.input_bounds().top() + geometry.line_height()
                        - geometry.cursor_bounds().top(),
                );
                editor.set_scroll_offset(offset, cx);
                offset
            })
        });
        draw(visual);
        visual.simulate_keystrokes("x");
        draw(visual);
        visual.update(|_, cx| assert_eq!(editor.read(cx).scroll_offset(), edge));
        assert_eq!(&text(visual, editor)[caret..caret + 1], "x");

        visual.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.replace_and_mark_text_in_range(None, "n", Some(1..1), window, cx);
            });
        });
        draw(visual);
        visual.update(|_, cx| assert_eq!(editor.read(cx).scroll_offset(), edge));
        visual.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.replace_text_in_range(None, "你", window, cx);
            });
        });
        draw(visual);
        visual.update(|_, cx| assert_eq!(editor.read(cx).scroll_offset(), edge));
        assert_eq!(&text(visual, editor)[caret + 1..caret + 4], "你");

        visual.update(|_, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_scroll_offset(point(px(0.), px(-100.)), cx)
            });
        });
        draw(visual);
        visual.simulate_keystrokes("y");
        draw(visual);
        visual.update(|_, cx| {
            let editor = editor.read(cx);
            let geometry = editor.prepaint_caret_geometry().unwrap();
            let caret_bottom = geometry.cursor_bounds().bottom() + editor.scroll_offset().y;
            assert!((caret_bottom - geometry.input_bounds().bottom()).abs() < px(0.01));
            assert!(editor.visible_row_range().unwrap().contains(&40));
        });
    });
}

#[test]
fn typing_keeps_horizontal_scroll_if_the_caret_still_fits() {
    let source = "m".repeat(200);
    with_editor(&source, false, false, false, |visual, editor| {
        select(visual, editor, 100..100);
        let offset = visual.update(|_, cx| {
            editor.update(cx, |editor, cx| {
                let geometry = editor.prepaint_caret_geometry().unwrap();
                let caret_right = geometry.cursor_bounds().right();
                let viewport_right = geometry.input_bounds().right();
                let offset = point(
                    editor.scroll_offset().x + viewport_right - caret_right - px(17.),
                    px(0.),
                );
                editor.set_scroll_offset(offset, cx);
                offset
            })
        });
        draw(visual);
        visual.simulate_keystrokes("i");
        draw(visual);
        visual.update(|_, cx| assert_eq!(editor.read(cx).scroll_offset(), offset));
        visual.simulate_keystrokes("m m m m");
        draw(visual);
        visual.update(|_, cx| {
            let editor = editor.read(cx);
            let geometry = editor.prepaint_caret_geometry().unwrap();
            assert!(editor.scroll_offset().x < offset.x);
            assert!(
                (geometry.cursor_bounds().right() - geometry.input_bounds().right()).abs()
                    < px(0.01)
            );
        });
    });
}

#[test]
fn deletes_selected_lines_without_touching_the_clipboard_and_restores_the_selection() {
    let source = "one\r\n二 line\r\nthree\r\nfour";
    with_editor(source, true, true, false, |visual, editor| {
        let start = source.find(" line").unwrap();
        let end = source.find("four").unwrap();
        select(visual, editor, start..end);
        visual.update(|_, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string("sentinel".into()));
        });

        visual.simulate_keystrokes("ctrl-shift-k");
        assert_eq!(text(visual, editor), "one\r\nfour");
        assert_eq!(clipboard(visual), "sentinel");
        visual.simulate_keystrokes("ctrl-z");
        assert_eq!(text(visual, editor), source);
        visual.update(|_, cx| assert_eq!(editor.read(cx).selected_range(), start..end));

        select(visual, editor, end..end);
        visual.simulate_keystrokes("ctrl-shift-k");
        assert_eq!(text(visual, editor), "one\r\n二 line\r\nthree");
    });
}

#[test]
fn moving_and_duplicating_selected_lines_tracks_the_selection_and_line_endings() {
    let source = "top\r\n二\r\nthree\r\nbottom";
    #[cfg(target_os = "windows")]
    let (copy_up, copy_down) = ("alt-shift-up", "alt-shift-down");
    #[cfg(not(target_os = "windows"))]
    let (copy_up, copy_down) = ("ctrl-shift-up", "ctrl-shift-down");

    with_editor(source, true, true, false, |visual, editor| {
        let block = source.find('二').unwrap()..source.find("bottom").unwrap();
        visual.update(|_, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_selected_range_with_direction(block.clone(), true, cx);
            });
        });
        draw(visual);
        visual.simulate_keystrokes("alt-up");
        assert_eq!(text(visual, editor), "二\r\nthree\r\ntop\r\nbottom");
        visual.update(|_, cx| {
            let editor = editor.read(cx);
            assert_eq!(editor.selected_range(), 0.."二\r\nthree\r\n".len());
            assert!(editor.is_selection_reversed());
        });
        visual.simulate_keystrokes("alt-up");
        assert_eq!(text(visual, editor), "二\r\nthree\r\ntop\r\nbottom");
        visual.simulate_keystrokes("alt-down");
        assert_eq!(text(visual, editor), source);
        visual.simulate_keystrokes("alt-down");
        assert_eq!(text(visual, editor), "top\r\nbottom\r\n二\r\nthree");
        visual.simulate_keystrokes("ctrl-z");
        assert_eq!(text(visual, editor), source);

        visual.simulate_keystrokes(copy_up);
        assert_eq!(
            text(visual, editor),
            "top\r\n二\r\nthree\r\n二\r\nthree\r\nbottom"
        );
        visual.update(|_, cx| assert_eq!(editor.read(cx).selected_range(), block));
        visual.simulate_keystrokes("ctrl-z");
        visual.simulate_keystrokes(copy_down);
        assert_eq!(
            text(visual, editor),
            "top\r\n二\r\nthree\r\n二\r\nthree\r\nbottom"
        );
        visual.update(|_, cx| {
            let shift = block.len();
            assert_eq!(
                editor.read(cx).selected_range(),
                block.start + shift..block.end + shift
            );
        });

        let caret = text(visual, editor).len();
        select(visual, editor, caret..caret);
        visual.simulate_keystrokes(copy_down);
        assert!(text(visual, editor).ends_with("bottom\r\nbottom"));
    });
}

#[test]
fn moving_disjoint_cursor_lines_keeps_the_active_row_and_final_newline() {
    with_editor("a\nb\nc\nd\ne\n", true, true, false, |visual, editor| {
        select(visual, editor, 2..2);
        visual.update(|_, cx| editor.update(cx, |editor, cx| editor.add_selection(6..6, cx)));
        visual.simulate_keystrokes("alt-down");
        assert_eq!(text(visual, editor), "a\nc\nb\ne\nd\n");
        visual.update(|_, cx| assert_eq!(editor.read(cx).cursor_position().line, 2));
        visual.simulate_keystrokes("alt-down");
        assert_eq!(text(visual, editor), "a\nc\ne\nb\nd\n");
        visual.simulate_keystrokes("ctrl-z");
        assert_eq!(text(visual, editor), "a\nc\nb\ne\nd\n");
    });
}

#[test]
fn toggles_line_comments_using_the_language_and_keeps_blank_lines() {
    let source = "  first\r\n\r\n\tsecond\r\nlast";
    for (language, marker) in [
        ("rust", Some("//")),
        ("python", Some("#")),
        ("jsonc", Some("//")),
        ("custom", Some("--")),
        ("json", None),
        ("markdown", None),
    ] {
        with_editor(source, true, true, false, |visual, editor| {
            visual.update(|_, cx| {
                if language == "custom" {
                    gpui_component::input::set_language_config(
                        language,
                        gpui_component::input::language_config::LanguageConfig::default()
                            .line_comment("--"),
                        cx,
                    );
                }
                editor.update(cx, |editor, cx| editor.set_highlighter(language, cx));
            });
            let end = source.find("last").unwrap();
            select(visual, editor, 0..end);
            visual.simulate_keystrokes("ctrl-/");
            if let Some(marker) = marker {
                assert_eq!(
                    text(visual, editor),
                    format!("  {marker} first\r\n\r\n\t{marker} second\r\nlast"),
                    "{language}"
                );
                visual.simulate_keystrokes("ctrl-/");
                assert_eq!(text(visual, editor), source, "{language}");
                visual.simulate_keystrokes("ctrl-z");
                assert!(text(visual, editor).contains(marker));
                visual.simulate_keystrokes("ctrl-z");
            }
            assert_eq!(text(visual, editor), source, "{language}");
            visual.update(|_, cx| assert_eq!(editor.read(cx).selected_range(), 0..end));
        });
    }
}

#[test]
fn programmatic_replacement_and_undo_redo_keep_the_existing_scroll_margin() {
    let source = "abcdefghi\n".repeat(100);
    with_editor(&source, true, true, false, |visual, editor| {
        let caret = 40 * "abcdefghi\n".len() + 2;
        select(visual, editor, caret..caret);
        let place_caret_near_top = |visual: &mut VisualTestContext| {
            let edge = visual.update(|_, cx| {
                editor.update(cx, |editor, cx| {
                    let offset = point(px(0.), -39. * editor.line_height().unwrap());
                    editor.set_scroll_offset(offset, cx);
                    offset
                })
            });
            draw(visual);
            edge
        };
        let assert_margin = |visual: &mut VisualTestContext, edge| {
            draw(visual);
            visual.update(|_, cx| {
                let editor = editor.read(cx);
                assert_eq!(
                    editor.scroll_offset(),
                    edge + point(px(0.), editor.line_height().unwrap() * 2.)
                );
            });
        };
        let edge = place_caret_near_top(visual);
        visual.update(|window, cx| {
            editor.update(cx, |editor, cx| {
                editor.edit_ranges(&[(caret..caret, "x".into())], window, cx);
            });
        });
        assert_margin(visual, edge);

        let edge = place_caret_near_top(visual);
        visual.simulate_keystrokes("ctrl-z");
        assert_margin(visual, edge);
        assert_eq!(text(visual, editor), source);

        let edge = place_caret_near_top(visual);
        visual.simulate_keystrokes("ctrl-y");
        assert_margin(visual, edge);
        assert_eq!(&text(visual, editor)[caret..caret + 1], "x");
    });
}

#[gpui::test]
fn fold_candidates_survive_edits_while_the_highlighter_reparses(cx: &mut TestAppContext) {
    use std::time::Duration;

    const SOURCE: &str = "fn main() {\n    let number = 1;\n    println!(\"{number}\");\n}\n";

    let mut editor = None;
    let (_, visual) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        let state = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("rust")
                .folding(true)
                .default_value(SOURCE);
            state.ensure_highlighter_factory(
                gpui_component::highlighter::input_highlighter_factory(),
            );
            state.prepare_language(window, cx);
            state
        });
        editor = Some(state.clone());
        let root = cx.new(|_| EditorView {
            editor: state,
            readonly: false,
        });
        Root::new(root, window, cx)
    });
    let editor = editor.unwrap();
    let finish_parse = |visual: &mut VisualTestContext| {
        visual.run_until_parked();
        visual.executor().advance_clock(Duration::from_millis(100));
        visual.run_until_parked();
        draw(visual);
    };
    let is_candidate = |visual: &mut VisualTestContext, row| {
        visual.update(|_, cx| editor.read(cx).display_map().is_fold_candidate(row))
    };

    finish_parse(visual);
    assert!(is_candidate(visual, 0));

    let number = SOURCE.find("1;").unwrap();
    visual.update(|window, cx| {
        editor.update(cx, |state, cx| {
            state.replace_text_in_range(Some(number..number + 1), "42", window, cx);
            assert!(state.display_map().is_fold_candidate(0));
            state.replace_text_in_range(Some(0..0), "\n", window, cx);
            assert!(!state.display_map().is_fold_candidate(0));
            assert!(state.display_map().is_fold_candidate(1));
            state.replace_text_in_range(Some(0..1), "", window, cx);
            assert!(state.display_map().is_fold_candidate(0));
        });
    });
    finish_parse(visual);
    assert!(is_candidate(visual, 0));

    visual.update(|window, cx| {
        editor.update(cx, |state, cx| {
            let end = state.text().len();
            state.replace_text_in_range(Some(0..end), "fn main() {}\n", window, cx);
        });
    });
    finish_parse(visual);
    assert!(!is_candidate(visual, 0));
}
