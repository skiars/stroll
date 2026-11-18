use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use super::{
    LanguageServer,
    external::{self, ExternalLsp},
};

pub(crate) const CONFIG_PATH: &str = ".stroll.json";
const CONFIG_LIMIT: u64 = 256 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct WorktreeLsp {
    entries: Vec<WorktreeServer>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct WorktreeServer {
    pub entry: ExternalLsp,
    pub fingerprint: String,
    pub roots: ProjectRoots,
    pub builtin: Option<LanguageServer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_snapshot: Option<PathBuf>,
    discover_typescript: bool,
    paths: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum ProjectRoots {
    Builtin(LanguageServer),
    Markers(Vec<String>),
}

impl ProjectRoots {
    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            Self::Builtin(_) => Ok(()),
            Self::Markers(markers) => external::validate_root_markers(markers),
        }
    }

    pub(crate) fn resolve(&self, root: &Path, file: &Path) -> PathBuf {
        match self {
            Self::Builtin(server) => server.project_root(root, file),
            Self::Markers(markers) => external::project_root(root, file, markers),
        }
    }
}

impl WorktreeLsp {
    pub(crate) fn load(
        root: &Path,
        default_command: impl Fn(LanguageServer) -> (Command, bool),
    ) -> Result<Self> {
        Self::read(root, default_command)
            .context(format!("Invalid {CONFIG_PATH} LSP configuration"))
    }

    fn read(
        root: &Path,
        default_command: impl Fn(LanguageServer) -> (Command, bool),
    ) -> Result<Self> {
        let path = root.join(CONFIG_PATH);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            metadata.is_file(),
            "Expected a regular worktree configuration file"
        );
        let mut content = String::new();
        File::open(&path)?
            .take(CONFIG_LIMIT + 1)
            .read_to_string(&mut content)?;
        ensure!(
            content.len() as u64 <= CONFIG_LIMIT,
            "Worktree configuration exceeds size limit"
        );
        let config: WorktreeFile = jsonc_parser::parse_to_serde_value(
            content.trim_start_matches('\u{feff}'),
            &Default::default(),
        )?;
        if config.lsp.is_empty() {
            return Ok(Self::default());
        }

        let user = external::load()?;
        let mut ids = HashSet::new();
        let mut entries = Vec::new();
        for overrides in config.lsp {
            let id = overrides
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            ensure!(!id.trim().is_empty(), "Expected a language server ID");
            ensure!(
                ids.insert(id.to_owned()),
                "Duplicate language server ID {id:?}"
            );
            entries.push(
                WorktreeServer::merge(id, &overrides, &user, &default_command)
                    .with_context(|| format!("Invalid language server {id:?}"))?,
            );
        }
        Ok(Self { entries })
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn entry(&self, id: &str) -> Option<&WorktreeServer> {
        self.entries.iter().find(|server| server.entry.id == id)
    }

    pub(crate) fn for_remote(mut self, snapshot: &Path) -> Self {
        for server in &mut self.entries {
            // The client needs routing and LSP options, but the helper owns the launch.
            server.entry.server.env.clear();
            server.entry.server.args.clear();
            server.remote_snapshot = Some(snapshot.to_owned());
        }
        self
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn select(&self, file: &Path) -> Option<(&WorktreeServer, &str)> {
        let path = file.to_str()?.replace(std::path::MAIN_SEPARATOR, "/");
        let mut best = None;
        for server in &self.entries {
            if !server.entry.disabled
                && let Some((language, length)) = server.entry.language_id(file)
                && (server.paths.is_empty()
                    || server.paths.iter().any(|pattern| {
                        path_glob(pattern).is_ok_and(|glob| glob.compile_matcher().is_match(&path))
                    }))
            {
                let rank = (!server.paths.is_empty(), length);
                if best
                    .as_ref()
                    .is_none_or(|(_, _, previous)| rank > *previous)
                {
                    best = Some((server, language, rank));
                }
            }
        }
        best.map(|(server, language, _)| (server, language))
    }
}

#[cfg(feature = "desktop")]
pub(crate) fn has_entries(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| {
            jsonc_parser::parse_to_serde_value::<WorktreeFile>(
                text.trim_start_matches('\u{feff}'),
                &Default::default(),
            )
            .ok()
        })
        .is_some_and(|config| !config.lsp.is_empty())
}

#[derive(Deserialize)]
struct WorktreeFile {
    #[serde(default)]
    lsp: Vec<Map<String, Value>>,
}

impl WorktreeServer {
    fn merge(
        id: &str,
        overrides: &Map<String, Value>,
        user: &[ExternalLsp],
        default_command: &impl Fn(LanguageServer) -> (Command, bool),
    ) -> Result<Self> {
        let builtin = LanguageServer::ALL
            .into_iter()
            .find(|server| server.name() == id);
        let user = user.iter().find(|entry| entry.id == id);
        let mut discover_typescript = false;
        let mut merged = if let Some(user) = user {
            serde_json::to_value(user)?
        } else if let Some(builtin) = builtin {
            let (command, use_default) = default_command(builtin);
            discover_typescript = use_default && builtin == LanguageServer::TypeScript;
            let languages: Map<_, _> = builtin
                .languages()
                .iter()
                .map(|(suffix, language)| (suffix.to_string(), json!(language)))
                .collect();
            json!({
                "id": id,
                "languages": languages,
                "server": {
                    "command": command.get_program().to_string_lossy(),
                    "args": command.get_args().map(OsStr::to_string_lossy).collect::<Vec<_>>(),
                    "initializationOptions": builtin.initialization_options()
                }
            })
        } else {
            json!({ "id": id, "languages": {}, "server": { "command": "" } })
        };

        let merged = merged.as_object_mut().expect("language server object");
        for (key, value) in overrides {
            if key == "server" {
                let fields = value.as_object().context("Expected a server object")?;
                discover_typescript &= !fields.contains_key("command");
                merged
                    .get_mut(key)
                    .and_then(Value::as_object_mut)
                    .expect("server command object")
                    .extend(fields.clone());
            } else {
                merged.insert(key.clone(), value.clone());
            }
        }
        let entry: ExternalLsp = serde_json::from_value(Value::Object(merged.clone()))?;
        ensure!(
            entry.disabled || !entry.languages.is_empty(),
            "Expected at least one language suffix"
        );
        if !entry.disabled {
            entry.validate()?;
            ensure!(
                entry.server.args.iter().all(|arg| !arg.contains('\0'))
                    && entry.server.env.iter().all(|(key, value)| {
                        !key.is_empty() && !key.contains(['=', '\0']) && !value.contains('\0')
                    }),
                "Invalid arguments or environment"
            );
        }
        let paths: Vec<String> = merged
            .remove("paths")
            .map(serde_json::from_value)
            .transpose()?
            .unwrap_or_default();
        for path in &paths {
            path_glob(path)?;
        }
        let roots = match builtin {
            Some(builtin) if user.is_none() && !overrides.contains_key("rootMarkers") => {
                ProjectRoots::Builtin(builtin)
            }
            _ => ProjectRoots::Markers(entry.root_markers.clone()),
        };
        let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&(
            &entry.server,
            discover_typescript,
        ))?));
        Ok(Self {
            entry,
            fingerprint,
            roots,
            builtin,
            remote_snapshot: None,
            discover_typescript,
            paths,
        })
    }

    pub(crate) fn enabled(&self) -> bool {
        !self.entry.disabled
    }

    pub(crate) fn command(
        &self,
        project: &Path,
        environment: Option<&HashMap<OsString, OsString>>,
    ) -> Result<Command> {
        let mut command = if self.discover_typescript {
            let resolved =
                LanguageServer::TypeScript.resolve_default_command(project, environment)?;
            let mut command = crate::process::background(resolved.get_program());
            command.args(&self.entry.server.args);
            command
        } else {
            self.entry.command()
        };
        command.current_dir(project);
        if let Some(environment) = environment {
            command.env_clear().envs(environment);
        }
        command.envs(&self.entry.server.env);
        Ok(command)
    }
}

fn path_glob(pattern: &str) -> Result<globset::Glob> {
    ensure!(
        !pattern.contains(['\\', '\0'])
            && pattern
                .split('/')
                .all(|part| !matches!(part, "" | "." | ".." | ".git")),
        "Expected a worktree-relative path pattern"
    );
    Ok(globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()?)
}
