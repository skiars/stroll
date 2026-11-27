use super::*;
use crate::{
    remote::RepoPath,
    repository::files::{FileOperation, valid_name},
};

actions!(
    workspace,
    [
        CopyTreeEntry,
        CutTreeEntry,
        PasteTreeEntry,
        MoveTreeClipboard
    ]
);
struct TreeKeys;
impl Global for TreeKeys {}

#[derive(Clone)]
pub(super) struct TreeDrag {
    pub(super) repo: Arc<Repository>,
    pub(super) path: PathBuf,
}

impl Render for TreeDrag {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::for_mode(cx.theme().mode);
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(palette.elevated)
            .text_color(palette.ink)
            .child(file_name(&self.path).to_owned())
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct TreeClipboard {
    path: PathBuf,
    cut: bool,
}

enum Source {
    Worktree(PathBuf),
    Local(PathBuf),
}

impl Source {
    fn path(&self) -> &Path {
        match self {
            Self::Worktree(path) | Self::Local(path) => path,
        }
    }
}

enum Paste {
    Internal(TreeClipboard),
    Native { paths: ExternalPaths, cut: bool },
}

impl Workspace {
    pub(super) fn bind_tree_keys(cx: &mut App) {
        if cx.has_global::<TreeKeys>() {
            return;
        }
        cx.set_global(TreeKeys);
        let (copy, cut, paste) = if cfg!(target_os = "macos") {
            ("cmd-c", "cmd-x", "cmd-v")
        } else {
            ("ctrl-c", "ctrl-x", "ctrl-v")
        };
        let context = Some("WorkspaceFileTree && !Input");
        let navigation = Some("FileTree && !Input");
        cx.bind_keys([
            KeyBinding::new("up", file_tree::NavigateTree::Up, navigation),
            KeyBinding::new("down", file_tree::NavigateTree::Down, navigation),
            KeyBinding::new("left", file_tree::NavigateTree::Left, navigation),
            KeyBinding::new("right", file_tree::NavigateTree::Right, navigation),
            KeyBinding::new(
                "space",
                file_tree::OpenTreeFile { preview: true },
                Some("FileTree && !Input && !VimTree"),
            ),
            KeyBinding::new(
                if cfg!(target_os = "macos") {
                    "cmd-down"
                } else {
                    "enter"
                },
                file_tree::OpenTreeFile { preview: false },
                navigation,
            ),
            #[cfg(target_os = "macos")]
            KeyBinding::new(
                "enter",
                file_tree::OpenTreeFile { preview: false },
                Some("FileTree && !WorkspaceFileTree && !Input"),
            ),
            KeyBinding::new("f2", file_tree::RenameTreeEntry, context),
            #[cfg(target_os = "macos")]
            KeyBinding::new("enter", file_tree::RenameTreeEntry, context),
            KeyBinding::new(
                if cfg!(target_os = "macos") {
                    "cmd-backspace"
                } else {
                    "delete"
                },
                file_tree::DeleteTreeEntry,
                context,
            ),
            KeyBinding::new("f5", file_tree::RefreshTree, context),
            KeyBinding::new(copy, CopyTreeEntry, context),
            KeyBinding::new(cut, CutTreeEntry, context),
            KeyBinding::new(paste, PasteTreeEntry, context),
        ]);
        #[cfg(target_os = "macos")]
        cx.bind_keys([KeyBinding::new("alt-cmd-v", MoveTreeClipboard, context)]);
    }

    pub(super) fn tree_drag_available(&self) -> bool {
        !self.tree_busy
            && self.tree_edit.is_none()
            && self.view == WorkspaceView::Editor
            && self.accepts_file_drop()
    }

    pub(super) fn tree_transfer_available(&self) -> bool {
        self.tree_drag_available()
            && self.repository.as_ref().is_some_and(|repo| {
                repo.ssh
                    .as_ref()
                    .is_none_or(|ssh| ssh.supports_workspace_transfer() && ssh.failure().is_none())
            })
    }

    pub(super) fn tree_clipboard_available(&self, cx: &App) -> bool {
        self.tree_transfer_available()
            && self
                .repository
                .as_ref()
                .is_some_and(|repo| repo.ssh.is_some() || native_clipboard_available(cx))
    }

    pub(super) fn tree_paste_available(&self, cx: &App) -> bool {
        self.tree_clipboard_available(cx)
            && (self.tree_clipboard.is_some()
                || (native_clipboard_available(cx) && self.can_import_external()))
    }

    fn can_import_external(&self) -> bool {
        self.tree_transfer_available()
            && self.repository.as_ref().is_some_and(|repo| {
                repo.ssh
                    .as_ref()
                    .is_none_or(|ssh| ssh.supports_workspace_import())
            })
    }

    pub(super) fn focus_tree(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tree_keyboard_path = path;
        self.tree_focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn tree_key_copy(&mut self, cut: bool, window: &mut Window, cx: &mut Context<Self>) {
        let entry = self
            .rows
            .iter()
            .find(|row| row.entry.path == self.tree_keyboard_path)
            .map(|row| row.entry.clone());
        if let Some(entry) = entry {
            self.tree_copy(&entry, cut, window, cx);
        }
    }

    pub(super) fn tree_copy(
        &mut self,
        entry: &Entry,
        cut: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.tree_clipboard_available(cx) || (!cut && entry.symlink) {
            return;
        }
        let repo = self.repository.as_ref().unwrap();
        if let Some(ssh) = &repo.ssh {
            if ssh.supports_workspace_import()
                && native_clipboard_available(cx)
                && read_native_clipboard(cx).is_some_and(|item| item.file_list().is_some())
            {
                write_native_clipboard(None, cx);
            }
            self.tree_clipboard = Some(TreeClipboard {
                path: entry.path.clone(),
                cut,
            });
        } else {
            let path = paths::simplified(&repo.root.join(&entry.path));
            let item =
                ClipboardItem::new_file_list(ExternalPaths([path].into_iter().collect()), cut);
            if !write_native_clipboard(Some(item), cx) {
                toast::error(window, cx, t!("workspace.tree_clipboard_unavailable"));
            }
        }
        cx.notify();
    }

    pub(super) fn tree_key_paste(
        &mut self,
        force_move: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = &self.tree_keyboard_path;
        let directory = if path.as_os_str().is_empty() {
            path.clone()
        } else if let Some(row) = self.rows.iter().find(|row| &row.entry.path == path) {
            if row.entry.directory {
                path.clone()
            } else {
                path.parent().unwrap_or(Path::new("")).to_owned()
            }
        } else {
            return;
        };
        self.tree_paste(directory, force_move, window, cx);
    }

    pub(super) fn tree_paste(
        &mut self,
        directory: PathBuf,
        force_move: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.tree_paste_available(cx) {
            return;
        }
        let remote = self.repository.as_ref().unwrap().ssh.is_some();
        if self.can_import_external() && native_clipboard_available(cx) {
            let clipboard = read_native_clipboard(cx);
            if let Some((paths, cut)) = clipboard.as_ref().and_then(ClipboardItem::file_list) {
                if remote && (cut || force_move) {
                    toast::error(window, cx, t!("workspace.tree_import_copy_only"));
                    return;
                }
                self.run_tree_transfer(
                    paths.paths().iter().cloned().map(Source::Local).collect(),
                    directory,
                    cut || force_move,
                    Some(Paste::Native {
                        paths: paths.clone(),
                        cut,
                    }),
                    window,
                    cx,
                );
                return;
            }
        }
        if remote {
            if let Some(item) = self.tree_clipboard.clone() {
                self.run_tree_transfer(
                    vec![Source::Worktree(item.path.clone())],
                    directory,
                    item.cut || force_move,
                    Some(Paste::Internal(item)),
                    window,
                    cx,
                );
                return;
            }
        }
        toast::info(window, cx, t!("workspace.tree_clipboard_empty"));
    }

    fn tree_drop(
        &mut self,
        drag: &TreeDrag,
        directory: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.accepts_tree_drag(drag, &directory) {
            self.run_tree_transfer(
                vec![Source::Worktree(drag.path.clone())],
                directory,
                true,
                None,
                window,
                cx,
            );
        }
    }

    fn accepts_tree_drag(&self, drag: &TreeDrag, directory: &Path) -> bool {
        self.tree_transfer_available()
            && self
                .repository
                .as_ref()
                .is_some_and(|repo| Arc::ptr_eq(repo, &drag.repo))
            && !directory.starts_with(&drag.path)
            && drag.path.parent() != Some(directory)
    }

    fn import_tree_drop(
        &mut self,
        paths: &ExternalPaths,
        directory: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.can_import_external() {
            self.run_tree_transfer(
                paths.paths().iter().cloned().map(Source::Local).collect(),
                directory,
                false,
                None,
                window,
                cx,
            );
        }
    }

    fn run_tree_transfer(
        &mut self,
        sources: Vec<Source>,
        directory: PathBuf,
        move_entry: bool,
        paste: Option<Paste>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.tree_transfer_available() || sources.is_empty() {
            return;
        }
        let repo = self.repository.as_ref().unwrap().clone();
        if move_entry
            && self
                .saving
                .iter()
                .chain(self.git_edit.iter())
                .any(|pending| {
                    sources.iter().any(|source| match source {
                        Source::Worktree(path) => pending.starts_with(path),
                        Source::Local(path) => paths::simplified(&repo.root)
                            .join(pending)
                            .starts_with(path),
                    })
                })
        {
            toast::error(window, cx, t!("workspace.tree_operation_busy"));
            return;
        }
        if move_entry {
            self.cancel_file_open();
        }
        self.tree_busy = true;
        let working = repo.clone();
        let working_dir = directory.clone();
        let work = cx.background_spawn(async move {
            let mut seen = BTreeSet::new();
            sources
                .into_iter()
                .filter_map(|source| {
                    let path = source.path().to_owned();
                    if !seen.insert(path.clone()) {
                        return None;
                    }
                    let result = (|| -> Result<_> {
                        let name = path
                            .file_name()
                            .filter(|name| name.to_str().is_some_and(valid_name))
                            .ok_or_else(|| anyhow::anyhow!(t!("workspace.tree_invalid_name")))?;
                        let destination = working_dir.join(name);
                        let old = match source {
                            Source::Worktree(path) => {
                                let version = working.entry_version(&path)?;
                                let operation = FileOperation::Transfer {
                                    destination: RepoPath::from_local(&destination)?,
                                    version,
                                    move_entry,
                                };
                                working.operate(&path, operation)?;
                                Some(path)
                            }
                            Source::Local(path) => {
                                working.import_entry(&path, &destination, move_entry)?
                            }
                        };
                        Ok((destination, old))
                    })();
                    Some((path, result))
                })
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx| {
            let results = work.await;
            _ = this.update_in(cx, |this, window, cx| {
                if !this
                    .repository
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &repo))
                {
                    return;
                }
                this.tree_busy = false;
                let mut refresh = BTreeSet::from([directory]);
                let mut errors = Vec::new();
                let mut failed = Vec::new();
                for (path, result) in results {
                    match result {
                        Ok((destination, Some(source))) if move_entry => {
                            refresh.insert(source.parent().unwrap_or(Path::new("")).to_owned());
                            let affected = this.files_for_external_rename(&source);
                            this.relocate_tree_path(
                                &repo,
                                &source,
                                &destination,
                                affected,
                                window,
                                cx,
                            );
                        }
                        Ok(_) => {}
                        Err(error) => {
                            errors.push(format!("{} — {error:#}", paths::display(&path)));
                            failed.push(path);
                        }
                    }
                }
                if move_entry {
                    match paste {
                        Some(Paste::Internal(item))
                            if failed.is_empty() && this.tree_clipboard.as_ref() == Some(&item) =>
                        {
                            this.tree_clipboard = None;
                        }
                        Some(Paste::Native { paths, cut })
                            if failed.len() < paths.paths().len() =>
                        {
                            let current = read_native_clipboard(cx);
                            if current.as_ref().and_then(ClipboardItem::file_list)
                                == Some((&paths, cut))
                            {
                                let item = (!failed.is_empty()).then(|| {
                                    ClipboardItem::new_file_list(ExternalPaths(failed.into()), true)
                                });
                                write_native_clipboard(item, cx);
                            }
                        }
                        _ => {}
                    }
                }
                this.refresh_tree_paths(refresh, window, cx);
                if !errors.is_empty() {
                    toast::error(window, cx, errors.join("\n"));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

pub(super) fn tree_drag_source(
    row: Stateful<Div>,
    entry: &Entry,
    repo: Option<&Arc<Repository>>,
) -> Stateful<Div> {
    let Some(repo) = repo else {
        return row;
    };
    let directory = entry.directory;
    row.on_drag(
        TreeDrag {
            repo: repo.clone(),
            path: entry.path.clone(),
        },
        |drag, _, _, cx| cx.new(|_| drag.clone()),
    )
    .when(repo.ssh.is_none(), |row| {
        row.external_drag_payload(move |drag: &TreeDrag, _, _| {
            let path = drag.repo.root.join(&drag.path);
            path.symlink_metadata().ok()?;
            let path = paths::simplified(&path);
            Some(ExternalDragPayload::Files(FileDragPaths::new([(
                path, directory,
            )])))
        })
    })
}

pub(super) fn tree_drop_target(
    row: Stateful<Div>,
    directory: PathBuf,
    workspace: WeakEntity<Workspace>,
    colors: Palette,
) -> Stateful<Div> {
    let can_drop = workspace.clone();
    let target = directory.clone();
    let internal = workspace.clone();
    let internal_directory = directory.clone();
    row.can_drop(move |value, _, cx| {
        can_drop
            .read_with(cx, |this, _| {
                if let Some(drag) = value.downcast_ref::<TreeDrag>() {
                    this.accepts_tree_drag(drag, &target)
                } else {
                    value.downcast_ref::<ExternalPaths>().is_some() && this.can_import_external()
                }
            })
            .unwrap_or(false)
    })
    .drag_over::<TreeDrag>(move |style, _, _, _| style.bg(colors.accent.opacity(0.18)))
    .drag_over::<ExternalPaths>(move |style, _, _, _| style.bg(colors.accent.opacity(0.18)))
    .on_drop(move |drag: &TreeDrag, window, cx| {
        _ = internal.update(cx, |this, cx| {
            this.tree_drop(drag, internal_directory.clone(), window, cx)
        });
    })
    .on_drop(move |paths: &ExternalPaths, window, cx| {
        _ = workspace.update(cx, |this, cx| {
            this.import_tree_drop(paths, directory.clone(), window, cx)
        });
    })
}

fn native_clipboard_available(cx: &App) -> bool {
    #[cfg(target_os = "linux")]
    return cx.compositor_name() == "Wayland";
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cx;
        cfg!(any(target_os = "macos", target_os = "windows"))
    }
}

fn read_native_clipboard(cx: &App) -> Option<ClipboardItem> {
    #[cfg(target_os = "macos")]
    if let Some(item) = mac_clipboard::read() {
        return Some(item);
    }
    cx.read_from_clipboard()
}

fn write_native_clipboard(item: Option<ClipboardItem>, cx: &mut App) -> bool {
    #[cfg(target_os = "macos")]
    {
        let _ = cx;
        return mac_clipboard::write(item);
    }
    #[cfg(not(target_os = "macos"))]
    {
        cx.write_to_clipboard(item.unwrap_or(ClipboardItem {
            entries: Vec::new(),
        }));
        true
    }
}

#[cfg(target_os = "macos")]
mod mac_clipboard {
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{
        NSPasteboard, NSPasteboardItem, NSPasteboardTypeFileURL, NSPasteboardWriting,
    };
    use objc2_foundation::{NSArray, NSString};

    use super::*;

    const CUT_TYPE: &str = "app.stroll.file-cut";

    pub(super) fn read() -> Option<ClipboardItem> {
        let pasteboard = NSPasteboard::generalPasteboard();
        let file_url = unsafe { NSPasteboardTypeFileURL };
        let items = pasteboard.pasteboardItems()?;
        let cut_type = NSString::from_str(CUT_TYPE);
        let mut cut = true;
        let paths = items
            .iter()
            .map(|item| {
                let value = item.stringForType(file_url)?;
                cut &= item.stringForType(&cut_type).is_some();
                url::Url::parse(&value.to_string())
                    .ok()?
                    .to_file_path()
                    .ok()
            })
            .collect::<Option<_>>()?;
        let paths = ExternalPaths(paths);
        (!paths.paths().is_empty()).then(|| ClipboardItem::new_file_list(paths, cut))
    }

    pub(super) fn write(item: Option<ClipboardItem>) -> bool {
        let pasteboard = NSPasteboard::generalPasteboard();
        let Some(item) = item else {
            pasteboard.clearContents();
            return true;
        };
        let Some((paths, cut)) = item.file_list() else {
            return false;
        };
        let file_url = unsafe { NSPasteboardTypeFileURL };
        let cut_type = NSString::from_str(CUT_TYPE);
        let items = paths
            .paths()
            .iter()
            .map(|path| {
                let uri = url::Url::from_file_path(path).ok()?;
                let item = NSPasteboardItem::new();
                if !item.setString_forType(&NSString::from_str(uri.as_str()), file_url)
                    || (cut && !item.setString_forType(&NSString::from_str("1"), &cut_type))
                {
                    return None;
                }
                Some(item)
            })
            .collect::<Option<Vec<_>>>();
        let Some(items) = items else {
            return false;
        };
        let objects = items
            .iter()
            .map(|item| ProtocolObject::<dyn NSPasteboardWriting>::from_ref(&**item))
            .collect::<Vec<_>>();
        pasteboard.clearContents();
        pasteboard.writeObjects(&NSArray::from_slice(&objects))
    }
}
