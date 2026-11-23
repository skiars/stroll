#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{env, path::PathBuf, process::ExitCode, sync::Arc};

use anyhow::{Result, ensure};
use gpui::*;
use gpui_component::{Root, TitleBar};
use stroll::config::ConfigFile;
use stroll::locale::text;
use stroll::repository::{Location, Store};
use stroll::workspace::{
    CloseRepositoryPicker, CloseSettings, CloseTab, CompleteRepositoryPath, FindInDocument,
    FindReferences, FormatDocument, NextRepositoryPath, NextTab, OpenLocator, OpenRepository,
    PreviousRepositoryPath, PreviousTab, SaveFile, SwitchWorkspaceView, ToggleSettings, ToggleWrap,
    Workspace, WorkspaceView, repository_window_bounds,
};

fn primary<'a>(mac: &'a str, other: &'a str) -> &'a str {
    if cfg!(target_os = "macos") {
        mac
    } else {
        other
    }
}

fn bind_workspace_keys(cx: &mut App) {
    let view_context = Some("!Locator && !RepoPicker && !SettingsPage");
    for (view, control, command) in [
        (WorkspaceView::Docs, "ctrl-1", "cmd-1"),
        (WorkspaceView::Editor, "ctrl-2", "cmd-2"),
        (WorkspaceView::Git, "ctrl-3", "cmd-3"),
    ] {
        let action = SwitchWorkspaceView(view);
        cx.bind_keys([KeyBinding::new(control, action, view_context)]);
        if cfg!(target_os = "macos") {
            cx.bind_keys([KeyBinding::new(
                command,
                action,
                Some("!Terminal && !Locator && !RepoPicker && !SettingsPage"),
            )]);
        }
    }
    cx.bind_keys([
        KeyBinding::new(
            primary("cmd-p", "ctrl-p"),
            OpenLocator,
            Some("!Terminal && !SettingsPage"),
        ),
        KeyBinding::new("escape", gpui_component::input::Escape, Some("Locator")),
        KeyBinding::new(
            "escape",
            gpui_component::input::Escape,
            Some("Locator > Input"),
        ),
        KeyBinding::new(
            if cfg!(target_os = "macos") {
                "cmd-,"
            } else {
                "ctrl-,"
            },
            ToggleSettings,
            Some("!Locator"),
        ),
        KeyBinding::new("escape", CloseSettings, Some("SettingsPage")),
        KeyBinding::new(
            "f12",
            gpui_component::input::GoToDefinition,
            Some("Input && mode == editor && !Locator"),
        ),
        #[cfg(target_os = "macos")]
        KeyBinding::new(
            "alt-escape",
            gpui_component::input::ShowCompletions,
            Some("WorkspaceEditor > (Input && mode == editor && !Locator)"),
        ),
        KeyBinding::new(
            "ctrl-space",
            gpui_component::input::ShowCompletions,
            Some("WorkspaceEditor > (Input && mode == editor && !Locator)"),
        ),
        KeyBinding::new(
            "shift-f12",
            FindReferences,
            Some("WorkspaceEditor > (Input && mode == editor && !Locator)"),
        ),
        KeyBinding::new(
            primary("cmd-o", "ctrl-o"),
            OpenRepository,
            Some("!Terminal && !Locator"),
        ),
        KeyBinding::new("escape", CloseRepositoryPicker, Some("RepoPicker")),
        KeyBinding::new("escape", CloseRepositoryPicker, Some("RepoPicker > Input")),
        KeyBinding::new("down", NextRepositoryPath, Some("RepoPicker > Input")),
        KeyBinding::new(
            "up",
            PreviousRepositoryPath,
            Some("RepoPathSuggestions > Input"),
        ),
        KeyBinding::new(
            "tab",
            CompleteRepositoryPath,
            Some("RepoPathSuggestions > Input"),
        ),
        KeyBinding::new(
            primary("cmd-f", "ctrl-f"),
            FindInDocument,
            Some("Reader && !Locator"),
        ),
        KeyBinding::new(
            primary("cmd-s", "ctrl-s"),
            SaveFile,
            Some("!Terminal && !Locator"),
        ),
        KeyBinding::new(
            "alt-shift-f",
            FormatDocument,
            Some("WorkspaceEditor > (Input && mode == editor && !Locator)"),
        ),
        KeyBinding::new("alt-z", ToggleWrap, Some("!Terminal && !Locator")),
        KeyBinding::new(
            primary("cmd-w", "ctrl-w"),
            CloseTab,
            Some("WorkspaceEditor && !Terminal && !Locator"),
        ),
        KeyBinding::new(
            "ctrl-tab",
            NextTab,
            Some("WorkspaceEditor && !Terminal && !Locator"),
        ),
        KeyBinding::new(
            "ctrl-shift-tab",
            PreviousTab,
            Some("WorkspaceEditor && !Terminal && !Locator"),
        ),
    ]);
}

fn launch() -> Result<()> {
    let config = ConfigFile::load_for_desktop();
    let mut arguments = env::args_os().skip(1).peekable();
    let view = match arguments.peek().and_then(|value| value.to_str()) {
        Some("--docs") => Some(WorkspaceView::Docs),
        Some("--git") => Some(WorkspaceView::Git),
        _ => None,
    };
    if view.is_some() {
        arguments.next();
    }
    let first = arguments.next();
    if first.as_deref().is_some_and(|value| value == "--help") {
        println!("{}", text("cli.usage"));
        return Ok(());
    }
    let first = if first.as_deref().is_some_and(|value| value == "--ssh") {
        let host = arguments
            .next()
            .ok_or_else(|| anyhow::anyhow!(text("cli.ssh_host_required")))?;
        let path = arguments
            .next()
            .ok_or_else(|| anyhow::anyhow!(text("cli.ssh_path_required")))?;
        let path = path
            .to_str()
            .filter(|path| path.starts_with('/'))
            .ok_or_else(|| anyhow::anyhow!(text("cli.ssh_path_required")))?;
        Some(format!("ssh://{}{path}", host.to_string_lossy()).into())
    } else {
        first
    };
    let requested = first.map(|root| Location {
        root: PathBuf::from(root),
        file: arguments.next().map(PathBuf::from),
    });
    ensure!(arguments.next().is_none(), text("cli.usage"));
    let store = Arc::new(Store::from_environment()?);
    let standalone = requested
        .as_ref()
        .filter(|location| {
            location.file.is_none() && view != Some(WorkspaceView::Git) && location.root.is_file()
        })
        .map(|location| location.root.clone());
    gpui_platform::application()
        .with_assets(gpui_kit_assets::AllAssets)
        .run(move |cx| {
            gpui_component::init(cx);
            Workspace::bind_terminal_keys(cx);
            bind_workspace_keys(cx);
            if let Some(path) = standalone {
                if let Err(error) = Workspace::open_standalone(store, config, path, None, cx) {
                    eprintln!("{error:#}");
                    cx.quit();
                    return;
                }
            } else {
                let mut options = TitleBar::window_options();
                if let Some(titlebar) = options.titlebar.as_mut() {
                    titlebar.title = Some("Stroll".into());
                }
                options.app_id = Some("io.stroll.Stroll".to_owned());
                options.window_decorations = Some(WindowDecorations::Client);
                options.window_background = WindowBackgroundAppearance::Transparent;
                options.window_min_size = Some(size(px(740.), px(480.)));
                options.window_bounds = Some(repository_window_bounds(
                    &store,
                    size(px(740.), px(480.)),
                    cx,
                ));
                cx.open_window(options, move |window, cx| {
                    let view =
                        cx.new(|cx| Workspace::with_startup_view(store, config, view, window, cx));
                    let weak = view.downgrade();
                    window.on_window_should_close(cx, move |window, cx| {
                        weak.update(cx, |workspace, cx| {
                            workspace.request_window_close(window, cx)
                        })
                        .unwrap_or(true)
                    });
                    view.update(cx, |workspace, cx| {
                        if let Some(location) = requested {
                            workspace.open_repository(location, window, cx);
                        } else {
                            workspace.restore_repository(window, cx);
                        }
                    });
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect(&text("cli.open_failed"));
            }
            cx.activate(true);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
        });
    Ok(())
}

fn main() -> ExitCode {
    if let Err(error) = launch() {
        eprintln!("{error:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod shortcut_tests {
    use super::*;

    struct Probe {
        focus: FocusHandle,
        context: &'static str,
        seen: Vec<&'static str>,
    }

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .key_context(self.context)
                .child(
                    div()
                        .key_context("Input mode=editor")
                        .track_focus(&self.focus),
                )
                .on_action(cx.listener(|this, _: &SaveFile, _, _| this.seen.push("save")))
                .on_action(cx.listener(|this, _: &OpenRepository, _, _| this.seen.push("open")))
                .on_action(cx.listener(|this, _: &CloseTab, _, _| this.seen.push("close")))
                .on_action(cx.listener(|this, _: &OpenLocator, _, _| this.seen.push("locator")))
                .on_action(cx.listener(|this, _: &FindInDocument, _, _| this.seen.push("find")))
                .on_action(cx.listener(|this, action: &SwitchWorkspaceView, _, _| {
                    this.seen.push(match action.0 {
                        WorkspaceView::Docs => "docs",
                        WorkspaceView::Editor => "workspace",
                        WorkspaceView::Git => "git",
                    })
                }))
                .on_action(
                    cx.listener(|this, _: &gpui_component::input::ShowCompletions, _, _| {
                        this.seen.push("complete")
                    }),
                )
                .on_action(
                    cx.listener(|this, _: &gpui_component::input::GoToDefinition, _, _| {
                        this.seen.push("definition")
                    }),
                )
                .on_action(
                    cx.listener(|this, _: &FindReferences, _, _| this.seen.push("references")),
                )
        }
    }

    #[::core::prelude::v1::test]
    fn platform_shortcuts_dispatch_in_their_intended_contexts() {
        let mut cx = TestAppContext::single();
        cx.update(bind_workspace_keys);
        let (probe, visual) = cx.add_window_view(|window, cx| {
            let focus = cx.focus_handle();
            focus.focus(window, cx);
            Probe {
                focus,
                context: "WorkspaceEditor",
                seen: Vec::new(),
            }
        });
        visual.simulate_keystrokes(primary(
            "cmd-s cmd-o cmd-w cmd-p",
            "ctrl-s ctrl-o ctrl-w ctrl-p",
        ));
        visual.simulate_keystrokes("ctrl-space f12 shift-f12");
        #[cfg(target_os = "macos")]
        visual.simulate_keystrokes("alt-escape");
        visual.update(|_, cx| {
            let expected = if cfg!(target_os = "macos") {
                vec![
                    "save",
                    "open",
                    "close",
                    "locator",
                    "complete",
                    "definition",
                    "references",
                    "complete",
                ]
            } else {
                vec![
                    "save",
                    "open",
                    "close",
                    "locator",
                    "complete",
                    "definition",
                    "references",
                ]
            };
            assert_eq!(probe.read(cx).seen, expected);
        });
        for (context, keys, expected) in [
            ("Reader", primary("cmd-f", "ctrl-f"), vec!["find"]),
            (
                "Terminal",
                primary("cmd-s cmd-o cmd-w cmd-p", "ctrl-s ctrl-o ctrl-w ctrl-p"),
                vec![],
            ),
            (
                "Locator",
                primary("cmd-s cmd-o cmd-w", "ctrl-s ctrl-o ctrl-w"),
                vec![],
            ),
        ] {
            visual.update(|window, cx| {
                probe.update(cx, |this, cx| {
                    this.context = context;
                    this.seen.clear();
                    cx.notify();
                });
                window.draw(cx).clear(cx);
            });
            visual.simulate_keystrokes(keys);
            visual.update(|_, cx| assert_eq!(probe.read(cx).seen, expected, "context {context}"));
        }
        #[cfg(target_os = "macos")]
        {
            visual.update(|window, cx| {
                probe.update(cx, |this, cx| {
                    this.context = "WorkspaceEditor";
                    this.seen.clear();
                    cx.notify();
                });
                window.draw(cx).clear(cx);
            });
            visual.simulate_keystrokes("ctrl-s ctrl-o ctrl-w ctrl-p");
            visual.update(|_, cx| assert!(probe.read(cx).seen.is_empty()));
        }
    }
    #[::core::prelude::v1::test]
    fn control_view_shortcuts_work_in_terminal_and_respect_overlays() {
        let mut cx = TestAppContext::single();
        cx.update(bind_workspace_keys);
        let (probe, visual) = cx.add_window_view(|window, cx| {
            let focus = cx.focus_handle();
            focus.focus(window, cx);
            Probe {
                focus,
                context: "WorkspaceEditor",
                seen: Vec::new(),
            }
        });
        let command_expected: &[&str] = if cfg!(target_os = "macos") {
            &["docs", "workspace", "git"]
        } else {
            &[]
        };
        for context in [
            "WorkspaceEditor",
            "Reader",
            "Terminal",
            "Locator",
            "RepoPicker",
            "SettingsPage",
        ] {
            visual.update(|window, cx| {
                probe.update(cx, |this, cx| {
                    this.context = context;
                    cx.notify();
                });
                window.draw(cx).clear(cx);
            });
            let views = &["docs", "workspace", "git"][..];
            let in_workspace = matches!(context, "WorkspaceEditor" | "Reader");
            for (keys, expected) in [
                (
                    "ctrl-1 ctrl-2 ctrl-3",
                    if in_workspace || context == "Terminal" {
                        views
                    } else {
                        &[]
                    },
                ),
                (
                    "cmd-1 cmd-2 cmd-3",
                    if in_workspace { command_expected } else { &[] },
                ),
            ] {
                visual.simulate_keystrokes(keys);
                visual.update(|_, cx| {
                    probe.update(cx, |this, _| {
                        assert_eq!(
                            std::mem::take(&mut this.seen),
                            expected,
                            "{context}: {keys}",
                        );
                    });
                });
            }
        }
    }
}
