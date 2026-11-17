pub(crate) mod external;

#[cfg(feature = "desktop")]
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
        }
    }

    #[cfg(feature = "desktop")]
    pub fn for_path(path: &Path) -> Option<(Self, &'static str)> {
        Some(match path.extension()?.to_str()? {
            "rs" => (Self::Rust, "rust"),
            "c" => (Self::Clangd, "c"),
            "C" | "cc" | "cpp" | "cxx" | "h" | "hh" | "hpp" | "hxx" => (Self::Clangd, "cpp"),
            "m" => (Self::Clangd, "objective-c"),
            "mm" => (Self::Clangd, "objective-cpp"),
            "ts" | "mts" | "cts" => (Self::TypeScript, "typescript"),
            "tsx" => (Self::TypeScript, "typescriptreact"),
            "js" | "mjs" | "cjs" => (Self::TypeScript, "javascript"),
            "jsx" => (Self::TypeScript, "javascriptreact"),
            "py" | "pyi" => (Self::Python, "python"),
            _ => return None,
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
            let mut command = Command::new(program);
            command.args(args);
            return Some((command, CommandSource::Config));
        }
        Some((self.default_command(), CommandSource::Path))
    }

    pub(crate) fn executable_variable(self) -> &'static str {
        match self {
            Self::Rust => "STROLL_RUST_ANALYZER",
            Self::Clangd => "STROLL_CLANGD",
            Self::TypeScript => "STROLL_TYPESCRIPT_LSP",
            Self::Python => "STROLL_PYTHON_LSP",
        }
    }

    pub(crate) fn command_for(self, executable: OsString) -> Command {
        let mut command = Command::new(executable);
        if self == Self::TypeScript {
            command.arg("--stdio");
        }
        command
    }

    fn default_command(self) -> Command {
        match self {
            Self::TypeScript => typescript_command(if cfg!(windows) { "tsc.cmd" } else { "tsc" }),
            _ => Command::new(self.name()),
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
            typescript_version(prepare(Command::new(program)))
        })?;
        Ok(prepare(typescript_command(executable)))
    }

    pub fn project_root(self, repository: &Path, file: &Path) -> PathBuf {
        let markers: &[_] = match self {
            Self::Rust => return repository.to_owned(),
            Self::Clangd => &["compile_commands.json", "compile_flags.txt", ".clangd"],
            Self::TypeScript => &["tsconfig.json", "jsconfig.json", "package.json"],
            Self::Python => &["pyproject.toml", "setup.py"],
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

    #[cfg(feature = "desktop")]
    pub fn initialization_options(self) -> Value {
        match self {
            Self::Rust => json!({
                "checkOnSave": false,
                "cargo": { "buildScripts": { "enable": false } },
                "procMacro": { "enable": false }
            }),
            Self::TypeScript => json!({ "disableAutomaticTypingAcquisition": true }),
            _ => Value::Null,
        }
    }
}

fn typescript_command(executable: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(executable);
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
