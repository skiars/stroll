use gpui_component::menu::PopupMenu;

use super::*;

#[derive(Clone, Default)]
pub(super) struct SystemFiles {
    #[cfg(target_os = "linux")]
    launchers: Vec<PathBuf>,
    #[cfg(target_os = "linux")]
    manager: Option<Manager>,
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct Manager {
    name: &'static str,
    program: PathBuf,
    selects_file: bool,
}

impl SystemFiles {
    #[cfg(target_os = "linux")]
    pub(super) fn load() -> Self {
        use std::os::unix::fs::PermissionsExt as _;
        use std::process::Command;

        let path = env::var_os("PATH").unwrap_or_default();
        let find = |program| {
            env::split_paths(&path)
                .filter(|dir| dir.is_absolute())
                .map(|directory| directory.join(program))
                .find(|path| {
                    path.metadata().is_ok_and(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    })
                })
        };
        let launchers = ["xdg-open", "gio"].into_iter().filter_map(find).collect();
        let manager = find("xdg-mime")
            .and_then(|probe| {
                Command::new(probe)
                    .args(["query", "default", "inode/directory"])
                    .output()
                    .ok()
                    .filter(|output| output.status.success())
                    .and_then(|output| String::from_utf8(output.stdout).ok())
            })
            .and_then(|desktop_id| {
                let (name, program, selects_file) = match desktop_id.trim() {
                    "org.kde.dolphin.desktop" => ("Dolphin", "dolphin", true),
                    "org.gnome.Nautilus.desktop" => ("Nautilus", "nautilus", true),
                    "thunar.desktop" | "Thunar.desktop" => ("Thunar", "thunar", false),
                    _ => return None,
                };
                Some(Manager {
                    name,
                    program: find(program)?,
                    selects_file,
                })
            });
        Self { launchers, manager }
    }

    fn manager_label(&self, historical: bool) -> SharedString {
        #[cfg(target_os = "linux")]
        let manager = self.manager.as_ref().map(|manager| manager.name);
        #[cfg(target_os = "windows")]
        let manager = Some("Explorer");
        #[cfg(target_os = "macos")]
        let manager = Some("Finder");
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        let manager: Option<&str> = None;

        match (manager, historical) {
            (Some(name), false) => t!("workspace.tree_open_in_manager", name = name).into(),
            (Some(name), true) => t!("workspace.tree_show_worktree_in_manager", name = name).into(),
            (None, false) => t!("workspace.tree_open_in_file_manager").into(),
            (None, true) => t!("workspace.tree_show_worktree_in_file_manager").into(),
        }
    }

    fn can_open(&self) -> bool {
        #[cfg(target_os = "linux")]
        return !self.launchers.is_empty();
        #[cfg(not(target_os = "linux"))]
        cfg!(any(target_os = "windows", target_os = "macos"))
    }

    fn can_reveal(&self) -> bool {
        #[cfg(target_os = "linux")]
        return self.manager.is_some() || self.can_open();
        #[cfg(not(target_os = "linux"))]
        self.can_open()
    }

    fn open(self, path: PathBuf, reveal: bool, window: &mut Window, cx: &mut App) {
        #[cfg(target_os = "linux")]
        {
            use std::process::{Command, Stdio};

            let file = reveal && !path.is_dir();
            let parent = path.parent().unwrap_or(&path).to_owned();
            if file && self.manager.is_none() {
                cx.reveal_path(&path);
                return;
            }
            let fallback_path = path.clone();
            let work = cx.background_spawn(async move {
                let run = |program: &Path, option: Option<&str>, target: &Path| {
                    Command::new(program)
                        .args(option)
                        .arg(target)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .is_ok_and(|status| status.success())
                };
                if reveal && let Some(manager) = self.manager {
                    let select = file && manager.selects_file;
                    let target = if file && !select { &parent } else { &path };
                    if run(&manager.program, select.then_some("--select"), target) {
                        return true;
                    }
                }
                if file {
                    return false;
                }
                self.launchers.iter().any(|launcher| {
                    run(
                        launcher,
                        (launcher.file_name() == Some("gio".as_ref())).then_some("open"),
                        &path,
                    )
                })
            });
            window
                .spawn(cx, async move |cx| {
                    if !work.await {
                        _ = cx.update(|window, cx| {
                            if file {
                                cx.reveal_path(&fallback_path);
                            } else {
                                toast::error(window, cx, t!("workspace.tree_system_open_failed"));
                            }
                        });
                    }
                })
                .detach();
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = window;
            if reveal && !path.is_dir() {
                cx.reveal_path(&paths::simplified(&path));
            } else {
                cx.open_with_system(&paths::simplified(&path));
            }
        }
    }
}

impl Workspace {
    pub(super) fn system_tree_menu(
        &self,
        mut menu: PopupMenu,
        path: &Path,
        directory: bool,
        git_row: bool,
        historical: bool,
    ) -> PopupMenu {
        let Some(repo) = self.repository.as_ref().filter(|repo| repo.ssh.is_none()) else {
            return menu;
        };
        if !path.as_os_str().is_empty() && crate::remote::RepoPath::from_local(path).is_err() {
            return menu;
        }
        let target = repo.root.join(path);
        let reveal = if target.exists() {
            Some(target.clone())
        } else if git_row {
            target
                .ancestors()
                .skip(1)
                .take_while(|ancestor| ancestor.starts_with(&repo.root))
                .find(|ancestor| ancestor.is_dir())
                .map(Path::to_owned)
        } else {
            None
        };
        let actions = self.system_files.clone();
        if !directory {
            let enabled = actions.can_open() && target.is_file();
            let open = actions.clone();
            let label = if historical {
                t!("workspace.tree_open_worktree_with_default")
            } else {
                t!("workspace.tree_open_with_default")
            };
            menu = menu.item(PopupMenuItem::new(label).disabled(!enabled).on_click(
                move |_, window, cx| open.clone().open(target.clone(), false, window, cx),
            ));
        }
        menu.item(
            PopupMenuItem::new(actions.manager_label(historical))
                .disabled(!actions.can_reveal() || reveal.is_none())
                .on_click(move |_, window, cx| {
                    if let Some(path) = &reveal {
                        actions.clone().open(path.clone(), true, window, cx);
                    }
                }),
        )
    }
}
