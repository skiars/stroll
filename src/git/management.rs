use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use rust_i18n::t;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Branch, ChangedFile, Comparison, RepoState, changes, command, object_id, read, text};
use crate::repository::{Worktree, local_worktrees};

mod worktree_removal;
pub(crate) use worktree_removal::{
    WorktreeRemovalAction, WorktreeRemovalPreview, preview_worktree_removal,
};
use worktree_removal::{apply_worktree_removal, remove_worktree, worktree_file_status};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Tracking {
    pub reference: String,
    pub remote: String,
    pub target: String,
    pub ahead: usize,
    pub behind: usize,
    pub gone: bool,
}

impl Tracking {
    pub(super) fn parse(reference: &str, remote: &str, target: &str, status: &str) -> Option<Self> {
        if reference.is_empty() {
            return None;
        }
        let mut tracking = Self {
            reference: reference.into(),
            remote: remote.into(),
            target: target.into(),
            ahead: 0,
            behind: 0,
            gone: status == "gone",
        };
        for count in status.split(", ") {
            if let Some(ahead) = count.strip_prefix("ahead ") {
                tracking.ahead = ahead.parse().unwrap_or_default();
            }
            if let Some(behind) = count.strip_prefix("behind ") {
                tracking.behind = behind.parse().unwrap_or_default();
            }
        }
        Some(tracking)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ManagedTree {
    pub tree: Worktree,
    pub current: bool,
    pub clean: Option<bool>,
    pub detached_unreferenced: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ManagedBranch {
    pub branch: Branch,
    pub worktree: Option<PathBuf>,
    pub merged: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct GitRemote {
    pub name: String,
    pub url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Management {
    pub trees: Vec<ManagedTree>,
    pub branches: Vec<ManagedBranch>,
    pub remotes: Vec<GitRemote>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct UpstreamCommit {
    pub oid: String,
    pub subject: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct UpstreamComparison {
    pub outgoing: Vec<UpstreamCommit>,
    pub incoming: Vec<UpstreamCommit>,
}

pub(crate) fn compare_upstream(
    root: &Path,
    source: &str,
    upstream: &str,
) -> Result<UpstreamComparison> {
    let source = resolve_start(root, source)?;
    let upstream = resolve_start(root, upstream)?;
    let range = format!("{source}...{upstream}");
    let commits = |side: &str| -> Result<Vec<UpstreamCommit>> {
        commit_subjects(root, [side, "--max-count=24", &range])
    };
    Ok(UpstreamComparison {
        outgoing: commits("--left-only")?,
        incoming: commits("--right-only")?,
    })
}

fn commit_subjects<'a>(
    root: &Path,
    revisions: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<UpstreamCommit>> {
    let output = read(
        root,
        ["log", "--format=%h%x00%s"]
            .into_iter()
            .chain(revisions)
            .chain(["--"]),
    )?;
    Ok(text(&output)
        .lines()
        .filter_map(|line| {
            let (oid, subject) = line.split_once('\0')?;
            Some(UpstreamCommit {
                oid: oid.into(),
                subject: subject.into(),
            })
        })
        .collect())
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PushOptions {
    pub remotes: Vec<String>,
    pub remote: Option<String>,
    pub branch: String,
    pub set_upstream: bool,
}

pub(crate) fn push_options(root: &Path, source: &str) -> Result<PushOptions> {
    let name = source
        .strip_prefix("refs/heads/")
        .ok_or_else(|| anyhow::anyhow!(t!("git.invalid_branch_name")))?;
    branch_ref(root, name)?;
    let remotes: Vec<String> = text(&read(root, ["remote"])?)
        .lines()
        .map(ToOwned::to_owned)
        .collect();
    let tracking = branch_tracking(root, source)?
        .filter(|item| remotes.contains(&item.remote) && item.target.starts_with("refs/heads/"));
    let remote = [
        format!("branch.{name}.pushRemote"),
        "remote.pushDefault".into(),
    ]
    .iter()
    .find_map(|key| {
        let result = read(root, ["config", "--get", key]).ok()?;
        let name = text(&result).trim_end().to_owned();
        remotes.contains(&name).then_some(name)
    })
    .or_else(|| tracking.as_ref().map(|item| item.remote.clone()))
    .or_else(|| {
        remotes
            .iter()
            .find(|name| *name == "origin" || remotes.len() == 1)
            .cloned()
    });
    let branch = tracking
        .as_ref()
        .filter(|item| Some(&item.remote) == remote.as_ref())
        .and_then(|item| item.target.strip_prefix("refs/heads/"))
        .unwrap_or(name)
        .to_owned();
    Ok(PushOptions {
        remotes,
        remote,
        branch,
        set_upstream: tracking.is_none(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct PushTarget {
    pub source: String,
    pub oid: String,
    pub remote: String,
    pub branch: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct PushLease {
    endpoint: [u8; 32],
    pub target_oid: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PushPreview {
    pub lease: PushLease,
    pub outgoing: Vec<UpstreamCommit>,
    pub files: Vec<ChangedFile>,
    pub overwritten: usize,
    pub new_branch_base: Option<String>,
}

fn push_endpoint(root: &Path, remote: &str) -> Result<String> {
    known_remote(root, remote)?;
    let result = text(&read(
        root,
        ["remote", "get-url", "--push", "--all", "--", remote],
    )?);
    let mut urls = result.lines().filter(|url| !url.is_empty());
    let endpoint = urls
        .next()
        .ok_or_else(|| anyhow::anyhow!(t!("git.remote_required")))?;
    ensure!(
        urls.all(|url| url == endpoint),
        t!("git.push_multiple_urls")
    );
    Ok(endpoint.to_owned())
}

pub(crate) fn push_branches(root: &Path, remote: &str) -> Result<Vec<String>> {
    let endpoint = push_endpoint(root, remote)?;
    let output = read(
        root,
        ["ls-remote", "--refs", "--", &endpoint, "refs/heads/*"],
    )?;
    let mut branches: Vec<_> = text(&output)
        .lines()
        .filter_map(|line| {
            let (oid, reference) = line.split_once('\t')?;
            let branch = reference.strip_prefix("refs/heads/")?;
            object_id(oid).then(|| branch.to_owned())
        })
        .collect();
    branches.sort_unstable();
    Ok(branches)
}

pub(crate) fn preview_push(root: &Path, target: PushTarget) -> Result<PushPreview> {
    let PushTarget {
        source,
        oid,
        remote,
        branch,
    } = target;
    current_checkout(root, &source, &oid)?;
    let reference = branch_ref(root, &branch)?;
    let endpoint = push_endpoint(root, &remote)?;
    let advertised = text(&read(
        root,
        ["ls-remote", "--symref", "--", &endpoint, "HEAD", &reference],
    )?);
    let mut remote_oid = None;
    let mut default_oid = None;
    let mut default_branch = None;
    for (value, name) in advertised.lines().filter_map(|line| line.split_once('\t')) {
        if name == reference && object_id(value) {
            remote_oid = Some(value.to_owned());
        } else if name == "HEAD" {
            if object_id(value) {
                default_oid = Some(value);
            } else {
                default_branch = value.strip_prefix("ref: refs/heads/");
            }
        }
    }

    let baseline = remote_oid.as_deref().or(default_oid);
    if let Some(base) = baseline
        && resolve_start(root, base).is_err()
    {
        // Import only the objects needed for the comparison. Keep tracking refs and FETCH_HEAD intact.
        read(
            root,
            [
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "--no-auto-maintenance",
                "--no-recurse-submodules",
                "--refmap=",
                "--",
                &endpoint,
                base,
            ],
        )?;
    }

    let outgoing = match baseline {
        Some(base) => commit_subjects(root, [format!("{base}..{oid}").as_str()])?,
        None => commit_subjects(root, [oid.as_str()])?,
    };
    let overwritten = if let Some(remote_oid) = &remote_oid {
        let bytes = read(
            root,
            ["rev-list", "--count", &format!("{oid}..{remote_oid}"), "--"],
        )?;
        text(&bytes).trim().parse()?
    } else {
        0
    };
    let diff_base = match (remote_oid.as_ref(), default_oid) {
        (Some(target), _) => Some(target.clone()),
        (None, Some(base)) => {
            let (bytes, _) = super::capture(
                command(root, ["merge-base", &oid, base]),
                super::METADATA_LIMIT,
                true,
            )?;
            let merged_base = text(&bytes).trim().to_owned();
            (!merged_base.is_empty()).then_some(merged_base)
        }
        (None, None) => None,
    };
    let files = changes(
        &RepoState::oneshot(root),
        Comparison::Versions {
            base: diff_base,
            target: Some(oid.clone()),
        },
    )?
    .files;
    current_checkout(root, &source, &oid)?;

    let new_branch_base = if remote_oid.is_none() {
        default_oid.map(|oid| default_branch.unwrap_or(oid).to_owned())
    } else {
        None
    };
    Ok(PushPreview {
        lease: PushLease {
            endpoint: Sha256::digest(endpoint.as_bytes()).into(),
            target_oid: remote_oid,
        },
        outgoing,
        files,
        overwritten,
        new_branch_base,
    })
}

fn push_reviewed(
    root: &Path,
    target: PushTarget,
    lease: PushLease,
    force: bool,
    set_upstream: bool,
) -> Result<()> {
    let PushTarget {
        source,
        oid,
        remote,
        branch,
    } = target;
    let name = source
        .strip_prefix("refs/heads/")
        .ok_or_else(|| anyhow::anyhow!(t!("git.invalid_branch_name")))?;
    current_checkout(root, &source, &oid)?;
    let reference = branch_ref(root, &branch)?;
    let endpoint = push_endpoint(root, &remote)?;
    ensure!(
        <[u8; 32]>::from(Sha256::digest(endpoint.as_bytes())) == lease.endpoint
            && lease.target_oid.as_deref().is_none_or(object_id),
        t!("git.push_preview_changed")
    );
    if !force && let Some(base) = lease.target_oid.as_deref() {
        ensure!(
            command(
                root,
                [
                    "--no-replace-objects",
                    "merge-base",
                    "--is-ancestor",
                    base,
                    &oid
                ]
            )
            .status()?
            .success(),
            t!("git.push_non_fast_forward")
        );
    }
    // An exact lease also protects ordinary pushes from a remote change after review.
    // The fast-forward check above ensures it cannot authorize a rewrite in ordinary mode.
    let guard = format!(
        "--force-with-lease={reference}:{}",
        lease.target_oid.as_deref().unwrap_or("")
    );
    read(
        root,
        [
            "push",
            "--no-mirror",
            "--no-follow-tags",
            "--recurse-submodules=no",
            &guard,
            "--",
            &remote,
            &format!("{oid}:{reference}"),
        ],
    )?;
    if set_upstream {
        // The refspec pins an OID. Git's --set-upstream only sets tracking for a named source ref.
        for (key, value) in [("remote", &remote), ("merge", &reference)] {
            read(
                root,
                [
                    "config",
                    "--local",
                    "--replace-all",
                    &format!("branch.{name}.{key}"),
                    value,
                ],
            )
            .map_err(|_| anyhow::anyhow!(t!("git.push_upstream_failed")))?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum WorktreeRemoval {
    #[default]
    Clean,
    Discard,
    MissingDirectory,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum ManageGit {
    AddTree {
        path: String,
        branch: String,
        start: Option<String>,
    },
    RemoveTree {
        path: PathBuf,
        reference: String,
        #[serde(default)]
        mode: WorktreeRemoval,
    },
    RemoveReviewedTree(Box<WorktreeRemovalPreview>),
    LockTree {
        path: PathBuf,
        locked: bool,
    },
    AddBranch {
        name: String,
        start: String,
    },
    RenameBranch {
        name: String,
        new_name: String,
        oid: String,
    },
    DeleteBranch {
        name: String,
        oid: String,
    },
    Fetch {
        remote: Option<String>,
    },
    Pull {
        source: String,
        oid: String,
        remote: String,
        upstream: String,
    },
    Push {
        source: String,
        oid: String,
        remote: String,
        branch: String,
        set_upstream: bool,
    },
    PushReviewed {
        target: PushTarget,
        lease: PushLease,
        force: bool,
        set_upstream: bool,
    },
    SetUpstream {
        source: String,
        upstream: String,
    },
    AddRemote {
        name: String,
        url: String,
    },
    SetRemoteUrl {
        name: String,
        url: String,
    },
    RemoveRemote {
        name: String,
    },
}

fn clean_worktree(path: &Path) -> Result<bool> {
    Ok(worktree_file_status(path)?.is_empty())
}

fn worktree_commit_can_be_removed(root: &Path, tree: &Worktree) -> Result<bool> {
    if tree.branch.is_some() {
        return Ok(true);
    }
    let checkout_missing = tree.missing || tree.prunable.is_some();
    let Some(oid) = tree.head.as_deref() else {
        return Ok(checkout_missing);
    };
    if checkout_missing
        && !command(
            root,
            [
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{oid}^{{commit}}"),
            ],
        )
        .output()?
        .status
        .success()
    {
        // A missing checkout can still have an administrative record after its commit has expired.
        return Ok(true);
    }
    let references = read(
        root,
        [
            "for-each-ref",
            "--format=%(refname)",
            "--contains",
            oid,
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
    )?;
    Ok(!references.is_empty())
}

fn ancestor(root: &Path, oid: &str, target: &str) -> bool {
    command(root, ["merge-base", "--is-ancestor", oid, target])
        .output()
        .is_ok_and(|output| output.status.success())
}

pub(crate) fn git_management(root: &Path) -> Result<Management> {
    let history = super::history(root)?;
    let trees: Vec<_> = local_worktrees(root)?
        .into_iter()
        .map(|tree| {
            let current = tree.root == root;
            let detached_unreferenced = (tree.available || tree.missing)
                && !worktree_commit_can_be_removed(root, &tree).unwrap_or(false);
            let clean = (tree.available && !tree.main && !current && !tree.locked)
                .then(|| clean_worktree(&tree.root).ok())
                .flatten();
            ManagedTree {
                tree,
                current,
                clean,
                detached_unreferenced,
            }
        })
        .collect();
    let branches = history
        .branches
        .into_iter()
        .filter(|branch| !branch.remote)
        .map(|branch| {
            let worktree = trees
                .iter()
                .find(|entry| entry.tree.branch.as_deref() == Some(&branch.reference))
                .map(|entry| entry.tree.root.clone());
            let target = branch
                .tracking
                .as_ref()
                .filter(|tracking| !tracking.gone)
                .map_or("HEAD", |tracking| tracking.reference.as_str());
            let merged = worktree.is_none() && ancestor(root, &branch.oid, target);
            ManagedBranch {
                branch,
                worktree,
                merged,
            }
        })
        .collect();
    let remotes = text(&read(root, ["remote"])?)
        .lines()
        .filter_map(|name| {
            let key = format!("remote.{name}.url");
            let url = read(root, ["config", "--get", &key]).unwrap_or_default();
            Some(GitRemote {
                name: name.into(),
                url: text(&url).trim_end_matches('\n').into(),
            })
        })
        .collect();
    Ok(Management {
        trees,
        branches,
        remotes,
    })
}

fn branch_ref(root: &Path, name: &str) -> Result<String> {
    let reference = format!("refs/heads/{name}");
    ensure!(
        !name.is_empty() && !name.starts_with('-'),
        t!("git.invalid_branch_name")
    );
    read(root, ["check-ref-format", &reference])
        .map_err(|_| anyhow::anyhow!(t!("git.invalid_branch_name")))?;
    Ok(reference)
}

fn current_checkout(root: &Path, source: &str, oid: &str) -> Result<()> {
    let actual = read(root, ["symbolic-ref", "--quiet", "HEAD"])?;
    ensure!(
        text(&actual).trim_end() == source,
        t!("git.management_changed")
    );
    ensure!(
        super::head(root)?.as_deref() == Some(oid),
        t!("git.management_changed")
    );
    Ok(())
}

fn branch_unchanged(root: &Path, name: &str, oid: &str) -> Result<()> {
    let reference = branch_ref(root, name)?;
    let actual = read(
        root,
        ["rev-parse", "--verify", "--end-of-options", &reference],
    )?;
    ensure!(
        text(&actual).trim_end() == oid,
        t!("git.management_changed")
    );
    ensure!(
        !local_worktrees(root)?
            .iter()
            .any(|tree| tree.branch.as_deref() == Some(&reference)),
        t!("git.branch_checked_out")
    );
    Ok(())
}

fn resolve_start(root: &Path, start: &str) -> Result<String> {
    let start = format!("{start}^{{commit}}");
    Ok(text(&read(
        root,
        ["rev-parse", "--verify", "--end-of-options", &start],
    )?)
    .trim_end()
    .into())
}

fn known_remote(root: &Path, name: &str) -> Result<()> {
    ensure!(name != "." && !name.is_empty(), t!("git.remote_required"));
    ensure!(
        text(&read(root, ["remote"])?)
            .lines()
            .any(|candidate| candidate == name),
        t!("git.management_changed")
    );
    Ok(())
}

pub(crate) fn manage_git(root: &Path, operation: ManageGit) -> Result<()> {
    use ManageGit::*;
    match operation {
        AddTree {
            path,
            branch,
            start,
        } => {
            branch_ref(root, &branch)?;
            let path = Path::new(&path);
            let path = if let Ok(suffix) = path.strip_prefix("~") {
                dirs::home_dir()
                    .ok_or_else(|| anyhow::anyhow!(t!("git.worktree_absolute_path")))?
                    .join(suffix)
            } else {
                path.to_owned()
            };
            ensure!(
                path.is_absolute() && path.to_str().is_some(),
                t!("git.worktree_absolute_path")
            );
            let path = path.to_str().unwrap();
            if let Some(start) = start {
                let oid = resolve_start(root, &start)?;
                read(root, ["worktree", "add", "-b", &branch, "--", path, &oid])?;
            } else {
                read(
                    root,
                    ["rev-parse", "--verify", &format!("refs/heads/{branch}")],
                )?;
                read(root, ["worktree", "add", "--", path, &branch])?;
            }
        }
        RemoveTree {
            path,
            reference,
            mode,
        } => {
            let tree = local_worktrees(root)?
                .into_iter()
                .find(|tree| tree.root == path && tree.reference == reference)
                .ok_or_else(|| anyhow::anyhow!(t!("git.management_changed")))?;
            ensure!(
                !tree.main
                    && tree.root != root
                    && !tree.locked
                    && match mode {
                        WorktreeRemoval::MissingDirectory => tree.missing,
                        WorktreeRemoval::Clean | WorktreeRemoval::Discard => tree.available,
                    },
                t!("git.management_changed")
            );
            ensure!(
                worktree_commit_can_be_removed(root, &tree)?,
                t!("git.worktree_unreferenced_head")
            );
            if mode == WorktreeRemoval::Clean {
                ensure!(clean_worktree(&path)?, t!("git.worktree_contains_files"));
            }
            remove_worktree(root, &path, mode == WorktreeRemoval::Discard)?;
        }
        RemoveReviewedTree(preview) => apply_worktree_removal(root, &preview)?,
        LockTree { path, locked } => {
            let tree = local_worktrees(root)?
                .into_iter()
                .find(|tree| tree.root == path)
                .ok_or_else(|| anyhow::anyhow!(t!("git.management_changed")))?;
            ensure!(
                tree.available && !tree.main && tree.locked != locked,
                t!("git.management_changed")
            );
            read(
                root,
                [
                    std::ffi::OsStr::new("worktree"),
                    if locked { "lock" } else { "unlock" }.as_ref(),
                    "--".as_ref(),
                    path.as_os_str(),
                ],
            )?;
        }
        AddBranch { name, start } => {
            branch_ref(root, &name)?;
            let oid = resolve_start(root, &start)?;
            read(root, ["branch", "--no-track", "--", &name, &oid])?;
        }
        RenameBranch {
            name,
            new_name,
            oid,
        } => {
            branch_unchanged(root, &name, &oid)?;
            branch_ref(root, &new_name)?;
            read(root, ["branch", "--move", "--", &name, &new_name])?;
        }
        DeleteBranch { name, oid } => {
            branch_unchanged(root, &name, &oid)?;
            read(root, ["branch", "--delete", "--", &name])?;
        }
        Fetch {
            remote: Some(remote),
        } => {
            known_remote(root, &remote)?;
            read(root, ["fetch", "--no-recurse-submodules", "--", &remote])?;
        }
        Fetch { remote: None } => {
            read(root, ["fetch", "--all", "--no-recurse-submodules"])?;
        }
        Pull {
            source,
            oid,
            remote,
            upstream,
        } => {
            current_checkout(root, &source, &oid)?;
            let configured = branch_tracking(root, &source)?;
            ensure!(
                configured.is_some_and(|item| item.remote == remote && item.reference == upstream),
                t!("git.management_changed")
            );
            ensure!(
                super::history_edit_progress(root).is_none(),
                t!("git.management_changed")
            );
            manage_git(
                root,
                Fetch {
                    remote: Some(remote),
                },
            )?;
            current_checkout(root, &source, &oid)?;
            let target = resolve_start(root, &upstream)?;
            read(
                root,
                [
                    "merge",
                    "--ff-only",
                    "--no-autostash",
                    "--no-edit",
                    "--",
                    &target,
                ],
            )?;
        }
        Push {
            source,
            oid,
            remote,
            branch,
            set_upstream,
        } => {
            current_checkout(root, &source, &oid)?;
            known_remote(root, &remote)?;
            let target = branch_ref(root, &branch)?;
            let from = if set_upstream {
                source.as_str()
            } else {
                oid.as_str()
            };
            let refspec = format!("{from}:{target}");
            if set_upstream {
                read(
                    root,
                    [
                        "push",
                        "--porcelain",
                        "--set-upstream",
                        "--",
                        &remote,
                        &refspec,
                    ],
                )?;
            } else {
                read(root, ["push", "--porcelain", "--", &remote, &refspec])?;
            }
        }
        PushReviewed {
            target,
            lease,
            force,
            set_upstream,
        } => {
            push_reviewed(root, target, lease, force, set_upstream)?;
        }
        SetUpstream { source, upstream } => {
            let name = source
                .strip_prefix("refs/heads/")
                .ok_or_else(|| anyhow::anyhow!(t!("git.invalid_branch_name")))?;
            branch_ref(root, name)?;
            ensure!(
                upstream.starts_with("refs/remotes/"),
                t!("git.remote_required")
            );
            read(root, ["branch", "--set-upstream-to", &upstream, "--", name])?;
        }
        AddRemote { name, url } => {
            ensure!(
                !name.starts_with('-') && !url.is_empty(),
                t!("git.remote_required")
            );
            read(root, ["remote", "add", "--", &name, &url])?;
        }
        SetRemoteUrl { name, url } => {
            known_remote(root, &name)?;
            ensure!(!url.is_empty(), t!("git.remote_required"));
            read(root, ["remote", "set-url", "--", &name, &url])?;
        }
        RemoveRemote { name } => {
            known_remote(root, &name)?;
            read(root, ["remote", "remove", "--", &name])?;
        }
    }
    Ok(())
}

pub(crate) fn branch_tracking(root: &Path, reference: &str) -> Result<Option<Tracking>> {
    let bytes = read(
        root,
        [
            "for-each-ref",
            "--format=%(refname)%00%(upstream)%00%(upstream:remotename)%00%(upstream:remoteref)%00%(upstream:track,nobracket)",
            "--",
            reference,
        ],
    )?;
    for line in text(&bytes).lines() {
        if let [actual, upstream, remote, target, status] =
            line.split('\0').collect::<Vec<_>>().as_slice()
            && *actual == reference
        {
            return Ok(Tracking::parse(upstream, remote, target, status));
        }
    }
    Ok(None)
}
