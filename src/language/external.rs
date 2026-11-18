use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
    path::{Component, Path, PathBuf},
    process::Command,
};

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExternalLsp {
    pub id: String,
    pub languages: BTreeMap<String, String>,
    #[serde(default)]
    pub(super) root_markers: Vec<String>,
    pub server: ExternalCommand,
    #[serde(default)]
    pub(super) disabled: bool,
}

impl Hash for ExternalLsp {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExternalCommand {
    pub command: String,
    #[serde(default)]
    pub(super) args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub initialization_options: Value,
    #[serde(default)]
    pub workspace_settings: Option<Value>,
}

#[cfg(feature = "desktop")]
impl ExternalCommand {
    pub(crate) fn lsp_options(&self) -> crate::lsp::ServerOptions {
        crate::lsp::ServerOptions {
            initialization: self.initialization_options.clone(),
            workspace: self.workspace_settings.clone(),
        }
    }
}

pub(crate) fn load() -> Result<Vec<ExternalLsp>> {
    let path = std::env::var_os("STROLL_LSP_CONFIG")
        .map(PathBuf::from)
        .or_else(|| super::language_home().map(|root| root.join("lsp.json")));
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let content = match std::fs::read(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("Could not read {path:?}")),
    };
    let entries: Vec<ExternalLsp> = serde_json::from_slice(&content)
        .with_context(|| format!("Invalid language server configuration {path:?}"))?;
    let mut ids = std::collections::HashSet::new();
    for entry in &entries {
        ensure!(
            ids.insert(&entry.id),
            "Duplicate language server ID {:?}",
            entry.id
        );
        entry
            .validate()
            .with_context(|| format!("Invalid language server {:?}", entry.id))?;
    }
    Ok(entries
        .into_iter()
        .filter(|entry| !entry.disabled)
        .collect())
}

impl ExternalLsp {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(!self.id.trim().is_empty(), "Expected a language server ID");
        ensure!(
            self.server
                .workspace_settings
                .as_ref()
                .is_none_or(Value::is_object),
            "Expected a language server workspace settings object"
        );
        ensure!(
            self.languages
                .iter()
                .all(|(suffix, id)| suffix.starts_with('.')
                    && !suffix.contains(['/', '\\'])
                    && !id.is_empty()),
            "Invalid language mapping"
        );
        let executable = Path::new(&self.server.command);
        ensure!(
            !self.server.command.trim().is_empty()
                && !self.server.command.contains('\0')
                && (executable.is_absolute()
                    || executable.file_name() == Some(executable.as_os_str())),
            "Expected an executable name or absolute path"
        );
        validate_root_markers(&self.root_markers)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn language_id(&self, file: &Path) -> Option<(&str, usize)> {
        let name = file.file_name()?.to_str()?;
        self.languages
            .iter()
            .filter(|(suffix, _)| name.ends_with(suffix.as_str()))
            .max_by_key(|(suffix, _)| suffix.len())
            .map(|(suffix, id)| (id.as_str(), suffix.len()))
    }

    pub(crate) fn project_root(&self, root: &Path, file: &Path) -> PathBuf {
        project_root(root, file, &self.root_markers)
    }

    pub(crate) fn command(&self) -> Command {
        let mut command = crate::process::background(&self.server.command);
        command.args(&self.server.args).envs(&self.server.env);
        command
    }
}

pub(super) fn validate_root_markers(markers: &[String]) -> Result<()> {
    ensure!(
        markers.iter().all(|marker| {
            let path = Path::new(marker);
            !marker.is_empty()
                && path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
        }),
        "Root markers must be relative paths"
    );
    Ok(())
}

pub(super) fn project_root(root: &Path, file: &Path, markers: &[String]) -> PathBuf {
    let paths: Vec<_> = markers
        .iter()
        .map(|marker| {
            let glob = if marker.contains('*') && !marker.contains(['/', '\\']) {
                globset::Glob::new(marker)
                    .ok()
                    .map(|glob| glob.compile_matcher())
            } else {
                None
            };
            (marker, glob)
        })
        .collect();
    let full = root.join(file);
    full.ancestors()
        .skip(1)
        .take_while(|dir| *dir != root)
        .find(|dir| {
            paths.iter().any(|(marker, glob)| {
                dir.join(marker).exists()
                    || glob.as_ref().is_some_and(|glob| {
                        std::fs::read_dir(dir).is_ok_and(|entries| {
                            entries
                                .flatten()
                                .any(|entry| glob.is_match(entry.file_name()))
                        })
                    })
            })
        })
        .unwrap_or(root)
        .to_owned()
}

#[cfg(all(test, feature = "desktop"))]
mod tests {
    use super::*;

    #[test]
    fn external_lsp_config_selects_language_and_nearest_root() -> Result<()> {
        let config: ExternalLsp = serde_json::from_str(
            r#"{
          "id": "example",
          "languages": {".example": "example", ".test.example": "example-test"},
          "rootMarkers": ["*.project"],
          "server": {
            "command": "example-lsp", "args": ["--stdio"],
            "env": {"EXAMPLE_TEST": "1"},
            "initializationOptions": {"entries": ["main.example"]}
          }
        }"#,
        )?;
        config.validate()?;
        let repo = tempfile::tempdir()?;
        let project = repo.path().join("project");
        std::fs::create_dir_all(project.join("src"))?;
        std::fs::write(project.join("example.project"), "")?;
        let relative = Path::new("project/src/main.test.example");
        assert_eq!(
            config.language_id(relative),
            Some(("example-test", ".test.example".len()))
        );
        assert_eq!(config.project_root(repo.path(), relative), project);
        assert_eq!(config.command().get_args().collect::<Vec<_>>(), ["--stdio"]);
        assert_eq!(
            config.server.initialization_options["entries"][0],
            "main.example"
        );
        Ok(())
    }
}
