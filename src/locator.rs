use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    ops::Range,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use fancy_regex::{Matches, Regex, RegexBuilder};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rust_i18n::t;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::repository::{Document, Repository};

pub(crate) const WORD_CHARACTERS: &str = r"[\p{L}\p{N}\p{M}_$]";

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
            pattern = format!("(?<!{WORD_CHARACTERS})(?:{pattern})(?!{WORD_CHARACTERS})");
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

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub(crate) enum FileType {
    #[default]
    All,
    Markdown,
}

impl FileType {
    fn accepts(self, path: &Path) -> bool {
        match self {
            Self::All => true,
            Self::Markdown => crate::repository::is_markdown(path),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Priority {
    Tracked,
    Visible,
    Other,
}

pub(crate) struct Candidate {
    priority: Priority,
    path: PathBuf,
}

fn valid_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.components().all(|part| {
            matches!(part, Component::Normal(name) if name.to_str()
                .is_some_and(|name| !name.eq_ignore_ascii_case(".git")))
        })
}

fn visible_candidates(paths: Vec<PathBuf>) -> Vec<Candidate> {
    let mut paths = paths;
    paths.retain(|path| valid_path(path));
    paths.sort_unstable();
    paths.dedup();
    paths
        .into_iter()
        .map(|path| Candidate {
            priority: Priority::Visible,
            path,
        })
        .collect()
}

#[cfg(feature = "desktop")]
pub(crate) fn legacy_remote_paths(repo: &Repository) -> Result<Vec<Candidate>> {
    let ssh = repo
        .ssh
        .as_ref()
        .context("A remote workspace is required")?;
    let paths = if ssh.supports_locator() {
        ssh.call(crate::remote::Request::LocatorPaths)?
    } else {
        repo.children
            .values()
            .flatten()
            .filter(|entry| !entry.directory && !entry.symlink)
            .map(|entry| entry.path.clone())
            .collect()
    };
    Ok(visible_candidates(paths))
}

pub(crate) type Catalog = Arc<[Candidate]>;

#[derive(Default)]
pub(crate) struct PathCache(Mutex<Option<(u64, Instant, Catalog)>>);

impl PathCache {
    pub(crate) fn load(
        &self,
        repo: &Repository,
        revision: u64,
        cancelled: &impl Fn() -> bool,
    ) -> Result<Catalog> {
        if let Some((_, _, paths)) =
            self.0.lock().unwrap().as_ref().filter(|(key, time, _)| {
                *key == revision && time.elapsed() < Duration::from_secs(3)
            })
        {
            return Ok(paths.clone());
        }
        let paths = if repo.ssh.is_some() {
            #[cfg(feature = "desktop")]
            {
                legacy_remote_paths(repo)?.into()
            }
            #[cfg(not(feature = "desktop"))]
            anyhow::bail!("Workspace search runs on the remote helper");
        } else {
            collect_paths(&repo.root, cancelled)?
        };
        let mut cache = self.0.lock().unwrap();
        if !cancelled() && cache.as_ref().is_none_or(|(key, _, _)| *key <= revision) {
            *cache = Some((revision, Instant::now(), paths.clone()));
        }
        Ok(paths)
    }
}

fn git_paths(root: &Path, args: &[&str]) -> Result<HashSet<PathBuf>> {
    let output = crate::repository::git(root, args)?;
    Ok(output
        .split(|&byte| byte == 0)
        .filter_map(|bytes| std::str::from_utf8(bytes).ok())
        // Git can list an embedded repository as a directory ending in a slash.
        .filter(|path| !path.ends_with('/'))
        .map(PathBuf::from)
        .collect())
}

fn collect_paths(root: &Path, cancelled: &impl Fn() -> bool) -> Result<Catalog> {
    if cancelled() {
        return Ok(Arc::from([]));
    }
    let tracked = git_paths(root, &["ls-files", "--cached", "-z"])?;
    if cancelled() {
        return Ok(Arc::from([]));
    }
    let visible = git_paths(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut pending = vec![PathBuf::new()];
    let mut paths = Vec::new();
    while let Some(parent) = pending.pop() {
        if cancelled() {
            return Ok(Arc::from([]));
        }
        if !parent.as_os_str().is_empty() {
            let directory = root.join(&parent);
            if directory.canonicalize().as_ref().ok() != Some(&directory) {
                continue;
            }
        }
        crate::repository::files::visit_tree_directory(root, &parent, |name, kind| {
            if kind.is_dir() {
                pending.push(parent.join(name));
            } else if kind.is_file() {
                let path = parent.join(name);
                let priority = if tracked.contains(&path) {
                    Priority::Tracked
                } else if visible.contains(&path) {
                    Priority::Visible
                } else {
                    Priority::Other
                };
                paths.push(Candidate { priority, path });
            }
        })?;
    }
    paths.sort_unstable_by(|a, b| (a.priority, &a.path).cmp(&(b.priority, &b.path)));
    Ok(paths.into())
}

struct PathFilter {
    include: GlobSet,
    exclude: GlobSet,
    file_type: FileType,
}

impl PathFilter {
    fn new(scope: &Scope, file_type: FileType) -> Result<Self> {
        let (include, exclude) = scope.patterns()?;
        Ok(Self {
            include,
            exclude,
            file_type,
        })
    }

    fn accepts(&self, path: &Path) -> bool {
        self.file_type.accepts(path)
            && (self.include.is_empty() || self.include.is_match(path))
            && !self.exclude.is_match(path)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct FileQuery {
    pub query: String,
    pub scope: Scope,
    pub recent: Vec<PathBuf>,
    pub file_type: FileType,
}

pub(crate) fn search_files(
    paths: &[Candidate],
    query: &FileQuery,
    cancelled: impl Fn() -> bool,
) -> Result<Results<PathBuf>> {
    const LIMIT: usize = 200;
    let filter = PathFilter::new(&query.scope, query.file_type)?;
    let needle = query
        .query
        .replace(std::path::MAIN_SEPARATOR, "/")
        .to_lowercase();
    let recent: HashMap<_, _> = query
        .recent
        .iter()
        .enumerate()
        .map(|(i, p)| (p, i))
        .collect();
    let mut ranked = Vec::new();
    for Candidate { priority, path } in paths {
        if cancelled() {
            return Ok(Results::default());
        }
        if filter.accepts(path)
            && let Some((quality, score)) = file_score(path, &needle)
        {
            let visit = recent.get(path).copied().unwrap_or(usize::MAX);
            ranked.push((Reverse(quality), *priority, Reverse(score), visit, path));
        }
    }
    let truncated = ranked.len() > LIMIT;
    if truncated {
        ranked.select_nth_unstable(LIMIT);
        ranked.truncate(LIMIT);
    }
    ranked.sort_unstable();
    Ok(Results {
        truncated,
        skipped: 0,
        items: ranked
            .into_iter()
            .map(|(_, _, _, _, path)| path.clone())
            .collect(),
    })
}

fn file_score(path: &Path, query: &str) -> Option<(u8, isize)> {
    if query.is_empty() {
        return Some((0, 0));
    }
    let path = path
        .to_str()?
        .replace(std::path::MAIN_SEPARATOR, "/")
        .to_lowercase();
    let name = path.rsplit('/').next()?;
    if path == query {
        return Some((6, 0));
    }
    if name == query {
        return Some((5, 0));
    }
    if name.starts_with(query) {
        return Some((4, -(name.len() as isize)));
    }
    if name.contains(query) {
        return Some((3, -(name.len() as isize)));
    }
    if path.contains(query) {
        return Some((2, -(path.len() as isize)));
    }
    let mut start = 0;
    let mut gaps = 0;
    for needle in query.chars() {
        let next = path[start..].find(needle)?;
        gaps += next;
        start += next + needle.len_utf8();
    }
    Some((1, -((path.len() + gaps) as isize)))
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Results<T> {
    pub items: Vec<T>,
    pub truncated: bool,
    pub skipped: usize,
}

impl<T> Default for Results<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            truncated: false,
            skipped: 0,
        }
    }
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

pub(crate) fn search_visible_text(
    repo: &Repository,
    query: &TextQuery,
) -> Result<Results<TextHit>> {
    if query.query.is_empty() {
        return Ok(Results::default());
    }
    let paths = visible_candidates(crate::repository::worktree_paths(&repo.root)?);
    TextSearch::new(paths.into(), query, FileType::All, || false)?.next_page(repo, || false)
}

struct FileMatches<'a> {
    hits: Matches<'a, 'a>,
    lines: std::iter::Peekable<std::str::MatchIndices<'a, char>>,
    line: u32,
    line_start: usize,
}

self_cell::self_cell! {
    struct SearchFile {
        owner: (Arc<Regex>, String),

        #[not_covariant]
        dependent: FileMatches,
    }
}

enum MatchStep {
    End,
    Empty,
    Hit(TextHit),
}

impl SearchFile {
    fn open(pattern: Arc<Regex>, text: String) -> Self {
        Self::new((pattern, text), |(pattern, text)| FileMatches {
            hits: pattern.find_iter(text),
            lines: text.match_indices('\n').peekable(),
            line: 0,
            line_start: 0,
        })
    }

    fn next_hit(&mut self, path: &Path, hash: &mut Option<[u8; 32]>) -> Result<MatchStep> {
        self.with_dependent_mut(|(_, text), file| {
            let Some(hit) = file.hits.next() else {
                return Ok(MatchStep::End);
            };
            let hit = hit.with_context(|| t!("locator.search_timeout").into_owned())?;
            if hit.start() == hit.end() {
                return Ok(MatchStep::Empty);
            }
            while let Some((offset, _)) = file.lines.next_if(|(offset, _)| *offset < hit.start()) {
                file.line_start = offset + 1;
                file.line += 1;
            }
            let line_end = file.lines.peek().map_or(text.len(), |(offset, _)| *offset);
            let left =
                text.ceil_char_boundary(file.line_start.max(hit.start().saturating_sub(100)));
            let right = text.floor_char_boundary(line_end.min(hit.start() + 400));
            Ok(MatchStep::Hit(TextHit {
                path: path.to_owned(),
                fingerprint: *hash.get_or_insert_with(|| fingerprint(text)),
                range: hit.range(),
                line: file.line,
                snippet: text[left..right].to_owned(),
                mark: hit.start() - left..hit.end().min(right) - left,
                clipped: left > file.line_start || right < line_end || hit.end() > line_end,
            }))
        })
    }
}

pub(crate) struct TextSearch {
    pattern: Arc<Regex>,
    paths: Catalog,
    matching: std::vec::IntoIter<usize>,
    current: Option<(usize, SearchFile, Option<[u8; 32]>)>,
}

impl TextSearch {
    pub(crate) fn new(
        paths: Catalog,
        query: &TextQuery,
        file_type: FileType,
        cancelled: impl Fn() -> bool,
    ) -> Result<Self> {
        let filter = PathFilter::new(&query.scope, file_type)?;
        let pattern = Arc::new(query.pattern()?);
        let mut matching = Vec::new();
        if !query.query.is_empty() {
            for (index, candidate) in paths.iter().enumerate() {
                if cancelled() {
                    matching.clear();
                    break;
                }
                if filter.accepts(&candidate.path) {
                    matching.push(index);
                }
            }
        }
        Ok(Self {
            pattern,
            paths,
            matching: matching.into_iter(),
            current: None,
        })
    }

    pub(crate) fn next_page(
        &mut self,
        repo: &Repository,
        cancelled: impl Fn() -> bool,
    ) -> Result<Results<TextHit>> {
        let mut result = Results::default();
        let started = Instant::now();
        let mut files = 0;
        let mut bytes = 0;
        loop {
            if cancelled() {
                break;
            }
            if self.current.is_none() && self.matching.len() == 0 {
                break;
            }
            if result.items.len() == 500 || started.elapsed() >= Duration::from_secs(5) {
                result.truncated = true;
                break;
            }
            if let Some((index, file, hash)) = &mut self.current {
                match file.next_hit(&self.paths[*index].path, hash)? {
                    MatchStep::Hit(hit) => result.items.push(hit),
                    MatchStep::Empty => {}
                    MatchStep::End => self.current = None,
                }
            } else {
                if files >= 5000 || bytes >= 32 * 1024 * 1024 {
                    result.truncated = true;
                    break;
                }
                let index = self.matching.next().unwrap();
                files += 1;
                let Ok(document) = Document::read(repo, &self.paths[index].path) else {
                    result.skipped += 1;
                    continue;
                };
                bytes += document.text.len();
                self.current = Some((
                    index,
                    SearchFile::open(self.pattern.clone(), document.text),
                    None,
                ));
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init(root: &Path, args: &[&str]) -> Result<()> {
        assert!(
            std::process::Command::new("git")
                .current_dir(root)
                .args(args)
                .status()?
                .success()
        );
        Ok(())
    }

    #[test]
    fn file_matches_include_ignored_paths_and_prefer_git_on_equal_quality() -> Result<()> {
        use std::fs;

        let folder = tempfile::tempdir()?;
        let root = folder.path();
        init(root, &["init", "-q"])?;
        fs::write(root.join(".gitignore"), "build/\ntracked/\n")?;
        fs::write(root.join(".git/info/exclude"), "*.local.MDX\n")?;
        for dir in ["build", "tracked", "notes", "nested"] {
            fs::create_dir(root.join(dir))?;
        }
        init(&root.join("nested"), &["init", "-q"])?;
        for path in [
            "tracked/guide.MDX",
            "notes/guide.MDX",
            "notes/guide.MDX.rs",
            "build/guide.MDX",
            "build/.hidden.local.MDX",
            "nested/guide.MDX",
            "nested/.git/internal.MDX",
        ] {
            fs::write(root.join(path), "")?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink("notes", root.join("alias"))?;
        init(
            root,
            &["add", "-f", "tracked/guide.MDX", "notes/guide.MDX.rs"],
        )?;
        let repo = Repository::open(root)?;
        let cache = PathCache::default();
        let paths = cache.load(&repo, 0, &|| false)?;
        let mut query = FileQuery {
            query: "guide.MDX".into(),
            scope: Scope::default(),
            recent: vec![PathBuf::from("build/guide.MDX")],
            file_type: FileType::All,
        };
        assert_eq!(
            search_files(&paths, &query, || false)?.items,
            [
                "tracked/guide.MDX",
                "notes/guide.MDX",
                "build/guide.MDX",
                "nested/guide.MDX",
                "notes/guide.MDX.rs",
            ]
            .map(PathBuf::from)
        );

        query.query = "build/guide.MDX".into();
        assert_eq!(
            search_files(&paths, &query, || false)?.items.first(),
            Some(&PathBuf::from("build/guide.MDX"))
        );

        query.query.clear();
        query.file_type = FileType::Markdown;
        query.scope.exclude = "nested/**".into();
        assert_eq!(
            search_files(&paths, &query, || false)?.items,
            [
                "tracked/guide.MDX",
                "notes/guide.MDX",
                "build/guide.MDX",
                "build/.hidden.local.MDX",
            ]
            .map(PathBuf::from)
        );
        query.scope.exclude.clear();
        query.scope.include = "build/**".into();
        assert_eq!(search_files(&paths, &query, || false)?.items.len(), 2);
        fs::rename(
            root.join("build/.hidden.local.MDX"),
            root.join("build/new.MD"),
        )?;
        let refreshed = cache.load(&repo, 1, &|| false)?;
        assert_eq!(
            search_files(&refreshed, &query, || false)?.items,
            ["build/guide.MDX", "build/new.MD",].map(PathBuf::from)
        );
        Ok(())
    }

    #[test]
    fn text_pages_resume_inside_a_file_and_markdown_filters_before_search() -> Result<()> {
        use std::fs;

        let folder = tempfile::tempdir()?;
        let root = folder.path();
        init(root, &["init", "-q"])?;
        fs::write(root.join(".gitignore"), "build/\n")?;
        fs::create_dir(root.join("build"))?;
        fs::write(root.join("src.rs"), "needle\n".repeat(501))?;
        fs::write(root.join("build/guide.MD"), "first\nneedle\n")?;
        init(root, &["add", "src.rs"])?;
        let repo = Repository::open(root)?;
        let paths = collect_paths(&repo.root, &|| false)?;
        let query = TextQuery {
            query: "needle".into(),
            ..Default::default()
        };
        let mut all = TextSearch::new(paths.clone(), &query, FileType::All, || false)?;
        let first = all.next_page(&repo, || false)?;
        assert!(first.truncated);
        assert_eq!(first.items.len(), 500);
        assert!(
            first
                .items
                .iter()
                .all(|hit| hit.path == Path::new("src.rs"))
        );
        assert_eq!(first.items.last().unwrap().line, 499);
        let second = all.next_page(&repo, || false)?;
        assert!(!second.truncated);
        assert_eq!(second.items.len(), 2);
        assert_eq!(second.items[0].path, Path::new("src.rs"));
        assert_eq!(second.items[0].line, 500);
        assert_eq!(second.items[0].range, 3500..3506);
        assert_eq!(second.items[1].path, Path::new("build/guide.MD"));
        let docs = TextSearch::new(paths, &query, FileType::Markdown, || false)?
            .next_page(&repo, || false)?;
        assert!(!docs.truncated);
        assert_eq!(docs.items.len(), 1);
        assert_eq!(docs.items[0].path, Path::new("build/guide.MD"));
        assert_eq!(docs.items[0].line, 1);
        Ok(())
    }

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
        let paths = collect_paths(&repo.root, &|| false)?;
        let result = TextSearch::new(paths.clone(), &query, FileType::All, || false)?
            .next_page(&repo, || false)?;
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
        assert!(
            TextSearch::new(paths, &query, FileType::All, || false)?
                .next_page(&repo, || true)?
                .items
                .is_empty()
        );
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
