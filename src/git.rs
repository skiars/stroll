use rust_i18n::t;

mod blame;
mod changes;
mod history;
mod operations;
mod repo;
pub(crate) mod watch;
mod worktree_edit;

pub(crate) use blame::*;
pub(crate) use changes::*;
pub(crate) use history::*;
pub(crate) use operations::*;
#[cfg(feature = "desktop")]
pub(crate) use repo::GitSession;
pub(crate) use repo::{RepoState, session_for};
pub(crate) use worktree_edit::*;

use std::{
    ffi::OsStr,
    io::{Read, Seek},
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};

const METADATA_LIMIT: usize = 32 * 1024 * 1024;

// Count Git processes started by the current test thread to catch per-file spawning regressions.
#[cfg(test)]
thread_local! {
    static GIT_SPAWNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_spawn_count() {
    GIT_SPAWNS.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn spawn_count() -> usize {
    GIT_SPAWNS.with(std::cell::Cell::get)
}

fn command(root: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Command {
    #[cfg(test)]
    GIT_SPAWNS.with(|count| count.set(count.get() + 1));
    let mut command = crate::process::background("git");
    command
        .args([
            "--no-pager",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.quotePath=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "diff.renames=true",
            "-c",
            "log.showSignature=false",
        ])
        .arg("-C")
        .arg(root)
        .args(args)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    command
}

fn read(root: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Result<Vec<u8>> {
    let (bytes, truncated) = capture(command(root, args), METADATA_LIMIT, false)?;
    if truncated {
        bail!(t!("git.output_limit"));
    }
    Ok(bytes)
}

/// Cap stdout without waiting for Git to finish producing a very large patch.
fn capture(mut command: Command, limit: usize, diff_exit: bool) -> Result<(Vec<u8>, bool)> {
    let (bytes, truncated, status, stderr) = capture_output(&mut command, limit)?;
    if !truncated && !(status.success() || diff_exit && status.code() == Some(1)) {
        let error = String::from_utf8_lossy(&stderr);
        let detail = if error.is_empty() {
            String::new()
        } else {
            format!("\n{}", error.trim_end())
        };
        bail!(t!("git.read_failed", status = status, detail = detail));
    }
    Ok((bytes, truncated))
}

pub(crate) fn capture_repository(mut command: Command) -> Result<Vec<u8>> {
    let (bytes, truncated, status, stderr) = capture_output(&mut command, METADATA_LIMIT)?;
    if truncated {
        bail!(t!("git.index_limit"));
    }
    if !status.success() {
        bail!("{}", String::from_utf8_lossy(&stderr).trim_end());
    }
    Ok(bytes)
}

fn capture_output(
    command: &mut Command,
    limit: usize,
) -> Result<(Vec<u8>, bool, std::process::ExitStatus, Vec<u8>)> {
    let mut stderr = tempfile::tempfile()?;
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(stderr.try_clone()?)
        .spawn()
        .context(t!("git.start_failed"))?;
    let mut bytes = Vec::new();
    let result = child
        .stdout
        .take()
        .unwrap()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes);
    let truncated = bytes.len() > limit;
    if truncated || result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    result?;
    let mut error = Vec::new();
    if truncated {
        bytes.truncate(limit);
    } else if !status.success() {
        stderr.rewind()?;
        stderr.take(4096).read_to_end(&mut error)?;
    }
    Ok((bytes, truncated, status, error))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Process-wide registry with one persistent local Git helper per worktree root.
///
/// Start helpers on demand from background tasks, where all `Backend` methods run.
/// If startup fails or the helper exits, that repository falls back to in-process Git without retries.
#[cfg(feature = "desktop")]
mod helper {
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
        sync::{Arc, Mutex, OnceLock},
    };

    use crate::remote::GitHelper;

    static HELPERS: OnceLock<Mutex<HashMap<PathBuf, Arc<GitHelper>>>> = OnceLock::new();

    fn enabled() -> bool {
        !matches!(
            std::env::var("STROLL_GIT_HELPER").as_deref(),
            Ok("0") | Ok("off") | Ok("false")
        )
    }

    pub(super) fn session(root: &Path) -> Option<Arc<GitHelper>> {
        if !enabled() {
            return None;
        }
        // Hold the lock while connecting so concurrent first requests share one helper process.
        let mut helpers = HELPERS.get_or_init(Default::default).lock().unwrap();
        if let Some(helper) = helpers.get(root) {
            return Some(helper.clone());
        }
        let helper = GitHelper::connect(root)
            .map_err(|error| {
                if std::env::var_os("STROLL_GIT_HELPER_LOG").is_some() {
                    eprintln!(
                        "Local Git helper unavailable. Falling back to in-process Git: {error:#}"
                    );
                }
            })
            .ok()
            .map(Arc::new);
        helpers.insert(root.to_owned(), helper.clone()?);
        helper
    }

    /// Remove a failed helper from the registry and run subsequent requests in-process.
    pub(super) fn retire(root: &Path) {
        if let Some(helpers) = HELPERS.get() {
            helpers.lock().unwrap().remove(root);
        }
    }
}

fn object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(feature = "desktop")]
pub(crate) fn short(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

pub(crate) fn openable(root: &Path, paths: &[Option<String>]) -> Vec<bool> {
    paths
        .iter()
        .map(|path| {
            path.as_ref()
                .is_some_and(|path| crate::repository::resolve_file(root, Path::new(path)).is_ok())
        })
        .collect()
}

/// Shared local Git session for reusing index and HEAD lookups and blob reads across files.
#[cfg(feature = "desktop")]
#[derive(Clone)]
pub(crate) struct Backend {
    root: std::path::PathBuf,
    ssh: Option<crate::remote::SshSession>,
    session: Option<std::sync::Arc<GitSession>>,
}

#[cfg(feature = "desktop")]
impl Backend {
    pub fn new(root: std::path::PathBuf, ssh: Option<crate::remote::SshSession>) -> Self {
        // Allocate the structure without starting any processes. Each worktree root shares it across the process.
        let session = ssh.is_none().then(|| session_for(&root));
        Self { root, ssh, session }
    }

    pub fn supports_git_edit(&self) -> bool {
        self.ssh.as_ref().is_none_or(|ssh| ssh.supports_git_edit())
    }

    pub fn supports_git_operations(&self) -> bool {
        self.ssh
            .as_ref()
            .is_none_or(|ssh| ssh.supports_git_operations())
    }

    pub fn commit_state(&self) -> Result<CommitState> {
        self.operation(crate::remote::Request::CommitState, commit_state)
    }

    pub fn supports_ai_context(&self) -> bool {
        self.ssh
            .as_ref()
            .is_none_or(|ssh| ssh.supports_ai_context())
    }

    pub fn staged_commit_context(
        &self,
        expected: CommitState,
    ) -> Result<Option<StagedCommitContext>> {
        anyhow::ensure!(self.supports_ai_context(), t!("ai.upgrade_helper"));
        self.operation(
            crate::remote::Request::StagedCommitContext(expected.clone()),
            |root| staged_commit_context(root, &expected),
        )
    }

    pub fn create_commit(&self, expected: CommitState, message: String) -> Result<String> {
        let result = self.operation(
            crate::remote::Request::CreateCommit(expected.clone(), message.clone()),
            |root| create_commit(root, &expected, &message),
        );
        self.invalidate();
        result
    }

    pub fn operation_history(&self) -> Result<Vec<Operation>> {
        self.operation(crate::remote::Request::OperationHistory, operation_history)
    }

    pub fn restore_history_text(
        &self,
        id: u64,
        undo: bool,
    ) -> Result<Option<(crate::remote::Bytes, [u8; 32])>> {
        match &self.ssh {
            Some(ssh) => ssh.restore_history_text(id, undo),
            None => restore_history_text(&self.root, id, undo),
        }
    }

    pub fn replay_operation(&self, id: u64, undo: bool) -> Result<()> {
        let request = if self.supports_git_history() {
            crate::remote::Request::ReplayGitHistory(id, undo)
        } else {
            anyhow::ensure!(undo, t!("git.upgrade_operations"));
            crate::remote::Request::UndoOperation(id)
        };
        let result = self.operation(request, |root| replay_operation(root, id, undo));
        self.invalidate();
        result
    }

    fn supports_git_history(&self) -> bool {
        self.ssh
            .as_ref()
            .is_none_or(|ssh| ssh.supports_git_history())
    }

    fn operation<T: serde::de::DeserializeOwned>(
        &self,
        request: crate::remote::Request,
        local: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        match &self.ssh {
            Some(ssh) => {
                anyhow::ensure!(ssh.supports_git_operations(), t!("git.upgrade_operations"));
                ssh.call(request)
            }
            None => local(&self.root),
        }
    }

    pub fn disconnected(&self) -> bool {
        self.ssh.as_ref().is_some_and(|ssh| ssh.failure().is_some())
    }

    pub fn history(&self) -> Result<History> {
        self.call(crate::remote::Request::History, |state| {
            history(state.root())
        })
    }

    pub fn commits(&self, oid: &str, query: &str, offset: usize) -> Result<CommitPage> {
        self.call(
            crate::remote::Request::Commits(oid.into(), query.into(), offset),
            |state| commits(state.root(), oid, query, offset),
        )
    }

    pub fn commit(&self, oid: &str) -> Result<Commit> {
        self.call(crate::remote::Request::Commit(oid.into()), |state| {
            commit(state.root(), oid)
        })
    }

    pub fn check_revision_comparison(&self) -> Result<()> {
        if let Some(ssh) = &self.ssh {
            anyhow::ensure!(
                ssh.supports_revision_comparison(),
                t!("git.upgrade_comparison")
            );
        }
        Ok(())
    }

    pub fn search_revisions(&self, query: &str) -> Result<Vec<RevisionMatch>> {
        self.call(
            crate::remote::Request::SearchRevisions(query.to_owned()),
            |state| search_revisions(state.root(), query),
        )
    }

    pub(crate) fn worktree_status(&self) -> Result<std::sync::Arc<WorktreeStatus>> {
        if let Some(ssh) = &self.ssh {
            if ssh.supports_workspace_files() {
                return ssh.call(crate::remote::Request::WorktreeStatus);
            }
            return self
                .review_files(Comparison::Worktree {
                    mode: Mode::All,
                    base: None,
                })?
                .0
                .status
                .context("Worktree status is unavailable");
        }
        self.session
            .as_ref()
            .context("Missing local Git session")?
            .call(|state| Ok(state.status()?.clone()))
    }

    pub fn changes(&self, comparison: Comparison) -> Result<Changes> {
        self.call(
            crate::remote::Request::Changes(comparison.clone()),
            |state| changes(state, comparison),
        )
    }

    /// Returns whether the comparison's statistics still need to be loaded.
    pub fn review_files(&self, comparison: Comparison) -> Result<(Changes, bool)> {
        let split = match &self.ssh {
            Some(ssh) => ssh.supports_review_files(),
            None => helper::session(&self.root).is_none_or(|helper| helper.supports_review_files()),
        };
        if !split {
            return self.changes(comparison).map(|detail| (detail, false));
        }
        self.call(
            crate::remote::Request::ReviewFiles(comparison.clone()),
            |state| review_files(state, comparison),
        )
        .map(|detail| (detail, true))
    }

    pub fn review_stats(&self, detail: &Changes) -> Result<ChangeStats> {
        self.call(
            crate::remote::Request::ReviewStats(detail.comparison.clone(), detail.base.clone()),
            |state| review_stats(state, detail.comparison.clone(), &detail.base),
        )
    }

    pub fn openable(&self, files: &[ChangedFile]) -> Result<Vec<bool>> {
        let paths = files
            .iter()
            .map(|file| (!file.submodule()).then(|| file.path.clone()))
            .collect::<Vec<_>>();
        self.call(crate::remote::Request::Openable(paths.clone()), |state| {
            Ok(openable(state.root(), &paths))
        })
    }

    pub fn patch(
        &self,
        detail: &Changes,
        file: &ChangedFile,
        context: usize,
        whitespace: bool,
    ) -> Result<Patch> {
        render_patch(
            file,
            self.review_patch_text(detail, file, context, whitespace)?,
        )
    }

    pub(crate) fn review_patch_text(
        &self,
        detail: &Changes,
        file: &ChangedFile,
        context: usize,
        whitespace: bool,
    ) -> Result<RawPatch> {
        self.call(
            crate::remote::Request::Patch(
                detail.comparison.clone(),
                file.path.clone(),
                context,
                whitespace,
            ),
            |state| raw_patch(state, detail, file, context, whitespace),
        )
    }

    pub fn snapshot(&self, detail: &Changes, file: &ChangedFile, side: Side) -> Result<Snapshot> {
        self.call(
            crate::remote::Request::Snapshot(detail.comparison.clone(), file.path.clone(), side),
            |state| snapshot(state, detail, file, side),
        )
    }

    pub fn stage(&self, expected: &WorktreeStatus, paths: &[String], staged: bool) -> Result<()> {
        let selected = paths
            .iter()
            .filter_map(|path| {
                expected
                    .get(path)
                    .map(|state| (path.clone(), state.clone()))
            })
            .collect();
        let result = self.call(
            crate::remote::Request::Stage(selected, paths.to_vec(), staged),
            |state| stage(state.root(), expected, paths, staged),
        );
        self.invalidate();
        result
    }

    pub fn file_changes(&self, path: &str, options: FileDiffOptions) -> Result<FileChanges> {
        self.call(
            crate::remote::Request::FileChanges(path.into(), options),
            |state| file_changes(state, path, options),
        )
    }

    /// Fetch patches for multiple files using shared repository state and grouped diffs locally, or one remote round trip.
    pub fn file_changes_batch(
        &self,
        requests: &[(String, FileDiffOptions)],
    ) -> Result<Vec<FileChanges>> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        match &self.ssh {
            Some(ssh) if ssh.supports_batch() => {
                ssh.call(crate::remote::Request::FileChangesBatch(requests.to_vec()))
            }
            Some(ssh) => {
                // Fall back to per-file requests when an older helper lacks batch support.
                let mut changes = Vec::with_capacity(requests.len());
                for (path, options) in requests {
                    changes.push(
                        ssh.call(crate::remote::Request::FileChanges(path.clone(), *options))?,
                    );
                }
                Ok(changes)
            }
            None => match &self.session {
                Some(session) => session.call(|state| file_changes_batch(state, requests)),
                None => {
                    let state = RepoState::oneshot(&self.root);
                    file_changes_batch(&state, requests)
                }
            },
        }
    }

    pub(crate) fn file_versions(&self, paths: &[String]) -> Vec<Option<FileVersion>> {
        if paths.is_empty() {
            return Vec::new();
        }
        let read = |state: &RepoState| {
            Ok(paths
                .iter()
                .map(|path| file_version(state, path).ok())
                .collect::<Vec<_>>())
        };
        if let Some(ssh) = &self.ssh {
            if ssh.supports_repository_watch() {
                return ssh
                    .call(crate::remote::Request::FileVersions(paths.to_vec()))
                    .unwrap_or_else(|_| vec![None; paths.len()]);
            }
            return paths
                .iter()
                .map(|path| self.file_version(path).ok())
                .collect();
        }
        self.session
            .as_ref()
            .and_then(|session| session.call(read).ok())
            .unwrap_or_else(|| vec![None; paths.len()])
    }

    pub fn file_version(&self, path: &str) -> Result<FileVersion> {
        self.call(crate::remote::Request::FileVersion(path.into()), |state| {
            file_version(state, path)
        })
    }

    pub fn layer_snapshot(&self, path: &str, layer: Layer, side: Side) -> Result<Snapshot> {
        self.call(
            crate::remote::Request::LayerSnapshot(path.into(), layer, side),
            |state| layer_snapshot(state, path, layer, side),
        )
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn index_text(&self, path: &str, version: &FileVersion) -> Result<Option<String>> {
        let base = match self.layer_snapshot(path, Layer::Unstaged, Side::Old)? {
            Snapshot::Text {
                text,
                truncated: false,
            } => Some(text.trim_start_matches('\u{feff}').replace("\r\n", "\n")),
            Snapshot::Missing => None,
            _ => bail!(t!("git.cannot_edit")),
        };
        anyhow::ensure!(
            self.file_version(path)? == *version,
            t!("git.worktree_changed")
        );
        Ok(base)
    }

    pub fn edit_file(&self, edit: FileEdit) -> Result<Option<Operation>> {
        let result = if edit.action != FileAction::Restore {
            self.call(crate::remote::Request::EditFile(edit.clone()), |state| {
                edit_file(state, &edit)
            })
            .map(|()| None)
        } else if self.supports_git_history() {
            self.operation(
                crate::remote::Request::EditWithHistory(edit.clone()),
                |root| edit_operation(root, &edit),
            )
        } else {
            self.operation(
                crate::remote::Request::EditOperation(edit.clone()),
                |root| edit_operation(root, &edit).map(drop),
            )
            .map(|()| None)
        };
        self.invalidate();
        result
    }

    /// Invalidate the cache after this process modifies the index or HEAD.
    fn invalidate(&self) {
        if let Some(session) = &self.session {
            session.invalidate();
        }
    }

    fn call<T: serde::de::DeserializeOwned>(
        &self,
        request: crate::remote::Request,
        local: impl FnOnce(&RepoState) -> Result<T>,
    ) -> Result<T> {
        use crate::remote::Request;
        let needs_comparison = matches!(
            request,
            Request::SearchRevisions(_)
                | Request::Changes(Comparison::Versions { .. })
                | Request::ReviewFiles(Comparison::Versions { .. })
                | Request::ReviewStats(Comparison::Versions { .. }, _)
                | Request::Patch(Comparison::Versions { .. }, ..)
                | Request::Snapshot(Comparison::Versions { .. }, ..)
        );
        if let Some(ssh) = &self.ssh {
            if needs_comparison {
                self.check_revision_comparison()?;
            }
            return ssh.call(request);
        }
        let needs_blame = matches!(request, Request::BlameBaseline(..) | Request::Blame(..));
        if let Some(helper) = helper::session(&self.root)
            && (!needs_blame || helper.supports_blame())
            && (!needs_comparison || helper.supports_revision_comparison())
        {
            match helper.call(request) {
                Ok(value) => return Ok(value),
                // Fall back to in-process Git if the helper exits or is killed.
                Err(_) if helper.failure().is_some() => helper::retire(&self.root),
                Err(error) => return Err(error),
            }
        }
        match &self.session {
            Some(session) => session.call(local),
            None => local(&RepoState::oneshot(&self.root)),
        }
    }
}

#[cfg(all(test, feature = "desktop"))]
mod tests {
    use std::{fs, path::Path};

    use super::*;

    /// Give each test fresh state to avoid reading stale cached repository facts.
    fn state(root: &Path) -> RepoState {
        RepoState::oneshot(root)
    }

    fn changes(root: &Path, comparison: Comparison) -> Result<Changes> {
        super::changes(&state(root), comparison)
    }

    fn snapshot(root: &Path, detail: &Changes, file: &ChangedFile, side: Side) -> Result<Snapshot> {
        super::snapshot(&state(root), detail, file, side)
    }

    fn git(root: &Path, args: &[&str]) -> Result<()> {
        read(root, args).map(|_| ())
    }

    fn fixture(format: &str) -> Result<tempfile::TempDir> {
        let repo = tempfile::tempdir()?;
        git(
            repo.path(),
            &[
                "init",
                "-q",
                "-b",
                "main",
                &format!("--object-format={format}"),
            ],
        )?;
        git(repo.path(), &["config", "user.name", "Test"])?;
        git(
            repo.path(),
            &["config", "user.email", "test@example.invalid"],
        )?;
        git(repo.path(), &["config", "commit.gpgSign", "false"])?;
        Ok(repo)
    }

    fn changed_files() -> Result<(tempfile::TempDir, std::path::PathBuf)> {
        let repo = fixture("sha1")?;
        let root = repo.path().canonicalize()?;
        for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            fs::write(
                root.join(name),
                "one
two
three
four
",
            )?;
        }
        git(&root, &["add", "."])?;
        git(&root, &["commit", "-qm", "initial"])?;
        for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            fs::write(
                root.join(name),
                "one
TWO
three
four
five
",
            )?;
        }
        Ok((repo, root))
    }

    #[test]
    fn finds_and_compares_arbitrary_versions() -> Result<()> {
        let repo = fixture("sha1")?;
        let root = repo.path();
        fs::write(root.join("code.txt"), "common\n")?;
        fs::write(root.join("recreated.txt"), "same content\n")?;
        git(root, &["add", "."])?;
        git(root, &["commit", "-qm", "common"])?;
        git(root, &["switch", "-qc", "topic"])?;
        fs::write(root.join("code.txt"), "topic version\n")?;
        git(
            root,
            &["commit", "-qam", "Outside main", "-m", "Unique search text"],
        )?;
        let topic = head(root)?.unwrap();
        git(root, &["tag", "-am", "Release notes", "release-topic"])?;
        git(root, &["switch", "-q", "main"])?;
        fs::write(root.join("code.txt"), "main version\n")?;
        git(root, &["commit", "-qam", "main version"])?;
        let main = head(root)?.unwrap();
        for query in [
            &topic,
            short(&topic),
            "topic",
            "refs/tags/release-topic",
            "unique SEARCH",
            "Outside main",
        ] {
            let found = search_revisions(root, query)?;
            assert_eq!(found[0].oid, topic, "query: {query}");
        }
        assert!(search_revisions(root, "--no-walk")?.is_empty());
        for (base, target, expected) in [
            (&topic, &main, "main version\n"),
            (&main, &topic, "topic version\n"),
        ] {
            let detail = changes(
                root,
                Comparison::Versions {
                    base: Some(base.clone()),
                    target: Some(target.clone()),
                },
            )?;
            assert!(detail.status.is_none());
            assert_eq!(detail.files.len(), 1);
            let file = &detail.files[0];
            let Snapshot::Text { text, .. } = snapshot(root, &detail, file, Side::New)? else {
                panic!("expected a target snapshot");
            };
            assert_eq!(text, expected);
        }

        fs::write(root.join("code.txt"), "staged version\n")?;
        git(root, &["add", "code.txt"])?;
        fs::write(root.join("code.txt"), "working version\n")?;
        fs::write(root.join("untracked.txt"), "untracked\n")?;
        git(root, &["rm", "--cached", "recreated.txt"])?;
        let detail = changes(
            root,
            Comparison::Versions {
                base: Some(topic),
                target: None,
            },
        )?;
        assert_eq!(detail.files.len(), 2);
        for (file, expected) in detail
            .files
            .iter()
            .zip(["working version\n", "untracked\n"])
        {
            let Snapshot::Text { text, .. } = snapshot(root, &detail, file, Side::New)? else {
                panic!("expected a disk snapshot");
            };
            assert_eq!(text, expected);
            let diff = super::raw_patch(&state(root), &detail, file, 3, false)?;
            assert!(diff.text.contains(&format!("+{expected}")));
        }
        Ok(())
    }

    #[test]
    fn split_review_metadata_matches_combined_commit_changes() -> Result<()> {
        let (_repo, root) = changed_files()?;
        git(&root, &["mv", "d.txt", "renamed.txt"])?;
        fs::write(root.join("renamed.txt"), "one\ntwo\nthree\nfour\n")?;
        fs::write(root.join("binary"), [0, 1, 2])?;
        git(&root, &["add", "-A"])?;
        git(&root, &["commit", "-qm", "review metadata"])?;
        let commit = commit(&root, &head(&root)?.unwrap())?;
        let comparison = Comparison::Commit {
            parent: commit.parents.first().cloned(),
            commit: Box::new(commit),
        };
        let state = state(&root);
        let combined = super::changes(&state, comparison.clone())?;
        let split = super::review_files(&state, comparison.clone())?;
        let stats = super::review_stats(&state, comparison, &split.base)?;
        assert_eq!(split.files.len(), combined.files.len());
        for (light, full) in split.files.iter().zip(&combined.files) {
            assert_eq!(light.path, full.path);
            assert_eq!(light.old_path, full.old_path);
            assert_eq!(light.code, full.code);
            assert!(light.counts.is_none());
            assert_eq!(stats[&full.path].counts, full.counts);
            assert_eq!(stats[&full.path].binary, full.binary);
        }
        assert_eq!(
            split
                .files
                .iter()
                .find(|file| file.code == 'R')
                .unwrap()
                .path,
            "renamed.txt"
        );
        assert!(stats["binary"].binary);
        Ok(())
    }

    const ALL: FileDiffOptions = FileDiffOptions {
        mode: Mode::All,
        context: 3,
        whitespace: false,
    };

    const ZERO: FileDiffOptions = FileDiffOptions {
        mode: Mode::All,
        context: 0,
        whitespace: false,
    };

    /// Limit Git process count for a viewport by sharing repository facts and a blob reader within each operation.
    ///
    /// Previously, each patch required 30–40 processes. Each `file_version` read ls-files, HEAD, and status,
    /// resolved the diff profile twice, and validated again at the end. Each blob needed two
    /// `cat-file` calls. This limit guards against regressions.
    #[test]
    fn shared_state_keeps_git_invocations_low() -> Result<()> {
        let (_repo, root) = changed_files()?;
        let comparison = Comparison::Worktree {
            mode: Mode::All,
            base: None,
        };
        let paths = ["a.txt", "b.txt", "c.txt", "d.txt"];

        // Shared state, grouped diffs, and a persistent blob reader keep process count low across the viewport.
        let session = GitSession::new(&root);
        reset_spawn_count();
        session.call(|state| {
            super::changes(state, comparison.clone())?;
            let requests = paths
                .iter()
                .flat_map(|path| [((*path).to_owned(), ALL), ((*path).to_owned(), ZERO)])
                .collect::<Vec<_>>();
            super::file_changes_batch(state, &requests)?;
            for path in paths {
                for side in [Side::Old, Side::New] {
                    super::snapshot(
                        state,
                        &Changes {
                            comparison: comparison.clone(),
                            base: String::new(),
                            files: Vec::new(),
                            status: None,
                        },
                        &ChangedFile::new(path.into(), 'M'),
                        side,
                    )?;
                }
            }
            Ok(())
        })?;
        let shared = spawn_count();

        assert!(
            shared <= 8,
            "Full patches for four files, including snapshots of both sides, used {shared} Git processes"
        );
        Ok(())
    }

    /// Persistent `cat-file --batch` must return the same content as one-shot reads.
    #[test]
    fn blob_reader_matches_one_shot_reads() -> Result<()> {
        let (_repo, root) = changed_files()?;
        let session = GitSession::new(&root);
        session.call(|state| {
            for side in [Side::Old, Side::New] {
                let blob = state.blob(&format!("HEAD:a.txt"), 4 * 1024 * 1024)?;
                let one_shot = super::read_blob(&root, "HEAD:a.txt", 4 * 1024 * 1024)?;
                let _ = side;
                assert_eq!(blob, one_shot);
            }
            // Missing objects and directories (non-blobs) both return None.
            assert!(state.blob("HEAD:missing.txt", 1024)?.is_none());
            assert!(state.blob("HEAD:", 1024)?.is_none());
            // Truncate oversized content at the limit while keeping the stream synchronized.
            let (bytes, truncated) = state.blob("HEAD:a.txt", 4)?.unwrap();
            assert!(truncated);
            assert_eq!(
                bytes,
                b"one
"
            );
            assert_eq!(
                state.blob("HEAD:b.txt", 1024)?.unwrap().0,
                b"one
two
three
four
"
            );
            Ok(())
        })?;
        Ok(())
    }

    /// Drop the cached index snapshot after staging so the panel reads fresh status.
    #[test]
    fn stage_invalidates_cached_index() -> Result<()> {
        let (_repo, root) = changed_files()?;
        let session = GitSession::new(&root);
        let before = session.call(|state| file_version(state, "a.txt"))?;
        assert!(before.status.as_ref().is_some_and(|status| status.unstaged));
        show_worktree_status(&session)?;
        let status = session.call(|state| super::worktree_status(state.root()))?;
        super::stage(&root, &status, &["a.txt".into()], true)?;
        session.invalidate();
        let after = session.call(|state| file_version(state, "a.txt"))?;
        assert!(after.status.as_ref().is_some_and(|status| status.staged));
        assert_ne!(before, after);
        Ok(())
    }

    fn show_worktree_status(session: &GitSession) -> Result<()> {
        // Populate the cache first to confirm that invalidation works.
        session.call(|state| {
            state.status()?;
            state.index()?;
            Ok(())
        })
    }

    #[test]
    fn history_search_and_root_comparison_in_sha256_repository() -> Result<()> {
        let repo = fixture("sha256")?;
        let root = repo.path();
        let path = if cfg!(windows) {
            "含 空格.rs"
        } else {
            "含\t空格.rs"
        };
        fs::write(root.join(path), "fn initial() {}\n")?;
        git(root, &["add", "."])?;
        git(root, &["commit", "-qm", "初始提交", "-m", "find in body"])?;
        let history = history(root)?;
        let head = history.head.as_deref().unwrap();
        assert_eq!(head.len(), 64);
        let page = commits(root, head, "FIND IN BODY", 0)?;
        assert_eq!(page.commits.len(), 1);
        assert_eq!(commits(root, head, short(head), 0)?.commits.len(), 1);
        assert!(commits(root, head, short(head), 1)?.commits.is_empty());
        let detail = changes(
            root,
            Comparison::Commit {
                commit: Box::new(page.commits[0].clone()),
                parent: None,
            },
        )?;
        let file = &detail.files[0];
        assert_eq!(file.path, path);
        assert_eq!(file.counts, Some((1, 0)));
        let patch = patch(root, &detail, file, 3, false)?;
        assert_eq!(patch.document.path().as_str(), file.path);
        assert_eq!(patch.document.additions(), 1);
        assert!(matches!(
            snapshot(root, &detail, file, Side::Old)?,
            Snapshot::Missing
        ));
        Ok(())
    }

    #[test]
    fn worktree_modes_and_index_operations_preserve_disk_contents() -> Result<()> {
        let repo = fixture("sha1")?;
        let root = &repo.path().canonicalize()?;
        let path = if cfg!(windows) {
            "- literal.txt"
        } else {
            ":(glob) literal.txt"
        };
        fs::write(root.join(path), "initial\n")?;
        let initial = changes(
            root,
            Comparison::Worktree {
                mode: Mode::All,
                base: None,
            },
        )?;
        let file = &initial.files[0];
        assert_eq!(file.code, '?');
        assert_eq!(patch(root, &initial, file, 3, false)?.counts, Some((1, 0)));
        stage(root, initial.status.as_ref().unwrap(), &[path.into()], true)?;
        assert!(
            stage(
                root,
                initial.status.as_ref().unwrap(),
                &[path.into()],
                false
            )
            .is_err()
        );
        let index = changes(
            root,
            Comparison::Worktree {
                mode: Mode::Staged,
                base: None,
            },
        )?;
        assert_eq!(index.files[0].code, 'A');
        fs::write(root.join(path), "modified\n")?;
        let disk = changes(
            root,
            Comparison::Worktree {
                mode: Mode::Unstaged,
                base: None,
            },
        )?;
        assert_eq!(disk.files[0].counts, Some((1, 1)));
        assert!(
            matches!(snapshot(root, &disk, &disk.files[0], Side::Old)?, Snapshot::Text { text, .. } if text == "initial\n")
        );
        stage(root, disk.status.as_ref().unwrap(), &[path.into()], false)?;
        assert_eq!(fs::read_to_string(root.join(path))?, "modified\n");
        assert_eq!(worktree_status(root)?[path].code, '?');
        Ok(())
    }

    #[test]
    fn merge_parents_and_renames_have_independent_file_lists() -> Result<()> {
        let repo = fixture("sha1")?;
        let root = repo.path();
        fs::write(root.join("old"), "line one\nline two\nline three\n")?;
        git(root, &["add", "."])?;
        git(root, &["commit", "-qm", "root"])?;
        git(root, &["switch", "-qc", "topic"])?;
        git(root, &["mv", "old", "新名字"])?;
        git(root, &["commit", "-qm", "rename on topic"])?;
        git(root, &["switch", "-q", "main"])?;
        fs::write(root.join("other"), [0u8, 1, 0, 2])?;
        git(root, &["add", "."])?;
        git(root, &["commit", "-qm", "add binary"])?;
        git(root, &["merge", "-q", "--no-ff", "topic", "-m", "merge"])?;
        git(root, &["tag", "-a", "release", "-m", "version"])?;
        let history = history(root)?;
        let merge = commit(root, history.head.as_deref().unwrap())?;
        assert!(
            history.refs[&merge.oid]
                .iter()
                .any(|reference| reference.tag)
        );
        let compare = |ix: usize| {
            changes(
                root,
                Comparison::Commit {
                    commit: Box::new(merge.clone()),
                    parent: Some(merge.parents[ix].clone()),
                },
            )
        };
        let first = compare(0)?;
        let file = &first.files[0];
        assert_eq!(
            (file.code, file.old_path.as_deref(), file.path.as_str()),
            ('R', Some("old"), "新名字")
        );
        assert_eq!(
            patch(root, &first, file, 3, false)?
                .document
                .path()
                .as_str(),
            "新名字"
        );
        git(root, &["mv", "新名字", "renamed again"])?;
        fs::write(
            root.join("renamed again"),
            "line one\nline two\nline three\nplus\n",
        )?;
        let status = worktree_status(root)?;
        stage(root, &status, &["renamed again".into()], true)?;
        let status = worktree_status(root)?;
        stage(root, &status, &["renamed again".into()], false)?;
        assert_eq!(
            fs::read_to_string(root.join("renamed again"))?,
            "line one\nline two\nline three\nplus\n"
        );
        assert!(worktree_status(root)?["renamed again"].untracked);
        let second = compare(1)?;
        assert!(second.files[0].binary);
        assert!(matches!(
            snapshot(root, &second, &second.files[0], Side::New)?,
            Snapshot::Binary
        ));
        Ok(())
    }
}
