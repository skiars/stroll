use std::{fs, path::Path};

use anyhow::Result;
use gpui::{Entity, VisualTestContext};
use gpui_component::input::EditorState;

use super::completion_tests::{frame, labels, wait, with_workspace_editor};

fn schema_fixture() -> Result<tempfile::TempDir> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("repository");
    fs::create_dir_all(&root)?;
    fs::write(
        root.join("data-schema.json"),
        r#"{
        "type": "object",
        "properties": {"mode": {"enum": ["fast", "slow"]}, "count": {"type": "number"}}
    }"#,
    )?;
    Ok(fixture)
}

fn wait_for_schema_diagnostic(
    visual: &mut VisualTestContext,
    editor: &Entity<EditorState>,
) -> Result<()> {
    wait(visual, "schema enum diagnostic", |visual| {
        visual.update(|_, cx| {
            editor.read(cx).diagnostics().is_some_and(|diagnostics| {
                diagnostics
                    .iter()
                    .any(|item| item.message.contains("fast") && item.message.contains("slow"))
            })
        })
    })
}

fn format(
    visual: &mut VisualTestContext,
    editor: &Entity<EditorState>,
    expected: &str,
) -> Result<()> {
    visual.simulate_keystrokes("alt-shift-f");
    wait(visual, "formatted document", |visual| {
        visual.update(|_, cx| editor.read(cx).text().to_string().contains(expected))
    })
}

#[test]
#[ignore = "requires the managed vscode bundle in STROLL_HOME"]
fn workspace_configuration_with_json_server() -> Result<()> {
    for (file, comment) in [
        ("sample.json", ""),
        ("sample.jsonc", "\n// Keep this comment\n"),
    ] {
        let fixture = schema_fixture()?;
        let source =
            format!(r#"{{{comment}"$schema":"./data-schema.json","mode":"broken","count":2}}"#);
        with_workspace_editor(
            fixture.path(),
            file,
            "",
            &source,
            0,
            |visual, workspace, editor| {
                let expected_id = Path::new(file).extension().unwrap().to_str().unwrap();
                visual.update(|_, cx| {
                    let workspace = workspace.read(cx);
                    let project = workspace
                        .active_file()
                        .unwrap()
                        .server_project
                        .as_ref()
                        .unwrap();
                    assert_eq!(project.language_id.as_ref(), expected_id);
                });
                wait_for_schema_diagnostic(visual, editor)?;
                format(visual, editor, "  \"mode\": \"broken\",\n  \"count\": 2")?;
                if !comment.is_empty() {
                    visual.update(|_, cx| {
                        assert!(
                            editor
                                .read(cx)
                                .text()
                                .to_string()
                                .contains("// Keep this comment")
                        );
                    });
                }
                Ok(())
            },
        )?;
    }
    Ok(())
}

#[test]
#[ignore = "requires the managed YAML bundle in STROLL_HOME"]
fn workspace_configuration_with_yaml_server() -> Result<()> {
    let fixture = schema_fixture()?;
    let source = "# yaml-language-server: $schema=./data-schema.json\nmode: broken\ncount:   2\n";
    with_workspace_editor(
        fixture.path(),
        "sample.yml",
        "",
        source,
        0,
        |visual, _, editor| {
            wait_for_schema_diagnostic(visual, editor)?;
            format(visual, editor, "mode: broken\ncount: 2\n")
        },
    )
}

#[test]
#[ignore = "requires the managed Tombi bundle in STROLL_HOME"]
fn workspace_configuration_with_tombi() -> Result<()> {
    let fixture = schema_fixture()?;
    fs::write(
        fixture.path().join("repository/tombi.toml"),
        "[format.rules]\nkey-value-equals-sign-space-width = 2\n",
    )?;
    let source = "#:schema ./data-schema.json\nmode=\"broken\"\ncount=2\n";
    with_workspace_editor(
        fixture.path(),
        "sample.toml",
        "",
        source,
        0,
        |visual, _, editor| {
            wait_for_schema_diagnostic(visual, editor)?;
            format(visual, editor, "mode  =  \"broken\"\ncount  =  2")?;

            let completion_source = "#:schema ./data-schema.json\nmode = \n";
            visual.update(|window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.replace_all(completion_source, window, cx);
                    let caret = completion_source.len() - 1;
                    editor.set_selected_range(caret..caret, cx);
                });
            });
            frame(visual);
            visual.simulate_keystrokes("ctrl-space");
            wait(visual, "Tombi schema completion", |visual| {
                labels(visual, editor) == ["\"fast\"", "\"slow\""]
            })?;
            visual.simulate_keystrokes("enter");
            wait(visual, "Tombi completion insertion", |visual| {
                visual.update(|_, cx| {
                    editor.read(cx).text().to_string()
                        == "#:schema ./data-schema.json\nmode = \"fast\"\n"
                })
            })
        },
    )
}
