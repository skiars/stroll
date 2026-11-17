use rust_i18n::t;

use std::{collections::HashMap, path::Path};

use anyhow::{Context, Result, bail, ensure};

use super::{capture, command, object_id, read, text};

const LOG_FORMAT: &str = "%H%x00%h%x00%P%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%s%x00%b";
pub(crate) const PAGE_SIZE: usize = 60;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Commit {
    pub oid: String,
    pub short: String,
    pub parents: Vec<String>,
    pub author: String,
    pub author_email: String,
    pub authored_at: String,
    pub committer: String,
    pub committer_email: String,
    pub committed_at: String,
    pub subject: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Branch {
    pub reference: String,
    pub label: String,
    pub oid: String,
    pub remote: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Reference {
    pub label: String,
    pub tag: bool,
    pub head: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct History {
    pub checkout: Option<String>,
    pub head: Option<String>,
    pub branches: Vec<Branch>,
    pub refs: std::sync::Arc<HashMap<String, Vec<Reference>>>,
    pub changes: super::WorktreeStatus,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct CommitPage {
    pub commits: Vec<Commit>,
    pub more: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct RevisionMatch {
    pub oid: String,
    pub short: String,
    pub subject: String,
}

pub(crate) fn head(root: &Path) -> Result<Option<String>> {
    revision(root, "HEAD^{commit}")
}

fn revision(root: &Path, value: &str) -> Result<Option<String>> {
    let output = command(
        root,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            value,
        ],
    )
    .output()?;
    match output.status.code() {
        Some(0) => {
            let oid = text(&output.stdout).trim_end().to_owned();
            ensure!(object_id(&oid), t!("git.invalid_object_id"));
            Ok(Some(oid))
        }
        Some(1) => Ok(None),
        _ => bail!("{}", text(&output.stderr).trim_end()),
    }
}

pub(crate) fn history(root: &Path) -> Result<History> {
    let checkout = command(root, ["symbolic-ref", "--quiet", "HEAD"]).output()?;
    let checkout = match checkout.status.code() {
        Some(0) => Some(text(&checkout.stdout).trim_end().to_owned()),
        Some(1) => None,
        _ => bail!("{}", text(&checkout.stderr).trim_end()),
    };
    let head = head(root)?;
    let output = read(
        root,
        [
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(*objectname)%00%(objecttype)%00%(*objecttype)%00%(symref)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
    )?;
    let mut branches = Vec::new();
    let mut refs = HashMap::<String, Vec<Reference>>::new();
    if checkout.is_none()
        && let Some(head) = &head
    {
        refs.entry(head.clone()).or_default().push(Reference {
            label: "HEAD".into(),
            tag: false,
            head: true,
        });
    }
    for line in output
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let fields = line.split(|&byte| byte == 0).map(text).collect::<Vec<_>>();
        let [reference, oid, peeled, kind, peeled_kind, symbolic] = fields.as_slice() else {
            continue;
        };
        if !symbolic.is_empty() {
            continue;
        }
        let (namespace, label) = reference
            .strip_prefix("refs/")
            .unwrap_or_default()
            .split_once('/')
            .unwrap_or_default();
        let tag = namespace == "tags";
        let oid = if peeled_kind == "commit" {
            peeled
        } else if kind == "commit" {
            oid
        } else {
            continue;
        };
        let is_head = checkout.as_ref() == Some(reference);
        let decoration = Reference {
            label: if is_head {
                format!("HEAD → {label}")
            } else {
                label.to_owned()
            },
            tag,
            head: is_head,
        };
        let decorations = refs.entry(oid.clone()).or_default();
        if is_head {
            decorations.insert(0, decoration);
        } else {
            decorations.push(decoration);
        }
        if !tag {
            branches.push(Branch {
                reference: reference.clone(),
                label: label.to_owned(),
                oid: oid.clone(),
                remote: namespace == "remotes",
            });
        }
    }
    Ok(History {
        checkout,
        head,
        branches,
        refs: refs.into(),
        changes: super::worktree_status(root)?,
    })
}

fn parse_log(bytes: &[u8]) -> Result<Vec<Commit>> {
    let Some(bytes) = bytes.strip_suffix(&[0]) else {
        ensure!(bytes.is_empty(), t!("git.invalid_log"));
        return Ok(Vec::new());
    };
    let fields = bytes.split(|&byte| byte == 0).collect::<Vec<_>>();
    let mut commits = Vec::with_capacity(fields.len() / 11);
    let (chunks, remainder) = fields.as_chunks::<11>();
    ensure!(remainder.is_empty(), t!("git.invalid_log"));
    for fields in chunks {
        commits.push(Commit {
            oid: text(fields[0]),
            short: text(fields[1]),
            parents: text(fields[2])
                .split_ascii_whitespace()
                .map(str::to_owned)
                .collect(),
            author: text(fields[3]),
            author_email: text(fields[4]),
            authored_at: text(fields[5]),
            committer: text(fields[6]),
            committer_email: text(fields[7]),
            committed_at: text(fields[8]),
            subject: text(fields[9]),
            body: text(fields[10]).trim_end().to_owned(),
        });
    }
    Ok(commits)
}

pub(crate) fn commit(root: &Path, oid: &str) -> Result<Commit> {
    ensure!(object_id(oid), t!("git.invalid_commit_id"));
    let output = read(
        root,
        [
            "log",
            "--no-notes",
            "--encoding=UTF-8",
            "-z",
            "-n1",
            &format!("--format={LOG_FORMAT}"),
            oid,
            "--",
        ],
    )?;
    parse_log(&output)?
        .into_iter()
        .next()
        .context(t!("git.commit_missing"))
}

pub(crate) fn commits(root: &Path, oid: &str, query: &str, offset: usize) -> Result<CommitPage> {
    ensure!(object_id(oid), t!("git.invalid_commit_id"));
    let id_match = if (4..=64).contains(&query.len())
        && query.bytes().all(|byte| byte.is_ascii_hexdigit())
        && let Some(candidate) = revision(root, &format!("{query}^{{commit}}"))?
        && command(root, ["merge-base", "--is-ancestor", &candidate, oid])
            .status()?
            .success()
    {
        Some(commit(root, &candidate)?)
    } else {
        None
    };

    let extra = id_match.filter(|commit| {
        let query = query.to_ascii_lowercase();
        !commit.subject.to_ascii_lowercase().contains(&query)
            && !commit.body.to_ascii_lowercase().contains(&query)
    });
    let limit = PAGE_SIZE + 1;
    let skip = offset.saturating_sub(usize::from(extra.is_some()));
    let mut args = vec![
        "log".to_owned(),
        "--date-order".into(),
        "--no-notes".into(),
        "--encoding=UTF-8".into(),
        "-z".into(),
        format!("--format={LOG_FORMAT}"),
        format!("--skip={skip}"),
        format!("-n{limit}"),
    ];
    if !query.is_empty() {
        args.extend([
            "--regexp-ignore-case".into(),
            "--fixed-strings".into(),
            format!("--grep={query}"),
        ]);
    }
    args.extend([oid.to_owned(), "--".into()]);
    let mut commits = parse_log(&read(root, &args)?)?;
    if offset == 0
        && let Some(found) = extra
    {
        commits.insert(0, found);
    }
    let more = commits.len() > PAGE_SIZE;
    commits.truncate(PAGE_SIZE);
    Ok(CommitPage { commits, more })
}

pub(crate) fn search_revisions(root: &Path, query: &str) -> Result<Vec<RevisionMatch>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    ensure!(!query.contains('\0'), t!("git.invalid_commit_id"));
    if let Some(oid) = revision(root, &format!("{query}^{{commit}}"))? {
        let commit = commit(root, &oid)?;
        return Ok(vec![RevisionMatch {
            oid,
            short: commit.short,
            subject: commit.subject,
        }]);
    }
    let output = read(
        root,
        [
            "log",
            "--all",
            "--date-order",
            "--no-notes",
            "--encoding=UTF-8",
            "--format=%H%x00%h%x00%s",
            "-z",
            "-n30",
            "--regexp-ignore-case",
            "--fixed-strings",
            &format!("--grep={query}"),
            "--",
        ],
    )?;
    let mut matches = Vec::new();
    if !output.is_empty() {
        let bytes = output.strip_suffix(&[0]).context(t!("git.invalid_log"))?;
        let fields = bytes.split(|&byte| byte == 0).collect::<Vec<_>>();
        let (rows, remainder) = fields.as_chunks::<3>();
        ensure!(remainder.is_empty(), t!("git.invalid_log"));
        matches.extend(rows.iter().map(|[oid, short, subject]| RevisionMatch {
            oid: text(oid),
            short: text(short),
            subject: text(subject),
        }));
    }
    Ok(matches)
}

pub(crate) fn empty_tree(root: &Path) -> Result<String> {
    let (bytes, _) = capture(
        command(root, ["hash-object", "-t", "tree", "--stdin"]),
        128,
        false,
    )?;
    let oid = text(&bytes).trim_end().to_owned();
    ensure!(object_id(&oid), t!("git.empty_tree_failed"));
    Ok(oid)
}
