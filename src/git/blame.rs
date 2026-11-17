use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::{RepoState, capture, command, worktree_edit};

const BLOB_LIMIT: usize = 4 * 1024 * 1024;
const RANGE_LIMIT: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BlameLine {
    pub oid: String,
    pub path: String,
    pub original: usize,
    pub line: usize,
    pub author: String,
    pub timestamp: i64,
    pub timezone: String,
    pub subject: String,
}

fn validate(path: &str, head: &str) -> Result<()> {
    worktree_edit::validate_path(path)?;
    ensure!(
        matches!(head.len(), 40 | 64) && head.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid blame revision"
    );
    Ok(())
}

pub(crate) fn blame_baseline(state: &RepoState, path: &str, head: &str) -> Result<Option<String>> {
    validate(path, head)?;
    let Some((bytes, false)) = state.blob(&format!("{head}:{path}"), BLOB_LIMIT)? else {
        return Ok(None);
    };
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    Ok(std::str::from_utf8(bytes)
        .ok()
        .filter(|text| !text.is_empty() && !text.contains('\0'))
        .map(|text| text.replace("\r\n", "\n"))
        .filter(|text| !text.contains('\r')))
}

pub(crate) fn blame_lines(
    state: &RepoState,
    path: &str,
    head: &str,
    first: usize,
    last: usize,
) -> Result<Vec<BlameLine>> {
    validate(path, head)?;
    ensure!(
        first > 0 && last >= first && last - first < RANGE_LIMIT,
        "Invalid blame range"
    );
    let lines = format!("{first},{last}");
    let (bytes, truncated) = capture(
        command(
            state.root(),
            [
                "-c",
                "blame.ignoreRevsFile=",
                "blame",
                "--line-porcelain",
                "--encoding=UTF-8",
                "-L",
                &lines,
                head,
                "--",
                path,
            ],
        ),
        BLOB_LIMIT,
        false,
    )?;
    ensure!(!truncated, "Blame output exceeds the size limit");
    Ok(parse(&String::from_utf8_lossy(&bytes)))
}

fn parse(porcelain: &str) -> Vec<BlameLine> {
    let mut result = Vec::new();
    let mut lines = porcelain.split('\n');
    while let Some(header) = lines.next() {
        let mut parts = header.split_whitespace();
        let Some((oid, original, line)) = parts
            .next()
            .zip(parts.next())
            .zip(parts.next())
            .and_then(|((oid, original), line)| {
                Some((oid, original.parse().ok()?, line.parse().ok()?))
            })
        else {
            break;
        };
        let mut entry = BlameLine {
            oid: oid.to_owned(),
            original,
            line,
            path: String::new(),
            author: String::new(),
            timestamp: 0,
            timezone: String::new(),
            subject: String::new(),
        };
        for field in lines.by_ref() {
            if field.starts_with('\t') {
                if !entry.path.is_empty() {
                    result.push(entry);
                }
                break;
            }
            let Some((name, value)) = field.split_once(' ') else {
                continue;
            };
            match name {
                "filename" => entry.path = worktree_edit::unquote(value),
                "author" => entry.author = value.to_owned(),
                "author-time" => entry.timestamp = value.parse().unwrap_or_default(),
                "author-tz" => entry.timezone = value.to_owned(),
                "summary" => entry.subject = value.to_owned(),
                _ => {}
            }
        }
    }
    result
}

#[cfg(feature = "desktop")]
impl super::Backend {
    pub fn supports_blame(&self) -> bool {
        self.ssh.as_ref().is_none_or(|ssh| ssh.supports_blame())
    }

    pub fn blame_baseline(&self, path: &str, head: &str) -> Result<Option<String>> {
        self.call(
            crate::remote::Request::BlameBaseline(path.into(), head.into()),
            |state| blame_baseline(state, path, head),
        )
    }

    pub fn blame_lines(
        &self,
        path: &str,
        head: &str,
        first: usize,
        last: usize,
    ) -> Result<Vec<BlameLine>> {
        self.call(
            crate::remote::Request::Blame(path.into(), head.into(), first, last),
            |state| blame_lines(state, path, head, first, last),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blames_a_pinned_revision_and_reports_the_original_path_and_line() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path();
        let git = |args: &[&str]| super::super::read(root, args.iter().copied());
        git(&["init", "-q"])?;
        git(&["config", "user.name", "History Author"])?;
        git(&["config", "user.email", "history@example.invalid"])?;
        git(&["config", "commit.gpgsign", "false"])?;
        let old = "original 名.txt";
        std::fs::write(root.join(old), b"\xef\xbb\xbffirst\r\nsecond\r\nlast")?;
        git(&["add", "--", old])?;
        git(&["commit", "-qm", "Create original"])?;
        git(&["mv", "--", old, "renamed.txt"])?;
        git(&["commit", "-qm", "Rename source"])?;

        let state = RepoState::oneshot(root);
        let head = state.head()?.unwrap();
        assert_eq!(
            blame_baseline(&state, "renamed.txt", &head)?.as_deref(),
            Some("first\nsecond\nlast")
        );
        assert!(blame_baseline(&state, "missing.txt", &head)?.is_none());
        assert!(blame_lines(&state, "renamed.txt", "--help", 1, 1).is_err());
        let lines = blame_lines(&state, "renamed.txt", &head, 2, 2)?;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].path, old);
        assert_eq!(lines[0].original, 2);
        assert_eq!(lines[0].line, 2);
        assert_eq!(lines[0].author, "History Author");
        assert_eq!(lines[0].subject, "Create original");
        Ok(())
    }
}
