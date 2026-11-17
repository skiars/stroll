use std::{fs, path::Path};

use anyhow::Result;
use gpui::{Entity, VisualTestContext};
use gpui_component::input::EditorState;

use super::{
    Workspace,
    completion_tests::{frame, labels, wait, with_workspace_editor},
};

fn jump(
    visual: &mut VisualTestContext,
    workspace: &Entity<Workspace>,
    source_offset: usize,
    target: &str,
    target_offset: usize,
) -> Result<()> {
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            let source = Path::new("working.md");
            workspace.open_file(source.to_owned(), false, window, cx);
            workspace.files[source].editor.update(cx, |editor, cx| {
                editor.focus(window, cx);
                editor.set_selected_range(source_offset..source_offset, cx);
            });
        });
    });
    frame(visual);
    visual.simulate_keystrokes("f12");
    wait(visual, target, |visual| {
        visual.update(|_, cx| {
            workspace.read(cx).active_file().is_some_and(|file| {
                file.path == Path::new(target) && file.editor.read(cx).cursor() == target_offset
            })
        })
    })
}

#[test]
fn workspace_markdown_links_without_lsp() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let docs = fixture.path().join("repository/docs");
    fs::create_dir_all(&docs)?;
    let guide = "# Guide\n\n## 标题\n\n## 标题\n";
    fs::write(docs.join("guide space.md"), guide)?;
    fs::write(docs.join("example.txt"), "first\nsecond\n")?;
    let source = concat!(
        "# Overview\n\n",
        "😀 [local](#overview)\n",
        "[other](docs/guide%20space.md#%E6%A0%87%E9%A2%98-1)\n",
        "[file](docs/example.txt)\n",
        "[line](docs/example.txt#L2)\n",
    );
    with_workspace_editor(
        fixture.path(),
        "working.md",
        "[lsp]\nenabled = false\n",
        source,
        0,
        |visual, workspace, _| {
            jump(
                visual,
                workspace,
                source.find("#overview").unwrap(),
                "working.md",
                0,
            )?;
            jump(
                visual,
                workspace,
                source.find("[other]").unwrap() + 1,
                "docs/guide space.md",
                guide.rfind("## 标题").unwrap(),
            )?;
            jump(
                visual,
                workspace,
                source.find("[file]").unwrap() + 1,
                "docs/example.txt",
                0,
            )?;
            jump(
                visual,
                workspace,
                source.find("[line]").unwrap() + 1,
                "docs/example.txt",
                "first\n".len(),
            )
        },
    )
}

#[test]
#[ignore = "requires a Marksman entry in STROLL_LSP_CONFIG"]
fn workspace_markdown_with_marksman() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("repository");
    fs::create_dir_all(root.join("docs"))?;
    let guide = "# Guide\n\n## Target heading\n";
    fs::write(root.join("guide.md"), guide)?;
    fs::write(root.join("docs/nested.md"), "# Nested\n")?;
    let source = "😀 [label][chapter]\n\n[chapter]: guide.md#target-heading\n\n[[guide#target-heading]]\n\n[open](./gu)\n\n[nested]\n";
    let caret = source.find("./gu").unwrap() + "./gu".len();
    with_workspace_editor(
        fixture.path(),
        "working.md",
        "",
        source,
        caret,
        |visual, workspace, editor| {
            visual.simulate_keystrokes("ctrl-space");
            wait(visual, "Marksman file completion", |visual| {
                labels(visual, editor) == ["guide.md"]
            })?;
            visual.simulate_keystrokes("enter");
            wait(visual, "Marksman completion insertion", |visual| {
                visual.update(|_, cx| {
                    editor.read(cx).text().to_string() == source.replace("./gu", "/guide.md")
                })
            })?;
            visual.update(|_, cx| {
                editor.update(cx, |editor, cx| {
                    let end =
                        editor.text().to_string().find("[nested]").unwrap() + "[nested]".len();
                    editor.set_selected_range(end..end, cx);
                });
            });
            visual.simulate_keystrokes("(");
            wait(visual, "Marksman automatic link completion", |visual| {
                labels(visual, editor)
                    .iter()
                    .any(|label| label == "docs/nested.md")
            })?;
            for key in ["d", "o", "c", "s", "/", "n", "e"] {
                visual.simulate_keystrokes(key);
                frame(visual);
            }
            wait(
                visual,
                "Marksman completion after a path separator",
                |visual| labels(visual, editor) == ["docs/nested.md"],
            )?;
            visual.simulate_keystrokes("enter");
            wait(visual, "Marksman nested path insertion", |visual| {
                visual.update(|_, cx| {
                    editor
                        .read(cx)
                        .text()
                        .to_string()
                        .contains("[nested](/docs/nested.md)")
                })
            })?;
            jump(
                visual,
                workspace,
                source.find("[chapter]").unwrap() + 1,
                "working.md",
                source.find("[chapter]:").unwrap(),
            )?;
            jump(
                visual,
                workspace,
                source.find("[[guide#target-heading]]").unwrap() + "[[guide#tar".len(),
                "guide.md",
                guide.find("## Target heading").unwrap(),
            )
        },
    )
}

#[test]
fn workspace_markdown_enter_continues_markup_and_undoes_it_as_one_edit() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    with_workspace_editor(
        fixture.path(),
        "working.MD",
        "[lsp]\nenabled = false\n",
        "",
        0,
        |visual, _, editor| {
            for (input, key, output) in [
                ("- 内容|", "enter", "- 内容\n- |"),
                ("+ item|\n+ later", "enter", "+ item\n+ |\n+ later"),
                ("009) item|", "enter", "009) item\n010) |"),
                ("- [X] done|", "enter", "- [X] done\n- [ ] |"),
                ("- [ ] |\nnext", "enter", "|\nnext"),
                (
                    "- parent\n  - child|",
                    "enter",
                    "- parent\n  - child\n  - |",
                ),
                ("- parent\n  - |", "enter", "- parent\n  |"),
                ("> - parent\n>   - |", "enter", "> - parent\n>   |"),
                (">\t-\titem|", "enter", ">\t-\titem\n>\t-\t|"),
                (
                    "> > first\n> > second|",
                    "enter",
                    "> > first\n> > second\n> > |",
                ),
                ("> > first\n> > |", "enter", "> > first\n> |"),
                ("- > quoted|", "enter", "- > quoted\n  > |"),
                ("- - child|", "enter", "- - child\n  - |"),
                ("> first\nlazy|", "enter", "> first\nlazy\n|"),
                ("- item| here", "enter", "- item\n| here"),
                ("- item|", "shift-enter", "- item\n|"),
                (
                    "- parent\n  \x60\x60\x60\n  - code|\n  \x60\x60\x60",
                    "enter",
                    "- parent\n  \x60\x60\x60\n  - code\n  |\n  \x60\x60\x60",
                ),
                ("    - code|", "enter", "    - code\n    |"),
                ("---|", "enter", "---\n|"),
            ] {
                let split_caret = |text: &str| {
                    let offset = text.find('|').unwrap();
                    (text.replace('|', ""), offset)
                };
                let (before, before_cursor) = split_caret(input);
                let (after, after_cursor) = split_caret(output);
                visual.update(|window, cx| {
                    editor.update(cx, |state, cx| {
                        state.set_value(before.as_str(), window, cx);
                        state.set_selected_range(before_cursor..before_cursor, cx);
                        state.focus(window, cx);
                    });
                    window.draw(cx).clear(cx);
                });
                frame(visual);
                visual.simulate_keystrokes(key);
                let check = |state: &EditorState, value: &str, cursor| {
                    assert_eq!(state.text().to_string(), value, "{input:?} via {key}");
                    assert_eq!(
                        state.selected_range(),
                        cursor..cursor,
                        "{input:?} via {key}"
                    );
                };
                visual.update(|window, cx| {
                    editor.update(cx, |state, cx| {
                        check(state, &after, after_cursor);
                        state.replay_text_history(true, window, cx);
                        check(state, &before, before_cursor);
                        state.replay_text_history(false, window, cx);
                        check(state, &after, after_cursor);
                    });
                });
            }
            Ok(())
        },
    )
}
