pub(crate) mod external;
#[cfg(feature = "desktop")]
pub(crate) mod resources;
mod vue;
pub(crate) mod worktree;

use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    io::{Read, Seek},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};

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
        }
    }

    pub(crate) const ALL: [Self; 6] = [
        Self::Rust,
        Self::Clangd,
        Self::TypeScript,
        Self::Python,
        Self::Vue,
        Self::Html,
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
        let variable = self.executable_variable();
        if let Some(value) = std::env::var_os(variable) {
            return Some((
                self.command_for(value),
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
        if self == Self::Vue {
            return self.name().to_owned();
        }
        let (command, _) = self
            .local_command(configured)
            .expect("startup LSP configuration was validated");
        command.get_program().to_string_lossy().into_owned()
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn supports_legacy_remote(self) -> bool {
        !matches!(self, Self::Vue | Self::Html)
    }

    pub(crate) fn executable_variable(self) -> &'static str {
        match self {
            Self::Rust => "STROLL_RUST_ANALYZER",
            Self::Clangd => "STROLL_CLANGD",
            Self::TypeScript => "STROLL_TYPESCRIPT_LSP",
            Self::Python => "STROLL_PYTHON_LSP",
            Self::Vue => "STROLL_VUE_NODE",
            Self::Html => "STROLL_HTML_LSP",
        }
    }

    pub(crate) fn command_for(self, executable: OsString) -> Command {
        let mut command = crate::process::background(executable);
        match self {
            Self::TypeScript | Self::Html => {
                command.arg("--stdio");
            }
            Self::Vue => vue::bridge_args(&mut command),
            _ => {}
        }
        command
    }

    pub(crate) fn default_command(self) -> Command {
        match self {
            Self::TypeScript => typescript_command(if cfg!(windows) { "tsc.cmd" } else { "tsc" }),
            Self::Vue => self.command_for("node".into()),
            Self::Html if cfg!(windows) => {
                self.command_for("vscode-html-language-server.cmd".into())
            }
            Self::Html => self.command_for(self.name().into()),
            _ => crate::process::background(self.name()),
        }
    }

    pub(crate) fn resolve_default_command(
        self,
        project: &Path,
        environment: Option<&HashMap<OsString, OsString>>,
    ) -> Result<Command> {
        let prepare = |mut command: Command| {
            command.current_dir(project);
            if let Some(environment) = environment {
                command.env_clear().envs(environment);
            }
            command
        };
        if self != Self::TypeScript {
            return Ok(prepare(self.default_command()));
        }
        let path = match environment {
            Some(environment) => environment.get(OsStr::new("PATH")).cloned(),
            None => std::env::var_os("PATH"),
        };
        let executable = find_typescript(path.as_deref(), project, |program| {
            typescript_version(prepare(crate::process::background(program)))
        })?;
        Ok(prepare(typescript_command(executable)))
    }

    pub fn project_root(self, repository: &Path, file: &Path) -> PathBuf {
        if self == Self::Vue {
            return vue::project_root(repository, file).unwrap_or_else(|| repository.to_owned());
        }
        let markers: &[_] = match self {
            Self::Rust | Self::Html => return repository.to_owned(),
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
            Self::Html => json!({ "provideFormatter": true }),
            _ => Value::Null,
        }
    }
}

fn typescript_command(executable: impl AsRef<OsStr>) -> Command {
    let mut command = crate::process::background(executable);
    command.args(["--lsp", "--stdio"]);
    command
}

fn find_typescript(
    path: Option<&OsStr>,
    project: &Path,
    version_of: impl Fn(&Path) -> Result<(u32, String)>,
) -> Result<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["tsc.cmd", "tsc.exe"]
    } else {
        &["tsc"]
    };
    let mut rejected = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for directory in path.into_iter().flat_map(std::env::split_paths) {
        for name in names {
            let candidate = project.join(&directory).join(name);
            if !candidate.is_file() || !seen.insert(candidate.clone()) {
                continue;
            }
            let reason = match version_of(&candidate) {
                Ok((major, _)) if major >= 7 => return Ok(candidate),
                Ok((_, version)) => format!("TypeScript {version}"),
                Err(error) => format!("{error:#}"),
            };
            rejected.push(format!("{}: {reason}", candidate.display()));
        }
    }
    let reason = if rejected.is_empty() {
        "No tsc executable was found on PATH.".to_owned()
    } else {
        rejected.join("\n")
    };
    bail!("TypeScript 7 or newer is required for tsc --lsp --stdio.\n{reason}")
}

fn typescript_version(mut command: Command) -> Result<(u32, String)> {
    let mut output = tempfile::tempfile()?;
    let mut child = command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(Stdio::null())
        .spawn()
        .context("Could not run tsc --version")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                result.context("Could not wait for tsc --version")?;
                bail!("tsc --version timed out");
            }
        }
    };
    ensure!(status.success(), "tsc --version failed ({status})");
    output.rewind()?;
    let mut version = String::new();
    output.take(4096).read_to_string(&mut version)?;
    let version = version
        .lines()
        .find_map(|line| line.trim().strip_prefix("Version "))
        .context("Unrecognized tsc --version output")?;
    let (major, _) = version
        .split_once('.')
        .context("Unrecognized tsc version")?;
    Ok((
        major.parse().context("Unrecognized tsc version")?,
        version.to_owned(),
    ))
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
