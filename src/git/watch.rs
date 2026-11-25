use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher as _};
use serde::{Deserialize, Serialize};

const MAX_PATHS: usize = 1024;
pub(crate) const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(140);

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct RepoChanges {
    pub paths: BTreeSet<String>,
    pub refs: bool,
    pub index: bool,
    pub rescan: bool,
    #[serde(default)]
    pub structure: bool,
    #[serde(default)]
    pub directories: BTreeSet<PathBuf>,
    #[serde(default)]
    pub renames: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub ignored_paths: BTreeSet<String>,
}

impl RepoChanges {
    pub fn merge(&mut self, changes: Self) {
        self.refs |= changes.refs;
        self.index |= changes.index;
        self.rescan |= changes.rescan;
        self.structure |= changes.structure;
        if self.directories.len() + changes.directories.len() <= MAX_PATHS {
            self.directories.extend(changes.directories);
        } else {
            self.rescan = true;
        }
        if self.renames.len() + changes.renames.len() <= MAX_PATHS {
            self.renames.extend(changes.renames);
        } else {
            self.rescan = true;
        }
        if !self.rescan {
            for path in changes.paths {
                if !changes.ignored_paths.contains(&path) {
                    self.ignored_paths.remove(&path);
                } else if !self.paths.contains(&path) {
                    self.ignored_paths.insert(path.clone());
                }
                self.paths.insert(path);
            }
            self.rescan = self.paths.len() > MAX_PATHS;
        }
        if self.rescan {
            self.paths.clear();
            self.directories.clear();
            self.ignored_paths.clear();
        }
    }

    pub fn classify_ignored_paths(&mut self, root: &Path) {
        let rule_dirs = self
            .paths
            .iter()
            .map(Path::new)
            .filter(|path| path.file_name().is_some_and(|name| name == ".gitignore"))
            .filter_map(Path::parent)
            .collect::<BTreeSet<_>>();
        let mut paths = self.paths.iter().map(PathBuf::from).collect::<Vec<_>>();
        paths.extend(
            rule_dirs
                .iter()
                .copied()
                .filter(|path| !path.as_os_str().is_empty())
                .map(PathBuf::from),
        );
        // Keep paths Git cannot classify, including paths below an indexed Gitlink.
        let ignored =
            crate::repository::files::check_ignored_paths(root, &paths, false).unwrap_or_default();
        // Git still reads an ignored .gitignore when it visits the parent directory.
        self.rescan |= rule_dirs.iter().any(|dir| !ignored.contains(*dir));
        self.ignored_paths = self
            .paths
            .iter()
            .filter(|path| ignored.contains(Path::new(path.as_str())))
            .cloned()
            .collect();
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
            && !self.refs
            && !self.index
            && !self.rescan
            && !self.structure
            && self.renames.is_empty()
            && self.directories.is_empty()
    }
}

#[derive(Clone)]
pub(crate) struct RepoEvents {
    pending: Arc<Mutex<RepoChanges>>,
    ready: async_channel::Sender<()>,
    pub wake: async_channel::Receiver<()>,
}

impl Default for RepoEvents {
    fn default() -> Self {
        let (ready, wake) = async_channel::bounded(1);
        Self {
            pending: Default::default(),
            ready,
            wake,
        }
    }
}

impl RepoEvents {
    pub fn push(&self, changes: RepoChanges) {
        if !changes.is_empty() {
            self.pending.lock().unwrap().merge(changes);
            _ = self.ready.try_send(());
        }
    }

    pub fn take(&self) -> RepoChanges {
        // Stale wakeups are harmless and rechecked by consumers.
        std::mem::take(&mut *self.pending.lock().unwrap())
    }

    pub fn close(&self) {
        self.ready.close();
    }
}

pub(crate) struct RepoWatcher {
    _watcher: notify::RecommendedWatcher,
    pub events: RepoEvents,
}

impl RepoWatcher {
    pub fn open(root: &Path, common: &Path) -> Result<Self> {
        let git = super::repo::git_dir(root).canonicalize()?;
        let common = common.canonicalize()?;
        let root = root.canonicalize()?;
        let worktree = root.clone();
        let admin = git.clone();
        let shared = common.clone();
        let (mut watcher, events) =
            Self::build(move |event| classify(&worktree, &admin, &shared, event))?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .context("Watch worktree")?;
        // Watch directories so replacing index, HEAD or packed-refs by rename is visible.
        for folder in [&git, &common] {
            if !folder.starts_with(&root) {
                watcher.watch(folder, RecursiveMode::NonRecursive)?;
            }
        }
        for folder in [
            common.join("refs"),
            common.join("reftable"),
            git.join("logs"),
        ] {
            if folder.is_dir() && !folder.starts_with(&root) {
                watcher.watch(&folder, RecursiveMode::Recursive)?;
            }
        }
        Ok(Self {
            _watcher: watcher,
            events,
        })
    }

    #[cfg(feature = "desktop")]
    pub fn open_file(path: &Path) -> Result<Self> {
        let file = path.to_owned();
        let (mut watcher, events) = Self::build(move |event| RepoChanges {
            rescan: event.paths.iter().any(|path| path == &file),
            ..Default::default()
        })?;
        // An editor may save by replacing the file with a new inode.
        let parent = path.parent().context("Find watched file directory")?;
        watcher.watch(parent, RecursiveMode::NonRecursive)?;
        Ok(Self {
            _watcher: watcher,
            events,
        })
    }

    fn build(
        classify: impl Fn(notify::Event) -> RepoChanges + Send + 'static,
    ) -> Result<(notify::RecommendedWatcher, RepoEvents)> {
        let events = RepoEvents::default();
        let output = events.clone();
        let watcher = notify::RecommendedWatcher::new(
            move |event: notify::Result<notify::Event>| {
                output.push(match event {
                    Ok(event) if !event.need_rescan() && !event.kind.is_access() => classify(event),
                    Ok(event) if !event.need_rescan() => return,
                    Ok(_) => RepoChanges {
                        rescan: true,
                        ..Default::default()
                    },
                    Err(_) => {
                        // The consumer will reconcile and switch to its fallback.
                        output.push(RepoChanges {
                            rescan: true,
                            ..Default::default()
                        });
                        output.close();
                        return;
                    }
                });
            },
            notify::Config::default().with_follow_symlinks(false),
        )?;
        Ok((watcher, events))
    }
}

fn classify(root: &Path, git: &Path, common: &Path, event: notify::Event) -> RepoChanges {
    use notify::{
        EventKind,
        event::{ModifyKind, RenameMode},
    };
    let structural = matches!(
        event.kind,
        EventKind::Any
            | EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(ModifyKind::Name(_) | ModifyKind::Any)
    );
    let mut changes = RepoChanges::default();
    for path in &event.paths {
        if let Ok(path) = path.strip_prefix(git) {
            changes.index |= path == Path::new("index");
            changes.rescan |= path == Path::new("info/exclude");
            changes.refs |= path == Path::new("HEAD") || path == Path::new("logs/HEAD");
        }
        if let Ok(path) = path.strip_prefix(common) {
            changes.refs |= path == Path::new("packed-refs")
                || (path.starts_with("refs") && path.extension().is_none_or(|ext| ext != "lock"))
                || path.starts_with("reftable");
        }
        if path.starts_with(git) || path.starts_with(common) {
            continue;
        }
        if let Ok(path) = path.strip_prefix(root) {
            if path == Path::new(".git") || path.as_os_str().is_empty() {
                changes.rescan = true;
            } else if let Some(name) = path.to_str() {
                changes.structure |= structural;
                if structural && let Some(parent) = path.parent() {
                    changes.directories.insert(parent.to_owned());
                }
                changes
                    .paths
                    .insert(name.replace(std::path::MAIN_SEPARATOR, "/"));
            } else {
                changes.rescan = true;
            }
        }
    }
    if matches!(
        event.kind,
        EventKind::Modify(ModifyKind::Name(RenameMode::Both))
    ) && let [from, to] = event.paths.as_slice()
        && let (Some(from), Some(to)) = (
            relative_event_path(root, from),
            relative_event_path(root, to),
        )
        && changes.paths.contains(&from)
        && changes.paths.contains(&to)
    {
        changes.renames.push((from, to));
    }
    changes
}

fn relative_event_path(root: &Path, path: &Path) -> Option<String> {
    Some(
        path.strip_prefix(root)
            .ok()?
            .to_str()?
            .replace(std::path::MAIN_SEPARATOR, "/"),
    )
}

impl Drop for RepoWatcher {
    fn drop(&mut self) {
        self.events.close();
    }
}
