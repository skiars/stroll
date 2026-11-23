use std::fs;

use anyhow::Result;

use super::completion_tests::{frame, wait, with_source_editor};

#[test]
#[ignore = "requires clangd on PATH"]
fn workspace_newline_uses_clang_format_only_for_the_new_indent() -> Result<()> {
    for (path, width) in [("main.c", 4), ("main.cpp", 6)] {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repository");
        fs::create_dir(&root)?;
        fs::write(
            root.join(".clang-format"),
            format!("BasedOnStyle: LLVM\nIndentWidth: {width}\n"),
        )?;
        let source = "void f() {\n  int x;\n}\n";
        let caret = source.find(';').unwrap() + 1;
        with_source_editor(
            fixture.path(),
            path,
            "[lsp.clangd]\ncommand = 'clangd'\n",
            source,
            caret,
            |visual, editor| {
                visual.simulate_keystrokes("enter");
                let expected = format!("void f() {{\n  int x;\n{}\n}}\n", " ".repeat(width));
                wait(visual, "clangd newline indentation", |visual| {
                    visual.update(|_, cx| {
                        let state = editor.read(cx);
                        state.text().to_string() == expected && state.cursor() == caret + 1 + width
                    })
                })?;

                let braces = "void f() {}\n";
                let caret = braces.find('}').unwrap();
                visual.update(|window, cx| {
                    editor.update(cx, |state, cx| {
                        state.set_value(braces, window, cx);
                        state.set_selected_range(caret..caret, cx);
                    });
                });
                frame(visual);
                visual.simulate_keystrokes("enter");
                let expected = format!("void f() {{\n{}\n}}\n", " ".repeat(width));
                wait(visual, "clangd paired-brace indentation", |visual| {
                    visual.update(|_, cx| {
                        let state = editor.read(cx);
                        state.text().to_string() == expected && state.cursor() == caret + 1 + width
                    })
                })
            },
        )?;
    }
    Ok(())
}

#[test]
#[ignore = "requires rust-analyzer on PATH"]
fn workspace_newline_uses_rust_analyzer_paired_brace_indent() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("repository");
    fs::create_dir(&root)?;
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = 'indent-fixture'\nversion = '0.0.0'\nedition = '2024'\n[lib]\npath = 'lib.rs'\n",
    )?;
    fs::write(root.join("rustfmt.toml"), "tab_spaces = 6\n")?;
    let source = "fn f() {}\n";
    let caret = source.find('}').unwrap();
    with_source_editor(
        fixture.path(),
        "lib.rs",
        "[lsp.rust]\ncommand = 'rust-analyzer'\n",
        source,
        caret,
        |visual, editor| {
            visual.simulate_keystrokes("enter");
            wait(visual, "rust-analyzer newline indentation", |visual| {
                visual.update(|_, cx| {
                    let state = editor.read(cx);
                    state.text().to_string() == "fn f() {\n    \n}\n" && state.cursor() == caret + 5
                })
            })
        },
    )
}

#[test]
fn workspace_uses_builtin_indentation_offline() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let source = "<template>\n  <MyCard></MyCard>\n</template>\n";
    let caret = source.find("</MyCard>").unwrap();
    with_source_editor(
        fixture.path(),
        "App.vue",
        "[lsp]\nenabled = false",
        source,
        caret,
        |visual, editor| {
            visual.simulate_keystrokes("enter");
            frame(visual);
            visual.update(|_, cx| {
                let state = editor.read(cx);
                let text = state.text().to_string();
                let inner = text.lines().nth(2).unwrap();
                assert!(inner.trim().is_empty() && inner.len() > 2, "{text}");
                assert_eq!(text.lines().nth(3), Some("  </MyCard>"));
                assert_eq!(state.cursor(), caret + 1 + inner.len());
            });
            Ok(())
        },
    )
}
