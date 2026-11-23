mod builtin;

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read as _,
    ops::Range,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context as _, Result, ensure};
use gpui::SharedString;
use gpui_component::{
    highlighter::LanguageRegistry,
    input::{
        AutoClosingPair, BracketPair, IndentationRules, SyntaxContext,
        language_config::LanguageConfig,
    },
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::repository::Repository;

const JSON_LIMIT: usize = 256 * 1024;

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Settings {
    language_extension_paths: Vec<PathBuf>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Manifest {
    contributes: Contributions,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Contributions {
    languages: Vec<Value>,
    grammars: Vec<Value>,
}

#[derive(Deserialize)]
struct Language {
    id: String,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    filenames: Vec<String>,
    #[serde(default)]
    aliases: Vec<String>,
    configuration: Option<PathBuf>,
    #[serde(skip)]
    editing: Option<LanguageConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextMateGrammar {
    language: Option<String>,
    scope_name: String,
    path: PathBuf,
    inject_to: Option<Value>,
}

#[derive(Default)]
pub(crate) struct LanguageResources {
    languages: Vec<Language>,
    names: HashMap<String, SharedString>,
    source_languages: HashSet<String>,
    registered: Vec<SharedString>,
    code_languages: Option<gpui_component::text::CodeLanguages>,
}

impl Drop for LanguageResources {
    fn drop(&mut self) {
        for name in &self.registered {
            LanguageRegistry::singleton().unregister(name);
        }
    }
}

impl LanguageResources {
    pub(crate) fn initialize(cx: &mut gpui::App) {
        builtin::install(cx);
    }

    pub(crate) fn builtin_for_path(path: &Path) -> Option<&'static str> {
        builtin::for_path(path)
    }

    pub(crate) fn for_file<'a>(repo: Option<&'a Repository>, path: &Path) -> &'a Self {
        if let Some(repo) = repo.filter(|_| !path.is_absolute()) {
            repo.language_resources()
        } else {
            Self::installed()
        }
    }

    pub(crate) fn installed() -> &'static Self {
        static INSTALLED: std::sync::OnceLock<LanguageResources> = std::sync::OnceLock::new();
        INSTALLED.get_or_init(|| Self::load(None))
    }

    pub(crate) fn load(repo: Option<&Repository>) -> Self {
        builtin::register_grammars();
        let mut result = Self::default();
        let mut textmate = TextMate::default();
        let settings_path = Path::new(".stroll.json");
        if let Some(repo) = repo.filter(|repo| repo.exists(settings_path)) {
            match read_repo(repo, settings_path, JSON_LIMIT).and_then(decode::<Settings>) {
                Ok(settings) => {
                    for path in settings.language_extension_paths.into_iter().take(32) {
                        result.load_extension(&path, Some(repo), &mut textmate);
                    }
                }
                Err(error) => eprintln!("Could not read {settings_path:?}: {error:#}"),
            }
        }
        if let Some(home) = crate::language::language_home()
            && let Ok(entries) = std::fs::read_dir(home.join("language-packs"))
        {
            let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
            paths.sort();
            for path in paths.into_iter().filter(|path| path.is_dir()).take(32) {
                result.load_extension(&path, None, &mut textmate);
            }
        }
        result.register_textmate(textmate);
        if !result.names.is_empty() {
            let mut names = std::collections::BTreeMap::new();
            for (name, key) in &result.names {
                names.insert(SharedString::from(name.clone()), key.clone());
            }
            for language in &result.languages {
                if let Some(key) = result.names.get(&language.id) {
                    for alias in &language.aliases {
                        names
                            .entry(SharedString::from(alias.clone()))
                            .or_insert_with(|| key.clone());
                    }
                }
            }
            result.code_languages = Some(Arc::new(names));
        }
        result
    }

    fn load_extension(&mut self, root: &Path, repo: Option<&Repository>, textmate: &mut TextMate) {
        let read = |path: &Path, size| -> Result<Vec<u8>> {
            ensure!(relative(path), "Invalid extension resource path");
            match repo {
                Some(repo) if repo.ssh.is_some() => {
                    ensure!(
                        relative(root),
                        "Remote extensions must be inside the worktree"
                    );
                    let path = root
                        .components()
                        .chain(path.components())
                        .filter_map(|part| match part {
                            Component::Normal(name) => Some(name),
                            _ => None,
                        })
                        .collect::<PathBuf>();
                    read_repo(repo, &path, size)
                }
                _ => {
                    let root = if let Some(repo) = repo {
                        repo.root.join(root)
                    } else {
                        root.to_owned()
                    };
                    let root = root.canonicalize()?;
                    let path = root.join(path).canonicalize()?;
                    ensure!(
                        path.starts_with(root),
                        "Extension resource escapes its directory"
                    );
                    read_local(&path, size)
                }
            }
        };

        let manifest = read(Path::new("package.json"), JSON_LIMIT).and_then(decode::<Manifest>);
        let contributions = match manifest {
            Ok(manifest) => manifest.contributes,
            Err(error) => {
                eprintln!("Could not load language extension {root:?}: {error:#}");
                return;
            }
        };

        for entry in contributions.languages.into_iter().take(128) {
            let language = (|| -> Result<_> {
                let mut language: Language = serde_json::from_value(entry)?;
                if let Some(path) = &language.configuration {
                    let configuration =
                        read(path, JSON_LIMIT).and_then(decode::<EditorConfiguration>)?;
                    language.editing = Some(configuration.into_config());
                }
                Ok(language)
            })();
            match language {
                Ok(language)
                    if valid_name(&language.id)
                        && language
                            .extensions
                            .iter()
                            .chain(&language.filenames)
                            .all(|name| valid_name(name)) =>
                {
                    self.languages.push(language);
                }
                Ok(_) => eprintln!("Invalid language contribution in {root:?}"),
                Err(error) => eprintln!("Could not load a language from {root:?}: {error:#}"),
            }
        }

        for entry in contributions.grammars.into_iter().take(128) {
            let result = (|| {
                let grammar: TextMateGrammar = serde_json::from_value(entry)?;
                ensure!(
                    grammar.inject_to.is_none(),
                    "TextMate grammar injections are unsupported"
                );
                ensure!(
                    grammar.path.extension().is_some_and(|ext| ext == "json"),
                    "Expected a JSON TextMate grammar"
                );
                let value = decode::<Value>(read(&grammar.path, JSON_LIMIT)?)?;
                textmate.add(grammar, value)
            })();
            if let Err(error) = result {
                eprintln!("Could not load a TextMate grammar from {root:?}: {error:#}");
            }
        }
    }

    fn register_name(&mut self, language: &str) -> SharedString {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let key: SharedString =
            format!("stroll-language-{}", NEXT.fetch_add(1, Ordering::Relaxed)).into();
        self.registered.push(key.clone());
        self.names.insert(language.to_owned(), key.clone());
        key
    }

    fn register_textmate(&mut self, mut textmate: TextMate) {
        if textmate.languages.is_empty() {
            return;
        }
        let palette: HashMap<_, _> = SCOPE_STYLES
            .iter()
            .enumerate()
            .map(|(index, &(_, name))| {
                (
                    format!("#{:06x}", index + 1).to_uppercase(),
                    SharedString::new_static(name),
                )
            })
            .collect();
        let rules: Vec<_> = SCOPE_STYLES.iter().enumerate().map(|(index, &(scope, _))| {
            json!({"scope": scope, "settings": {"foreground": format!("#{:06x}", index + 1)}})
        }).collect();

        let theme = json!({"name": "stroll", "colors": {"editor.foreground": "#000000", "editor.background": "#ffffff"}, "tokenColors": rules});
        if let Err(error) = textmate
            .registry
            .add_theme_from_bytes(theme.to_string().as_bytes())
        {
            eprintln!("Could not initialize TextMate scopes: {error}");
            return;
        }
        textmate.registry.link_grammars();
        let registry = Arc::new(textmate.registry);
        let palette = Arc::new(palette);
        for (language, (name, is_source)) in textmate.languages {
            let key = self.register_name(&language);
            let options =
                giallo::HighlightOptions::new(name, giallo::ThemeVariant::Single("stroll"))
                    .merge_whitespace(false)
                    .merge_same_style_tokens(false);
            let registry = registry.clone();
            let palette = palette.clone();
            LanguageRegistry::singleton().register_tokenizer(
                &key,
                Arc::new(move |source| tokenize(source, &registry, &options, &palette)),
            );
            if is_source {
                self.source_languages.insert(language);
            }
        }
    }

    pub(crate) fn code_languages(&self) -> Option<gpui_component::text::CodeLanguages> {
        self.code_languages.clone()
    }

    fn match_path(&self, path: &Path) -> Option<&Language> {
        let name = path.file_name()?.to_str()?;
        let exact = self
            .languages
            .iter()
            .find(|language| language.filenames.iter().any(|file| file == name));
        let language = exact.or_else(|| {
            self.languages
                .iter()
                .flat_map(|language| language.extensions.iter().map(move |ext| (language, ext)))
                .filter(|(_, ext)| ext.starts_with('.') && name.ends_with(ext.as_str()))
                .fold(None, |best, (language, suffix)| match best {
                    Some((_, length)) if length >= suffix.len() => best,
                    _ => Some((language, suffix.len())),
                })
                .map(|(language, _)| language)
        })?;
        Some(language)
    }

    fn matched_grammar(&self, path: &Path) -> Option<(&Language, &SharedString)> {
        let language = self.match_path(path)?;
        Some((language, self.names.get(&language.id)?))
    }

    pub(crate) fn for_path(&self, path: &Path) -> Option<SharedString> {
        self.matched_grammar(path)
            .map(|(_, name)| name.clone())
            .or_else(|| Self::builtin_for_path(path).map(Into::into))
    }

    pub(crate) fn syntax_for_id(&self, id: &str) -> Option<SharedString> {
        self.names.get(id).cloned().or_else(|| {
            LanguageRegistry::singleton()
                .language(id)
                .map(|grammar| grammar.name)
        })
    }

    pub(crate) fn display_name_for_path(&self, path: &Path) -> Option<SharedString> {
        let (language, _) = self.matched_grammar(path)?;
        let display_name = language
            .aliases
            .iter()
            .map(|alias| alias.trim())
            .find(|alias| !alias.is_empty())
            .unwrap_or(&language.id);
        Some(display_name.to_owned().into())
    }

    pub(crate) fn is_source_file(&self, path: &Path) -> bool {
        self.match_path(path)
            .is_some_and(|language| self.source_languages.contains(&language.id))
    }

    pub(crate) fn configure_editor(&self, path: &Path, cx: &mut gpui::App) {
        let Some(language) = self.match_path(path) else {
            return;
        };
        if let (Some(key), Some(editing)) = (self.names.get(&language.id), &language.editing) {
            gpui_component::input::set_language_config(key, editing.clone(), cx);
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditorConfiguration {
    brackets: Option<Vec<[String; 2]>>,
    auto_closing_pairs: Option<Vec<ClosingPair>>,
    auto_close_before: Option<String>,
    indentation_rules: Option<Indentation>,
    comments: Option<Comments>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Comments {
    line_comment: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ClosingPair {
    Pair([String; 2]),
    Config {
        open: String,
        close: String,
        #[serde(default, rename = "notIn")]
        not_in: Vec<String>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Indentation {
    increase_indent_pattern: String,
    decrease_indent_pattern: String,
}

impl EditorConfiguration {
    fn into_config(self) -> LanguageConfig {
        let mut config = LanguageConfig::default();
        if let Some(marker) = self.comments.and_then(|comments| comments.line_comment) {
            config = config.line_comment(marker);
        }
        if let Some(brackets) = self.brackets {
            config = config.brackets(brackets.into_iter().take(128).filter_map(|[open, close]| {
                (open.encode_utf16().count() == 1
                    && close.encode_utf16().count() == 1
                    && open != close)
                    .then(|| BracketPair::new(open, close))
            }));
        }
        if let Some(pairs) = self.auto_closing_pairs {
            config = config.auto_closing_pairs(pairs.into_iter().take(128).filter_map(|pair| {
                let (open, close, contexts) = match pair {
                    ClosingPair::Pair([open, close]) => (open, close, Vec::new()),
                    ClosingPair::Config {
                        open,
                        close,
                        not_in,
                    } => (open, close, not_in),
                };
                (open.encode_utf16().count() == 1 && close.encode_utf16().count() == 1).then(|| {
                    AutoClosingPair::new(open, close).not_in(contexts.iter().filter_map(
                        |context| match context.as_str() {
                            "string" => Some(SyntaxContext::String),
                            "comment" => Some(SyntaxContext::Comment),
                            _ => None,
                        },
                    ))
                })
            }));
        }
        if let Some(before) = self.auto_close_before {
            config = config.auto_close_before(before);
        }
        if let Some(rules) = self.indentation_rules
            && rules.increase_indent_pattern.len() <= 1000
            && rules.decrease_indent_pattern.len() <= 1000
            && let (Ok(increase), Ok(decrease)) = (
                regex::Regex::new(&rules.increase_indent_pattern),
                regex::Regex::new(&rules.decrease_indent_pattern),
            )
        {
            config = config.indentation_rules(IndentationRules::new(increase, decrease));
        }
        config
    }
}

#[derive(Default)]
struct TextMate {
    registry: giallo::Registry,
    languages: HashMap<String, (String, bool)>,
    scopes: HashSet<String>,
}

impl TextMate {
    fn add(&mut self, contribution: TextMateGrammar, mut value: Value) -> Result<()> {
        ensure!(
            valid_name(&contribution.scope_name)
                && value["scopeName"].as_str() == Some(&contribution.scope_name)
                && value["patterns"].is_array(),
            "Invalid TextMate scope or patterns"
        );
        if self.scopes.contains(&contribution.scope_name) {
            return Ok(());
        }
        let name = format!("grammar-{}", self.scopes.len());
        value["name"] = json!(&name);
        self.registry
            .add_grammar_from_bytes(value.to_string().as_bytes())?;
        let is_source = contribution.scope_name.starts_with("source.");
        self.scopes.insert(contribution.scope_name);
        if let Some(language) = contribution.language {
            self.languages.entry(language).or_insert((name, is_source));
        }
        Ok(())
    }
}

const SCOPE_STYLES: &[(&str, &str)] = &[
    ("comment", "comment"),
    ("comment.block.documentation", "comment.doc"),
    ("string", "string"),
    ("constant.numeric", "number"),
    ("constant.language", "constant"),
    ("keyword", "keyword"),
    ("keyword.operator", "operator"),
    ("storage", "keyword"),
    ("support.type", "type"),
    ("entity.name.type", "type"),
    ("entity.name.function", "function"),
    ("entity.name.function.member", "function.method"),
    ("variable", "variable"),
    ("variable.other.property", "property"),
    ("entity.name.namespace", "namespace"),
    ("entity.name.tag", "tag"),
    ("punctuation.definition.tag", "punctuation.bracket"),
    ("meta.interpolation", ""),
    ("invalid", ""),
];

fn tokenize(
    source: &str,
    registry: &giallo::Registry,
    options: &giallo::HighlightOptions,
    palette: &HashMap<String, SharedString>,
) -> Vec<(Range<usize>, SharedString)> {
    // The engine normalizes line endings. Index the original line starts
    // independently so CRLF, Unicode, and newline bytes remain untouched.
    if source.len() > 512 * 1024 {
        return Vec::new();
    }
    let Ok(output) = registry.highlight(source, options) else {
        return Vec::new();
    };
    let mut captures = Vec::new();
    let mut base = 0;
    let mut lines = source.split_inclusive('\n');
    for tokens in output.tokens {
        let Some(line) = lines.next() else { break };
        let content = line
            .strip_suffix('\n')
            .unwrap_or(line)
            .trim_end_matches('\r');
        let mut offset = 0;
        for token in tokens {
            let end = offset + token.text.len();
            if end > content.len() || content.get(offset..end) != Some(token.text.as_str()) {
                break;
            }
            if let giallo::ThemeVariant::Single(style) = token.style
                && let Some(name) = palette.get(&style.foreground.as_hex())
                && !name.is_empty()
            {
                captures.push((base + offset..base + end, name.clone()));
            }
            offset = end;
        }
        base += line.len();
    }
    captures
}

fn decode<T: DeserializeOwned>(bytes: Vec<u8>) -> Result<T> {
    let text = std::str::from_utf8(&bytes)?.trim_start_matches('\u{feff}');
    Ok(jsonc_parser::parse_to_serde_value(
        text,
        &Default::default(),
    )?)
}

fn read_repo(repo: &Repository, path: &Path, size: usize) -> Result<Vec<u8>> {
    if repo.ssh.is_none() {
        return read_local(&repo.resolve(path)?, size);
    }
    let bytes = repo.bytes(path, false)?;
    ensure!(bytes.len() <= size, "Language resource exceeds size limit");
    Ok(bytes)
}

fn read_local(path: &Path, size: usize) -> Result<Vec<u8>> {
    ensure!(path.is_file(), "Expected a regular language resource file");
    let file = File::open(path).with_context(|| format!("Could not open {path:?}"))?;
    ensure!(
        file.metadata()?.is_file(),
        "Expected a regular language resource file"
    );
    let mut bytes = Vec::new();
    file.take(size as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= size, "Language resource exceeds size limit");
    Ok(bytes)
}

fn relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
}

fn valid_name(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && !value.contains(['/', '\\', '\0'])
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{HighlightStyle, rgb};
    use gpui_base::input::HighlightStyleResolver;
    use gpui_component::{
        Rope,
        highlighter::{HighlightTheme, SyntaxHighlighter},
    };

    #[gpui::test]
    fn vue_highlighting_resolves_embedded_syntax(cx: &mut gpui::TestAppContext) {
        struct Mark(&'static str);
        impl HighlightStyleResolver for Mark {
            fn style(&self, name: &str) -> Option<HighlightStyle> {
                (name == self.0 || name.starts_with(&format!("{}.", self.0))).then_some(
                    HighlightStyle {
                        color: Some(rgb(0xffffff).into()),
                        ..Default::default()
                    },
                )
            }
        }

        let source = r#"<script>
let plainJs = 23
</script>
<script setup lang="ts">
const count: number = 42
</script>
<script lang="jsx">
export const jsxElement = <Button />
</script>
<script lang="tsx">
export const render = (visible: boolean) => <div>{visible}</div>
</script>
<template>
  <Panel v-if="count > 1">{{ count + 99 }}</Panel>
</template>
<style scoped>
  .panel { color: red; }
</style>
<style lang="scss">
  $ink: red;
</style>
<style lang="less">
  @accent: blue;
</style>"#;
        cx.update(LanguageResources::initialize);
        let resources = LanguageResources::default();
        let language = resources.for_path(Path::new("App.vue")).unwrap();
        assert_eq!(language, "vue");
        for (language, document) in [
            (language.as_ref(), source.to_string()),
            ("markdown", format!("```vue\n{source}\n```")),
        ] {
            let mut syntax = SyntaxHighlighter::new(language);
            assert!(syntax.update(None, &Rope::from(document.as_str()), None));
            for (token, capture) in [
                ("let", "keyword"),
                ("const", "keyword"),
                ("number", "type"),
                ("Button", "type"),
                ("boolean", "type"),
                ("v-if", "keyword"),
                ("99", "number"),
                ("color", "property"),
                ("$ink", "variable"),
                ("@accent", "variable"),
            ] {
                let offset = document.find(token).unwrap();
                let ranges = syntax.styles(&(0..document.len()), &Mark(capture));
                assert!(
                    ranges.iter().any(|(range, _)| range.contains(&offset)),
                    "missing {capture} for {token} in {language}: {ranges:?}",
                );
            }
        }
    }

    #[test]
    fn textmate_extension_uses_worktree_scoped_utf8_highlights() -> Result<()> {
        let directory = tempfile::tempdir()?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(directory.path())
                .status()?
                .success()
        );
        let extension = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/textmate");
        std::fs::write(
            directory.path().join(".stroll.json"),
            format!(
                "{{\n // Worktree language configuration\n \"languageExtensionPaths\": [{}],\n}}",
                json!(extension)
            ),
        )?;
        let repo = Repository::open(directory.path())?;
        let resources = repo.language_resources();
        let lang = resources
            .for_path(Path::new("sample.example"))
            .context("Example grammar")?;
        let languages = resources
            .code_languages()
            .context("Code block language mappings")?;
        assert_eq!(languages.get("example"), Some(&lang));
        let mut syntax = SyntaxHighlighter::new(&lang);
        let source = "# 示例 🧪\r\nlet value = 1\r\nreturn value\r\n";
        let theme = HighlightTheme::default_dark();
        assert!(syntax.update(None, &Rope::from(source), None));
        let styles = syntax.styles(&(0..source.len()), theme.as_ref());
        for keyword in ["let", "return"] {
            let start = source.find(keyword).unwrap();
            assert!(
                styles.iter().any(|(range, style)| {
                    range.start <= start
                        && range.end >= start + keyword.len()
                        && Some(*style) == theme.style("keyword")
                }),
                "{keyword}: {styles:?}"
            );
        }
        assert_ne!(SyntaxHighlighter::new("example").language(), &lang);
        let second = tempfile::tempdir()?;
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(second.path())
                .status()?
                .success()
        );
        assert!(
            Repository::open(second.path())?
                .language_resources()
                .for_path(Path::new("sample.example"))
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn textmate_source_files_use_the_same_language_match_as_highlighting() -> Result<()> {
        let mut resources = LanguageResources::default();
        resources.languages = serde_json::from_value(json!([
            {"id": "code", "extensions": [".example"], "aliases": ["Example", "example"]},
            {"id": "prose", "filenames": ["README.example"]},
            {"id": "unregistered", "extensions": [".incomplete.example"]}
        ]))?;
        let mut textmate = TextMate::default();
        for (id, scope) in [("code", "source.example"), ("prose", "text.example")] {
            textmate.add(
                TextMateGrammar {
                    language: Some(id.into()),
                    scope_name: scope.into(),
                    path: PathBuf::new(),
                    inject_to: None,
                },
                json!({"scopeName": scope, "patterns": []}),
            )?;
        }
        resources.register_textmate(textmate);

        assert!(resources.is_source_file(Path::new("sample.example")));
        assert_eq!(
            resources
                .display_name_for_path(Path::new("sample.example"))
                .as_deref(),
            Some("Example")
        );
        assert!(!resources.is_source_file(Path::new("sample.txt")));
        assert!(
            resources
                .display_name_for_path(Path::new("sample.txt"))
                .is_none()
        );
        let prose = Path::new("README.example");
        assert!(resources.for_path(prose).is_some());
        assert_eq!(
            resources.display_name_for_path(prose).as_deref(),
            Some("prose")
        );
        assert!(!resources.is_source_file(prose));
        let unregistered = Path::new("sample.incomplete.example");
        assert!(resources.for_path(unregistered).is_none());
        assert!(resources.display_name_for_path(unregistered).is_none());
        assert!(!resources.is_source_file(unregistered));
        Ok(())
    }
}
