pub(crate) mod external;
#[cfg(feature = "desktop")]
pub(crate) mod resources;
pub(crate) mod tools;
mod vue;
pub(crate) mod worktree;

use serde_json::{Value, json};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) fn language_home() -> Option<PathBuf> {
    std::env::var_os("STROLL_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".stroll")))
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LanguageServer {
    Rust,
    Clangd,
    TypeScript,
    Python,
    Vue,
    Html,
    Json,
    Tombi,
    Yaml,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct BuiltinProject {
    pub(crate) server: LanguageServer,
    pub(crate) root: PathBuf,
    pub(crate) language_id: String,
}

impl BuiltinProject {
    pub(crate) fn resolve(
        repository: &Path,
        file: &Path,
        excluded: &[LanguageServer],
    ) -> Option<Self> {
        let (server, language_id) = LanguageServer::for_path(file)?;
        if server == LanguageServer::TypeScript
            && !excluded.contains(&LanguageServer::Vue)
            && let Some(root) = vue::project_root(repository, file)
        {
            return Some(Self {
                server: LanguageServer::Vue,
                root,
                language_id: language_id.into(),
            });
        }
        (!excluded.contains(&server)).then(|| Self {
            server,
            root: server.project_root(repository, file),
            language_id: language_id.into(),
        })
    }
}

#[cfg(feature = "desktop")]
pub(crate) enum CommandSource {
    Environment(&'static str),
    Config,
    Path,
}

impl LanguageServer {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust-analyzer",
            Self::Clangd => "clangd",
            Self::TypeScript => "tsc",
            Self::Python => "pylsp",
            Self::Vue => "vue-language-server",
            Self::Html => "vscode-html-language-server",
            Self::Json => "vscode-json-language-server",
            Self::Tombi => "tombi",
            Self::Yaml => "yaml-language-server",
        }
    }

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Yaml => "yaml",
            _ => self.name(),
        }
    }

    pub(crate) const ALL: [Self; 9] = [
        Self::Rust,
        Self::Clangd,
        Self::TypeScript,
        Self::Python,
        Self::Vue,
        Self::Html,
        Self::Json,
        Self::Tombi,
        Self::Yaml,
    ];

    pub(crate) fn languages(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Rust => &[(".rs", "rust")],
            Self::Clangd => &[
                (".c", "c"),
                (".C", "cpp"),
                (".cc", "cpp"),
                (".cpp", "cpp"),
                (".cxx", "cpp"),
                (".h", "cpp"),
                (".hh", "cpp"),
                (".hpp", "cpp"),
                (".hxx", "cpp"),
                (".m", "objective-c"),
                (".mm", "objective-cpp"),
            ],
            Self::TypeScript => &[
                (".ts", "typescript"),
                (".mts", "typescript"),
                (".cts", "typescript"),
                (".tsx", "typescriptreact"),
                (".js", "javascript"),
                (".mjs", "javascript"),
                (".cjs", "javascript"),
                (".jsx", "javascriptreact"),
            ],
            Self::Python => &[(".py", "python"), (".pyi", "python")],
            Self::Vue => &[(".vue", "vue")],
            Self::Html => &[(".html", "html"), (".htm", "html")],
            Self::Json => &[(".json", "json"), (".jsonc", "jsonc")],
            Self::Tombi => &[(".toml", "toml")],
            Self::Yaml => &[(".yaml", "yaml"), (".yml", "yaml")],
        }
    }

    pub fn for_path(path: &Path) -> Option<(Self, &'static str)> {
        let extension = path.extension()?.to_str()?;
        Self::ALL.into_iter().find_map(|server| {
            server.languages().iter().find_map(|(suffix, id)| {
                (suffix.strip_prefix('.') == Some(extension)).then_some((server, *id))
            })
        })
    }

    #[cfg(feature = "desktop")]
    pub fn command(self) -> Command {
        self.local_command("").expect("valid default LSP command").0
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn local_command(self, configured: &str) -> Option<(Command, CommandSource)> {
        if let Some(variable) = self.executable_variable()
            && let Some(value) = std::env::var_os(variable)
        {
            return Some((
                self.environment_command(value),
                CommandSource::Environment(variable),
            ));
        }
        if !configured.is_empty() {
            let words = parse_command_line(configured)?;
            let (program, args) = words.split_first()?;
            let mut command = crate::process::background(program);
            command.args(args);
            return Some((command, CommandSource::Config));
        }
        Some((self.default_command(), CommandSource::Path))
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn local_display_name(self, configured: &str) -> String {
        let (command, _) = self
            .local_command(configured)
            .expect("startup LSP configuration was validated");
        command.get_program().to_string_lossy().into_owned()
    }

    pub(crate) fn executable_variable(self) -> Option<&'static str> {
        Some(match self {
            Self::Rust => "STROLL_RUST_ANALYZER",
            Self::Clangd => "STROLL_CLANGD",
            Self::TypeScript => "STROLL_TYPESCRIPT_LSP",
            Self::Python => "STROLL_PYTHON_LSP",
            Self::Vue => return None,
            Self::Html => "STROLL_HTML_LSP",
            Self::Json => "STROLL_JSON_LSP",
            Self::Tombi => "STROLL_TOMBI_LSP",
            Self::Yaml => "STROLL_YAML_LSP",
        })
    }

    pub(crate) fn command_for(self, executable: OsString) -> Command {
        let mut command = crate::process::background(executable);
        command.args(self.default_arguments());
        command
    }

    pub(crate) fn environment_command(self, executable: OsString) -> Command {
        if self == Self::TypeScript {
            let mut command = crate::process::background(executable);
            command.arg("--stdio");
            command
        } else {
            self.command_for(executable)
        }
    }

    pub(crate) fn default_command(self) -> Command {
        match self {
            Self::Html | Self::Json | Self::Yaml if cfg!(windows) => {
                self.command_for(format!("{}.cmd", self.name()).into())
            }
            _ => self.command_for(self.name().into()),
        }
    }

    pub(crate) fn default_arguments(self) -> &'static [&'static str] {
        match self {
            Self::TypeScript => &["--lsp", "--stdio"],
            Self::Html | Self::Json | Self::Yaml | Self::Vue => &["--stdio"],
            Self::Tombi => &["lsp"],
            _ => &[],
        }
    }

    pub(crate) fn workspace_settings(self) -> Option<Value> {
        (self == Self::Yaml).then(|| json!({"yaml": {"format": {"enable": true}}}))
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn lsp_options(self) -> crate::lsp::ServerOptions {
        crate::lsp::ServerOptions {
            initialization: self.initialization_options(),
            workspace: self.workspace_settings(),
            dynamic_formatting: true,
        }
    }

    pub fn project_root(self, repository: &Path, file: &Path) -> PathBuf {
        if self == Self::Vue {
            return vue::project_root(repository, file).unwrap_or_else(|| repository.to_owned());
        }
        let markers: &[_] = match self {
            Self::Rust | Self::Html | Self::Json | Self::Tombi | Self::Yaml => {
                return repository.to_owned();
            }
            Self::Clangd => &["compile_commands.json", "compile_flags.txt", ".clangd"],
            Self::TypeScript => &["tsconfig.json", "jsconfig.json", "package.json"],
            Self::Python => &["pyproject.toml", "setup.py"],
            Self::Vue => unreachable!(),
        };
        // Let rust-analyzer discover the whole Cargo workspace. For other servers,
        // prefer the nearest project configuration, staying inside the repository.
        for directory in repository
            .join(file)
            .ancestors()
            .skip(1)
            .take_while(|path| *path != repository)
        {
            if markers
                .iter()
                .any(|marker| directory.join(marker).is_file())
            {
                return directory.to_owned();
            }
        }
        repository.to_owned()
    }

    pub fn initialization_options(self) -> Value {
        match self {
            Self::Rust => json!({
                "checkOnSave": false,
                "cargo": { "buildScripts": { "enable": false } },
                "procMacro": { "enable": false }
            }),
            Self::TypeScript => json!({ "disableAutomaticTypingAcquisition": true }),
            Self::Html | Self::Json => json!({ "provideFormatter": true }),
            _ => Value::Null,
        }
    }
}

#[cfg(feature = "desktop")]
pub(crate) fn parse_command_line(line: &str) -> Option<Vec<String>> {
    if line.contains(char::is_control) {
        return None;
    }
    let words = shlex::split(line)?;
    let program = words.first().filter(|program| !program.trim().is_empty())?;
    let program = Path::new(program);
    (program.is_absolute() || program.file_name() == Some(program.as_os_str())).then_some(words)
}

#[cfg(all(test, feature = "desktop"))]
mod tests {
    use super::*;
    use anyhow::Result;

    #[test]
    fn builtin_project_routes_files_with_their_package_and_respects_overrides() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let repository = fixture.path();
        let app = repository.join("packages/app");
        let plain = repository.join("packages/plain");
        std::fs::create_dir_all(app.join("src"))?;
        std::fs::create_dir_all(&plain)?;
        std::fs::write(
            app.join("package.json"),
            r#"{"devDependencies":{"nuxt":"^4"}}"#,
        )?;
        std::fs::write(plain.join("tsconfig.json"), "{}")?;

        let resolve = |path: &str, excluded: &[LanguageServer]| {
            BuiltinProject::resolve(repository, Path::new(path), excluded)
        };
        for (file, language_id) in [
            ("packages/app/src/App.vue", "vue"),
            ("packages/app/src/main.ts", "typescript"),
            ("packages/app/src/View.tsx", "typescriptreact"),
            ("packages/app/src/init.js", "javascript"),
        ] {
            assert_eq!(
                resolve(file, &[]),
                Some(BuiltinProject {
                    server: LanguageServer::Vue,
                    root: app.clone(),
                    language_id: language_id.into(),
                })
            );
        }
        for (file, root, excluded) in [
            ("packages/plain/main.ts", plain, &[][..]),
            ("packages/app/src/main.ts", app, &[LanguageServer::Vue]),
        ] {
            assert_eq!(
                resolve(file, excluded),
                Some(BuiltinProject {
                    server: LanguageServer::TypeScript,
                    root,
                    language_id: "typescript".into(),
                })
            );
        }
        assert!(resolve("packages/app/src/App.vue", &[LanguageServer::Vue]).is_none());
        assert!(resolve("packages/plain/main.ts", &[LanguageServer::TypeScript]).is_none());
        Ok(())
    }

    #[test]
    fn html_is_a_builtin_stdio_server_for_html_documents() {
        let root = Path::new("/worktree");
        let server = LanguageServer::Html;
        for file in ["public/index.html", "public/about.htm"] {
            let path = Path::new(file);
            let project = BuiltinProject::resolve(root, path, &[]).unwrap();
            assert_eq!(project.server, server);
            assert_eq!(project.language_id, "html");
            assert_eq!(project.root, root);
            assert!(BuiltinProject::resolve(root, path, &[server]).is_none());
        }
        assert!(LanguageServer::for_path(Path::new("page.xhtml")).is_none());
        let command = server.default_command();
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["--stdio"]);
        assert_eq!(server.initialization_options()["provideFormatter"], true);
    }

    #[test]
    fn parses_lsp_program_and_arguments() {
        let executable = if cfg!(windows) {
            r"C:\Program Files\Node\tsc.cmd"
        } else {
            "/opt/Node Tools/tsc"
        };
        let command = format!("'{executable}' --lsp --stdio --log 'name with spaces'");
        assert_eq!(
            parse_command_line(&command).unwrap(),
            [executable, "--lsp", "--stdio", "--log", "name with spaces"]
        );
        assert_eq!(
            parse_command_line("tsc --lsp --stdio").unwrap(),
            ["tsc", "--lsp", "--stdio"]
        );
        for invalid in [
            "'' --stdio",
            "'  ' --stdio",
            "tsc \"--stdio",
            "tsc\n--stdio",
            "",
        ] {
            assert!(parse_command_line(invalid).is_none(), "{invalid:?}");
        }
    }
}
