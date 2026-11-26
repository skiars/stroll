use std::{fs, process::Command, time::Duration};

use anyhow::{Context, Result, ensure};
use futures_lite::future::{block_on, race};
use gpui_component::input::Rope;
use lsp_types::{HoverContents, Position, Range};
use serde_json::Value;
use stroll::lsp::{Client, LanguageServer};

#[test]
fn synchronizes_two_buffers_and_requests_language_features() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let first = dir.path().join("甲 #.rs");
    let second = dir.path().join("definition.rs");
    let original = Rope::from("😀 old\n");
    let edited = Rope::from("😀 调用()\n");
    fs::write(&first, original.to_string())?;
    fs::write(&second, "pub fn definition() {}\n")?;

    let mut command = Command::new(if cfg!(windows) { "python" } else { "python3" });
    command.args(["-u", "-c", include_str!("fixtures/language_server.py")]);
    let client = Client::start(command, dir.path().to_owned(), Value::Null)?;
    let (definition, _) = client.open_document(
        &second,
        "rust",
        Rope::from("pub fn definition() {}\n"),
        Rope::from("pub fn definition() {}\n"),
    )?;
    let (document, refresh) = client.open_document(&first, "rust", original.clone(), original)?;

    let (timeout, timed_out) = async_channel::bounded::<()>(1);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        _ = timeout.try_send(());
    });
    block_on(race(
        async {
            refresh
                .recv()
                .await
                .context("server stopped before semantic refresh")
        },
        async {
            _ = timed_out.recv().await;
            anyhow::bail!("missing semantic refresh")
        },
    ))?;
    let hover = block_on(document.hover(edited.clone(), "😀 ".len()))?.context("missing hover")?;
    assert_eq!(
        hover.range,
        Some(Range::new(Position::new(0, 3), Position::new(0, 5)))
    );
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("expected Markdown documentation");
    };
    assert_eq!(markup.value, "**跨文件文档**");

    let (tokens, legend) =
        block_on(document.semantic_tokens(edited.clone()))?.context("missing tokens")?;
    assert_eq!(legend.token_types[0].as_str(), "function");
    assert_eq!(tokens.data[0].delta_start, 3);
    let links = block_on(document.definitions(edited.clone(), "😀 ".len()))?;
    assert_eq!(links.len(), 1);
    assert!(links[0].target_uri.as_str().ends_with("/definition.rs"));
    assert_eq!(links[0].target_selection_range.start, Position::new(0, 7));

    document.saved(edited);
    drop(document);
    drop(definition);
    drop(client);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let marker = dir.path().join("server-exited");
    while !marker.exists() {
        ensure!(
            std::time::Instant::now() < deadline,
            "server did not shut down cleanly"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[test]
fn selects_language_ids_and_nearest_typescript_project() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("repo");
    let package = root.join("packages/app");
    fs::create_dir_all(package.join("src"))?;
    fs::write(package.join("tsconfig.json"), "{}")?;
    let file = std::path::Path::new("packages/app/src/Editor.tsx");
    let (server, language) = LanguageServer::for_path(file).unwrap();
    assert_eq!(server, LanguageServer::TypeScript);
    assert_eq!(language, "typescriptreact");
    assert_eq!(server.project_root(&root, file), package);
    assert!(LanguageServer::for_path(std::path::Path::new("README.md")).is_none());
    Ok(())
}

#[cfg(unix)]
#[test]
fn remote_helper_uses_the_managed_typescript_executable() -> Result<()> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = tempfile::tempdir()?;
    let root = fixture.path().canonicalize()?;
    ensure!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()?
            .success()
    );
    let shell = root.join("login-shell");
    let path = root.join("path");
    let home = root.join("home");
    let version = "7.0.2";
    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let bundle = home
        .join("lsp-tools/typescript")
        .join(format!("{version}-{platform}"));
    fs::create_dir(&path)?;
    fs::create_dir_all(bundle.join("lib"))?;
    for (file, script) in [
        (
            &shell,
            "#!/bin/sh\nprintf '\\0stroll-env\\0PATH=%s\\0STROLL_HOME=%s\\0' \"$TEST_LSP_PATH\" \"$TEST_STROLL_HOME\"\n",
        ),
        (
            &path.join("tsc"),
            "#!/bin/sh\nprintf 'TypeScript on PATH'\n",
        ),
        (
            &bundle.join("lib/tsc"),
            "#!/bin/sh\n[ \"$*\" = '--lsp --stdio' ] || exit 2\nprintf 'managed LSP stream'\n",
        ),
    ] {
        fs::write(file, script)?;
        fs::set_permissions(file, fs::Permissions::from_mode(0o755))?;
    }
    fs::write(
        bundle.join("lib/lib.d.ts"),
        "declare const fixture: boolean;",
    )?;
    let installed = serde_json::json!({"bundle":"typescript","version":version});
    fs::write(
        bundle.join("install.json"),
        serde_json::to_vec(&serde_json::json!({
            "bundle": "typescript", "version": version, "platform": platform,
        }))?,
    )?;

    let launch = |managed: Option<&Value>| -> Result<_> {
        let options = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&serde_json::json!({
            "repository": root, "project": root, "server": "TypeScript", "managed": managed,
        }))?);
        Ok(Command::new(env!("CARGO_BIN_EXE_stroll-remote"))
            .args(["--lsp", &options])
            .env("SHELL", &shell)
            .env("TEST_LSP_PATH", &path)
            .env("TEST_STROLL_HOME", &home)
            .output()?)
    };
    let failed = launch(None)?;
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    assert!(
        std::str::from_utf8(&failed.stderr)?
            .contains("Managed language tools have not been installed")
    );

    let started = launch(Some(&installed))?;
    assert!(started.status.success(), "{:?}", started.stderr);
    assert_eq!(started.stdout, b"managed LSP stream");
    assert!(started.stderr.is_empty());
    Ok(())
}

#[cfg(unix)]
#[test]
fn remote_helper_resolves_external_language_server_from_its_own_config() -> Result<()> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = tempfile::tempdir()?;
    let root = fixture.path().canonicalize()?;
    ensure!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()?
            .success()
    );
    let shell = root.join("login-shell");
    let server = root.join("example-lsp");
    for (path, script) in [
        (
            &shell,
            "#!/bin/sh\nprintf '\\0stroll-env\\0PATH=/usr/bin:/bin\\0'\n",
        ),
        (
            &server,
            "#!/bin/sh\n[ \"$1\" = --stdio ] && [ \"$LSP_TEST\" = yes ] || exit 2\nprintf 'language server stream'\n",
        ),
    ] {
        fs::write(path, script)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    let config = root.join("lsp-test.json");
    fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!([{
            "id": "example",
            "languages": {".example": "example"},
            "server": {"command": server, "args": ["--stdio"], "env": {"LSP_TEST": "yes"}}
        }]))?,
    )?;
    for (id, expected) in [("example", true), ("unregistered", false)] {
        let request = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&serde_json::json!({
            "repository": root, "project": root, "server": {"id": id}
        }))?);
        let result = Command::new(env!("CARGO_BIN_EXE_stroll-remote"))
            .args(["--lsp", &request])
            .env("SHELL", &shell)
            .env("STROLL_LSP_CONFIG", &config)
            .output()?;
        assert_eq!(result.status.success(), expected, "{:?}", result.stderr);
        if expected {
            assert_eq!(result.stdout, b"language server stream");
        } else {
            assert!(result.stdout.is_empty());
        }
    }
    Ok(())
}
