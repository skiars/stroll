use super::*;
use crate::{config::ConfigFile, workspace::WorkspaceView};
use gpui::{TestAppContext, TestDispatcher, VisualTestContext};
use std::{fs, process::Command, sync::Arc};

fn git(root: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn open(workspace: &Entity<Workspace>, visual: &mut VisualTestContext, root: &Path) {
    visual.update(|window, cx| {
        workspace.update(cx, |this, cx| {
            this.open_repository(
                Location {
                    root: root.to_owned(),
                    file: None,
                },
                window,
                cx,
            );
        });
    });
    visual.run_until_parked();
    visual.update(|_, cx| {
        let this = workspace.read(cx);
        assert!(
            this.repository_ui.error.is_none(),
            "{:?}",
            this.repository_ui.error
        );
        assert_eq!(this.repository.as_ref().unwrap().root, root);
    });
}

fn draw_picker(workspace: &Entity<Workspace>, visual: &mut VisualTestContext) {
    visual.update(|window, cx| {
        workspace.update(cx, |this, cx| {
            this.repository_ui.menu_open = true;
            this.update_repository_menu_focus(cx);
            cx.notify();
        });
        window.simulate_next_frame(cx);
        window.draw(cx).clear(cx);
    });
    visual.executor().advance_clock(Duration::from_millis(300));
    visual.update(|window, cx| window.draw(cx).clear(cx));
}

/// Run against a disposable loopback SSH server running the real helper.
/// The ssh executable on PATH must append its PID to the test log before exec'ing OpenSSH.
#[::core::prelude::v1::test]
#[ignore = "requires a loopback SSH server and an instrumented SSH client"]
fn ssh_switch_reuses_connection_refreshes_view_and_reconnects() -> anyhow::Result<()> {
    let authority = env::var("STROLL_TEST_SSH_SWITCH_AUTHORITY")?;
    let log = env::var_os("STROLL_TEST_SSH_PID_LOG").expect("Expected a test SSH PID log");
    let pids = || -> anyhow::Result<Vec<u32>> {
        Ok(fs::read_to_string(&log)?
            .lines()
            .map(str::parse)
            .collect::<Result<_, _>>()?)
    };
    let fixture = tempfile::tempdir()?;
    let remote = fixture.path().join("remote");
    let local = fixture.path().join("local");
    for root in [&remote, &local] {
        fs::create_dir(root)?;
        git(root, &["init", "-qb", "main"])?;
    }
    fs::write(remote.join("original.txt"), "original\n")?;
    let ssh_root = PathBuf::from(format!("ssh://{authority}{}", remote.display()));
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        crate::test_support::text_system(true),
    );
    cx.update(gpui_component::init);
    let (workspace, visual) = cx.add_window_view(|window, cx| {
        Workspace::with_config(store, config, WorkspaceView::Editor, window, cx)
    });
    visual.simulate_resize(size(px(1280.), px(720.)));

    open(&workspace, visual, &ssh_root);
    let original_pids = pids()?;
    assert_eq!(original_pids.len(), 1);
    draw_picker(&workspace, visual);
    assert!(visual.debug_bounds("ssh-online").is_none());

    open(&workspace, visual, &local);
    draw_picker(&workspace, visual);
    let title = visual
        .debug_bounds("repository-row-title")
        .expect("Repository title with SSH device name");
    let path = visual
        .debug_bounds("recent-repository-path")
        .expect("Repository path");
    let dot = visual.debug_bounds("ssh-online").expect("SSH online dot");
    assert!((dot.origin.x - title.right() - px(4.)).abs() <= px(1.));
    assert!((dot.center().y - title.center().y).abs() <= px(1.));
    assert!(dot.bottom() <= path.origin.y);

    fs::write(remote.join("added.txt"), "added while away\n")?;
    git(&remote, &["symbolic-ref", "HEAD", "refs/heads/updated"])?;
    open(&workspace, visual, &ssh_root);
    assert_eq!(pids()?, original_pids);
    visual.update(|_, cx| {
        let this = workspace.read(cx);
        let cached = this.repository.as_ref().unwrap();
        let refreshed = this.repository_snapshot.as_ref().unwrap();
        assert_eq!(cached.branch, "main");
        assert_eq!(refreshed.branch, "updated");
        assert!(
            this.rows
                .iter()
                .any(|row| row.entry.path == Path::new("added.txt"))
        );
    });

    open(&workspace, visual, &local);
    // Only the isolated SSH client started by this test is terminated.
    assert!(
        Command::new("kill")
            .arg(original_pids[0].to_string())
            .status()?
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while visual.update(|_, cx| {
        workspace
            .read(cx)
            .online_repository_ssh(&ssh_root)
            .is_some()
    }) {
        anyhow::ensure!(Instant::now() < deadline, "SSH disconnect was not observed");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Redraw after the existing connection monitor observes the disconnect.
    visual.executor().advance_clock(Duration::from_secs(1));
    draw_picker(&workspace, visual);
    assert!(visual.debug_bounds("ssh-online").is_none());
    open(&workspace, visual, &ssh_root);
    let reconnected_pids = pids()?;
    assert_eq!(reconnected_pids.len(), 2);
    assert_ne!(reconnected_pids[1], original_pids[0]);
    Ok(())
}
