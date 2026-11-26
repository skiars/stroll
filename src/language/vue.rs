use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub(super) fn bridge_args(command: &mut Command, tools: &Path) {
    command
        .args([
            "--input-type=commonjs",
            "-e",
            include_str!("vue_bridge.cjs"),
            "--",
        ])
        .arg(tools);
}

/// Keep TypeScript and Vue documents in the same session within a Vue package.
pub(super) fn project_root(repository: &Path, file: &Path) -> Option<PathBuf> {
    let file = repository.join(file);
    for directory in file
        .parent()?
        .ancestors()
        .take_while(|dir| dir.starts_with(repository))
    {
        if let Ok(manifest) = std::fs::read(directory.join("package.json"))
            && let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&manifest)
            && [
                "dependencies",
                "devDependencies",
                "peerDependencies",
                "optionalDependencies",
            ]
            .iter()
            .any(|key| {
                ["vue", "nuxt", "@nuxt/bridge"]
                    .iter()
                    .any(|name| manifest[key][name].is_string())
            })
        {
            return Some(directory.to_owned());
        }
    }
    None
}
