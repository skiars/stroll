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
    PreviousRepositoryPath, PreviousTab, SaveFile, ToggleSettings, ToggleTerminal, ToggleWrap,
    Workspace, WorkspaceView, repository_window_bounds,
};

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
            if cfg!(target_os = "macos") {
                cx.bind_keys([KeyBinding::new(
                    "cmd-p",
                    OpenLocator,
                    Some("!Terminal && !SettingsPage"),
                )]);
            }
            cx.bind_keys([
                KeyBinding::new("ctrl-p", OpenLocator, Some("!Terminal && !SettingsPage")),
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
                KeyBinding::new("ctrl-o", OpenRepository, Some("!Terminal && !Locator")),
                KeyBinding::new("ctrl-`", ToggleTerminal, Some("!Locator")),
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
                KeyBinding::new("ctrl-f", FindInDocument, Some("Reader && !Locator")),
                KeyBinding::new("ctrl-s", SaveFile, Some("!Terminal && !Locator")),
                KeyBinding::new(
                    "alt-shift-f",
                    FormatDocument,
                    Some("WorkspaceEditor > (Input && mode == editor && !Locator)"),
                ),
                KeyBinding::new("alt-z", ToggleWrap, Some("!Terminal && !Locator")),
                KeyBinding::new(
                    "ctrl-w",
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
    // The local Git helper runs Git over stdio without initializing the UI or user state.
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--git-helper")) {
        return match stroll::remote::serve() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error:#}");
                ExitCode::FAILURE
            }
        };
    }
    if let Err(error) = launch() {
        eprintln!("{error:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
