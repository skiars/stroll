use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use gpui_component::input::{HoverDocumentation, HoverParagraph};
use lsp_types::{HoverContents, MarkupKind};
use markdown::mdast::{Node, Paragraph};
use regex::Regex;
use sha2::{Digest, Sha256};

use super::{
    Cancellation, Snapshot,
    provider::{Failure, Provider, Thinking},
};
use crate::repository::Repository;

const PROMPT: &str = include_str!("prompts/hover_translation.txt");
const CACHE_VERSION: &str = "zh-CN-v1";
const HOVER_LIMIT: usize = 128_000;
const PROSE_LIMIT: usize = 200_000;
const FRAGMENT_LIMIT: usize = 8_000;
const BATCH_LIMIT: usize = 6_000;
const BATCH_FRAGMENTS: usize = 32;
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(60);
const RATE_WINDOW: Duration = Duration::from_secs(60);
type Translations = HashMap<String, String>;

mod math;

impl Snapshot {
    pub(crate) async fn translate_hover(
        self,
        repo: Arc<Repository>,
        path: PathBuf,
        contents: HoverContents,
    ) -> Option<HoverDocumentation> {
        let cancellation = Cancellation::default();
        let cancelled = cancellation.flag();
        blocking::unblock(move || {
            if !self.is_current() || cancelled.load(Ordering::Relaxed) {
                return None;
            }
            let hover = HoverText::parse(contents)?;
            let fragments = hover.fragments();
            if fragments.is_empty()
                || fragments.iter().map(|text| units(text)).sum::<usize>() > PROSE_LIMIT
                || fragments.iter().any(|text| units(text) > FRAGMENT_LIMIT)
                || cancelled.load(Ordering::Relaxed)
                || !repo.allows_ai_hover(&path)
                || !self.is_current()
            {
                return None;
            }
            let provider = Provider::open(&self.settings, &self.service.credential_scope).ok()?;
            let translated =
                translate(&fragments, &self.service.cache, &provider, &cancelled).ok()?;
            self.is_current().then(|| hover.documentation(&translated))
        })
        .await
    }
}

enum HoverText {
    Plain(Vec<String>),
    Markdown(Vec<ProseParagraph>),
}

impl HoverText {
    fn parse(contents: HoverContents) -> Option<Self> {
        let HoverContents::Markup(markup) = contents else {
            return None;
        };
        let source = markup.value;
        if units(&source) > HOVER_LIMIT {
            return None;
        }
        match markup.kind {
            MarkupKind::PlainText => Some(Self::Plain(plaintext_prose(&source))),
            MarkupKind::Markdown => {
                let mut options = markdown::ParseOptions::default();
                options.constructs.gfm_table = true;
                options.constructs.gfm_strikethrough = true;
                options.gfm_strikethrough_single_tilde = false;
                options.constructs.math_flow = true;
                options.constructs.html_flow = false;
                options.constructs.html_text = false;
                let mut root = markdown::to_mdast(&source, &options).ok()?;
                if source.contains('$') {
                    let masked = math::mask_literal_dollars(&source, &root);
                    options.constructs.math_text = true;
                    root = markdown::to_mdast(&masked, &options).ok()?;
                }
                let mut paragraphs = Vec::new();
                let mut definitions = HashMap::new();
                collect_definitions(&root, &mut definitions);
                collect_paragraphs(&root, &source, &definitions, &mut paragraphs);
                Some(Self::Markdown(paragraphs))
            }
        }
    }

    fn fragments(&self) -> Vec<&str> {
        let mut seen = HashSet::new();
        match self {
            Self::Plain(items) => items
                .iter()
                .map(String::as_str)
                .filter(|s| seen.insert(*s))
                .collect(),
            Self::Markdown(paragraphs) => paragraphs
                .iter()
                .flat_map(ProseParagraph::candidates)
                .filter(|s| seen.insert(*s))
                .collect(),
        }
    }

    fn documentation(&self, translated: &Translations) -> HoverDocumentation {
        match self {
            Self::Plain(items) => HoverDocumentation {
                plaintext: Some(
                    items
                        .iter()
                        .filter_map(|text| translated.get(text))
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n")
                        .into(),
                ),
                ..Default::default()
            },
            Self::Markdown(paragraphs) => HoverDocumentation {
                paragraphs: paragraphs
                    .iter()
                    .filter_map(|paragraph| paragraph.render(translated))
                    .collect::<BTreeMap<_, _>>()
                    .into(),
                ..Default::default()
            },
        }
    }
}

fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

fn needs_chinese(text: &str) -> bool {
    static PATHS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
        r"(?i)(?:https?://|www\.)\S+|\S*[/\\]\S+|(?-u:\b[\w-]+)\.(?:md|ts|js|json|png|svg)(?-u:\b)",
    ).unwrap()
    });
    static OTHER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[\p{L}&&[^\p{Han}\p{Latin}]]").unwrap());
    static LATIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{Latin}+").unwrap());
    static HAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{Han}").unwrap());
    let prose = PATHS.replace_all(text, "");
    if OTHER.is_match(&prose) {
        return true;
    }
    let latin_words = LATIN.find_iter(&prose).count();
    if latin_words == 0 {
        return false;
    }
    let han = HAN.find_iter(&prose).count();
    han < 2 || latin_words > 3.max(han / 2)
}

fn is_han(ch: char) -> bool {
    static HAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\p{Han}$").unwrap());
    HAN.is_match(ch.encode_utf8(&mut [0; 4]))
}

fn unwrap(text: &str) -> String {
    static WRAP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]*([\r\n]+)[ \t]*").unwrap());
    let mut out = String::with_capacity(text.len());
    let mut offset = 0;
    for wrap in WRAP.captures_iter(text) {
        let whole = wrap.get(0).unwrap();
        out.push_str(&text[offset..whole.start()]);
        if !matches!(&wrap[1], "\n" | "\r\n")
            || !out.chars().next_back().is_some_and(is_han)
            || !text[whole.end()..].chars().next().is_some_and(is_han)
        {
            out.push(' ');
        }
        offset = whole.end();
    }
    out.push_str(&text[offset..]);
    out
}

fn plaintext_prose(source: &str) -> Vec<String> {
    static PARAGRAPHS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\s*\n").unwrap());
    static DECLARATIONS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
        r"^\s*(?:(?:\([^)]*\)\s*)?(?:function|class|interface|type|const|let|var|def|import|export|package|fn|pub|struct|enum)(?-u:\b)|[$>]\s|(?-u:[\w.$]+)\s*\([^)]*\)\s*(?::|=>|\{|;|$)|[{}\[\]]\s*[,;]?\s*$)",
    ).unwrap()
    });
    let mut seen = HashSet::new();
    PARAGRAPHS
        .split(source)
        .filter_map(|paragraph| {
            let lines: Vec<_> = paragraph
                .split('\n')
                .filter(|line| !DECLARATIONS.is_match(line))
                .collect();
            needs_chinese(&lines.join(" ")).then(|| {
                unwrap(
                    &lines
                        .into_iter()
                        .filter(|line| needs_chinese(line))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
            })
        })
        .filter(|text| seen.insert(text.clone()))
        .collect()
}

#[derive(Debug)]
enum Run {
    Text(String),
    Wrap,
    Markup(String),
    Image(String),
}

#[derive(Debug)]
struct ProseParagraph {
    offset: usize,
    runs: Vec<Run>,
}

impl ProseParagraph {
    fn parse(
        paragraph: &Paragraph,
        source: &str,
        definitions: &HashMap<&str, &str>,
    ) -> Option<Self> {
        let offset = paragraph.position.as_ref()?.start.offset;
        let mut tokens = Vec::new();
        for node in &paragraph.children {
            collect_runs(node, source, definitions, &mut tokens);
        }
        if !needs_chinese(
            &tokens
                .iter()
                .filter_map(|token| match token {
                    Run::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
        ) {
            return None;
        }
        let mut tokens = tokens.into_iter().peekable();
        let mut runs = Vec::new();
        let mut previous_eligible = false;
        while let Some(token) = tokens.next() {
            if matches!(token, Run::Wrap) {
                if let (Some(Run::Text(previous)), Some(Run::Text(next))) =
                    (runs.last_mut(), tokens.peek())
                    && previous_eligible
                    && needs_chinese(next)
                {
                    *previous = unwrap(&format!("{previous}\n{next}"));
                    tokens.next();
                    continue;
                }
                let cjk = matches!(
                    (runs.last(), tokens.peek()),
                    (Some(Run::Text(previous)), Some(Run::Text(next)))
                    if previous.chars().next_back().is_some_and(is_han)
                        && next.chars().next().is_some_and(is_han)
                );
                if !cjk {
                    runs.push(Run::Markup(" ".into()));
                }
                previous_eligible = false;
            } else {
                previous_eligible = matches!(&token, Run::Text(text) if needs_chinese(text));
                runs.push(token);
            }
        }
        Some(Self { offset, runs })
    }

    fn candidates(&self) -> impl Iterator<Item = &str> {
        self.runs.iter().filter_map(|run| match run {
            Run::Text(text) if needs_chinese(text) => Some(text.as_str()),
            _ => None,
        })
    }

    fn render(&self, translated: &Translations) -> Option<(usize, HoverParagraph)> {
        if !self.candidates().any(|text| translated.contains_key(text)) {
            return None;
        }
        let mut original = String::new();
        let mut translation = String::new();
        for run in &self.runs {
            match run {
                Run::Text(text) => {
                    literal_markdown(&mut original, text);
                    if let Some(value) = translated.get(text).filter(|_| needs_chinese(text)) {
                        literal_markdown(&mut translation, value);
                    } else {
                        literal_markdown(&mut translation, text);
                    }
                }
                Run::Markup(mark) => {
                    original.push_str(mark);
                    translation.push_str(mark);
                }
                Run::Image(mark) => original.push_str(mark),
                Run::Wrap => unreachable!(),
            }
        }
        Some((
            self.offset,
            HoverParagraph {
                original: original.into(),
                translation: translation.into(),
            },
        ))
    }
}

fn literal_markdown(target: &mut String, text: &str) {
    use std::fmt::Write as _;
    for ch in text.chars() {
        if ch.is_ascii_punctuation() {
            let _ = write!(target, "&#{};", ch as u32);
        } else {
            target.push(ch);
        }
    }
}

fn collect_definitions<'a>(node: &'a Node, definitions: &mut HashMap<&'a str, &'a str>) {
    if let Node::Definition(definition) = node {
        definitions
            .entry(&definition.identifier)
            .or_insert(&definition.url);
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_definitions(child, definitions);
        }
    }
}

fn collect_paragraphs(
    node: &Node,
    source: &str,
    definitions: &HashMap<&str, &str>,
    out: &mut Vec<ProseParagraph>,
) {
    match node {
        Node::Paragraph(paragraph) => {
            out.extend(ProseParagraph::parse(paragraph, source, definitions))
        }
        Node::Root(_) | Node::Blockquote(_) | Node::List(_) | Node::ListItem(_) => {
            for child in node.children().unwrap() {
                collect_paragraphs(child, source, definitions, out);
            }
        }
        _ => {}
    }
}

fn collect_runs(node: &Node, source: &str, definitions: &HashMap<&str, &str>, runs: &mut Vec<Run>) {
    fn raw<'a>(source: &'a str, node: &Node) -> &'a str {
        node.position()
            .and_then(|pos| source.get(pos.start.offset..pos.end.offset))
            .unwrap_or("")
    }
    let (open, close) = match node {
        Node::Text(text) => {
            let breaks = text.value.bytes().filter(|&ch| ch == b'\n').count();
            for (index, line) in text.value.split('\n').enumerate() {
                if index > 0 {
                    runs.push(Run::Wrap);
                }
                if !line.is_empty() {
                    // Markdown-it trims padding at source soft breaks.
                    let line = if index > 0 {
                        line.trim_start_matches([' ', '\t'])
                    } else {
                        line
                    };
                    let line = if index < breaks {
                        line.trim_end_matches([' ', '\t', '\r'])
                    } else {
                        line
                    };
                    if !line.is_empty() {
                        runs.push(Run::Text(line.replace(math::LITERAL_DOLLAR, "$")));
                    }
                }
            }
            return;
        }
        Node::Break(_) => {
            runs.push(Run::Wrap);
            return;
        }
        Node::Strong(_) => ("**", "**".into()),
        Node::Emphasis(_) => ("*", "*".into()),
        Node::Delete(_) => ("~~", "~~".into()),
        Node::Link(link) => ("[", link_end(&link.url)),
        Node::LinkReference(link) => (
            "[",
            link_end(
                definitions
                    .get(link.identifier.as_str())
                    .copied()
                    .unwrap_or(""),
            ),
        ),
        Node::Image(_) | Node::ImageReference(_) => {
            runs.push(Run::Image(raw(source, node).to_owned()));
            return;
        }
        _ => {
            runs.push(Run::Markup(unwrap(raw(source, node))));
            return;
        }
    };
    runs.push(Run::Markup(open.into()));
    if let Some(children) = node.children() {
        for child in children {
            collect_runs(child, source, definitions, runs);
        }
    }
    runs.push(Run::Markup(close));
}

fn link_end(url: &str) -> String {
    let url = url
        .replace(['\n', '\r'], "")
        .replace('\\', "%5C")
        .replace('<', "%3C")
        .replace('>', "%3E");
    format!("](<{url}>)")
}

#[derive(Default)]
struct Work {
    active: usize,
    requests: VecDeque<Instant>,
    pending: HashMap<PathBuf, Arc<Pending>>,
}

#[derive(Default)]
struct Pending {
    result: Mutex<Option<Result<(), Failure>>>,
    ready: Condvar,
}

static WORK: LazyLock<Mutex<Work>> = LazyLock::new(Mutex::default);

fn cache_file(root: &Path, text: &str, provider: &Provider) -> PathBuf {
    let key = serde_json::to_vec(&[CACHE_VERSION, &provider.base, &provider.model, text]).unwrap();
    root.join(format!("{}.json", hex::encode(Sha256::digest(key))))
}

fn cached(path: &Path) -> Option<String> {
    let translated: String = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (!translated.trim().is_empty()).then(|| unwrap(&translated))
}

fn translate(
    fragments: &[&str],
    cache: &Path,
    provider: &Provider,
    cancelled: &AtomicBool,
) -> Result<Translations, Failure> {
    let mut output = HashMap::new();
    let mut index = 0;
    while index < fragments.len() {
        check_cancelled(cancelled)?;
        let start = index;
        let mut size = 0;
        while let Some(text) = fragments
            .get(index)
            .filter(|_| index - start < BATCH_FRAGMENTS)
        {
            let length = units(text);
            if index > start && size + length > BATCH_LIMIT {
                break;
            }
            size += length;
            index += 1;
        }
        translate_batch(
            &fragments[start..index],
            cache,
            provider,
            &mut output,
            cancelled,
        )?;
    }
    Ok(output)
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), Failure> {
    if cancelled.load(Ordering::Relaxed) {
        Err(Failure::Cancelled)
    } else {
        Ok(())
    }
}

fn translate_batch(
    texts: &[&str],
    cache: &Path,
    provider: &Provider,
    output: &mut Translations,
    cancelled: &AtomicBool,
) -> Result<(), Failure> {
    let mut remaining: Vec<_> = texts
        .iter()
        .map(|text| (*text, cache_file(cache, text, provider)))
        .collect();
    let (missing, task) = loop {
        check_cancelled(cancelled)?;
        let mut work = WORK.lock().unwrap();
        let mut wait = None;
        remaining.retain(|(text, path)| {
            if let Some(task) = work.pending.get(path) {
                wait = Some(task.clone());
            } else if let Some(value) = cached(path) {
                output.insert((*text).to_owned(), value);
                return false;
            }
            true
        });
        if remaining.is_empty() {
            return Ok(());
        }
        if let Some(wait) = wait {
            drop(work);
            let mut result = wait.result.lock().unwrap();
            while result.is_none() {
                check_cancelled(cancelled)?;
                result = wait
                    .ready
                    .wait_timeout(result, Duration::from_millis(100))
                    .unwrap()
                    .0;
            }
            (*result).unwrap()?;
        } else {
            let task = Arc::new(Pending::default());
            for (_, path) in &remaining {
                work.pending.insert(path.clone(), task.clone());
            }
            break (remaining, task);
        }
    };
    // Once a batch is on the wire, finish it so other hovers can reuse the result.
    let result = run_batch(&missing, provider, output);
    let mut work = WORK.lock().unwrap();
    for (_, path) in &missing {
        work.pending.remove(path);
    }
    *task.result.lock().unwrap() = Some(result);
    task.ready.notify_all();
    result
}

fn run_batch(
    missing: &[(&str, PathBuf)],
    provider: &Provider,
    output: &mut Translations,
) -> Result<(), Failure> {
    let mut save = |(text, path): &(&str, PathBuf), value: String| {
        let bytes = serde_json::to_vec(&value).map_err(|_| Failure::CacheWrite)?;
        crate::repository::atomic_write(path, &bytes).map_err(|_| {
            eprintln!("stroll:hover-translation: cache-write-failed");
            Failure::CacheWrite
        })?;
        output.insert((*text).to_owned(), value);
        Ok::<_, Failure>(())
    };
    let texts: Vec<_> = missing.iter().map(|(text, _)| *text).collect();
    match request(&texts, provider) {
        Ok(values) => {
            for (fragment, value) in missing.iter().zip(values) {
                save(fragment, value)?;
            }
        }
        Err(error) if texts.len() > 1 && error.response_error() => {
            for fragment in missing {
                let value = request(&[fragment.0], provider)?.pop().unwrap();
                save(fragment, value)?;
            }
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

struct ActiveRequest;

impl ActiveRequest {
    fn start() -> Result<Self, Failure> {
        let mut work = WORK.lock().unwrap();
        if work.active >= 4 {
            return Err(Failure::Busy);
        }
        work.active += 1;
        Ok(Self)
    }
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        WORK.lock().unwrap().active -= 1;
    }
}

fn request(texts: &[&str], provider: &Provider) -> Result<Vec<String>, Failure> {
    let _active = ActiveRequest::start()?;
    let started = Instant::now();
    let input = texts.join("\n\n");
    for attempt in 1..=2 {
        {
            let mut work = WORK.lock().unwrap();
            let now = Instant::now();
            work.requests
                .retain(|instant| now.duration_since(*instant) < RATE_WINDOW);
            if work.requests.len() >= 120 {
                return Err(Failure::Busy);
            }
            work.requests.push_back(now);
        }
        match provider
            .complete(
                PROMPT,
                &input,
                8192,
                ATTEMPT_TIMEOUT,
                256 * 1024,
                Thinking::Disable,
            )
            .and_then(|text| parse_response(&text, texts.len()))
        {
            Ok(output) => return Ok(output),
            Err(error) if texts.len() > 1 && error.response_error() => return Err(error),
            Err(error) if attempt == 1 && error.retryable() => {
                std::thread::sleep(Duration::from_millis(200))
            }
            Err(error) => {
                eprintln!(
                    "stroll:hover-translation: {error}, attempts={attempt}, fragments={}, elapsed_ms={}",
                    texts.len(),
                    started.elapsed().as_millis()
                );
                return Err(error);
            }
        }
    }
    unreachable!()
}

fn parse_response(content: &str, count: usize) -> Result<Vec<String>, Failure> {
    static FENCE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?is)^\x60\x60\x60(?:text|plaintext)?\s*\n(.*?)\n\x60\x60\x60$").unwrap()
    });
    static PARAGRAPH_BREAK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\n[ \t]*\n(?:[ \t]*\n)*").unwrap());
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let normalized = normalized.trim();
    let output = FENCE
        .captures(normalized)
        .and_then(|m| m.get(1))
        .map_or(normalized, |m| m.as_str());
    let parts: Vec<_> = PARAGRAPH_BREAK.split(output).collect();
    if parts.len() != count
        || parts
            .iter()
            .any(|part| part.trim().is_empty() || units(part) > 32_000)
    {
        return Err(Failure::ResponseShape);
    }
    Ok(parts.into_iter().map(|part| unwrap(part.trim())).collect())
}

#[cfg(test)]
mod tests {
    use lsp_types::MarkupContent;

    use super::*;
    use crate::ai::provider::tests::mock_provider;

    fn markdown(value: &str) -> HoverText {
        HoverText::parse(HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: value.into(),
        }))
        .unwrap()
    }

    #[test]
    fn hover_prose_fixtures() {
        for (source, needed) in [
            ("中文说明。123", false),
            ("https://example.com/docs", false),
            ("src/main.ts", false),
            ("中文 mixed text", false),
            ("使用 TypeScript API 获取 JSON 数据", false),
            ("Read the guide", true),
            ("日本語の説明", true),
            ("Привет", true),
            ("Returns the value for 中文参数.", true),
        ] {
            assert_eq!(needs_chinese(source), needed, "{source}");
        }
        assert_eq!(
            markdown(concat!(
                "# Current implementation\n\n## `Type definitions`\n\n",
                "```rust\npub fn count(items: &[Item]) -> usize\n```\n\n",
                "Threads an effectful\naccumulator from left to right.\n\n",
                "Empty inputs return pure(initial).  \nThe receiver initializes strictly.\n\n",
                "| Header |\n| --- |\n| Cell |\n\n- List prose\n\n> Quoted prose",
            ))
            .fragments(),
            [
                "Threads an effectful accumulator from left to right.",
                "Empty inputs return pure(initial). The receiver initializes strictly.",
                "List prose",
                "Quoted prose",
            ]
        );
        assert_eq!(
            markdown(concat!(
                "```ts\nfunction secret(): void\n```\n\n",
                "Read `secret_code()` and [guide](https://private.example).\n\n",
                "    indented_code()\n\n$x + secret$\n\n$$\nprivate_formula\n$$",
            ))
            .fragments(),
            ["Read ", " and ", "guide"]
        );
        for (source, fragments) in [
            (
                "Use $HOME paths and $x$ math",
                &["Use $HOME paths and ", " math"][..],
            ),
            ("x $foo$4 word", &["x $foo$4 word"][..]),
            ("Read foo$bar$ and after", &["Read foo$bar$ and after"][..]),
            ("Use $$x + secret$$ here", &["Use ", " here"][..]),
            (
                "See <foo@example.com> guide",
                &["See ", "foo@example.com", " guide"][..],
            ),
            (
                "Read ~strikethrough~ and ~~delete~~.",
                &["Read ~strikethrough~ and ", "delete"][..],
            ),
            ("Read\nthese\nthree\nlines", &["Read these three lines"][..]),
            ("$$unclosed\n\nAnother paragraph", &[][..]),
        ] {
            assert_eq!(markdown(source).fragments(), fragments, "{source}");
        }
        assert_eq!(
            plaintext_prose("function read(): string\nRead documentation\n\n中文说明"),
            ["Read documentation"]
        );
        assert_eq!(
            markdown(
                "使用 **TypeScript** API 输出紧凑的诊断树。\n\nReturns the compact diagnostic tree."
            )
            .fragments(),
            ["Returns the compact diagnostic tree."]
        );
        assert!(markdown("生成一棵紧凑的诊断树，并在每个节点标记它在源代码中的位置。详细用法见[指南](https://example.com)。").fragments().is_empty());
    }

    #[test]
    fn hover_rendering_fixtures() {
        let doc = markdown(
            "Read **carefully** and `code()`. [Guide](https://example.com) ![diagram](local.png)",
        );
        let translated = HashMap::from([
            ("Read ".into(), "阅读 ".into()),
            ("carefully".into(), "<script>仔细</script>".into()),
            ("Guide".into(), "指南".into()),
        ]);
        let pairs = doc.documentation(&translated);
        let pair = &pairs.paragraphs[&0];
        assert!(pair.original.contains("**carefully**"));
        assert!(pair.original.contains("![diagram](local.png)"));
        assert!(
            pair.translation
                .contains("**&#60;script&#62;仔细&#60;&#47;script&#62;**")
        );
        assert!(pair.translation.contains("[指南](<https://example.com>)"));
        assert!(pair.translation.contains("`code()`"));
        assert!(!pair.translation.contains("local.png"));
        assert_eq!(
            parse_response("第一段第一行\r\n第一段第二行\r\n\r\n第二段", 2).unwrap(),
            ["第一段第一行第一段第二行", "第二段"]
        );
        assert_eq!(
            parse_response("```plaintext\n直接译文\n```", 1).unwrap(),
            ["直接译文"]
        );
        assert_eq!(parse_response("only one", 2), Err(Failure::ResponseShape));
    }

    #[test]
    fn translation_retries_splits_and_reuses_completed_fragments() {
        const SHORT: &str =
            r#"{"choices":[{"finish_reason":"stop","message":{"content":"标题"}}]}"#;
        const BODY: &str = r#"{"choices":[{"finish_reason":"stop","message":{"content":"正文"}}]}"#;
        let (provider, server) = mock_provider(
            "reader",
            vec![
                (503, "private body"),
                (200, SHORT),
                (200, SHORT),
                (429, "private body"),
                (200, BODY),
            ],
        );
        let cache = tempfile::tempdir().unwrap();
        let fragments = ["First paragraph", "Second paragraph"];
        let cancelled = AtomicBool::new(false);
        let translate = || translate(&fragments, cache.path(), &provider, &cancelled);
        assert_eq!(translate(), Err(Failure::RateLimited));
        assert!(cache_file(cache.path(), fragments[0], &provider).exists());
        let output = translate().unwrap();
        assert_eq!(output[fragments[0]], "标题");
        assert_eq!(output[fragments[1]], "正文");
        assert_eq!(translate().unwrap(), output);
        let requests = server.join().unwrap();
        assert_eq!(
            requests
                .iter()
                .map(|r| r["messages"][1]["content"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "First paragraph\n\nSecond paragraph",
                "First paragraph\n\nSecond paragraph",
                "First paragraph",
                "Second paragraph",
                "Second paragraph",
            ]
        );
        assert_eq!(requests[0]["max_tokens"], 8192);
        assert_eq!(requests[0]["stream"], false);
        assert!(requests[0].get("response_format").is_none());
    }

    #[test]
    fn translation_deduplicates_concurrent_requests() {
        let (provider, server) = mock_provider(
            "reader",
            vec![(
                200,
                r#"{"choices":[{"finish_reason":"stop","message":{"content":"指南"}}]}"#,
            )],
        );
        let cache = tempfile::tempdir().unwrap();
        let run = || translate(&["Guide"], cache.path(), &provider, &AtomicBool::new(false));
        std::thread::scope(|threads| {
            let first = threads.spawn(run);
            let second = threads.spawn(run);
            assert_eq!(
                first.join().unwrap().unwrap(),
                second.join().unwrap().unwrap()
            );
        });
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
