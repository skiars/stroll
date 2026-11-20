use std::{fs, process::Command};

use anyhow::{Context as _, Result, ensure};
use gpui_component::Root;
use serde_json::{Value, json};

use super::{completion_tests::wait, *};
use crate::{
    config::ConfigFile,
    language::worktree::CONFIG_PATH,
    repository::{Location, Repository, Store, atomic_write},
    session::{WorkspaceView, WorktreeSession},
};

fn open(root: PathBuf, file: &str, visual: &mut VisualTestContext, workspace: &Entity<Workspace>) {
    visual.update(|window, cx| {
        workspace.update(cx, |this, cx| {
            this.open_repository(
                Location {
                    root,
                    file: Some(file.into()),
                },
                window,
                cx,
            );
        });
    });
}

fn test_workspace(
    app: &mut TestAppContext,
    store: Arc<Store>,
    config_path: PathBuf,
) -> (Entity<Workspace>, &mut VisualTestContext) {
    app.update(gpui_component::init);
    let mut workspace = None;
    let (_, visual) = app.add_window_view(|window, cx| {
        let config = ConfigFile::load(Some(config_path));
        let view =
            cx.new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
        workspace = Some(view.clone());
        Root::new(view, window, cx)
    });
    visual.simulate_resize(size(px(940.), px(580.)));
    (workspace.unwrap(), visual)
}

fn test_app() -> TestAppContext {
    TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        crate::test_support::text_system(true),
    )
}

fn events(root: &Path, log: &str) -> Vec<Value> {
    fs::read_to_string(root.join(log))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn open_source(visual: &mut VisualTestContext, workspace: &Entity<Workspace>, path: &str) {
    visual.update(|window, cx| {
        workspace.update(cx, |this, cx| {
            this.open_file(path.into(), false, window, cx)
        });
    });
}

fn reload_workflow(ssh_authority: Option<&str>) -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().canonicalize()?;
    ensure!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()?
            .success()
    );
    fs::write(root.join(".git/info/exclude"), ".stroll.json\n*.log\n")?;
    fs::create_dir(root.join("nested"))?;
    fs::write(root.join("first.cpp"), "int original;\n")?;
    fs::write(root.join("nested/next.cpp"), "int nested;\n")?;
    fs::write(root.join("file.case"), "example\n")?;
    let script = root.join("lsp.py");
    fs::write(
        &script,
        include_str!("../../../tests/fixtures/lsp_worktree_reload.py"),
    )?;
    let command = if cfg!(windows) { "python" } else { "python3" };
    let config = |profile: &str, language: &str| {
        serde_json::to_string_pretty(&json!({
            "lsp": [
                {"id": "clangd", "rootMarkers": [], "server": {
                    "command": command, "args": ["-u", script, "cpp.log", profile],
                }},
                {"id": "nested", "paths": ["nested/**"], "languages": {".cpp": "cpp"}, "server": {
                    "command": command, "args": ["-u", script, "nested.log", "nested"],
                }},
                {"id": "cases", "languages": {".case": language}, "server": {
                    "command": command, "args": ["-u", script, "other.log", "other"],
                }},
            ]
        }))
    };
    let file = root.join(CONFIG_PATH);
    atomic_write(&file, config("debug", "case")?.as_bytes())?;

    let locator = ssh_authority.map_or_else(
        || root.clone(),
        |authority| PathBuf::from(format!("ssh://{authority}{}", root.display())),
    );
    let state = tempfile::tempdir()?;
    let store = Arc::new(Store::new(state.path().join("state"))?);
    let mut app = test_app();
    let (workspace, visual) = test_workspace(&mut app, store, state.path().join("config.toml"));
    open(locator, "first.cpp", visual, &workspace);
    wait(visual, "worktree LSP trust prompt", |visual| {
        visual.has_pending_prompt()
    })?;
    assert!(events(&root, "cpp.log").is_empty());
    visual.simulate_prompt_answer(&t!("lsp.trust_worktree"));

    wait(visual, "Debug didOpen", |_| {
        events(&root, "cpp.log")
            .iter()
            .any(|event| event["event"] == "open" && event["profile"] == "debug")
    })?;
    visual.update(|window, cx| {
        let editor = workspace.read(cx).active_file().unwrap().editor.clone();
        editor.update(cx, |editor, cx| {
            editor.set_value("int unsaved;\n", window, cx)
        });
        workspace.update(cx, |this, cx| this.editor_changed(editor, window, cx));
    });

    open_source(visual, &workspace, "nested/next.cpp");
    wait(visual, "nested clangd", |_| {
        events(&root, "nested.log")
            .iter()
            .any(|event| event["event"] == "open")
    })?;
    atomic_write(&file, b"{ broken")?;
    wait(visual, "invalid configuration reported", |visual| {
        visual.update(|_, cx| workspace.read(cx).worktree_language.error.is_some())
    })?;
    assert_eq!(events(&root, "cpp.log").len(), 2);

    open_source(visual, &workspace, "file.case");
    wait(visual, "custom suffix LSP", |_| {
        events(&root, "other.log")
            .iter()
            .any(|event| event["event"] == "open" && event["languageId"] == "case")
    })?;

    atomic_write(&file, config("cm33", "case-v2")?.as_bytes())?;
    wait(visual, "CM33 process and remapped case document", |_| {
        events(&root, "cpp.log").iter().any(|event| {
            event["event"] == "open"
                && event["profile"] == "cm33"
                && event["text"] == "int unsaved;\n"
        }) && events(&root, "other.log")
            .iter()
            .any(|event| event["event"] == "open" && event["languageId"] == "case-v2")
    })
    .with_context(|| {
        format!(
            "C++: {:?}; case: {:?}",
            events(&root, "cpp.log"),
            events(&root, "other.log")
        )
    })?;

    let cpp = events(&root, "cpp.log");
    assert_eq!(
        cpp.iter()
            .map(|event| event["event"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["start", "open", "exit", "start", "open"]
    );
    assert_ne!(cpp[0]["pid"], cpp[3]["pid"]);
    atomic_write(
        &file,
        format!("// Comment-only save\n{}", config("cm33", "case-v2")?).as_bytes(),
    )?;
    visual
        .executor()
        .advance_clock(std::time::Duration::from_millis(300));
    visual.run_until_parked();
    for log in ["nested.log", "other.log"] {
        assert_eq!(
            events(&root, log)
                .iter()
                .filter(|event| event["event"] == "start")
                .count(),
            1
        );
    }
    assert_eq!(events(&root, "cpp.log"), cpp);
    Ok(())
}

#[::core::prelude::v1::test]
fn worktree_lsp_reloads_only_affected_sessions() -> Result<()> {
    reload_workflow(None)
}

#[::core::prelude::v1::test]
#[ignore = "requires a disposable loopback SSH server running stroll-remote"]
fn worktree_lsp_reloads_only_affected_sessions_over_ssh() -> Result<()> {
    reload_workflow(Some(&std::env::var("STROLL_TEST_WORKTREE_LSP_SSH")?))
}

#[::core::prelude::v1::test]
#[ignore = "requires a prepared nucleus checkout and clangd launcher"]
fn worktree_lsp_switches_nucleus_debug_and_cm33() -> Result<()> {
    let root = PathBuf::from(
        std::env::var_os("STROLL_TEST_NUCLEUS_LSP_ROOT").context("nucleus checkout")?,
    );
    let wrapper = std::env::var("STROLL_TEST_NUCLEUS_LSP_CLANGD")?;
    let driver = std::env::var("STROLL_TEST_NUCLEUS_LSP_DRIVER")?;
    let authority = std::env::var("STROLL_TEST_WORKTREE_LSP_SSH").ok();
    let locator = authority.map_or_else(
        || root.clone(),
        |authority| PathBuf::from(format!("ssh://{authority}{}", root.display())),
    );
    let config = |directory: &str| {
        serde_json::to_vec_pretty(&json!({
            "lsp": [{"id": "clangd", "rootMarkers": [], "server": {
                "command": wrapper,
                "args": [format!("--compile-commands-dir={directory}"), "--log=verbose",
                    "--background-index=false", format!("--query-driver={driver}")],
            }}]
        }))
    };
    for profile in ["cmake-build-debug", "cmake-build-cm33"] {
        match fs::remove_file(root.join(format!("{profile}.clangd.log"))) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let path = root.join(CONFIG_PATH);
    atomic_write(&path, &config("cmake-build-debug")?)?;
    let fixture = tempfile::tempdir()?;
    let store = Arc::new(Store::new(fixture.path().join("state"))?);
    let session = WorktreeSession {
        worktree_lsp_trusted: true,
        ..Default::default()
    };
    store.save_session(&Repository::open(&locator)?, &session)?;

    let mut app = test_app();
    let (workspace, visual) =
        test_workspace(&mut app, store.clone(), fixture.path().join("config.toml"));
    open(locator, "src/core/object/gx_object.cpp", visual, &workspace);
    let applied = |directory: &str, flag: &str| {
        fs::read_to_string(root.join(format!("{directory}.clangd.log"))).is_ok_and(|log| {
            log.lines().any(|line| {
                line.contains("ASTWorker building file") && line.contains("gx_object.cpp")
            }) && log
                .lines()
                .any(|line| line.contains("gx_object.cpp") && line.contains(flag))
        })
    };
    wait(visual, "nucleus Debug compile command", |_| {
        applied("cmake-build-debug", "-fsanitize=address")
    })?;
    atomic_write(&path, &config("cmake-build-cm33")?)?;
    wait(visual, "nucleus CM33 compile command", |_| {
        applied("cmake-build-cm33", "-mcpu=cortex-m33")
    })?;
    Ok(())
}
