use super::*;

#[::core::prelude::v1::test]
fn git_ai_button_fills_an_editable_draft_from_the_staged_snapshot() -> anyhow::Result<()> {
    use crate::config::ConfigFile;
    use std::time::{Duration, Instant};

    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("repo");
    fs::create_dir(&root)?;
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), "a staged change\n")?;
    git(&root, &["add", "a.txt"]);
    fs::write(root.join("a.txt"), "an unstaged change\n")?;

    crate::ai::install_test_keyring();
    const MESSAGE: &str = "feat: add the staged file";
    let (url, provider) = crate::ai::mock_endpoint(vec![(
        200,
        r#"{"choices":[{"finish_reason":"stop","message":{"content":"feat: add the staged file"}}]}"#,
    )]);
    let config_path = fixture.path().join("config.toml");
    let mut config = ConfigFile::load(Some(config_path.clone()));
    let mut settings = config.current().clone();
    settings.ai = crate::ai::save_provider(&config_path, &settings.ai, &url, "reader", "mock-key")?;
    config.save(settings)?;
    assert!(!fs::read_to_string(&config_path)?.contains("mock-key"));

    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let platform = gpui_platform::current_platform(true);
    let mut app = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    app.update(gpui_component::init);
    let (workspace, visual) = app.add_window_view(|window, cx| {
        Workspace::with_config(store, config, WorkspaceView::Git, window, cx)
    });
    visual.simulate_resize(size(px(1280.), px(850.)));
    visual.update(|window, cx| {
        workspace.update(cx, |workspace, cx| {
            workspace.open_repository(
                Location {
                    root: root.clone(),
                    file: None,
                },
                window,
                cx,
            );
        })
    });
    visual.run_until_parked();
    let panel = visual.update(|_, cx| workspace.read(cx).git.clone().unwrap());
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            panel.load_review(Selection::Worktree(Mode::All), false, window, cx);
            panel.show_history = false;
            cx.notify();
        })
    });
    for _ in 0..4 {
        visual.run_until_parked();
        visual.update(|window, cx| window.draw(cx).clear(cx));
    }
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| {
            // Hold a read-only refresh open while the button is clicked and the response arrives.
            panel.detail_loading = true;
            cx.notify();
        });
        window.draw(cx).clear(cx);
    });
    let button = visual.debug_bounds("git-generate-commit-message").unwrap();
    visual.simulate_click(button.center(), Modifiers::default());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        visual.run_until_parked();
        let pending = visual.update(|_, cx| panel.read(cx).ai_commit_pending);
        if !pending || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    visual.update(|window, cx| {
        assert!(panel.read(cx).detail_loading);
        let input = panel.read(cx).commit_message.clone();
        assert_eq!(input.read(cx).value(), MESSAGE);
        input.update(cx, |input, cx| {
            input.set_value("An edited message", window, cx)
        });
        assert_eq!(input.read(cx).value(), "An edited message");
    });
    let requests = provider.join().unwrap();
    let sent = requests[0]["messages"][1]["content"].as_str().unwrap();
    assert!(sent.contains("+a staged change"));
    assert!(!sent.contains("an unstaged change"));
    assert!(
        !Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(&root)
            .output()?
            .status
            .success()
    );
    Ok(())
}
