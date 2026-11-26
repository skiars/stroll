use rust_i18n::t;

use std::{
    collections::BTreeMap,
    env,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write as _},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, ensure};
use globset::{GlobBuilder, GlobMatcher};
use gpui::TextRenderingPreference;
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table, TableLike};

use crate::{
    language::parse_command_line, locale::Language, lsp::LanguageServer, repository::atomic_write,
};

const VERSION: u32 = 1;
pub(crate) const FONT_SIZE_MIN: f64 = 10.;
pub(crate) const FONT_SIZE_MAX: f64 = 32.;
pub(crate) const UI_FONT_SIZE_MIN: f64 = 12.;
pub(crate) const UI_FONT_SIZE_MAX: f64 = 14.;
pub(crate) const EDITOR_LINE_HEIGHT_RANGE: std::ops::RangeInclusive<f64> = 1.0..=3.0;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct Config {
    pub appearance: Appearance,
    pub docs: Docs,
    pub editor: Editor,
    pub files: Files,
    pub git: Git,
    pub terminal: Terminal,
    pub lsp: Lsp,
    pub ai: crate::ai::Settings,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct Files {
    pub associations: BTreeMap<String, String>,
}

#[derive(Clone, Default)]
pub(crate) struct FileAssociations {
    globs: Vec<(GlobMatcher, String)>,
    invalid: Vec<String>,
}

impl FileAssociations {
    fn compile(files: &Files) -> Self {
        let mut associations = Self::default();
        for (pattern, language) in &files.associations {
            if pattern.is_empty() || pattern.contains(['/', '\\', '\0']) {
                associations.invalid.push(pattern.clone());
            } else if let Ok(glob) = GlobBuilder::new(pattern).build() {
                associations
                    .globs
                    .push((glob.compile_matcher(), language.clone()));
            } else {
                associations.invalid.push(pattern.clone());
            }
        }
        associations
            .globs
            .sort_by_key(|(glob, _)| std::cmp::Reverse(glob.glob().glob().len()));
        associations
    }

    pub(crate) fn language_for(&self, path: &Path) -> Option<&str> {
        let filename = Path::new(path.file_name()?);
        self.globs
            .iter()
            .find(|(glob, _)| {
                let pattern = glob.glob().glob();
                !pattern.contains(['*', '?', '[', '{']) && filename.as_os_str() == pattern
            })
            .or_else(|| self.globs.iter().find(|(glob, _)| glob.is_match(filename)))
            .map(|(_, language)| language.as_str())
    }

    pub(crate) fn invalid_patterns(&self) -> impl Iterator<Item = &str> {
        self.invalid.iter().map(String::as_str)
    }

    pub(crate) fn rules(&self) -> impl Iterator<Item = (&str, &str)> {
        self.globs
            .iter()
            .map(|(glob, language)| (glob.glob().glob(), language.as_str()))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct Appearance {
    pub theme: Theme,
    pub gradient_title_bar: bool,
    pub code_theme: CodeTheme,
    pub code_contrast: CodeContrast,
    pub language: Language,
    pub ui_font_family: String,
    pub ui_font_size: f64,
    pub fallback_font_family: String,
    pub smooth_scroll: bool,
    pub file_tree_sticky_scroll: bool,
    pub file_tree_horizontal_scroll: bool,
    pub text_rendering: TextRenderingPreference,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            gradient_title_bar: false,
            code_theme: CodeTheme::default(),
            code_contrast: CodeContrast::Enhanced,
            language: Language::System,
            ui_font_family: String::new(),
            ui_font_size: 13.,
            fallback_font_family: String::new(),
            smooth_scroll: true,
            file_tree_sticky_scroll: true,
            file_tree_horizontal_scroll: false,
            text_rendering: TextRenderingPreference::Auto,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

/// Code color scheme for editors and document previews.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CodeTheme {
    #[default]
    #[serde(alias = "default")]
    Prism,
    Orchard,
    Spectrum,
    Neon,
    Lilac,
    LilacWhite,
    Balanced,
    Paper,
}

impl CodeTheme {
    pub(crate) const ALL: [Self; 8] = [
        Self::Prism,
        Self::Orchard,
        Self::Spectrum,
        Self::Neon,
        Self::Lilac,
        Self::LilacWhite,
        Self::Balanced,
        Self::Paper,
    ];

    pub(crate) fn config_value(self) -> &'static str {
        match self {
            Self::Prism => "prism",
            Self::Orchard => "orchard",
            Self::Spectrum => "spectrum",
            Self::Neon => "neon",
            Self::Lilac => "lilac",
            Self::LilacWhite => "lilac-white",
            Self::Balanced => "balanced",
            Self::Paper => "paper",
        }
    }

    pub(crate) fn from_config(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|theme| theme.config_value() == value)
            .unwrap_or_default()
    }
}

/// Contrast target for code text.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CodeContrast {
    Standard,
    #[default]
    Enhanced,
    High,
}

impl CodeContrast {
    pub(crate) const ALL: [Self; 3] = [Self::Standard, Self::Enhanced, Self::High];

    pub(crate) fn config_value(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Enhanced => "enhanced",
            Self::High => "high",
        }
    }

    pub(crate) fn from_config(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|contrast| contrast.config_value() == value)
            .unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct Docs {
    pub body_font_size: f64,
    pub code_block_font_size: f64,
}

impl Default for Docs {
    fn default() -> Self {
        Self {
            body_font_size: 16.,
            code_block_font_size: 13.5,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct Editor {
    pub font_family: String,
    pub font_size: f64,
    pub line_height: f64,
    pub line_numbers: bool,
    pub soft_wrap: bool,
    pub inlay_hints: bool,
    pub selection_highlight: bool,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            font_family: String::new(),
            font_size: 13.5,
            line_height: 1.5,
            line_numbers: true,
            soft_wrap: true,
            inlay_hints: false,
            selection_highlight: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct Git {
    pub diff_layout: DiffLayout,
    pub soft_wrap: bool,
    pub ignore_whitespace: bool,
}

impl Default for Git {
    fn default() -> Self {
        Self {
            diff_layout: DiffLayout::Unified,
            soft_wrap: true,
            ignore_whitespace: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiffLayout {
    #[default]
    Unified,
    Split,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct Terminal {
    pub shell: String,
    pub restore_scrollback: bool,
}

impl Terminal {
    pub(crate) fn executable(&self) -> &str {
        #[cfg(windows)]
        return crate::paths::unquote_windows_path(self.shell.trim());
        #[cfg(not(windows))]
        &self.shell
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct Lsp {
    pub enabled: bool,
    pub rust: Server,
    pub clangd: Server,
    pub typescript: Server,
    pub python: Server,
    pub vue: Server,
    pub html: Server,
}

impl Default for Lsp {
    fn default() -> Self {
        Self {
            enabled: true,
            rust: Server::default(),
            clangd: Server::default(),
            typescript: Server::default(),
            python: Server::default(),
            vue: Server::default(),
            html: Server::default(),
        }
    }
}

impl Lsp {
    pub(crate) fn server(&self, server: LanguageServer) -> &Server {
        match server {
            LanguageServer::Rust => &self.rust,
            LanguageServer::Clangd => &self.clangd,
            LanguageServer::TypeScript => &self.typescript,
            LanguageServer::Python => &self.python,
            LanguageServer::Vue => &self.vue,
            LanguageServer::Html => &self.html,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct Server {
    pub command: String,
}

impl Config {
    fn validate(&self) -> Result<()> {
        for (key, size, min, max) in [
            (
                "appearance.ui_font_size",
                self.appearance.ui_font_size,
                UI_FONT_SIZE_MIN,
                UI_FONT_SIZE_MAX,
            ),
            (
                "docs.body_font_size",
                self.docs.body_font_size,
                FONT_SIZE_MIN,
                FONT_SIZE_MAX,
            ),
            (
                "docs.code_block_font_size",
                self.docs.code_block_font_size,
                FONT_SIZE_MIN,
                FONT_SIZE_MAX,
            ),
            (
                "editor.font_size",
                self.editor.font_size,
                FONT_SIZE_MIN,
                FONT_SIZE_MAX,
            ),
        ] {
            ensure!(
                (min..=max).contains(&size),
                t!("config.font_range", key = key, min = min, max = max)
            );
        }
        ensure!(
            EDITOR_LINE_HEIGHT_RANGE.contains(&self.editor.line_height),
            t!(
                "config.line_height_range",
                min = EDITOR_LINE_HEIGHT_RANGE.start(),
                max = EDITOR_LINE_HEIGHT_RANGE.end()
            )
        );
        for (key, family) in [
            ("appearance.ui_font_family", &self.appearance.ui_font_family),
            (
                "appearance.fallback_font_family",
                &self.appearance.fallback_font_family,
            ),
            ("editor.font_family", &self.editor.font_family),
        ] {
            ensure!(
                !family.contains(char::is_control),
                t!("config.control_characters", key = key)
            );
        }
        let shell = self.terminal.executable();
        #[cfg(windows)]
        ensure!(
            shell.is_empty()
                || (!self.terminal.shell.contains(char::is_control)
                    && !shell.contains('"')
                    && (Path::new(shell).is_absolute()
                        || (!shell.contains(char::is_whitespace)
                            && !shell.contains(['\\', '/', ':', '<', '>', '|', '*', '?'])
                            && Path::new(shell).file_name().is_some()))),
            t!("config.invalid_windows_shell")
        );
        #[cfg(not(windows))]
        ensure!(
            shell.is_empty()
                || (Path::new(shell).is_absolute() && !shell.contains(char::is_control)),
            t!("config.invalid_shell")
        );
        for (key, command) in [
            ("lsp.rust.command", &self.lsp.rust.command),
            ("lsp.clangd.command", &self.lsp.clangd.command),
            ("lsp.typescript.command", &self.lsp.typescript.command),
            ("lsp.python.command", &self.lsp.python.command),
            ("lsp.vue.command", &self.lsp.vue.command),
            ("lsp.html.command", &self.lsp.html.command),
        ] {
            ensure!(
                command.is_empty() || parse_command_line(command).is_some(),
                t!("config.invalid_lsp_command", key = key)
            );
        }
        Ok(())
    }
}

/// Owns the last successfully loaded configuration and the bytes it came from.
#[derive(Clone)]
pub struct ConfigFile {
    path: Option<PathBuf>,
    config: Config,
    pub(crate) file_associations: FileAssociations,
    baseline: Option<Vec<u8>>,
    pub(crate) load_error: Option<String>,
}

impl ConfigFile {
    pub fn load_for_desktop() -> Self {
        Language::System.activate();
        let file = Self::from_environment();
        file.config.appearance.language.activate();
        file
    }

    pub fn from_environment() -> Self {
        let path = env::var_os("STROLL_CONFIG")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::config_dir().map(|dir| dir.join("stroll/config.toml")));
        Self::load(path)
    }

    pub fn load(path: Option<PathBuf>) -> Self {
        let path = path.map(|path| std::path::absolute(&path).unwrap_or(path));
        let mut file = Self {
            path,
            config: Config::default(),
            file_associations: FileAssociations::default(),
            baseline: None,
            load_error: None,
        };
        _ = file.reload();
        file
    }

    pub(crate) fn path(&self) -> Result<&Path> {
        self.path.as_deref().context(t!("config.no_directory"))
    }

    pub(crate) fn current(&self) -> &Config {
        &self.config
    }

    pub(crate) fn same_disk_state(&self, other: &Self) -> bool {
        self.baseline == other.baseline && self.load_error == other.load_error
    }

    pub(crate) fn path_for_editing(&mut self) -> Result<PathBuf> {
        let path = self.path()?.to_owned();
        if path.try_exists()? {
            return Ok(path);
        }
        let _lock = lock_config(&path)?;
        let bytes = new_document().to_string().into_bytes();
        let parent = path.parent().context(t!("config.missing_directory"))?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&bytes)?;
        match file.persist_noclobber(&path) {
            Ok(_) => {}
            Err(error) if error.error.kind() == ErrorKind::AlreadyExists => return Ok(path),
            Err(error) => {
                return Err(error).with_context(|| {
                    t!("config.write_failed", path = path.display()).into_owned()
                });
            }
        }
        if self.baseline.is_none() && self.load_error.is_none() {
            self.baseline = Some(bytes);
        }
        Ok(path)
    }

    pub(crate) fn reload(&mut self) -> Result<()> {
        let result = self.read();
        self.load_error = result.as_ref().err().map(|error| format!("{error:#}"));
        let (bytes, config) = result?;
        self.baseline = bytes;
        if self.config.files != config.files {
            self.file_associations = FileAssociations::compile(&config.files);
        }
        self.config = config;
        Ok(())
    }

    fn read(&self) -> Result<(Option<Vec<u8>>, Config)> {
        let path = self.path()?;
        let bytes = read_file(path)?;
        if self.load_error.is_none() && bytes == self.baseline {
            return Ok((bytes, self.config.clone()));
        }
        let config = match &bytes {
            Some(bytes) => decode(bytes)
                .with_context(|| t!("common.read_failed", path = path.display()).into_owned())?,
            None => Config::default(),
        };
        Ok((bytes, config))
    }

    pub(crate) fn save(&mut self, config: Config) -> Result<()> {
        config.validate()?;
        let path = self.path()?;
        let _lock = lock_config(path)?;
        self.check_disk(path)?;
        let mut document = match &self.baseline {
            Some(bytes) => std::str::from_utf8(bytes)?.parse::<DocumentMut>()?,
            None => new_document(),
        };
        if !document.contains_key("version") {
            document.insert("version", toml_edit::value(i64::from(VERSION)));
        }
        migrate_text_rendering(&mut document);
        let old = toml_edit::ser::to_document(&self.config)?;
        let new = toml_edit::ser::to_document(&config)?;
        patch_table(document.as_table_mut(), old.as_table(), new.as_table());
        let bytes = document.to_string().into_bytes();
        self.check_disk(path)?;
        atomic_write(path, &bytes)
            .with_context(|| t!("config.write_failed", path = path.display()).into_owned())?;
        self.baseline = Some(bytes);
        if self.config.files != config.files {
            self.file_associations = FileAssociations::compile(&config.files);
        }
        self.config = config;
        Ok(())
    }

    fn check_disk(&self, path: &Path) -> Result<()> {
        ensure!(
            self.load_error.is_none() && read_file(path)? == self.baseline,
            t!("config.modified", path = path.display())
        );
        Ok(())
    }
}

fn read_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| t!("common.read_failed", path = path.display()).into_owned())
        }
    }
}

fn decode(bytes: &[u8]) -> Result<Config> {
    #[derive(Deserialize)]
    struct Version {
        #[serde(default = "current_version")]
        version: u32,
    }
    fn current_version() -> u32 {
        VERSION
    }

    let source = std::str::from_utf8(bytes)?;
    let version: Version = toml_edit::de::from_str(source)?;
    ensure!(
        version.version == VERSION,
        t!("config.unsupported_version", version = version.version)
    );
    let mut document = source.parse::<DocumentMut>()?;
    migrate_text_rendering(&mut document);
    let config: Config = toml_edit::de::from_document(document)?;
    config.validate()?;
    Ok(config)
}

fn migrate_text_rendering(document: &mut DocumentMut) {
    let Some(appearance) = document
        .get_mut("appearance")
        .and_then(Item::as_table_like_mut)
    else {
        return;
    };
    let Some(legacy) = appearance.remove("font_hinting") else {
        return;
    };
    if !appearance.contains_key("text_rendering") {
        let value = match legacy.as_str() {
            Some("on") => "pixel_aligned",
            Some("off") => "high_precision",
            _ => {
                appearance.insert("text_rendering", legacy);
                return;
            }
        };
        let mut replacement = toml_edit::Value::from(value);
        if let Some(legacy) = legacy.as_value() {
            *replacement.decor_mut() = legacy.decor().clone();
        }
        appearance.insert("text_rendering", Item::Value(replacement));
    }
}

fn new_document() -> DocumentMut {
    let mut document = DocumentMut::new();
    document.insert("version", toml_edit::value(i64::from(VERSION)));
    document
}

struct ConfigLock(File);

impl Drop for ConfigLock {
    fn drop(&mut self) {
        // Release the lock even if a concurrent fork inherited the descriptor
        // and has yet to close it on exec.
        _ = self.0.unlock();
    }
}

fn lock_config(path: &Path) -> Result<ConfigLock> {
    let parent = path.parent().context(t!("config.missing_directory"))?;
    fs::create_dir_all(parent)?;
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    lock.try_lock()
        .with_context(|| t!("config.locked", path = path.display()).into_owned())?;
    Ok(ConfigLock(lock))
}

fn patch_table(target: &mut dyn TableLike, old: &dyn TableLike, new: &dyn TableLike) {
    for (key, _) in old.iter() {
        if !new.contains_key(key) {
            target.remove(key);
        }
    }
    for (key, value) in new.iter() {
        let old_value = old.get(key);
        if let (Some(old), Some(new)) = (
            old_value.and_then(Item::as_table_like),
            value.as_table_like(),
        ) {
            let mut empty = Table::new();
            empty.set_implicit(true);
            if let Some(table) = target.get_mut(key).and_then(Item::as_table_like_mut) {
                patch_table(table, old, new);
            } else {
                patch_table(&mut empty, old, new);
                if !empty.is_empty() {
                    target.insert(key, Item::Table(empty));
                }
            }
        } else if old_value.is_none_or(|old| value.to_string() != old.to_string()) {
            let Some(mut replacement) = value.as_value().cloned() else {
                unreachable!("settings are scalar values or tables");
            };
            if let Some(existing) = target.get_mut(key) {
                if let Some(existing) = existing.as_value() {
                    *replacement.decor_mut() = existing.decor().clone();
                }
                *existing = Item::Value(replacement);
            } else {
                target.insert(key, Item::Value(replacement));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_association_rules_round_trip_and_match_basenames() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"[files.associations]
    "*.ux" = "html" # Keep the comment
    "*.test.ux" = "javascript"
    "a.test.ux" = "text"
    "nested/*.ux" = "css"
    "[bad" = "css"
    "#,
        )?;
        let mut config = ConfigFile::load(Some(path.clone()));
        assert!(config.load_error.is_none());
        let language = |name: &str| config.file_associations.language_for(Path::new(name));
        assert_eq!(language("nested/a.ux"), Some("html"));
        assert_eq!(language("nested/b.test.ux"), Some("javascript"));
        assert_eq!(language("nested/a.test.ux"), Some("text"));
        assert_eq!(language("nested/a.UX"), None);
        assert_eq!(
            config
                .file_associations
                .invalid_patterns()
                .collect::<Vec<_>>(),
            ["[bad", "nested/*.ux"]
        );

        let mut draft = config.current().clone();
        draft.files.associations.remove("*.test.ux");
        draft
            .files
            .associations
            .insert("*.foo".into(), "toml".into());
        config.save(draft.clone())?;
        assert!(fs::read_to_string(&path)?.contains("\"*.ux\" = \"html\" # Keep the comment"));
        assert_eq!(ConfigFile::load(Some(path)).current(), &draft);
        Ok(())
    }

    #[test]
    fn migrates_legacy_hinting_when_loading_and_saving() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        for (old, preference, new) in [
            ("auto", TextRenderingPreference::Auto, "auto"),
            ("on", TextRenderingPreference::PixelAligned, "pixel_aligned"),
            (
                "off",
                TextRenderingPreference::HighPrecision,
                "high_precision",
            ),
        ] {
            fs::write(
                &path,
                format!("[appearance]\nfont_hinting = \"{old}\" # preferred rendering\n"),
            )?;
            let mut file = ConfigFile::load(Some(path.clone()));
            assert!(file.load_error.is_none());
            let mut config = file.current().clone();
            assert_eq!(config.appearance.text_rendering, preference);
            config.appearance.smooth_scroll = false;
            file.save(config.clone())?;
            let saved = fs::read_to_string(&path)?;
            assert!(saved.contains(&format!("text_rendering = \"{new}\" # preferred rendering")));
            assert!(!saved.contains("font_hinting"));
            assert_eq!(decode(saved.as_bytes())?, config);
        }
        let both = b"[appearance]\nfont_hinting = 'on'\ntext_rendering = 'high_precision'";
        assert_eq!(
            decode(both)?.appearance.text_rendering,
            TextRenderingPreference::HighPrecision
        );
        Ok(())
    }

    #[test]
    fn code_theme_values_round_trip_through_the_config_file() -> Result<()> {
        for (theme, contrast) in CodeTheme::ALL
            .into_iter()
            .flat_map(|theme| CodeContrast::ALL.map(|contrast| (theme, contrast)))
        {
            let source = format!(
                "[appearance]\ncode_theme = \"{}\"\ncode_contrast = \"{}\"\n",
                theme.config_value(),
                contrast.config_value()
            );
            let config = decode(source.as_bytes())?;
            assert_eq!(config.appearance.code_theme, theme, "{source}");
            assert_eq!(config.appearance.code_contrast, contrast, "{source}");
        }
        assert_eq!(
            decode(b"[appearance]\ncode_theme = \"default\"\n")?
                .appearance
                .code_theme,
            CodeTheme::Prism,
        );
        assert!(decode(b"[appearance]\ncode_theme = \"unknown\"\n").is_err());
        Ok(())
    }

    #[test]
    fn edits_only_changed_values_and_round_trips_all_sections() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "# User config\nversion = 1\n\n[docs]\nbody_font_size = 15.5 # prose\ncode_block_font_size = 12 # code blocks\n\n[editor]\nfont_size = 15 # pixels\nextra = true\n\n[lsp]\nrust = { command = 'rust-analyzer' }\n\n[unknown]\nkey = ['value']\n",
        )?;
        let mut file = ConfigFile::load(Some(path.clone()));
        assert!(file.load_error.is_none(), "{:?}", file.load_error);
        let mut config = file.current().clone();
        assert_eq!(config.docs.body_font_size, 15.5);
        assert_eq!(config.docs.code_block_font_size, 12.);
        assert_eq!(config.editor.font_size, 15.);
        assert_eq!(config.editor.line_height, 1.5);
        assert_eq!(config.appearance.language, Language::System);
        assert!(!config.appearance.gradient_title_bar);
        assert!(config.appearance.ui_font_family.is_empty());
        assert_eq!(config.appearance.ui_font_size, 13.);
        assert!(config.appearance.fallback_font_family.is_empty());
        assert_eq!(
            config.appearance.text_rendering,
            TextRenderingPreference::Auto
        );
        config.appearance.theme = Theme::Dark;
        config.appearance.gradient_title_bar = true;
        config.appearance.code_theme = CodeTheme::Paper;
        config.appearance.code_contrast = CodeContrast::High;
        config.appearance.language = Language::Chinese;
        config.appearance.ui_font_family = "Inter".into();
        config.appearance.ui_font_size = 13.5;
        config.appearance.fallback_font_family = "Noto Sans CJK SC".into();
        config.appearance.text_rendering = TextRenderingPreference::HighPrecision;
        config.docs.body_font_size = 17.;
        config.docs.code_block_font_size = 14.;
        config.editor.font_size = 18.;
        config.editor.line_height = 2.2;
        assert!(!config.editor.inlay_hints);
        config.editor.inlay_hints = true;
        config.git.diff_layout = DiffLayout::Split;
        config.terminal.shell = if cfg!(windows) {
            r"C:\Windows\cmd.exe"
        } else {
            "/bin/sh"
        }
        .into();
        config.lsp.rust.command = "custom-rust-analyzer".into();
        config.lsp.python.command = "custom-pylsp --check-parent-process".into();
        config.lsp.typescript.command = "tsc --lsp --stdio".into();
        file.save(config.clone())?;

        let saved = fs::read_to_string(&path)?;
        for expected in [
            "# User config",
            "language = \"zh-CN\"",
            "gradient_title_bar = true",
            "code_theme = \"paper\"",
            "code_contrast = \"high\"",
            "ui_font_family = \"Inter\"",
            "ui_font_size = 13.5",
            "fallback_font_family = \"Noto Sans CJK SC\"",
            "text_rendering = \"high_precision\"",
            "body_font_size = 17.0 # prose",
            "code_block_font_size = 14.0 # code blocks",
            "18.0 # pixels",
            "line_height = 2.2",
            "inlay_hints = true",
            "extra = true",
            "tsc --lsp --stdio",
            "[unknown]",
            "key = ['value']",
        ] {
            assert!(saved.contains(expected), "{expected:?} absent from {saved}");
        }
        assert!(!saved.contains("line_numbers"));
        let loaded = ConfigFile::load(Some(path));
        assert!(
            loaded.load_error.is_none(),
            "{:?}\n{saved}",
            loaded.load_error
        );
        assert_eq!(loaded.current(), &config);
        Ok(())
    }

    #[test]
    fn ui_font_size_is_validated_with_its_own_bounds() {
        for (value, valid) in [
            ("12", true),
            ("14", true),
            ("11.5", false),
            ("14.5", false),
            ("nan", false),
        ] {
            let config = format!("[appearance]\nui_font_size = {value}\n");
            assert_eq!(decode(config.as_bytes()).is_ok(), valid, "{value}");
        }
    }

    #[test]
    fn editor_line_height_accepts_only_ratios_in_range() {
        for (value, valid) in [
            ("1.0", true),
            ("3.0", true),
            ("0.9", false),
            ("3.1", false),
            ("nan", false),
            ("inf", false),
            ("'20px'", false),
        ] {
            let config = format!("[editor]\nline_height = {value}\n");
            assert_eq!(decode(config.as_bytes()).is_ok(), valid, "{value}");
        }
    }

    #[test]
    fn malformed_files_and_external_edits_preserve_the_last_good_config() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        let mut file = ConfigFile::load(Some(path.clone()));
        assert_eq!(file.current(), &Config::default());
        assert!(!path.exists());
        let mut config = file.current().clone();
        config.appearance.smooth_scroll = false;
        file.save(config.clone())?;
        for invalid in [
            "version = 2",
            "[editor\n",
            "[editor]\nfont_size = nan",
            "[terminal]\nshell = './relative'",
            "[lsp.typescript]\ncommand = 'tsc \"--lsp --stdio'",
        ] {
            fs::write(&path, invalid)?;
            assert!(file.save(Config::default()).is_err());
            assert!(file.reload().is_err(), "{invalid}");
            assert_eq!(file.current(), &config);
            assert_eq!(fs::read_to_string(&path)?, invalid);
        }
        fs::remove_file(&path)?;
        file.reload()?;
        assert_eq!(file.current(), &Config::default());
        file.save(config.clone())?;
        assert_eq!(ConfigFile::load(Some(path)).current(), &config);
        Ok(())
    }

    #[test]
    fn second_instance_must_reload_before_saving() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        let mut first = ConfigFile::load(Some(path.clone()));
        let mut second = ConfigFile::load(Some(path.clone()));
        let mut config = Config::default();
        config.git.ignore_whitespace = true;
        first.save(config.clone())?;
        assert!(second.save(Config::default()).is_err());
        second.reload()?;
        config.editor.line_numbers = false;
        second.save(config.clone())?;
        assert_eq!(ConfigFile::load(Some(path)).current(), &config);
        Ok(())
    }
    #[cfg(windows)]
    #[test]
    fn windows_shell_accepts_executable_names_and_copied_paths() -> Result<()> {
        let mut config = Config::default();
        for (value, executable) in [
            ("pwsh.exe", "pwsh.exe"),
            ("powershell", "powershell"),
            (
                r#""D:\Program Files\PowerShell\pwsh.exe""#,
                r"D:\Program Files\PowerShell\pwsh.exe",
            ),
            ("", ""),
        ] {
            config.terminal.shell = value.into();
            config.validate()?;
            assert_eq!(config.terminal.executable(), executable);
        }
        for value in ["pwsh.exe -NoProfile", r".\pwsh.exe", r"D:pwsh.exe"] {
            config.terminal.shell = value.into();
            assert!(config.validate().is_err(), "{value}");
        }
        Ok(())
    }
}
