use std::{
    ops::Range,
    path::{Component, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use fancy_regex::{Regex, RegexBuilder};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rust_i18n::t;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::repository::{Document, Repository};

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct TextQuery {
    pub query: String,
    pub scope: Scope,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

impl TextQuery {
    pub(crate) fn pattern(&self) -> Result<Regex> {
        ensure!(self.query.len() <= 4096, t!("locator.query_length"));
        let mut pattern = if self.regex {
            self.query.clone()
        } else {
            regex::escape(&self.query)
        };
        if self.whole_word {
            pattern = format!(
                r"(?<![\p{{L}}\p{{N}}\p{{M}}_$])(?:{pattern})(?![\p{{L}}\p{{N}}\p{{M}}_$])"
            );
        }
        let flags = if self.case_sensitive { "(?m)" } else { "(?im)" };
        RegexBuilder::new(&format!("{flags}{pattern}"))
            .backtrack_limit(100_000)
            .build()
            .with_context(|| t!("locator.invalid_regex").into_owned())
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Scope {
    pub include: String,
    pub exclude: String,
}

impl Scope {
    fn patterns(&self) -> Result<(GlobSet, GlobSet)> {
        fn compile(raw: &str) -> Result<GlobSet> {
            ensure!(raw.len() <= 4096, t!("locator.query_length"));
            let mut builder = GlobSetBuilder::new();
            for pattern in raw
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
            {
                builder.add(
                    GlobBuilder::new(pattern)
                        .literal_separator(true)
                        .build()
                        .with_context(|| {
                            t!("locator.invalid_glob", pattern = pattern).into_owned()
                        })?,
                );
            }
            Ok(builder.build()?)
        }
        Ok((compile(&self.include)?, compile(&self.exclude)?))
    }
}

pub(crate) fn listed_paths(repo: &Repository) -> Result<Vec<PathBuf>> {
    let mut paths = match &repo.ssh {
        #[cfg(feature = "desktop")]
        Some(ssh) if ssh.supports_locator() => ssh.call(crate::remote::Request::LocatorPaths)?,
        Some(_) => repo
            .children
            .values()
            .flatten()
            .filter(|entry| !entry.directory)
            .map(|entry| entry.path.clone())
            .collect(),
        None => crate::repository::worktree_paths(&repo.root)?,
    };
    paths.retain(|path| {
        path.components()
            .all(|part| matches!(part, Component::Normal(name) if name != ".git"))
    });
    paths.sort_unstable();
    paths.dedup();
    Ok(paths)
}

fn scoped_paths<'a>(
    paths: &'a [PathBuf],
    scope: &Scope,
) -> Result<impl Iterator<Item = &'a PathBuf>> {
    let (include, exclude) = scope.patterns()?;
    Ok(paths.iter().filter(move |path| {
        (include.is_empty() || include.is_match(path)) && !exclude.is_match(path)
    }))
}

#[cfg(feature = "desktop")]
pub(crate) fn search_files(
    paths: &[PathBuf],
    query: &str,
    scope: &Scope,
    recent: &[PathBuf],
) -> Result<Results<PathBuf>> {
    let query = query.to_lowercase();
    let mut ranked = scoped_paths(paths, scope)?
        .filter_map(|path| {
            let score = file_score(&path, &query)?;
            let visit = recent
                .iter()
                .position(|item| item == path)
                .unwrap_or(usize::MAX);
            Some((std::cmp::Reverse(score), visit, path.clone()))
        })
        .collect::<Vec<_>>();
    ranked.sort_unstable();
    Ok(Results {
        truncated: ranked.len() > 200,
        skipped: 0,
        items: ranked
            .into_iter()
            .take(200)
            .map(|(_, _, path)| path)
            .collect(),
    })
}

#[cfg(feature = "desktop")]
fn file_score(path: &std::path::Path, query: &str) -> Option<isize> {
    if query.is_empty() {
        return Some(0);
    }
    let name = path.file_name()?.to_str()?.to_lowercase();
    let path = path
        .to_str()?
        .replace(std::path::MAIN_SEPARATOR, "/")
        .to_lowercase();
    if name == query {
        return Some(10_000);
    }
    if name.starts_with(query) {
        return Some(8000 - name.len() as isize);
    }
    if name.contains(query) {
        return Some(6000 - name.len() as isize);
    }
    if path.contains(query) {
        return Some(4000 - path.len() as isize);
    }
    let mut start = 0;
    let mut gaps = 0;
    for needle in query.chars() {
        let next = path[start..].find(needle)?;
        gaps += next;
        start += next + needle.len_utf8();
    }
    Some(1000 - (path.len() + gaps) as isize)
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Results<T> {
    pub items: Vec<T>,
    pub truncated: bool,
    pub skipped: usize,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct TextHit {
    pub path: PathBuf,
    pub fingerprint: [u8; 32],
    pub range: Range<usize>,
    pub line: u32,
    pub snippet: String,
    pub mark: Range<usize>,
    pub clipped: bool,
}

pub(crate) fn fingerprint(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

pub(crate) fn search_text(
    repo: &Repository,
    query: &TextQuery,
    cancelled: impl Fn() -> bool,
) -> Result<Results<TextHit>> {
    let mut result = Results {
        items: Vec::new(),
        truncated: false,
        skipped: 0,
    };
    if query.query.is_empty() {
        return Ok(result);
    }
    let pattern = query.pattern()?;
    let started = Instant::now();
    let mut bytes = 0;
    let paths = listed_paths(repo)?;
    for (index, path) in scoped_paths(&paths, &query.scope)?.enumerate() {
        if cancelled() {
            break;
        }
        if index >= 5000 || bytes >= 32 * 1024 * 1024 || started.elapsed() >= Duration::from_secs(5)
        {
            result.truncated = true;
            break;
        }
        let Ok(document) = Document::read(repo, path) else {
            result.skipped += 1;
            continue;
        };
        let text = document.text;
        bytes += text.len();
        let mut lines = text.match_indices('\n').peekable();
        let mut line = 0;
        let mut start = 0;
        let mut hash = None;
        for hit in pattern.find_iter(&text) {
            if cancelled() {
                return Ok(result);
            }
            if started.elapsed() >= Duration::from_secs(5) {
                result.truncated = true;
                return Ok(result);
            }
            let hit = hit.with_context(|| t!("locator.search_timeout").into_owned())?;
            if hit.start() == hit.end() {
                continue;
            }
            if result.items.len() == 500 {
                result.truncated = true;
                return Ok(result);
            }
            while let Some((offset, _)) = lines.next_if(|(offset, _)| *offset < hit.start()) {
                start = offset + 1;
                line += 1;
            }
            let line_end = lines.peek().map_or(text.len(), |(offset, _)| *offset);
            let left = text.ceil_char_boundary(start.max(hit.start().saturating_sub(100)));
            let right = text.floor_char_boundary(line_end.min(hit.start() + 400));
            result.items.push(TextHit {
                path: path.clone(),
                fingerprint: *hash.get_or_insert_with(|| fingerprint(&text)),
                range: hit.range(),
                line,
                snippet: text[left..right].to_owned(),
                mark: hit.start() - left..hit.end().min(right) - left,
                clipped: left > start || right < line_end || hit.end() > line_end,
            });
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searches_scoped_utf8_crlf_text_with_unicode_word_boundaries() -> Result<()> {
        let folder = tempfile::tempdir()?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(folder.path())
                .status()?
                .success()
        );
        let root = folder.path();
        std::fs::create_dir(root.join("src"))?;
        std::fs::write(
            root.join("src/main.rs"),
            "\u{feff}🙂 Alpha\r\nAlphaβ\r\nALPHA\r\n",
        )?;
        std::fs::write(root.join("src/excluded.rs"), "Alpha")?;
        std::fs::write(root.join("README.md"), "Alpha")?;
        std::fs::write(root.join("src/binary.rs"), [0])?;
        let repo = Repository::open(root)?;
        let query = TextQuery {
            query: "alpha".to_owned(),
            scope: Scope {
                include: "src/**".into(),
                exclude: "**/excluded.rs".into(),
            },
            whole_word: true,
            ..Default::default()
        };
        let result = search_text(&repo, &query, || false)?;
        assert_eq!(result.skipped, 1);
        assert_eq!(
            result
                .items
                .iter()
                .map(|item| item.line)
                .collect::<Vec<_>>(),
            [0, 2]
        );
        let hit = &result.items[0];
        assert_eq!(&hit.snippet[hit.mark.clone()], "Alpha");
        assert_eq!(hit.range, 5..10);
        assert!(search_text(&repo, &query, || true)?.items.is_empty());
        Ok(())
    }

    #[test]
    fn returns_multiline_ranges_and_reports_invalid_expressions() -> Result<()> {
        let query = TextQuery {
            query: "(?<=x)foo\\nbar".into(),
            regex: true,
            ..Default::default()
        };
        let found = query.pattern()?.find("xfoo\nbar")?.unwrap();
        assert_eq!(found.range(), 1..8);
        let invalid = TextQuery {
            query: "[".into(),
            ..query
        };
        assert!(invalid.pattern().is_err());
        Ok(())
    }
}
