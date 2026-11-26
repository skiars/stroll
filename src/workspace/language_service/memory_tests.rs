use std::fs;

use anyhow::Result;
use futures_lite::future::block_on;
use gpui::{VisualTestContext, px, size};
use gpui_component::input::Rope;

use super::completion_tests::{frame, wait, with_workspace_editor};

#[test]
fn language_service_panel_samples_the_current_process_tree() -> Result<()> {
    const CHILD_RSS_INCREASE: u64 = 16 * 1024 * 1024;

    let fixture = tempfile::tempdir()?;
    let script = fixture.path().join("server.py");
    fs::write(
        &script,
        include_str!("../../../tests/fixtures/lsp_memory.py"),
    )?;
    let config = format!("[lsp.rust]\ncommand = 'python3 -u {}'\n", script.display());
    let source = "fn main() {}\n";
    with_workspace_editor(
        fixture.path(),
        "main.rs",
        &config,
        source,
        0,
        |visual, workspace, _| {
            let click_trigger = |visual: &mut VisualTestContext| {
                let trigger = visual.debug_bounds("status-language-service").unwrap();
                visual.simulate_click(trigger.center(), Default::default());
                frame(visual);
            };
            let memory = |visual: &mut VisualTestContext| {
                visual.update(|_, cx| workspace.read(cx).language_service_memory())
            };
            let (process, document) = visual.update(|_, cx| {
                let view = workspace.read(cx);
                let editor = view
                    .active_file()
                    .unwrap()
                    .language_server
                    .as_ref()
                    .unwrap();
                (
                    view.language_memory_process().unwrap(),
                    editor.document.clone(),
                )
            });
            assert!(memory(visual).is_none());
            click_trigger(visual);
            assert!(
                visual
                    .debug_bounds("language-service-server: python3 · 2.3.4")
                    .is_some()
            );
            wait(visual, "first LSP memory sample", |visual| {
                memory(visual).is_some_and(|sample| sample.bytes > 0)
            })?;
            let initial = memory(visual).unwrap();
            assert!(visual.debug_bounds("language-service-memory").is_some());

            block_on(document.hover(Rope::from(source), 0))?;
            wait(visual, "LSP child process memory", |visual| {
                memory(visual)
                    .is_some_and(|sample| sample.bytes >= initial.bytes + CHILD_RSS_INCREASE)
            })?;
            let with_child = memory(visual).unwrap();

            click_trigger(visual);
            assert!(visual.update(|_, cx| workspace.read(cx).lsp_panel.is_none()));
            assert!(memory(visual).is_none());
            wait(visual, "LSP panel dismissal", |visual| {
                visual.debug_bounds("language-service-memory").is_none()
            })?;
            block_on(document.hover(Rope::from(source), 0))?;

            click_trigger(visual);
            wait(visual, "LSP memory after reopening the panel", |visual| {
                memory(visual)
                    .is_some_and(|sample| sample.bytes + CHILD_RSS_INCREASE <= with_child.bytes)
            })?;
            visual.update(|_, cx| {
                workspace.update(cx, |view, cx| {
                    view.control_language_service(true, cx);
                    assert!(view.language_service_memory().is_none());
                });
            });
            wait(visual, "replacement LSP process sample", |visual| {
                visual.update(|_, cx| {
                    let view = workspace.read(cx);
                    view.language_memory_process()
                        .is_some_and(|current| current != process)
                        && view
                            .language_service_memory()
                            .is_some_and(|sample| sample.bytes > 0)
                })
            })?;

            visual.simulate_resize(size(px(520.), px(480.)));
            frame(visual);
            let row = visual.debug_bounds("language-service-memory").unwrap();
            assert!(row.left() >= px(0.) && row.right() <= px(520.));
            Ok(())
        },
    )
}
