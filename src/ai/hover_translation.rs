use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Condvar, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::SharedString;
use gpui_component::input::{HoverDocumentation, HoverParagraph};
use lsp_types::{HoverContents, MarkupKind};
use markdown::mdast::Node;
use regex::Regex;
use sha2::{Digest, Sha256};

use super::{
    Cancellation, Snapshot,
    provider::{Failure, Provider, Thinking},
    translation_cache::{CacheKey, TranslationCache},
};
use crate::repository::Repository;

const PROMPT: &str = include_str!("prompts/translation.txt");
const CACHE_VERSION: &str = "zh-CN-v1";
const HOVER_LIMIT: usize = 128_000;
const PROSE_LIMIT: usize = 200_000;
const FRAGMENT_LIMIT: usize = 8_000;
const BATCH_LIMIT: usize = 6_000;
const BATCH_FRAGMENTS: usize = 32;
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(60);
const RATE_WINDOW: Duration = Duration::from_secs(60);
type Translations = HashMap<String, String>;

mod document;
mod math;
mod paragraph;

pub(crate) use document::{DocumentTranslations, document_prose_keys};

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
            let format = match hover {
                HoverText::Plain(_) => TranslationFormat::Plain,
                HoverText::Markdown(_) => TranslationFormat::Markdown,
            };
            let paragraphs = hover.inputs();
            let mut seen = HashSet::new();
            let fragments: Vec<_> = paragraphs
                .iter()
                .map(String::as_str)
                .filter(|text| seen.insert(*text))
                .collect();
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
            let translated = translate(
                &fragments,
                &self.service.cache,
                &provider,
                &cancelled,
                format,
            )
            .ok()?;
            (!translated.is_empty() && self.is_current())
                .then(|| hover.documentation(&paragraphs, &translated))
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
                let root = parse_hover_markdown(&source, false)?;
                let mut paragraphs = Vec::new();
                let mut definitions = HashMap::new();
                collect_definitions(&root, &mut definitions);
                collect_paragraphs(&root, &source, &definitions, &mut paragraphs);
                Some(Self::Markdown(paragraphs))
            }
        }
    }

    fn inputs(&self) -> Vec<String> {
        match self {
            Self::Plain(items) => items.clone(),
            Self::Markdown(paragraphs) => paragraphs
                .iter()
                .map(ProseParagraph::markdown_input)
                .collect(),
        }
    }

    fn documentation(&self, inputs: &[String], translated: &Translations) -> HoverDocumentation {
        match self {
            Self::Plain(_) => HoverDocumentation {
                plaintext: Some(
                    inputs
                        .iter()
                        .filter_map(|input| translated.get(input).map(String::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                        .into(),
                ),
                ..Default::default()
            },
            Self::Markdown(paragraphs) => HoverDocumentation {
                paragraphs: paragraphs
                    .iter()
                    .zip(inputs)
                    .filter_map(|(paragraph, input)| {
                        Some((
                            paragraph.offset,
                            HoverParagraph {
                                original: paragraph.original().into(),
                                translation: translated.get(input)?.clone().into(),
                            },
                        ))
                    })
                    .collect::<BTreeMap<_, _>>()
                    .into(),
                ..Default::default()
            },
        }
    }
}

fn parse_hover_markdown(source: &str, detect_html: bool) -> Option<Node> {
    let mut options = markdown::ParseOptions::default();
    options.constructs.gfm_table = true;
    options.constructs.gfm_strikethrough = true;
    options.gfm_strikethrough_single_tilde = false;
    options.constructs.math_flow = true;
    options.constructs.html_flow = detect_html;
    options.constructs.html_text = detect_html;
    let mut root = markdown::to_mdast(source, &options).ok()?;
    if source.contains('$') {
        let masked = math::mask_literal_dollars(source, &root);
        options.constructs.math_text = true;
        root = markdown::to_mdast(&masked, &options).ok()?;
    }
    Some(root)
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
            let text = unwrap(&lines.join("\n"));
            needs_chinese(&text).then_some(text)
        })
        .filter(|text| seen.insert(text.clone()))
        .collect()
}

#[derive(Debug)]
enum Run {
    Text(String),
    Wrap,
    Markup(String),
    OriginalOnly(String),
}

#[derive(Debug)]
struct ProseParagraph {
    offset: usize,
    runs: Vec<Run>,
}

impl ProseParagraph {
    fn parse(
        node: &Node,
        source: &str,
        definitions: &HashMap<&str, &str>,
        for_document: bool,
    ) -> Option<Self> {
        let offset = node.position()?.start.offset;
        let mut tokens = Vec::new();
        for child in node.children()? {
            collect_runs(child, source, definitions, &mut tokens, for_document);
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
                    runs.push(Run::Text(" ".into()));
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

    fn original(&self) -> String {
        let mut original = String::new();
        for run in &self.runs {
            match run {
                Run::Text(text) => literal_markdown(&mut original, text),
                Run::Markup(mark) | Run::OriginalOnly(mark) => original.push_str(mark),
                Run::Wrap => unreachable!(),
            }
        }
        original
    }

    fn markdown_input(&self) -> String {
        let mut input = String::new();
        for run in &self.runs {
            match run {
                Run::Text(text) => {
                    for ch in text.chars() {
                        if matches!(
                            ch,
                            '\\' | '&' | '<' | '>' | '[' | ']' | '`' | '*' | '_' | '~' | '$'
                        ) {
                            input.push('\\');
                        }
                        input.push(ch);
                    }
                }
                Run::Markup(markdown) => input.push_str(markdown),
                Run::OriginalOnly(_) => {}
                Run::Wrap => unreachable!(),
            }
        }
        input
    }

    fn translation(&self, translated: &Translations) -> SharedString {
        let mut translation = String::new();
        for run in &self.runs {
            match run {
                Run::Text(text) => {
                    let value = translated.get(text).filter(|_| needs_chinese(text));
                    literal_markdown(&mut translation, value.unwrap_or(text));
                }
                Run::Markup(mark) => translation.push_str(mark),
                Run::OriginalOnly(_) => {}
                Run::Wrap => unreachable!(),
            }
        }
        translation.into()
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
        Node::Paragraph(_) => out.extend(ProseParagraph::parse(node, source, definitions, false)),
        Node::Root(_) | Node::Blockquote(_) | Node::List(_) | Node::ListItem(_) => {
            for child in node.children().unwrap() {
                collect_paragraphs(child, source, definitions, out);
            }
        }
        _ => {}
    }
}

fn collect_runs(
    node: &Node,
    source: &str,
    definitions: &HashMap<&str, &str>,
    runs: &mut Vec<Run>,
    for_document: bool,
) {
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
        Node::Paragraph(_) => {
            if !runs.is_empty() {
                runs.push(Run::Wrap);
            }
            ("", String::new())
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
        Node::InlineMath(_) | Node::Html(_) | Node::FootnoteReference(_) if for_document => {
            runs.push(Run::OriginalOnly(raw(source, node).to_owned()));
            return;
        }
        Node::Image(_) | Node::ImageReference(_) => {
            runs.push(Run::OriginalOnly(raw(source, node).to_owned()));
            return;
        }
        _ => {
            runs.push(Run::Markup(unwrap(raw(source, node))));
            return;
        }
    };
    if !open.is_empty() {
        runs.push(Run::Markup(open.into()));
    }
    if let Some(children) = node.children() {
        for child in children {
            collect_runs(child, source, definitions, runs, for_document);
        }
    }
    if !close.is_empty() {
        runs.push(Run::Markup(close));
    }
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
    pending: HashMap<(Arc<PathBuf>, CacheKey), Arc<Pending>>,
}

#[derive(Default)]
struct Pending {
    result: Mutex<Option<Result<Translations, Failure>>>,
    ready: Condvar,
}

static WORK: LazyLock<Mutex<Work>> = LazyLock::new(Mutex::default);

#[derive(Clone, Copy, PartialEq, Eq)]
enum TranslationFormat {
    Fragments,
    Plain,
    Markdown,
}

impl TranslationFormat {
    fn cache_key(self, text: &str, provider: &Provider) -> CacheKey {
        let version = match self {
            Self::Fragments => CACHE_VERSION,
            Self::Plain => "zh-CN-hover-plain-v1",
            Self::Markdown => "zh-CN-hover-markdown-v1",
        };
        let key = serde_json::to_vec(&[version, &provider.base, &provider.model, text]).unwrap();
        Sha256::digest(key).into()
    }

    fn accepts(self, source: &str, translated: &str) -> bool {
        self == Self::Fragments
            || (!translated.trim().is_empty()
                && units(translated) <= 32_000
                && (self != Self::Markdown || paragraph::same_markdown(source, translated)))
    }
}

fn translate(
    fragments: &[&str],
    cache: &TranslationCache,
    provider: &Provider,
    cancelled: &AtomicBool,
    format: TranslationFormat,
) -> Result<Translations, Failure> {
    let mut output = HashMap::new();
    translate_batches(fragments, cache, provider, cancelled, format, |batch| {
        output.extend(batch);
        true
    })?;
    Ok(output)
}

fn translate_batches(
    fragments: &[&str],
    cache: &TranslationCache,
    provider: &Provider,
    cancelled: &AtomicBool,
    format: TranslationFormat,
    mut report: impl FnMut(Translations) -> bool,
) -> Result<(), Failure> {
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
        let mut output = HashMap::new();
        translate_batch(
            &fragments[start..index],
            cache,
            provider,
            &mut output,
            cancelled,
            format,
        )?;
        check_cancelled(cancelled)?;
        if !report(output) {
            return Err(Failure::Cancelled);
        }
    }
    Ok(())
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
    cache: &TranslationCache,
    provider: &Provider,
    output: &mut Translations,
    cancelled: &AtomicBool,
    format: TranslationFormat,
) -> Result<(), Failure> {
    let mut remaining: Vec<_> = texts
        .iter()
        .map(|text| {
            (
                *text,
                (cache.path().clone(), format.cache_key(text, provider)),
            )
        })
        .collect();
    let task = loop {
        check_cancelled(cancelled)?;
        let mut work = WORK.lock().unwrap();
        if let Some(wait) = remaining
            .iter()
            .find_map(|(_, key)| work.pending.get(key))
            .cloned()
        {
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
            let result = result.as_ref().unwrap().as_ref();
            if result == Err(&Failure::Cancelled) {
                continue;
            }
            let translated = result.map_err(|error| *error)?;
            remaining.retain(|(text, _)| {
                if let Some(value) = translated.get(*text) {
                    output.insert((*text).to_owned(), value.clone());
                    false
                } else {
                    true
                }
            });
            if remaining.is_empty() {
                return Ok(());
            }
        } else {
            // Reserve keys before reading SQLite so another hover can share this result.
            let task = Arc::new(Pending::default());
            for (_, key) in &remaining {
                work.pending.insert(key.clone(), task.clone());
            }
            break task;
        }
    };
    let fragments: Vec<_> = remaining
        .iter()
        .map(|(text, (_, key))| (*text, *key))
        .collect();
    // Hover batches finish for other waiters. Document jobs can stop before
    // sending a retry or a fallback request when their preview is no longer visible.
    let result = run_batch(
        &fragments,
        provider,
        cache,
        (format == TranslationFormat::Fragments).then_some(cancelled),
        format,
    );
    {
        let mut work = WORK.lock().unwrap();
        for (_, key) in &remaining {
            work.pending.remove(key);
        }
        *task.result.lock().unwrap() = Some(result.clone());
        task.ready.notify_all();
    }
    output.extend(result?);
    Ok(())
}

fn run_batch(
    fragments: &[(&str, CacheKey)],
    provider: &Provider,
    cache: &TranslationCache,
    cancelled: Option<&AtomicBool>,
    format: TranslationFormat,
) -> Result<Translations, Failure> {
    let keys: Vec<_> = fragments.iter().map(|(_, key)| *key).collect();
    let mut hits = cache.load(&keys).unwrap_or_default();
    let mut output = HashMap::new();
    let missing: Vec<_> = fragments
        .iter()
        .filter(|(text, key)| {
            if let Some(value) = hits.remove(key) {
                output.insert((*text).to_owned(), value);
                false
            } else {
                true
            }
        })
        .collect();
    if missing.is_empty() {
        return Ok(output);
    }

    let texts: Vec<_> = missing.iter().map(|(text, _)| *text).collect();
    let mut values = match request(&texts, provider, cancelled, format) {
        Ok(values) => Some(values),
        Err(error)
            if error.response_error()
                && (texts.len() > 1 || format != TranslationFormat::Fragments) =>
        {
            None
        }
        Err(_) if format != TranslationFormat::Fragments => return Ok(output),
        Err(error) => return Err(error),
    }
    .into_iter()
    .flatten();
    let mut entries = Vec::new();
    let mut save = |(text, key): &(&str, CacheKey), value: String| {
        output.insert((*text).to_owned(), value.clone());
        entries.push((*key, value));
    };
    let result = missing.into_iter().try_for_each(|fragment| {
        let text = fragment.0;
        if let Some(value) = values.next().filter(|value| format.accepts(text, value)) {
            save(fragment, value);
            return Ok(());
        }
        match request(&[text], provider, cancelled, format) {
            Ok(mut values) => {
                let value = values.pop().unwrap();
                if format.accepts(text, &value) {
                    save(fragment, value);
                }
            }
            Err(_) if format != TranslationFormat::Fragments => {}
            Err(error) => return Err(error),
        }
        Ok(())
    });
    // Keep completed fragments even when a later request fails.
    if cache.store(&entries).is_err() {
        eprintln!("stroll:hover-translation: cache-write-failed");
    }
    result?;
    Ok(output)
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

fn request(
    texts: &[&str],
    provider: &Provider,
    cancelled: Option<&AtomicBool>,
    format: TranslationFormat,
) -> Result<Vec<String>, Failure> {
    let _active = ActiveRequest::start()?;
    let started = Instant::now();
    let input = texts.join("\n\n");
    for attempt in 1..=2 {
        if let Some(cancelled) = cancelled {
            check_cancelled(cancelled)?;
        }
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
            .and_then(|text| parse_response(&text, texts.len(), format))
        {
            Ok(output) => return Ok(output),
            Err(error)
                if error.response_error()
                    && (texts.len() > 1 || format != TranslationFormat::Fragments) =>
            {
                return Err(error);
            }
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

fn parse_response(
    content: &str,
    count: usize,
    format: TranslationFormat,
) -> Result<Vec<String>, Failure> {
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
        || (format == TranslationFormat::Fragments
            && parts
                .iter()
                .any(|part| part.trim().is_empty() || units(part) > 32_000))
    {
        return Err(Failure::ResponseShape);
    }
    Ok(parts.into_iter().map(|part| unwrap(part.trim())).collect())
}

#[cfg(test)]
mod tests {
    use lsp_types::MarkupContent;

    use super::*;
    use crate::ai::provider::tests::{mock_provider, mock_provider_before_reply};

    // Loopback tests share the production request limiter.
    static REQUEST_TESTS: Mutex<()> = Mutex::new(());

    fn markdown(value: &str) -> HoverText {
        HoverText::parse(HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: value.into(),
        }))
        .unwrap()
    }

    fn markdown_inputs(value: &str) -> Vec<String> {
        markdown(value).inputs()
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
            markdown_inputs(concat!(
                "# Current implementation\n\n## `Type definitions`\n\n",
                "```rust\npub fn count(items: &[Item]) -> usize\n```\n\n",
                "Threads an effectful\naccumulator from left to right.\n\n",
                "Empty inputs return pure(initial).  \nThe receiver initializes strictly.\n\n",
                "| Header |\n| --- |\n| Cell |\n\n- List prose\n\n> Quoted prose",
            )),
            [
                "Threads an effectful accumulator from left to right.",
                "Empty inputs return pure(initial). The receiver initializes strictly.",
                "List prose",
                "Quoted prose",
            ]
        );
        assert_eq!(
            markdown_inputs(concat!(
                "```ts\nfunction secret(): void\n```\n\n",
                "Read `secret_code()` and [guide](https://private.example).\n\n",
                "    indented_code()\n\n$x + secret$\n\n$$\nprivate_formula\n$$",
            )),
            ["Read `secret_code()` and [guide](<https://private.example>)."]
        );
        for (source, fragments) in [
            (
                "Use $HOME paths and $x$ math",
                &[r"Use \$HOME paths and $x$ math"][..],
            ),
            ("x $foo$4 word", &[r"x \$foo\$4 word"][..]),
            (
                "Read foo$bar$ and after",
                &[r"Read foo\$bar\$ and after"][..],
            ),
            ("Use $$x + secret$$ here", &["Use $$x + secret$$ here"][..]),
            (
                "See <foo@example.com> guide",
                &["See [foo@example.com](<mailto:foo@example.com>) guide"][..],
            ),
            (
                "Read ~strikethrough~ and ~~delete~~.",
                &[r"Read \~strikethrough\~ and ~~delete~~."][..],
            ),
            ("Read\nthese\nthree\nlines", &["Read these three lines"][..]),
            ("$$unclosed\n\nAnother paragraph", &[][..]),
        ] {
            assert_eq!(markdown_inputs(source), fragments, "{source}");
        }
        assert_eq!(
            plaintext_prose("function read(): string\nRead documentation\n123\n\n中文说明"),
            ["Read documentation 123"]
        );
        assert_eq!(
            markdown_inputs(
                "使用 **TypeScript** API 输出紧凑的诊断树。\n\nReturns the compact diagnostic tree."
            ),
            ["Returns the compact diagnostic tree."]
        );
        assert!(markdown_inputs("生成一棵紧凑的诊断树，并在每个节点标记它在源代码中的位置。详细用法见[指南](https://example.com)。").is_empty());
    }

    #[test]
    fn hover_rendering_fixtures() {
        let doc = markdown(
            "Read **carefully** and `code()`. [Guide](https://example.com) ![diagram](local.png)",
        );
        let inputs = doc.inputs();
        let value = "参阅[指南](https://example.com)，并 **仔细** 阅读 `code()`。";
        assert!(paragraph::same_markdown(&inputs[0], value));
        let translated = HashMap::from([(inputs[0].clone(), value.into())]);
        let pairs = doc.documentation(&inputs, &translated);
        let pair = &pairs.paragraphs[&0];
        assert!(pair.original.contains("**carefully**"));
        assert!(pair.original.contains("![diagram](local.png)"));
        assert_eq!(pair.translation.as_ref(), value);
        assert_eq!(
            parse_response(
                "第一段第一行\r\n第一段第二行\r\n\r\n第二段",
                2,
                TranslationFormat::Markdown
            )
            .unwrap(),
            ["第一段第一行第一段第二行", "第二段"]
        );
        assert_eq!(
            parse_response(
                "```plaintext\n直接译文\n```",
                1,
                TranslationFormat::Markdown
            )
            .unwrap(),
            ["直接译文"]
        );
        assert_eq!(
            parse_response("only one", 2, TranslationFormat::Markdown),
            Err(Failure::ResponseShape)
        );
    }

    #[test]
    fn hover_markdown_preserves_code_and_links_and_rejects_added_markup() {
        let input = "Call [`read()`](<https://example.com>) or retry.";
        let translated = "或重试调用 [`read()`](https://example.com)。";
        assert!(paragraph::same_markdown(input, translated));
        for invalid in [
            "调用 [`read()`](https://other.example)。",
            "调用 `read()`（[说明](https://example.com)）。",
            "调用 [`write()`](https://example.com)。",
            "调用 [`read()`](https://example.com)。<img src='https://other.example'>",
            "调用 [`read()`](https://example.com)。![图片](https://other.example)",
        ] {
            assert!(!paragraph::same_markdown(input, invalid), "{invalid}");
        }
    }

    #[test]
    fn hover_retries_only_invalid_paragraphs_and_caches_valid_neighbors() {
        let _requests = REQUEST_TESTS.lock().unwrap();
        const BATCH: &str = r#"{"choices":[{"finish_reason":"stop","message":{"content":"标题\n\n缺少代码\n\n正文"}}]}"#;
        const INVALID: &str =
            r#"{"choices":[{"finish_reason":"stop","message":{"content":"仍缺少代码"}}]}"#;
        let (provider, server) = mock_provider("hover", vec![(200, BATCH), (200, INVALID)]);
        let directory = tempfile::tempdir().unwrap();
        let cache = TranslationCache::new(directory.path().join("translations.sqlite3"));
        let format = TranslationFormat::Markdown;
        let detail = "Read `value` now.";
        let run =
            |texts: &[&str]| translate(texts, &cache, &provider, &AtomicBool::new(false), format);
        let output = run(&["Heading", detail, "Body"]).unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output["Heading"], "标题");
        assert_eq!(output["Body"], "正文");
        assert_eq!(run(&["Heading", "Body"]).unwrap(), output);
        assert!(
            cache
                .load(&[format.cache_key(detail, &provider)])
                .unwrap()
                .is_empty()
        );
        assert_ne!(
            format.cache_key("Heading", &provider),
            TranslationFormat::Fragments.cache_key("Heading", &provider),
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0]["messages"][1]["content"],
            "Heading\n\nRead `value` now.\n\nBody"
        );
        assert_eq!(requests[1]["messages"][1]["content"], detail);
        assert!(requests[0].get("response_format").is_none());
    }

    #[test]
    fn hover_retries_each_paragraph_when_a_batch_loses_a_boundary() {
        let _requests = REQUEST_TESTS.lock().unwrap();
        const FIRST: &str =
            r#"{"choices":[{"finish_reason":"stop","message":{"content":"第一段"}}]}"#;
        const SECOND: &str =
            r#"{"choices":[{"finish_reason":"stop","message":{"content":"第二段"}}]}"#;
        let (provider, server) =
            mock_provider("hover", vec![(200, SECOND), (200, FIRST), (200, SECOND)]);
        let directory = tempfile::tempdir().unwrap();
        let cache = TranslationCache::new(directory.path().join("translations.sqlite3"));
        let output = translate(
            &["First paragraph", "Second paragraph"],
            &cache,
            &provider,
            &AtomicBool::new(false),
            TranslationFormat::Markdown,
        )
        .unwrap();
        assert_eq!(output["First paragraph"], "第一段");
        assert_eq!(output["Second paragraph"], "第二段");
        let requests = server.join().unwrap();
        assert_eq!(requests[1]["messages"][1]["content"], "First paragraph");
        assert_eq!(requests[2]["messages"][1]["content"], "Second paragraph");
    }

    #[test]
    fn translation_retries_splits_and_reuses_completed_fragments() {
        let _requests = REQUEST_TESTS.lock().unwrap();
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
        let directory = tempfile::tempdir().unwrap();
        let cache = TranslationCache::new(directory.path().join("translations.sqlite3"));
        let fragments = ["First paragraph", "Second paragraph"];
        let cancelled = AtomicBool::new(false);
        let translate = || {
            translate(
                &fragments,
                &cache,
                &provider,
                &cancelled,
                TranslationFormat::Fragments,
            )
        };
        assert_eq!(translate(), Err(Failure::RateLimited));
        assert_eq!(
            cache
                .load(&[TranslationFormat::Fragments.cache_key(fragments[0], &provider)])
                .unwrap()
                .len(),
            1
        );
        let output = translate().unwrap();
        assert_eq!(output[fragments[0]], "标题");
        assert_eq!(output[fragments[1]], "正文");
        let reopened = TranslationCache::new(cache.path().as_ref().clone());
        assert_eq!(
            super::translate(
                &fragments,
                &reopened,
                &provider,
                &cancelled,
                TranslationFormat::Fragments
            )
            .unwrap(),
            output,
        );
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
        let _requests = REQUEST_TESTS.lock().unwrap();
        let (provider, server) = mock_provider(
            "reader",
            vec![(
                200,
                r#"{"choices":[{"finish_reason":"stop","message":{"content":"指南"}}]}"#,
            )],
        );
        let directory = tempfile::tempdir().unwrap();
        let cache = TranslationCache::new(directory.path().join("translations.sqlite3"));
        let run = || {
            translate(
                &["Guide"],
                &cache,
                &provider,
                &AtomicBool::new(false),
                TranslationFormat::Markdown,
            )
        };
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

    #[test]
    fn concurrent_hovers_share_translation_when_cache_write_fails() {
        let _requests = REQUEST_TESTS.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("translations.sqlite3");
        let cache = TranslationCache::new(path.clone());
        cache.load(&[[0; 32]]).unwrap();
        rusqlite::Connection::open(path)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_cache_write BEFORE INSERT ON translations
                 BEGIN SELECT RAISE(ABORT, 'cache write failed'); END;",
            )
            .unwrap();

        let (request_arrived, arrived) = std::sync::mpsc::channel();
        let (send_reply, reply) = std::sync::mpsc::channel();
        let (provider, server) = mock_provider_before_reply(
            "reader",
            vec![(
                200,
                r#"{"choices":[{"finish_reason":"stop","message":{"content":"指南"}}]}"#,
            )],
            move || {
                request_arrived.send(()).unwrap();
                reply.recv_timeout(Duration::from_secs(5)).unwrap();
            },
        );
        let pending = (
            cache.path().clone(),
            TranslationFormat::Markdown.cache_key("Guide", &provider),
        );
        let run = || {
            translate(
                &["Guide"],
                &cache,
                &provider,
                &AtomicBool::new(false),
                TranslationFormat::Markdown,
            )
        };
        std::thread::scope(|threads| {
            let first = threads.spawn(run);
            arrived.recv_timeout(Duration::from_secs(5)).unwrap();
            let second = threads.spawn(run);
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                // The first request and the registry each hold a reference.
                if Arc::strong_count(&WORK.lock().unwrap().pending[&pending]) == 3 {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "second hover did not join the request"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            send_reply.send(()).unwrap();
            assert_eq!(first.join().unwrap().unwrap()["Guide"], "指南");
            assert_eq!(second.join().unwrap().unwrap()["Guide"], "指南");
        });
        assert!(cache.load(&[pending.1]).unwrap().is_empty());
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
