use super::*;
use crate::{config::ConfigFile, workspace::WorkspaceView};
use gpui_component::{Theme, scroll::ScrollbarMode};
use std::{fs, process::Command, sync::Arc};

fn frame(visual: &mut VisualTestContext) {
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear(cx));
}

#[::core::prelude::v1::test]
fn repository_picker_keeps_switches_in_place_and_submits_the_displayed_action() -> anyhow::Result<()>
{
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().canonicalize()?;
    let current = root.join("current");
    let sandbox = root.join("sandbox");
    let nested = sandbox.join("nested");
    let store = Arc::new(Store::new(root.join("state"))?);
    let other_repos = (0..7).map(|ix| sandbox.join(format!("other-{ix}")));
    for path in [current.clone(), nested.clone()]
        .into_iter()
        .chain(other_repos)
    {
        fs::create_dir_all(&path)?;
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&path)
                .status()?
                .success()
        );
        store.remember_repository(&Repository::open(&path)?)?;
    }
    let config = ConfigFile::load(Some(root.join("config.toml")));
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        crate::test_support::text_system(true),
    );
    app.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
    });
    let (workspace, visual) = app.add_window_view(|window, cx| {
        Workspace::with_config(store, config, WorkspaceView::Editor, window, cx)
    });
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.repository_ui.home = Some(root.join("empty-home"));
            workspace.open_repository(
                Location {
                    root: current.clone(),
                    file: None,
                },
                window,
                cx,
            );
        });
    });
    frame(visual);
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            assert_eq!(workspace.repository.as_ref().unwrap().root, current);
            workspace.choose_repository(&OpenRepository, window, cx);
        });
    });

    const SELECTORS: [&str; 5] = [
        "repository-picker-surface",
        "repository-location:local",
        "repository-location:ssh",
        "repository-tab-recent",
        "repository-tab-browse",
    ];
    let click = |visual: &mut VisualTestContext, position| {
        visual.simulate_click(position, Modifiers::none());
        frame(visual);
    };
    for (width, height) in [(1000., 720.), (740., 480.)] {
        visual.simulate_resize(size(px(width), px(height)));
        frame(visual);
        let bounds = SELECTORS.map(|id| visual.debug_bounds(id).expect(id));
        let path_button = visual.debug_bounds("submit-repository").unwrap();
        assert_eq!(path_button.size.width, path_button.size.height);
        let fixed_header = |visual: &mut VisualTestContext| {
            assert_eq!(
                visual.debug_bounds(SELECTORS[0]).unwrap().origin,
                bounds[0].origin
            );
            for (id, bounds) in SELECTORS[1..3]
                .iter()
                .copied()
                .zip(bounds[1..3].iter().copied())
            {
                assert_eq!(
                    visual.debug_bounds(id),
                    Some(bounds),
                    "{id} at {width} × {height}"
                );
            }
        };
        let fixed_tabs = |visual: &mut VisualTestContext, tabs| {
            assert_eq!(
                [SELECTORS[3], SELECTORS[4]].map(|id| visual.debug_bounds(id).unwrap()),
                tabs
            );
        };
        let local_tabs = [bounds[3], bounds[4]];
        click(visual, bounds[4].center());
        fixed_header(visual);
        fixed_tabs(visual, local_tabs);
        click(visual, bounds[4].center());
        fixed_header(visual);
        fixed_tabs(visual, local_tabs);
        visual.update(|_, cx| {
            let ui = &workspace.read(cx).repository_ui;
            assert!(ui.tab == PickerTab::Browse);
            assert_eq!(ui.directory.as_ref().unwrap().path, root);
        });
        click(visual, bounds[2].center());
        fixed_header(visual);
        let ssh_tabs = [SELECTORS[3], SELECTORS[4]].map(|id| visual.debug_bounds(id).unwrap());
        assert!(ssh_tabs[0].top() > local_tabs[0].top());
        let remote_button = visual.debug_bounds("submit-repository").unwrap();
        let host_button = visual.debug_bounds("select-ssh-host").unwrap();
        assert_eq!(remote_button.size, path_button.size);
        assert_eq!(remote_button.size.height, host_button.size.height);
        assert_eq!(remote_button.right(), host_button.right());
        assert!(host_button.size.width < px(120.));
        click(visual, bounds[2].center());
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let ui = &mut workspace.repository_ui;
                assert!(ui.remote.active);
                ui.remote
                    .input()
                    .update(cx, |input, cx| input.set_value("", window, cx));
                ui.input()
                    .update(cx, |input, cx| input.set_value("/srv/project", window, cx));
                assert_eq!(ui.path_action(cx), (PathAction::ConnectAndGo, None));
                ui.remote
                    .input()
                    .update(cx, |input, cx| input.set_value("devbox", window, cx));
                assert_eq!(
                    ui.path_action(cx),
                    (
                        PathAction::ConnectAndGo,
                        Some(PathBuf::from("/srv/project"))
                    )
                );
                ui.remote.error = Some("Connection failed. ".repeat(30));
                cx.notify();
            });
        });
        frame(visual);
        fixed_header(visual);
        let error_tabs = [SELECTORS[3], SELECTORS[4]].map(|id| visual.debug_bounds(id).unwrap());
        assert!(error_tabs[0].top() > ssh_tabs[0].top());
        click(visual, error_tabs[0].center());
        fixed_header(visual);
        fixed_tabs(visual, error_tabs);
        click(visual, bounds[1].center());
        fixed_header(visual);
        fixed_tabs(visual, local_tabs);
    }

    visual.update(|_, cx| Theme::set_scrollbar_mode(ScrollbarMode::Always, cx));
    frame(visual);
    let tabs = visual.debug_bounds("repository-picker-tabs").unwrap();
    let recent_id = format!("recent-row:{}", current.display()).leak();
    let recent = visual.debug_bounds(recent_id).unwrap();
    let gutter_x = tabs.right() + px(5.);
    assert_eq!(recent.right(), tabs.right());
    click(visual, point(gutter_x, recent.bottom() + px(90.)));
    assert!(visual.debug_bounds(recent_id).unwrap().top() < recent.top());

    let browse = visual
        .debug_bounds("repository-tab-browse")
        .unwrap()
        .center();
    click(visual, browse);
    let enter_path = |visual: &mut VisualTestContext, path: &Path, expected| {
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                let ui = &workspace.repository_ui;
                ui.input().update(cx, |input, cx| {
                    input.set_value(paths::display(path), window, cx)
                });
                assert_eq!(ui.path_action(cx), (expected, Some(path.to_owned())));
            });
        });
        frame(visual);
    };
    let submit = |visual: &mut VisualTestContext| {
        let position = visual.debug_bounds("submit-repository").unwrap().center();
        click(visual, position);
    };
    enter_path(visual, &current, PathAction::Open);
    enter_path(visual, &sandbox, PathAction::Browse);
    submit(visual);
    visual.update(|_, cx| {
        let workspace = workspace.read(cx);
        assert!(workspace.repository_ui.picker_open);
        assert_eq!(
            workspace.repository_ui.directory.as_ref().unwrap().path,
            sandbox
        );
    });
    let label = paths::display(&nested);
    let row_id = format!("browse-directory-row:{label}").leak();
    let arrow_id = format!("open-listed-directory:{label}").leak();
    let row = visual.debug_bounds(row_id).unwrap();
    let arrow = visual.debug_bounds(arrow_id).unwrap();
    let browse_position = point(row.left() + px(24.), row.center().y);
    assert_eq!(row.right(), tabs.right());
    assert_eq!(arrow.right(), row.right());
    assert_eq!(arrow.top(), row.top());
    assert_eq!(arrow.bottom(), row.bottom());
    for position in [browse_position, arrow.center()] {
        visual.simulate_mouse_move(position, None, Modifiers::none());
        frame(visual);
        visual.update(|window, cx| {
            let hover = Palette::for_mode(cx.theme().mode).hover;
            let bounds = row.scale(window.scale_factor());
            assert!(window.painted_quads().iter().any(|quad| {
                quad.bounds == bounds && quad.background.as_solid() == Some(hover)
            }));
        });
    }
    click(visual, point(gutter_x, row.bottom() + px(80.)));
    assert!(visual.debug_bounds(row_id).unwrap().top() < row.top());

    enter_path(visual, &sandbox, PathAction::Browse);
    submit(visual);
    click(visual, browse_position);
    visual.update(|_, cx| {
        let workspace = workspace.read(cx);
        assert!(workspace.repository_ui.picker_open);
        assert_eq!(
            workspace.repository_ui.directory.as_ref().unwrap().path,
            nested
        );
        assert_eq!(workspace.repository.as_ref().unwrap().root, current);
    });

    enter_path(visual, &nested, PathAction::Open);
    enter_path(visual, &root.join("missing"), PathAction::OpenOrBrowse);
    submit(visual);
    visual.update(|_, cx| assert!(workspace.read(cx).repository_ui.error.is_some()));
    enter_path(visual, &nested, PathAction::Open);
    submit(visual);
    visual.update(|_, cx| {
        let workspace = workspace.read(cx);
        assert!(!workspace.repository_ui.picker_open);
        assert_eq!(workspace.repository.as_ref().unwrap().root, nested);
    });
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.choose_repository(&OpenRepository, window, cx);
        });
    });
    frame(visual);
    click(visual, browse);
    enter_path(visual, &sandbox, PathAction::Browse);
    submit(visual);
    let target = sandbox.join("other-0");
    let action_id = format!("open-listed-directory:{}", paths::display(&target)).leak();
    let action = visual.debug_bounds(action_id).unwrap();
    click(visual, action.center());
    visual.update(|_, cx| {
        let workspace = workspace.read(cx);
        assert!(!workspace.repository_ui.picker_open);
        assert_eq!(workspace.repository.as_ref().unwrap().root, target);
    });
    Ok(())
}

#[::core::prelude::v1::test]
fn repository_menu_reveals_projects_focused_by_keyboard() -> anyhow::Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().canonicalize()?;
    let state = root.join("state");
    let store = Store::new(state.clone())?;
    let projects: Vec<_> = (0..32)
        .map(|ix| root.join(format!("project-{ix}")))
        .collect();
    for path in &projects {
        fs::create_dir(path)?;
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(path)
                .status()?
                .success()
        );
        store.remember_repository(&Repository::open(path)?)?;
    }
    let store = Arc::new(Store::new(state)?);
    let config = ConfigFile::load(Some(root.join("config.toml")));
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        crate::test_support::text_system(true),
    );
    app.update(|cx| {
        gpui_component::init(cx);
        cx.set_reduce_motion(true);
    });
    let (workspace, visual) = app.add_window_view(|window, cx| {
        Workspace::with_config(store, config, WorkspaceView::Editor, window, cx)
    });
    visual.simulate_resize(size(px(740.), px(480.)));
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| workspace.restore_repository(window, cx));
    });
    frame(visual);
    let trigger = visual
        .debug_bounds("repository-menu-trigger")
        .unwrap()
        .center();
    visual.simulate_click(trigger, Modifiers::none());
    frame(visual);
    let project_row = format!("recent-repository:{}", projects[2].display()).leak();
    let viewport = visual.debug_bounds("repository-menu-lists").unwrap();
    assert!(visual.debug_bounds(project_row).unwrap().top() >= viewport.bottom());
    visual.update(|_, cx| {
        let actual: Vec<_> = workspace
            .read(cx)
            .quick_recents()
            .map(|item| &item.root)
            .collect();
        assert_eq!(actual, projects[2..31].iter().rev().collect::<Vec<_>>());
    });

    // Up wraps to the fixed footer, then moves into the last project.
    for _ in 0..2 {
        visual.simulate_keystrokes("up");
        frame(visual);
    }
    let row = visual.debug_bounds(project_row).unwrap();
    assert!(row.top() >= viewport.top() && row.bottom() <= viewport.bottom());

    // Move up far enough to reveal the first project again, then open it.
    for _ in 0..28 {
        visual.simulate_keystrokes("up");
        frame(visual);
    }
    let first_row = format!("recent-repository:{}", projects[30].display()).leak();
    let row = visual.debug_bounds(first_row).unwrap();
    assert!(row.top() >= viewport.top() && row.bottom() <= viewport.bottom());
    let keystroke = Keystroke::parse("enter").unwrap();
    visual.simulate_event(KeyDownEvent {
        keystroke: keystroke.clone(),
        is_held: false,
        prefer_character_input: false,
    });
    visual.simulate_event(KeyUpEvent { keystroke });
    frame(visual);
    visual.update(|_, cx| {
        let workspace = workspace.read(cx);
        assert_eq!(workspace.repository.as_ref().unwrap().root, projects[30]);
        assert_eq!(
            workspace.store.recent_repositories().unwrap()[0].root,
            projects[30]
        );
    });
    Ok(())
}
