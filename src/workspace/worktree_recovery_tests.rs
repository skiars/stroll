use super::*;
use gpui_component::Root;
use std::{process::Command, time::Instant};

fn wait(
    visual: &mut VisualTestContext,
    description: &str,
    ready: impl Fn(&mut VisualTestContext) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        visual.executor().advance_clock(Duration::from_millis(50));
        visual.run_until_parked();
        if ready(visual) {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "Timed out waiting for {description}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn new_workspace<'a>(
    app: &'a mut TestAppContext,
    store: Arc<Store>,
    config_file: &Path,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    let mut workspace = None;
    let (_, visual) = app.add_window_view(|window, cx| {
        let config = ConfigFile::load(Some(config_file.to_owned()));
        let view = cx.new(|cx| Workspace::with_startup_view(store, config, None, window, cx));
        workspace = Some(view.clone());
        Root::new(view, window, cx)
    });
    visual.simulate_resize(size(px(1000.), px(650.)));
    (workspace.unwrap(), visual)
}

#[::core::prelude::v1::test]
fn moved_and_deleted_worktrees_restore_the_expected_workspace() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("main");
    let original = fixture.path().join("original");
    let moved = fixture.path().join("moved");
    let moved_again = fixture.path().join("moved again");
    fs::create_dir(&root)?;
    let git = |args: &[&str]| -> Result<()> {
        let result = Command::new("git").current_dir(&root).args(args).output()?;
        anyhow::ensure!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        Ok(())
    };
    git(&["init", "-qb", "main"])?;
    let path = Path::new("notes.txt");
    fs::write(root.join(path), "original text\n")?;
    fs::write(root.join("README.md"), "# Main workspace\n")?;
    git(&["add", "."])?;
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-qm",
        "Initial",
    ])?;
    git(&[
        "worktree",
        "add",
        "-qb",
        "review",
        original.to_str().unwrap(),
    ])?;

    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let main = Repository::open(&root)?;
    store.save_session(
        &main,
        &WorktreeSession {
            view: Some(WorkspaceView::Docs),
            document: Some("README.md".into()),
            ..Default::default()
        },
    )?;
    let draft = "unsaved review text\n";
    let mut linked = Repository::open(&original)?;
    assert!(store.bind_worktree(&mut linked)?.is_none());
    let mut tabs = crate::tabs::TabSession::default();
    tabs.open(path.to_owned(), false);
    store.save_session(
        &linked,
        &WorktreeSession {
            tabs,
            worktree_lsp_trusted: true,
            ..Default::default()
        },
    )?;
    Document::open(&linked, path, &store)?.persist_draft(&linked, path, &store, draft, true)?;

    let config_path = fixture.path().join("config.toml");
    fs::write(&config_path, "[lsp]\nenabled = false\n")?;
    let new_app = || {
        let cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        cx.update(gpui_component::init);
        cx
    };
    let open = |visual: &mut VisualTestContext, view: &Entity<Workspace>, root: &Path| {
        visual.update(|window, cx| {
            view.update(cx, |workspace, cx| {
                workspace.open_repository(
                    Location {
                        root: root.to_owned(),
                        file: None,
                    },
                    window,
                    cx,
                );
            });
        });
    };
    let has_draft = |visual: &mut VisualTestContext, view: &Entity<Workspace>, root: &Path| {
        visual.update(|_, cx| {
            let workspace = view.read(cx);
            workspace
                .repository
                .as_ref()
                .is_some_and(|repo| root.canonicalize().is_ok_and(|root| repo.root == root))
                && workspace
                    .active_file()
                    .is_some_and(|file| file.editor.read(cx).text() == draft)
        })
    };

    {
        let mut cx = new_app();
        let (workspace, visual) = new_workspace(&mut cx, store.clone(), &config_path);
        open(visual, &workspace, &original);
        wait(visual, "review draft with worktree identity", |visual| {
            has_draft(visual, &workspace, &original)
        })?;
        cx.quit();
    }

    git(&[
        "worktree",
        "move",
        original.to_str().unwrap(),
        moved.to_str().unwrap(),
    ])?;
    let mut cx = new_app();
    let (workspace, visual) = new_workspace(&mut cx, store.clone(), &config_path);
    visual.update(|window, cx| {
        workspace.update(cx, |this, cx| this.restore_repository(window, cx));
    });
    wait(visual, "moved worktree at startup", |visual| {
        has_draft(visual, &workspace, &moved)
    })?;

    visual.update(|_, cx| assert!(workspace.read(cx).session.worktree_lsp_trusted));

    git(&[
        "worktree",
        "move",
        moved.to_str().unwrap(),
        moved_again.to_str().unwrap(),
    ])?;
    wait(visual, "live worktree move", |visual| {
        has_draft(visual, &workspace, &moved_again)
    })?;

    git(&["worktree", "remove", moved_again.to_str().unwrap()])?;
    wait(visual, "main workspace and its Docs view", |visual| {
        visual.update(|_, cx| {
            let workspace = workspace.read(cx);
            workspace
                .repository
                .as_ref()
                .is_some_and(|repo| repo.root == main.root)
                && workspace.view == WorkspaceView::Docs
                && workspace.session.document.as_deref() == Some(Path::new("README.md"))
        })
    })?;
    assert_eq!(store.location()?.unwrap().root, main.root);

    git(&[
        "worktree",
        "add",
        "-q",
        original.to_str().unwrap(),
        "review",
    ])?;
    let mut replacement = Repository::open(&original)?;
    assert!(store.bind_worktree(&mut replacement)?.is_some());
    assert!(!store.has_draft(&replacement, path));
    open(visual, &workspace, &original);
    wait(visual, "session recovery prompt", |visual| {
        visual.has_pending_prompt()
    })?;
    visual.simulate_prompt_answer(&t!("repository.restore_worktree"));
    wait(visual, "explicitly restored draft", |visual| {
        has_draft(visual, &workspace, &original)
    })?;
    visual.update(|_, cx| assert!(!workspace.read(cx).session.worktree_lsp_trusted));
    cx.quit();
    Ok(())
}
