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

#[cfg(feature = "desktop")]
#[derive(Clone)]
pub(crate) struct CommitRange {
    pub base: Option<String>,
    pub target: String,
    pub oldest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Branch {
    pub reference: String,
    pub label: String,
    pub oid: String,
    pub remote: bool,
    #[serde(default)]
    pub tracking: Option<super::Tracking>,
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

#[cfg(feature = "desktop")]
impl History {
    pub(crate) fn has_browsable_ref(&self, reference: &str) -> bool {
        if reference == "HEAD" {
            return self.checkout.is_none();
        }
        reference == self.checkout.as_deref().unwrap_or("HEAD")
            || self.browsable_ref_oid(reference).is_some()
    }

    pub(crate) fn browsable_ref_oid(&self, reference: &str) -> Option<&str> {
        if reference == "HEAD" {
            return self.head.as_deref();
        }
        if let Some(tag) = reference.strip_prefix("refs/tags/") {
            return self
                .refs
                .iter()
                .find(|(_, refs)| {
                    refs.iter()
                        .any(|reference| reference.tag && reference.label == tag)
                })
                .map(|(oid, _)| oid.as_str());
        }
        self.branches
            .iter()
            .find(|branch| branch.reference == reference)
            .map(|branch| branch.oid.as_str())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct CommitPage {
    pub commits: Vec<Commit>,
    pub more: bool,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SourceHistoryTarget {
    pub head: String,
    pub path: String,
    pub ranges: Vec<(usize, usize)>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct SourceHistoryCommit {
    pub commit: Commit,
    pub path: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct SourceHistoryPage {
    pub commits: Vec<SourceHistoryCommit>,
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
            "--format=%(refname)%00%(objectname)%00%(*objectname)%00%(objecttype)%00%(*objecttype)%00%(symref)%00%(upstream)%00%(upstream:remotename)%00%(upstream:remoteref)%00%(upstream:track,nobracket)",
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
        let [
            reference,
            oid,
            peeled,
            kind,
            peeled_kind,
            symbolic,
            upstream,
            remote,
            target,
            status,
        ] = fields.as_slice()
        else {
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
                tracking: super::Tracking::parse(upstream, remote, target, status),
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
        commits.push(log_commit(fields));
    }
    Ok(commits)
}

fn log_commit(fields: &[&[u8]; 11]) -> Commit {
    Commit {
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
    }
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

pub(crate) fn source_history(
    root: &Path,
    target: &SourceHistoryTarget,
    offset: usize,
) -> Result<SourceHistoryPage> {
    super::worktree_edit::validate_path(&target.path)?;
    ensure!(object_id(&target.head), t!("git.invalid_commit_id"));
    ensure!(
        target
            .ranges
            .iter()
            .all(|&(first, last)| first > 0 && first <= last),
        t!("git.invalid_source_history_range")
    );
    let file_history = target.ranges.is_empty();
    // Skipping commits in Git's --follow walk can skip the rename itself.
    let skip_records = if file_history { offset } else { 0 };
    let limit = skip_records.saturating_add(PAGE_SIZE + 1);
    let mut args = vec![
        "log".to_owned(),
        "--no-notes".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        "--no-textconv".into(),
        "--encoding=UTF-8".into(),
        "-z".into(),
        format!("--format=tformat:%x00{LOG_FORMAT}"),
        format!("-n{limit}"),
        target.head.clone(),
    ];
    if file_history {
        args.extend([
            "--follow".into(),
            "--name-status".into(),
            "--".into(),
            target.path.clone(),
        ]);
    } else {
        args.extend([
            format!("--skip={offset}"),
            "--no-follow".into(),
            "--src-prefix=a/".into(),
            "--dst-prefix=b/".into(),
        ]);
        for (first, last) in &target.ranges {
            args.extend(["-L".into(), format!("{first},{last}:{}", target.path)]);
        }
    }

    let output = read(root, &args)?;
    let mut rest = output.as_slice();
    let mut commits = Vec::new();
    let mut skipped = 0;
    while !rest.is_empty() {
        let record = rest.strip_prefix(b"\0").context(t!("git.invalid_log"))?;
        let mut tokens = record.splitn(12, |&byte| byte == 0);
        let (fields, remaining) = read_history_fields(&mut tokens)?;
        let commit = log_commit(&fields);
        ensure!(object_id(&commit.oid), t!("git.invalid_log"));
        let (path, remaining) = if file_history {
            file_history_path(remaining, &target.path)?
        } else {
            let end = remaining
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(remaining.len());
            let (patch, remaining) = remaining.split_at(end);
            let path = patch.split(|&byte| byte == b'\n').find_map(|line| {
                line.strip_prefix(b"+++ ")
                    .and_then(|path| super::worktree_edit::header_path(path, "b/"))
            });
            (path.unwrap_or_else(|| target.path.clone()), remaining)
        };
        if skipped < skip_records {
            skipped += 1;
        } else {
            commits.push(SourceHistoryCommit { commit, path });
        }
        rest = remaining;
    }
    let more = commits.len() > PAGE_SIZE;
    commits.truncate(PAGE_SIZE);
    Ok(SourceHistoryPage { commits, more })
}

fn read_history_fields<'a>(
    tokens: &mut impl Iterator<Item = &'a [u8]>,
) -> Result<([&'a [u8]; 11], &'a [u8])> {
    let mut fields = [&[][..]; 11];
    for field in &mut fields {
        *field = tokens.next().context(t!("git.invalid_log"))?;
    }
    Ok((fields, tokens.next().unwrap_or_default()))
}

fn file_history_path<'a>(mut rest: &'a [u8], default: &str) -> Result<(String, &'a [u8])> {
    let mut path = None;
    while !rest.is_empty() && rest[0] != 0 {
        let mut fields = rest.splitn(3, |&byte| byte == 0);
        let status = fields.next().unwrap_or_default().trim_ascii_start();
        let name = fields.next().context(t!("git.invalid_log"))?;
        rest = fields.next().context(t!("git.invalid_log"))?;
        let name = if matches!(status.first(), Some(b'R' | b'C')) {
            let end = rest
                .iter()
                .position(|&byte| byte == 0)
                .context(t!("git.invalid_log"))?;
            let name = &rest[..end];
            rest = &rest[end + 1..];
            name
        } else {
            name
        };
        if path.is_none() || name == default.as_bytes() {
            path = Some(text(name));
        }
    }
    Ok((path.unwrap_or_else(|| default.to_owned()), rest))
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
