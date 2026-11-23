use rust_i18n::t;

use std::fs;

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use rusqlite::{OptionalExtension as _, TransactionBehavior, params};

use super::{Location, Repository, Store, git};
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
    let dir = git(root, &["rev-parse", "--git-common-dir"])?;
    let dir = dir.strip_suffix(b"\n").unwrap_or(&dir);
    Ok(root.join(path_from_git(dir)?).canonicalize()?)
}

pub(crate) fn directory_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

pub(super) fn path_from_git(bytes: &[u8]) -> Result<OsString> {
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
        local_worktrees(&self.root)
    }
}

pub(super) fn local_worktrees(root: &Path) -> Result<Vec<Worktree>> {
    let output = git(root, &["worktree", "list", "--porcelain", "-z"])?;
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

const RECENT_REPOSITORY_LIMIT: usize = 30;

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct RecentRepository {
    pub(crate) root: PathBuf,
    pub(crate) common_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) worktree_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    previous_worktrees: Vec<RecentWorktree>,
}

#[derive(Clone, Deserialize, Serialize)]
struct RecentWorktree {
    root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

impl RecentRepository {
    #[cfg(any(feature = "desktop", test))]
    pub(crate) fn name(&self) -> String {
        project_name(&self.root, &self.common_dir)
    }

    pub(crate) fn record(recents: &mut Vec<Self>, repo: &Repository) {
        #[cfg(feature = "desktop")]
        let worktree_id = repo.worktree_state.as_ref().map(|state| state.id.clone());
        #[cfg(not(feature = "desktop"))]
        let worktree_id: Option<String> = None;

        let mut previous_worktrees = Vec::new();
        if let Some(index) = recents
            .iter()
            .position(|item| item.common_dir == repo.common_dir)
        {
            let recent = recents.remove(index);
            previous_worktrees = recent.previous_worktrees;
            previous_worktrees.insert(
                0,
                RecentWorktree {
                    root: recent.root,
                    id: recent.worktree_id,
                },
            );
            previous_worktrees.retain(|item| match (&item.id, &worktree_id) {
                (Some(previous), Some(current)) => previous != current,
                _ => item.root != repo.root,
            });
        }
        recents.retain(|item| item.common_dir != repo.common_dir && item.root != repo.root);
        recents.insert(
            0,
            Self {
                root: repo.root.clone(),
                common_dir: repo.common_dir.clone(),
                worktree_id,
                previous_worktrees,
            },
        );
        recents.truncate(RECENT_REPOSITORY_LIMIT);
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn sort_worktrees(&self, trees: &mut [Worktree], remote: bool) {
        let history = std::iter::once((&self.root, self.worktree_id.as_ref()))
            .chain(
                self.previous_worktrees
                    .iter()
                    .map(|item| (&item.root, item.id.as_ref())),
            )
            .collect::<Vec<_>>();
        let has_ids = history.iter().any(|(_, id)| id.is_some());
        trees.sort_by_cached_key(|tree| {
            let id = (!remote && has_ids && !tree.main && tree.available)
                .then(|| super::worktree_state::worktree_id(&tree.root))
                .flatten();
            history
                .iter()
                .position(|(root, saved_id)| {
                    saved_id
                        .filter(|_| tree.available)
                        .map_or(tree.root == **root, |saved_id| {
                            id.as_ref() == Some(saved_id)
                        })
                })
                .unwrap_or(usize::MAX)
        });
    }
}

#[cfg(any(feature = "desktop", test))]
pub(crate) fn project_name(root: &Path, common_dir: &Path) -> String {
    if let Ok(Some(target)) = crate::remote::SshTarget::parse(root) {
        let project = ssh_project_name(&target, common_dir);
        return format!("{project} · {}", target.authority());
    }
    let project = if common_dir.file_name().is_some_and(|name| name == ".git") {
        common_dir.parent().unwrap_or(root)
    } else {
        root
    };
    directory_name(project)
}

#[cfg(any(feature = "desktop", test))]
pub(crate) fn ssh_project_name(target: &crate::remote::SshTarget, common_dir: &Path) -> String {
    common_dir
        .to_str()
        .and_then(|path| path.strip_suffix("/.git"))
        .and_then(|path| path.rsplit('/').next())
        .or_else(|| target.path.file_name().and_then(|name| name.to_str()))
        .map_or_else(|| t!("repository.unnamed").into_owned(), str::to_owned)
}

#[derive(Deserialize, Serialize)]
struct StartupLocation {
    location: Location,
    worktree_id: Option<String>,
    common_dir: PathBuf,
}

fn startup_location(db: &rusqlite::Connection) -> Result<Option<StartupLocation>> {
    let value: Option<Vec<u8>> = db
        .query_row(
            "SELECT value FROM startup_location WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.as_deref().map(serde_json::from_slice).transpose()?)
}

impl Store {
    pub fn location(&self) -> Result<Option<Location>> {
        self.with_state(|db| Ok(startup_location(db)?.map(|state| state.location)))
    }

    #[cfg(feature = "desktop")]
    pub(super) fn saved_repository(
        &self,
        root: &Path,
    ) -> Result<Option<(PathBuf, Option<String>)>> {
        self.with_state(|db| {
            let tx = db.transaction()?;
            if let Some(state) = startup_location(&tx)?
                && state.location.root == root
            {
                return Ok(Some((state.common_dir, state.worktree_id)));
            }
            let value: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT value FROM recent_repositories WHERE root = ?1",
                    [root.as_os_str().as_encoded_bytes()],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(value
                .as_deref()
                .map(serde_json::from_slice::<RecentRepository>)
                .transpose()?
                .map(|recent| (recent.common_dir, recent.worktree_id)))
        })
    }

    #[cfg(any(feature = "desktop", test))]
    pub(crate) fn recent_repositories(&self) -> Result<Vec<RecentRepository>> {
        self.with_state(|db| {
            let mut query =
                db.prepare_cached("SELECT value FROM recent_repositories ORDER BY opened DESC")?;
            query
                .query_map([], |row| row.get::<_, Vec<u8>>(0))?
                .map(|row| Ok(serde_json::from_slice(&row?)?))
                .collect()
        })
    }

    pub fn remember_repository(&self, repo: &Repository) -> Result<()> {
        self.with_state(|db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            #[cfg(feature = "desktop")]
            if let Some(state) = &repo.worktree_state {
                super::worktree_state::write_worktree(&tx, state)?;
            }

            let common_dir = repo.common_dir.as_os_str().as_encoded_bytes();
            let root = repo.root.as_os_str().as_encoded_bytes();
            let previous: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT value FROM recent_repositories WHERE common_dir = ?1",
                    [common_dir],
                    |row| row.get(0),
                )
                .optional()?;
            let mut recents = previous.as_deref()
                .map(serde_json::from_slice)
                .transpose()?
                .into_iter()
                .collect();
            RecentRepository::record(&mut recents, repo);
            let recent = &recents[0];
            tx.execute(
                "DELETE FROM recent_repositories WHERE root = ?1 AND common_dir != ?2",
                params![root, common_dir],
            )?;
            tx.execute(
                "INSERT INTO recent_repositories (common_dir, root, opened, value)
                 VALUES (?1, ?2, (SELECT coalesce(max(opened), 0) + 1 FROM recent_repositories), ?3)
                 ON CONFLICT(common_dir) DO UPDATE SET
                    root = excluded.root, opened = excluded.opened, value = excluded.value",
                params![common_dir, root, serde_json::to_vec(recent)?],
            )?;
            tx.execute(
                "DELETE FROM recent_repositories WHERE common_dir IN (
                    SELECT common_dir FROM recent_repositories ORDER BY opened DESC LIMIT -1 OFFSET ?1
                )",
                [RECENT_REPOSITORY_LIMIT as i64],
            )?;
            let startup = StartupLocation {
                location: Location { root: repo.root.clone(), file: None },
                worktree_id: recent.worktree_id.clone(),
                common_dir: repo.common_dir.clone(),
            };
            tx.execute(
                "INSERT INTO startup_location VALUES (1, ?1)
                 ON CONFLICT(id) DO UPDATE SET value = excluded.value",
                [serde_json::to_vec(&startup)?],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    #[cfg(any(feature = "desktop", test))]
    pub(crate) fn forget_recent(&self, common_dir: &Path) -> Result<()> {
        self.with_state(|db| {
            db.execute(
                "DELETE FROM recent_repositories WHERE common_dir = ?1",
                [common_dir.as_os_str().as_encoded_bytes()],
            )?;
            Ok(())
        })
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
        let init_repo = |name| -> Result<Repository> {
            let root = directory.path().join(name);
            fs::create_dir(&root)?;
            git(&root, &["init", "-q"])?;
            Repository::open(&root)
        };
        let one = init_repo("first")?;
        let two = init_repo("second")?;
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            // The two independent connections contend for the same state directory.
            let remember_one = scope.spawn(|| {
                start.wait();
                first.remember_repository(&one)
            });
            start.wait();
            let result = second.remember_repository(&two);
            remember_one.join().unwrap()?;
            result
        })?;
        let recents = first.recent_repositories()?;
        assert_eq!(recents.len(), 2);
        assert_eq!(first.location()?.unwrap().root, recents[0].root);
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
        store.remember_repository(&main)?;
        assert_eq!(store.location()?.unwrap().root, main.root);

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

    #[cfg(feature = "desktop")]
    #[test]
    fn worktree_recency_survives_reopening_the_store() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().canonicalize()?;
        let project = root.join("project");
        let first = root.join("first");
        let second = root.join("second");
        let unused = root.join("unused");
        let other = root.join("other");
        let state = root.join("state");
        fs::create_dir(&project)?;
        fs::create_dir(&other)?;
        git(&project, &["init", "-qb", "main"])?;
        git(&other, &["init", "-q"])?;
        git(
            &project,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ],
        )?;
        for (branch, path) in [("first", &first), ("second", &second), ("unused", &unused)] {
            git(
                &project,
                &["worktree", "add", "-qb", branch, path.to_str().unwrap()],
            )?;
        }
        let main = Repository::open(&project)?;
        let store = Store::new(state.clone())?;
        store.remember_repository(&main)?;
        for path in [&first, &second, &other, &first] {
            let mut repo = Repository::open(path)?;
            store.bind_worktree(&mut repo)?;
            store.remember_repository(&repo)?;
        }
        drop(store);

        let moved = root.join("moved-second");
        git(
            &project,
            &[
                "worktree",
                "move",
                second.to_str().unwrap(),
                moved.to_str().unwrap(),
            ],
        )?;
        let store = Store::new(state)?;
        let recents = store.recent_repositories()?;
        assert_eq!(
            recents.iter().map(|item| &item.root).collect::<Vec<_>>(),
            [&first, &other]
        );
        let mut trees = main.worktrees()?;
        recents[0].sort_worktrees(&mut trees, false);
        assert_eq!(
            trees.iter().map(|tree| &tree.root).collect::<Vec<_>>(),
            [&first, &moved, &project, &unused]
        );
        Ok(())
    }
}
