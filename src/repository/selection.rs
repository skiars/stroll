use rust_i18n::t;

use std::fs;

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use super::{Location, Repository, Store, atomic_write, git, read_json};
use crate::paths;

pub(crate) fn repository_root(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.try_exists().with_context(|| t!(
            "repository.access_failed",
            path = paths::display(path)
        )
        .into_owned())?,
        t!(
            "repository.directory_not_found",
            path = paths::display(path)
        )
    );
    ensure!(
        path.is_dir(),
        t!("repository.select_directory", path = paths::display(path))
    );
    let top = match git(path, &["rev-parse", "--show-toplevel"]) {
        Ok(top) => top,
        Err(error) if error.to_string().starts_with("fatal: not a git repository") => {
            bail!(t!(
                "repository.not_a_repository",
                path = paths::display(path)
            ));
        }
        Err(error)
            if error
                .to_string()
                .starts_with("fatal: this operation must be run in a work tree") =>
        {
            bail!(t!(
                "repository.worktree_required",
                path = paths::display(path)
            ));
        }
        Err(error) => {
            return Err(error).with_context(|| {
                t!("repository.open_git_failed", path = paths::display(path)).into_owned()
            });
        }
    };
    let top = top.strip_suffix(b"\n").unwrap_or(&top);
    PathBuf::from(path_from_git(top)?)
        .canonicalize()
        .context(t!("repository.no_root"))
}

pub(super) fn common_dir(root: &Path) -> Result<PathBuf> {
    let dir = git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let dir = dir.strip_suffix(b"\n").unwrap_or(&dir);
    Ok(root.join(path_from_git(dir)?).canonicalize()?)
}

pub(crate) fn directory_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn path_from_git(bytes: &[u8]) -> Result<OsString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        Ok(OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    {
        Ok(std::str::from_utf8(bytes)?.into())
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Worktree {
    pub(crate) root: PathBuf,
    pub(crate) reference: String,
    pub(crate) main: bool,
    pub(crate) available: bool,
}

impl Repository {
    pub(crate) fn worktrees(&self) -> Result<Vec<Worktree>> {
        if let Some(ssh) = &self.ssh {
            let mut trees: Vec<Worktree> = ssh.call(crate::remote::Request::Worktrees)?;
            for tree in &mut trees {
                tree.root = ssh.target().locator(&tree.root);
            }
            return Ok(trees);
        }
        let output = git(&self.root, &["worktree", "list", "--porcelain", "-z"])?;
        let mut records = output.split(|byte| *byte == 0).peekable();
        let mut worktrees = Vec::new();
        let mut first = true;
        while let Some(header) = records.next() {
            let Some(path) = header.strip_prefix(b"worktree ") else {
                continue;
            };
            let path = PathBuf::from(path_from_git(path)?);
            let mut branch = None;
            let mut head = None;
            let mut bare = false;
            let mut prunable = false;
            while records.peek().is_some_and(|field| !field.is_empty()) {
                let field = records.next().expect("peeked worktree field");
                if let Some(name) = field.strip_prefix(b"branch refs/heads/") {
                    branch = Some(String::from_utf8_lossy(name).into_owned());
                } else if let Some(oid) = field.strip_prefix(b"HEAD ") {
                    head = Some(String::from_utf8_lossy(&oid[..oid.len().min(7)]).into_owned());
                } else if field == b"bare" {
                    bare = true;
                } else if field.starts_with(b"prunable") {
                    prunable = true;
                }
            }
            if !bare {
                let available = !prunable && path.is_dir();
                worktrees.push(Worktree {
                    root: path.canonicalize().unwrap_or(path),
                    reference: branch.unwrap_or_else(|| match head {
                        Some(oid) => format!("HEAD · {oid}"),
                        None => "HEAD".into(),
                    }),
                    main: first,
                    available,
                });
            }
            first = false;
        }
        Ok(worktrees)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct DirectoryEntry {
    pub(crate) path: PathBuf,
    pub(crate) repository: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct DirectoryListing {
    pub(crate) path: PathBuf,
    pub(crate) repository: bool,
    pub(crate) entries: Vec<DirectoryEntry>,
}

impl DirectoryListing {
    pub(crate) fn read(path: PathBuf) -> Result<Self> {
        Ok(Self {
            entries: directory_entries(&path, "")?,
            repository: is_repository_root(&path),
            path,
        })
    }
}

impl DirectoryEntry {
    pub(crate) fn complete(parent: &Path, prefix: &str) -> Result<Vec<Self>> {
        directory_entries(parent, prefix)
    }
}

fn directory_entries(path: &Path, prefix: &str) -> Result<Vec<DirectoryEntry>> {
    let entries = fs::read_dir(path).with_context(|| {
        t!("repository.browse_failed", path = paths::display(path)).into_owned()
    })?;
    let prefix = prefix.to_lowercase();
    let mut entries: Vec<_> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            if name == ".git" || name == "node_modules" {
                return None;
            }
            let lower = name.to_string_lossy().to_lowercase();
            let path = entry.path();
            (lower.contains(&prefix) && path.is_dir()).then(|| (lower.starts_with(&prefix), path))
        })
        .collect();
    if entries.iter().any(|(starts, _)| *starts) {
        entries.retain(|(starts, _)| *starts);
    }
    let mut entries: Vec<_> = entries
        .into_iter()
        .map(|(_, path)| DirectoryEntry {
            repository: is_repository_root(&path),
            path,
        })
        .collect();
    entries.sort_by(|a, b| {
        b.repository
            .cmp(&a.repository)
            .then_with(|| a.path.file_name().cmp(&b.path.file_name()))
    });
    entries.truncate(if prefix.is_empty() { 4096 } else { 64 });
    Ok(entries)
}

fn is_repository_root(path: &Path) -> bool {
    // Git uses a directory for regular clones and a gitfile for linked worktrees.
    path.join(".git").exists()
        && repository_root(path)
            .is_ok_and(|root| path.canonicalize().is_ok_and(|path| root == path))
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct RecentRepository {
    pub(crate) root: PathBuf,
    pub(crate) common_dir: PathBuf,
}

impl RecentRepository {
    #[cfg(any(feature = "desktop", test))]
    pub(crate) fn name(&self) -> String {
        project_name(&self.root, &self.common_dir)
    }

    pub(crate) fn record(recents: &mut Vec<Self>, repo: &Repository) {
        recents.retain(|item| item.common_dir != repo.common_dir && item.root != repo.root);
        recents.insert(
            0,
            Self {
                root: repo.root.clone(),
                common_dir: repo.common_dir.clone(),
            },
        );
        recents.truncate(30);
    }
}

#[cfg(any(feature = "desktop", test))]
pub(crate) fn project_name(root: &Path, common_dir: &Path) -> String {
    if let Ok(Some(target)) = crate::remote::SshTarget::parse(root) {
        let unnamed = t!("repository.unnamed");
        let project = common_dir
            .to_str()
            .and_then(|path| path.strip_suffix("/.git"))
            .and_then(|path| path.rsplit('/').next())
            .or_else(|| target.path.file_name().and_then(|name| name.to_str()))
            .unwrap_or(&unnamed);
        return format!("{project} · {}", target.authority());
    }
    let project = if common_dir.file_name().is_some_and(|name| name == ".git") {
        common_dir.parent().unwrap_or(root)
    } else {
        root
    };
    directory_name(project)
}

#[derive(Deserialize, Serialize)]
pub(super) struct WorkspaceState {
    #[serde(flatten)]
    pub(super) location: Location,
    // None identifies a legacy workspace.json. An explicit empty list stays empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recents: Option<Vec<RecentRepository>>,
}

impl WorkspaceState {
    fn recents(self) -> Vec<RecentRepository> {
        self.recents
            .unwrap_or_else(|| legacy_recent(&self.location.root))
    }
}

fn legacy_recent(root: &Path) -> Vec<RecentRepository> {
    let common_dir = common_dir(root).unwrap_or_else(|_| root.join(".git"));
    vec![RecentRepository {
        root: root.to_owned(),
        common_dir,
    }]
}

impl Store {
    pub(super) fn workspace_state(&self) -> Result<Option<WorkspaceState>> {
        read_json(&self.root.join("workspace.json"))
    }

    pub(crate) fn recent_repositories(&self) -> Result<Vec<RecentRepository>> {
        Ok(self
            .workspace_state()?
            .map(WorkspaceState::recents)
            .unwrap_or_default())
    }

    pub fn remember_repository(&self, repo: &Repository) -> Result<()> {
        let mut recents = self.recent_repositories()?;
        RecentRepository::record(&mut recents, repo);
        self.save_workspace_state(WorkspaceState {
            location: Location {
                root: repo.root.clone(),
                file: None,
            },
            recents: Some(recents),
        })
    }

    #[cfg(any(feature = "desktop", test))]
    pub(crate) fn forget_recent(&self, common_dir: &Path) -> Result<()> {
        if let Some(mut state) = self.workspace_state()? {
            let mut recents = state
                .recents
                .take()
                .unwrap_or_else(|| legacy_recent(&state.location.root));
            recents.retain(|item| item.common_dir != common_dir);
            state.recents = Some(recents);
            self.save_workspace_state(state)?;
        }
        Ok(())
    }

    fn save_workspace_state(&self, state: WorkspaceState) -> Result<()> {
        atomic_write(
            &self.root.join("workspace.json"),
            &serde_json::to_vec(&state)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(windows, feature = "desktop"))]
    #[test]
    fn maps_language_server_file_uris_to_worktree_paths() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let worktree = directory.path().join("worktree");
        fs::create_dir(&worktree)?;
        git(&worktree, &["init", "-q"])?;
        let path = Path::new("定义 #.rs");
        fs::write(worktree.join(path), "fn definition() {}")?;
        let repo = Repository::open(&worktree)?;
        let uri = url::Url::from_file_path(repo.root.join(path)).unwrap();
        assert_eq!(repo.source_path(uri.as_str()).as_deref(), Some(path));

        let outside = directory.path().join("outside.rs");
        fs::write(&outside, "")?;
        let uri = url::Url::from_file_path(outside).unwrap();
        assert!(repo.source_path(uri.as_str()).is_none());
        let uri = url::Url::from_file_path(repo.root.join(".git/config")).unwrap();
        assert!(repo.source_path(uri.as_str()).is_none());
        Ok(())
    }

    #[test]
    fn multiple_workspaces_can_share_a_state_directory() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let state = directory.path().join("state");
        let first = Store::new(state.clone())?;
        let second = Store::new(state)?;
        assert!(first.recent_repositories()?.is_empty());
        assert!(second.recent_repositories()?.is_empty());
        Ok(())
    }

    #[test]
    fn linked_worktree_has_its_own_session_and_updates_the_project_recent() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let project = directory.path().join("project");
        let linked = directory.path().join("linked tree");
        let detached = directory.path().join("detached tree");
        let state = directory.path().join("state");
        fs::create_dir(&project)?;
        git(&project, &["init", "-q", "-b", "main"])?;
        git(
            &project,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ],
        )?;
        git(
            &project,
            &["worktree", "add", "-qb", "review", linked.to_str().unwrap()],
        )?;
        git(
            &project,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                detached.to_str().unwrap(),
            ],
        )?;
        let main = Repository::open(&project)?;
        let review = Repository::open(&linked)?;
        assert_eq!(main.common_dir, review.common_dir);
        let trees = review.worktrees()?;
        assert!(
            trees
                .iter()
                .any(|tree| tree.main && tree.root == main.root && tree.reference == "main")
        );
        assert!(
            trees.iter().any(|tree| tree.available
                && tree.root == review.root
                && tree.reference == "review")
        );
        assert!(
            trees
                .iter()
                .any(|tree| tree.root == detached.canonicalize().unwrap()
                    && tree.reference.starts_with("HEAD · "))
        );

        fs::create_dir(linked.join("nested"))?;
        let listing = DirectoryListing::read(directory.path().to_owned())?;
        assert!(
            listing
                .entries
                .iter()
                .any(|entry| entry.path == linked && entry.repository)
        );
        let listing = DirectoryListing::read(linked.clone())?;
        assert!(listing.repository);
        assert!(!listing.entries[0].repository);
        assert_eq!(repository_root(&linked.join("nested"))?, review.root);

        let store = Store::new(state.clone())?;
        let legacy = Location {
            root: project.clone(),
            file: Some("legacy.md".into()),
        };
        fs::write(state.join("workspace.json"), serde_json::to_vec(&legacy)?)?;
        assert_eq!(store.recent_repositories()?[0].root, legacy.root);
        assert_eq!(store.location()?.unwrap().file, legacy.file);

        fs::write(project.join("draft.md"), "draft")?;
        let mut tabs = crate::tabs::TabSession::default();
        tabs.open("draft.md".into(), false);
        store.save_tabs(&main, &tabs)?;
        store.remember_repository(&review)?;
        let recents = store.recent_repositories()?;
        assert_eq!(recents.len(), 1);
        assert_eq!(recents[0].root, review.root);
        assert_eq!(recents[0].name(), "project");
        assert_eq!(store.tabs(&main)?.active(), Some(Path::new("draft.md")));
        assert!(store.tabs(&review)?.active().is_none());
        drop(store);

        fs::remove_dir_all(&linked)?;
        assert!(
            main.worktrees()?
                .iter()
                .any(|tree| tree.reference == "review" && !tree.available)
        );
        let store = Store::new(state)?;
        assert_eq!(store.recent_repositories()?[0].root, review.root);
        store.forget_recent(&review.common_dir)?;
        assert!(store.recent_repositories()?.is_empty());
        assert_eq!(store.location()?.unwrap().root, review.root);
        assert_eq!(store.tabs(&main)?.active(), Some(Path::new("draft.md")));
        Ok(())
    }
}
