use std::{fs, process::Command, time::Instant};

use anyhow::{Context as _, Result, ensure};
use futures_lite::future::block_on;

use super::super::completion_tests::{frame, wait, with_open_workspace};
use super::*;
use crate::{config::ConfigFile, repository::Location};

#[::core::prelude::v1::test]
fn workspace_install_progress_keeps_the_lsp_panel_height() -> Result<()> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("repository");
    fs::create_dir(&root)?;
    ensure!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()?
            .success()
    );
    let file = PathBuf::from("notes.txt");
    fs::write(root.join(&file), "Fixture for the installation panel\n")?;
    let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
    let location = Location {
        root: root.clone(),
        file: Some(file.clone()),
    };
    with_open_workspace(fixture.path(), config, location, |visual, workspace| {
        wait(visual, "source file", |visual| {
            visual.update(|_, cx| workspace.read(cx).active_file().is_some())
        })?;
        let server = LanguageServer::TypeScript;
        let bundle = Bundle::TypeScript;
        let key = ServerKey {
            server: ServiceServer::Builtin(server),
            root,
            remote: false,
        };
        let shared = visual.update(|_, cx| -> Result<_> {
            workspace.update(cx, |workspace, cx| {
                workspace.files.get_mut(&file).unwrap().server_project = Some(ServerProject {
                    key: key.clone(),
                    language_id: "typescript".into(),
                });
                let tools = Tools::new(fixture.path().join("tools"))?;
                workspace.store.dismiss_lsp_prompt("local", server.id())?;
                workspace.language_tools_checked(
                    &key,
                    server,
                    bundle,
                    Probe {
                        source: Source::Local(tools),
                        host: "local".into(),
                        installed: None,
                    },
                    cx,
                );
                let LanguageService::Setup(setup) = &workspace.language_servers[&key] else {
                    unreachable!()
                };
                Ok(setup.shared.clone())
            })
        })?;
        visual.simulate_resize(size(px(300.), px(480.)));
        frame(visual);
        let trigger = visual
            .debug_bounds("status-language-service")
            .unwrap()
            .center();
        visual.simulate_click(trigger, Modifiers::default());

        let long_archive =
            "vue typescript language server with a particularly long archive filename.tgz";
        let stages = [
            None,
            Some(InstallStage::Waiting),
            Some(InstallStage::Downloading {
                archive: "node.tgz".into(),
                received: 127,
                size: None,
            }),
            Some(InstallStage::Downloading {
                archive: long_archive.into(),
                received: 42_000_000,
                size: Some(80_000_000),
            }),
            Some(InstallStage::Verifying(long_archive.into())),
            Some(InstallStage::Extracting(long_archive.into())),
            Some(InstallStage::Publishing),
        ];
        let mut first = None;
        for stage in stages {
            visual.update(|_, cx| {
                shared.update(cx, |installation, cx| {
                    installation.state = InstallState::Installing(stage.map(|stage| Progress {
                        version: bundle.version().into(),
                        completed: 0,
                        total: 2,
                        bundle,
                        stage,
                    }));
                    cx.notify();
                });
            });
            for _ in 0..12 {
                frame(visual);
            }
            let panel = visual
                .debug_bounds("language-service-panel")
                .expect("LSP panel");
            let height = panel.size.height;
            assert_eq!(
                height,
                *first.get_or_insert(height),
                "the installation stage resized the LSP panel"
            );
        }
        Ok(())
    })
}

#[::core::prelude::v1::test]
#[ignore = "downloads real TypeScript into an empty STROLL_HOME"]
fn workspace_managed_typescript_installs_from_the_panel_after_prompt_dismissal() -> Result<()> {
    let home =
        std::env::var_os("STROLL_HOME").context("Set STROLL_HOME to an empty test directory")?;
    let tools = Tools::new(home.into())?;
    ensure!(
        tools.lookup(Bundle::TypeScript, None)?.is_none(),
        "TypeScript is already installed"
    );
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("project");
    fs::create_dir(&root)?;
    ensure!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()?
            .success()
    );
    let source = "const answer: number = 42;\nanswer;\n";
    fs::write(root.join("main.ts"), source)?;
    fs::write(root.join("other.js"), "const other = 2;\n")?;
    let open = || Location {
        root: root.clone(),
        file: Some("main.ts".into()),
    };
    let config = || ConfigFile::load(Some(fixture.path().join("config.toml")));
    let status = |visual: &mut VisualTestContext, workspace: &Entity<Workspace>| {
        visual.update(|_, cx| {
            let view = workspace.read(cx);
            view.language_service_state(
                view.active_file()
                    .and_then(|file| file.server_project.as_ref()),
                cx,
            )
        })
    };

    with_open_workspace(fixture.path(), config(), open(), |visual, workspace| {
        visual.update(|window, _| window.activate_window());
        wait(visual, "TypeScript setup notification", |visual| {
            visual
                .debug_bounds("install-language-server-toast")
                .is_some()
        })?;
        assert!(status(visual, workspace) == ServiceStatus::NotInstalled);
        visual.update(|window, cx| {
            let file = workspace.read(cx).active_file().unwrap();
            assert!(file.editor.read(cx).focus_handle(cx).is_focused(window));
        });
        assert!(tools.lookup(Bundle::TypeScript, None)?.is_none());
        for _ in 0..12 {
            frame(visual);
        }
        let close = visual.debug_bounds("notification-close").unwrap().center();
        visual.simulate_mouse_move(close, None, Modifiers::default());
        frame(visual);
        let close = visual.debug_bounds("notification-close").unwrap().center();
        visual.simulate_click(close, Modifiers::default());
        assert!(!workspace.read_with(visual, |workspace, _| {
            workspace.store.lsp_prompt_dismissed("local", "tsc")
        })?);
        Ok(())
    })?;
    with_open_workspace(fixture.path(), config(), open(), |visual, workspace| {
        visual.update(|window, _| window.activate_window());
        wait(visual, "notification after temporary close", |visual| {
            visual
                .debug_bounds("dismiss-language-server-toast")
                .is_some()
        })?;
        for _ in 0..12 {
            frame(visual);
        }
        let dismiss = visual
            .debug_bounds("dismiss-language-server-toast")
            .unwrap()
            .center();
        visual.simulate_click(dismiss, Modifiers::default());
        assert!(workspace.read_with(visual, |workspace, _| {
            workspace.store.lsp_prompt_dismissed("local", "tsc")
        })?);
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_file("other.js".into(), false, window, cx)
            });
        });
        wait(visual, "JavaScript source", |visual| {
            visual.update(|_, cx| {
                workspace
                    .read(cx)
                    .active_file()
                    .is_some_and(|file| file.path == Path::new("other.js"))
            })
        })?;
        Ok(())
    })?;
    with_open_workspace(fixture.path(), config(), open(), |visual, workspace| {
        wait(visual, "missing TypeScript status", |visual| {
            status(visual, workspace) == ServiceStatus::NotInstalled
        })?;
        assert!(
            visual
                .debug_bounds("install-language-server-toast")
                .is_none()
        );
        let trigger = visual
            .debug_bounds("status-language-service")
            .unwrap()
            .center();
        visual.simulate_click(trigger, Modifiers::default());
        let shared = visual.update(|_, cx| {
            let workspace = workspace.read(cx);
            let project = workspace
                .active_file()
                .unwrap()
                .server_project
                .as_ref()
                .unwrap();
            let LanguageService::Setup(setup) = &workspace.language_servers[&project.key] else {
                unreachable!()
            };
            setup.shared.clone()
        });
        // The test dispatcher runs blocking downloads to completion before it draws.
        visual.update(|_, cx| {
            shared.update(cx, |installation, cx| {
                installation.state = InstallState::Installing(Some(Progress {
                    version: Bundle::TypeScript.version().to_owned(),
                    completed: 0,
                    total: 1,
                    bundle: Bundle::TypeScript,
                    stage: InstallStage::Downloading {
                        archive: "typescript.tgz".into(),
                        received: 1024,
                        size: Some(4096),
                    },
                }));
                cx.notify();
            });
        });
        wait(visual, "detailed installation progress", |visual| {
            visual.debug_bounds("lsp-install-progress").is_some()
        })?;
        visual.update(|_, cx| {
            shared.update(cx, |installation, cx| {
                installation.state = InstallState::Missing;
                cx.notify();
            });
        });
        wait(visual, "panel install action", |visual| {
            visual.debug_bounds("install-language-service").is_some()
        })?;
        let install = visual
            .debug_bounds("install-language-service")
            .unwrap()
            .center();
        visual.simulate_click(install, Modifiers::default());
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            frame(visual);
            match status(visual, workspace) {
                ServiceStatus::Ready => break,
                ServiceStatus::Failed(error) => anyhow::bail!("{error}"),
                ServiceStatus::InstallFailed => {
                    let error = visual.update(|_, cx| {
                        let workspace = workspace.read(cx);
                        let project = workspace
                            .active_file()
                            .unwrap()
                            .server_project
                            .as_ref()
                            .unwrap();
                        match workspace.language_servers.get(&project.key) {
                            Some(LanguageService::Setup(setup)) => setup.error(cx).unwrap(),
                            _ => unreachable!(),
                        }
                    });
                    anyhow::bail!("{error}");
                }
                _ => ensure!(
                    Instant::now() < deadline,
                    "Timed out waiting for TypeScript"
                ),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let doc = visual.update(|_, cx| {
            let workspace = workspace.read(cx);
            assert!(workspace.store.lsp_prompt_dismissed("local", "tsc").unwrap());
            let file = workspace.active_file().unwrap();
            let key = &file.server_project.as_ref().unwrap().key;
            assert!(matches!(workspace.language_servers.get(key), Some(LanguageService::Session { managed: Some(tool), .. }) if tool.version == Bundle::TypeScript.version()));
            file.language_server.as_ref().unwrap().document.clone()
        });
        let hover = block_on(doc.hover(source.into(), source.rfind("answer").unwrap()))?;
        ensure!(
            hover.is_some(),
            "TypeScript hover returned no documentation"
        );
        Ok(())
    })?;
    with_open_workspace(fixture.path(), config(), open(), |visual, workspace| {
        wait(visual, "cached TypeScript session", |visual| {
            status(visual, workspace) == ServiceStatus::Ready
        })
    })
}

#[::core::prelude::v1::test]
#[ignore = "requires a disposable loopback helper with an empty remote STROLL_HOME"]
fn workspace_managed_typescript_shares_ssh_setup_between_windows() -> Result<()> {
    let host = std::env::var("STROLL_TEST_MANAGED_LSP_SSH")?;
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("repository");
    fs::create_dir(&root)?;
    ensure!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()?
            .success()
    );
    let source = "const message: string = 'stroll';\nmessage;\n";
    fs::write(root.join("main.ts"), source)?;
    fs::write(root.join("other.ts"), source)?;
    let remote = PathBuf::from(format!("ssh://{host}{}", root.display()));
    let location = |file: &str| Location {
        root: remote.clone(),
        file: Some(file.into()),
    };
    let config = || ConfigFile::load(Some(fixture.path().join("config.toml")));
    with_open_workspace(
        fixture.path(),
        config(),
        location("main.ts"),
        |visual, first| {
            visual.update(|window, _| window.activate_window());
            wait(visual, "first SSH installation prompt", |visual| {
                visual
                    .debug_bounds("install-language-server-toast")
                    .is_some()
            })?;
            let second = visual.update(|_, cx| -> Result<Entity<Workspace>> {
                let store = first.read(cx).store.clone();
                let mut entity = None;
                cx.open_window(
                    WindowOptions {
                        focus: false,
                        ..Default::default()
                    },
                    |window, cx| {
                        let view = cx.new(|cx| {
                            Workspace::with_config(
                                store,
                                config(),
                                crate::workspace::WorkspaceView::Editor,
                                window,
                                cx,
                            )
                        });
                        view.update(cx, |view, cx| {
                            view.open_repository(location("other.ts"), window, cx)
                        });
                        entity = Some(view.clone());
                        cx.new(|cx| gpui_component::Root::new(view, window, cx))
                    },
                )?;
                Ok(entity.unwrap())
            })?;
            let current_install = |view: &Workspace| {
                let key = &view.active_file()?.server_project.as_ref()?.key;
                match view.language_servers.get(key)? {
                    LanguageService::Setup(setup) => Some(setup.shared.clone()),
                    _ => None,
                }
            };
            wait(visual, "shared missing SSH installation", |visual| {
                visual.update(|_, cx| current_install(second.read(cx)).is_some())
            })?;
            visual.update(|_, cx| {
                assert_eq!(
                    current_install(first.read(cx)),
                    current_install(second.read(cx))
                );
                assert_eq!(cx.global::<SetupRegistry>().shown.len(), 1);
            });
            for _ in 0..12 {
                frame(visual);
            }
            let install = visual
                .debug_bounds("install-language-server-toast")
                .unwrap()
                .center();
            visual.simulate_click(install, Modifiers::default());

            let deadline = Instant::now() + Duration::from_secs(60);
            let document = loop {
                frame(visual);
                let result = visual.update(|_, cx| -> Result<_> {
                    let mut ready = Vec::new();
                    for workspace in [&first, &second] {
                        let view = workspace.read(cx);
                        let file = view.active_file().unwrap();
                        let project = file.server_project.as_ref().unwrap();
                        let status = view.language_service_state(Some(project), cx);
                        match status {
                            ServiceStatus::Ready => {
                                assert!(project.key.is_remote());
                                ready.push(file.language_server.as_ref().unwrap().document.clone());
                            }
                            ServiceStatus::Failed(error) => anyhow::bail!("{error}"),
                            ServiceStatus::InstallFailed => {
                                let LanguageService::Setup(setup) =
                                    &view.language_servers[&project.key]
                                else {
                                    unreachable!()
                                };
                                anyhow::bail!("{}", setup.error(cx).unwrap());
                            }
                            _ => {}
                        }
                    }
                    Ok((ready.len() == 2).then(|| ready.remove(0)))
                })?;
                if let Some(document) = result {
                    break document;
                }
                ensure!(
                    Instant::now() < deadline,
                    "Timed out waiting for the remote TypeScript sessions"
                );
                std::thread::sleep(Duration::from_millis(10));
            };
            ensure!(
                block_on(document.hover(source.into(), source.rfind("message").unwrap()))?
                    .is_some()
            );
            Ok(())
        },
    )
}
