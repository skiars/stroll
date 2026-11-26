//! Installation and launch of pinned language-server tools on the project host.

use std::{
    collections::HashMap,
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::LazyLock,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};

use super::LanguageServer;

const CATALOGUE: &str = include_str!("tools/catalogue.json");

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Bundle {
    TypeScript,
    Vscode,
    Tombi,
    Yaml,
    Vue,
    Node,
}

#[derive(Deserialize)]
struct Recipe {
    version: String,
    #[serde(default)]
    compatible: Vec<String>,
    #[serde(default)]
    archives: Vec<Archive>,
    #[serde(default)]
    platforms: HashMap<String, Vec<Archive>>,
}

#[derive(Deserialize)]
struct Archive {
    url: String,
    hash: String,
    #[serde(default)]
    directory: PathBuf,
    #[serde(default)]
    files: HashMap<PathBuf, PathBuf>,
}

static RECIPES: LazyLock<HashMap<Bundle, Recipe>> =
    LazyLock::new(|| serde_json::from_str(CATALOGUE).expect("valid language tool catalogue"));

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Installed {
    pub(crate) bundle: Bundle,
    pub(crate) version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Record {
    bundle: Bundle,
    version: String,
    platform: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Progress {
    pub(crate) version: String,
    pub(crate) completed: usize,
    pub(crate) total: usize,
    pub(crate) bundle: Bundle,
    pub(crate) stage: InstallStage,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum InstallStage {
    Waiting,
    Downloading {
        archive: String,
        received: u64,
        size: Option<u64>,
    },
    Verifying(String),
    Extracting(String),
    Publishing,
}

impl Bundle {
    #[cfg(feature = "desktop")]
    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::TypeScript => "TypeScript",
            Self::Vscode => "VS Code JSON / HTML",
            Self::Tombi => "Tombi",
            Self::Yaml => "YAML Language Server",
            Self::Vue => "Vue Language Server",
            Self::Node => "Node.js",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::TypeScript => "typescript",
            Self::Vscode => "vscode",
            Self::Tombi => "tombi",
            Self::Yaml => "yaml",
            Self::Vue => "vue",
            Self::Node => "node",
        }
    }

    fn recipe(self) -> &'static Recipe {
        &RECIPES[&self]
    }

    pub(crate) fn version(self) -> &'static str {
        &self.recipe().version
    }

    fn minimum_node(self) -> Option<(u32, u32, u32)> {
        match self {
            Self::Vue => Some((20, 0, 0)),
            Self::Vscode => Some((22, 0, 0)),
            Self::Yaml => Some((22, 13, 0)),
            _ => None,
        }
    }

    fn archives(self, platform: &str) -> Result<&'static [Archive]> {
        let recipe = self.recipe();
        if recipe.platforms.is_empty() {
            Ok(&recipe.archives)
        } else {
            recipe
                .platforms
                .get(platform)
                .map(Vec::as_slice)
                .with_context(|| {
                    format!(
                        "No {} language tool bundle is available for {platform}",
                        self.id()
                    )
                })
        }
    }

    fn native_executable(self) -> Option<&'static str> {
        match self {
            Self::TypeScript => Some(executable("lib/tsc", "lib/tsc.exe")),
            Self::Tombi => Some(executable("tombi", "tombi.exe")),
            Self::Node => Some(node_executable()),
            _ => None,
        }
    }

    fn verify_native_version(self, directory: &Path, version: &str) -> Result<()> {
        let Some(program) = self.native_executable() else {
            return Ok(());
        };
        let output = crate::process::background(directory.join(program))
            .arg("--version")
            .env_remove("NODE_OPTIONS")
            .output()
            .with_context(|| format!("Could not run {program} on this host"))?;
        let version_output = String::from_utf8_lossy(&output.stdout);
        ensure!(
            output.status.success()
                && version_output
                    .split_whitespace()
                    .any(|part| part.trim_start_matches('v') == version),
            "Unexpected version from {program}: {}{}",
            version_output.trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(())
    }

    fn validate(self, directory: &Path) -> Result<()> {
        let paths: &[&str] = match self {
            Self::TypeScript => &[executable("lib/tsc", "lib/tsc.exe"), "lib/lib.d.ts"],
            Self::Tombi => &[executable("tombi", "tombi.exe")],
            Self::Node => &[executable("node", "node.exe")],
            Self::Vscode => &[JSON_SERVER, HTML_SERVER],
            Self::Yaml => &[YAML_SERVER],
            Self::Vue => &[
                "node_modules/@vue/language-server/bin/vue-language-server.js",
                "node_modules/@vue/typescript-plugin/package.json",
                "node_modules/typescript-language-server/lib/cli.mjs",
                "node_modules/typescript/lib/tsserver.js",
                "node_modules/vscode-jsonrpc/package.json",
            ],
        };
        for name in paths {
            let metadata = directory
                .join(name)
                .metadata()
                .with_context(|| format!("Missing {name}"))?;
            ensure!(metadata.is_file() && metadata.len() > 0, "Invalid {name}");
            #[cfg(unix)]
            if self.native_executable() == Some(*name) {
                use std::os::unix::fs::PermissionsExt as _;
                ensure!(
                    metadata.permissions().mode() & 0o111 != 0,
                    "{name} is not executable"
                );
            }
        }
        Ok(())
    }
}

const JSON_SERVER: &str =
    "node_modules/vscode-langservers-extracted/bin/vscode-json-language-server";
const HTML_SERVER: &str =
    "node_modules/vscode-langservers-extracted/bin/vscode-html-language-server";
const YAML_SERVER: &str = "node_modules/yaml-language-server/bin/yaml-language-server";

fn executable(unix: &'static str, windows: &'static str) -> &'static str {
    if cfg!(windows) { windows } else { unix }
}

impl LanguageServer {
    pub(crate) fn bundle(self) -> Option<Bundle> {
        Some(match self {
            Self::TypeScript => Bundle::TypeScript,
            Self::Json | Self::Html => Bundle::Vscode,
            Self::Tombi => Bundle::Tombi,
            Self::Yaml => Bundle::Yaml,
            Self::Vue => Bundle::Vue,
            _ => return None,
        })
    }
}

#[derive(Clone)]
pub(crate) struct Tools {
    root: PathBuf,
    platform: String,
}

impl Tools {
    pub(crate) fn new(home: PathBuf) -> Result<Self> {
        Ok(Self {
            root: std::path::absolute(home)?.join("lsp-tools"),
            platform: format!("{}-{}", env::consts::OS, env::consts::ARCH),
        })
    }

    fn directory(&self, bundle: Bundle, version: &str) -> PathBuf {
        self.root
            .join(bundle.id())
            .join(format!("{version}-{}", self.platform))
    }

    fn record(&self, bundle: Bundle, version: &str) -> Record {
        Record {
            bundle,
            version: version.to_owned(),
            platform: self.platform.clone(),
        }
    }

    fn valid(&self, bundle: Bundle, version: &str) -> bool {
        let directory = self.directory(bundle, version);
        fs::read(directory.join("install.json"))
            .ok()
            .and_then(|data| serde_json::from_slice::<Record>(&data).ok())
            .is_some_and(|record| record == self.record(bundle, version))
            && bundle.validate(&directory).is_ok()
    }

    fn cached(&self, bundle: Bundle) -> Option<String> {
        let recipe = bundle.recipe();
        std::iter::once(&recipe.version)
            .chain(&recipe.compatible)
            .find(|version| self.valid(bundle, version))
            .cloned()
    }

    fn node_program(
        &self,
        minimum: (u32, u32, u32),
        environment: Option<&Environment>,
    ) -> Option<PathBuf> {
        let path = match environment {
            Some(environment) => environment.get(OsStr::new("PATH")).cloned(),
            None => env::var_os("PATH"),
        };
        find_node(path.as_deref(), minimum, |candidate| {
            probe_node(candidate, environment)
        })
        .or_else(|| {
            let version = self.cached(Bundle::Node)?;
            (parse_node_version(&version)? >= minimum).then(|| {
                self.directory(Bundle::Node, &version)
                    .join(node_executable())
            })
        })
    }

    pub(crate) fn lookup(
        &self,
        bundle: Bundle,
        environment: Option<&Environment>,
    ) -> Result<Option<Installed>> {
        bundle.archives(&self.platform)?;
        Ok(self
            .cached(bundle)
            .filter(|_| {
                bundle
                    .minimum_node()
                    .is_none_or(|minimum| self.node_program(minimum, environment).is_some())
            })
            .map(|version| Installed { bundle, version }))
    }

    pub(crate) fn install(
        &self,
        bundle: Bundle,
        environment: Option<&Environment>,
        progress: impl FnMut(Progress),
    ) -> Result<Installed> {
        self.install_from(bundle, environment, progress, download)
    }

    fn install_from(
        &self,
        bundle: Bundle,
        environment: Option<&Environment>,
        mut progress: impl FnMut(Progress),
        fetch: impl Fn(&str, &mut File, &mut dyn FnMut(u64, Option<u64>)) -> Result<()>,
    ) -> Result<Installed> {
        let install_node = bundle
            .minimum_node()
            .is_some_and(|minimum| self.node_program(minimum, environment).is_none());
        let bundles = [Bundle::Node, bundle];
        for &part in &bundles[usize::from(!install_node)..] {
            if self.cached(part).is_some() {
                continue;
            }
            let version = part.version();
            let archives = part.archives(&self.platform)?;
            let directory = self.directory(part, version);
            let parent = directory.parent().unwrap();
            fs::create_dir_all(parent)?;
            let name = directory.file_name().unwrap().to_str().unwrap();
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(parent.join(format!(".{name}.lock")))?;
            let mut report = |completed, stage| {
                progress(Progress {
                    version: version.to_owned(),
                    completed,
                    total: archives.len(),
                    bundle: part,
                    stage,
                });
            };
            report(0, InstallStage::Waiting);
            lock.lock()?;
            if self.cached(part).is_some() {
                continue;
            }
            let prefix = format!(".{name}-partial-");
            for entry in fs::read_dir(parent)?.flatten() {
                if entry.file_name().to_string_lossy().starts_with(&prefix) {
                    fs::remove_dir_all(entry.path())?;
                }
            }
            let staged = tempfile::Builder::new()
                .prefix(&prefix)
                .tempdir_in(parent)?;
            let contents = staged.path().join("contents");
            fs::create_dir(&contents)?;
            for (index, archive) in archives.iter().enumerate() {
                let name = archive.url.rsplit('/').next().unwrap_or(&archive.url);
                let mut file = tempfile::tempfile_in(staged.path())?;
                let mut downloading = |received, size| {
                    report(
                        index,
                        InstallStage::Downloading {
                            archive: name.to_owned(),
                            received,
                            size,
                        },
                    );
                };
                downloading(0, None);
                fetch(&archive.url, &mut file, &mut downloading)
                    .with_context(|| format!("Could not download {}", archive.url))?;
                report(index, InstallStage::Verifying(name.to_owned()));
                archive.verify(&mut file)?;
                report(index, InstallStage::Extracting(name.to_owned()));
                archive.unpack(file, &contents)?;
            }
            report(archives.len(), InstallStage::Publishing);
            part.validate(&contents)?;
            part.verify_native_version(&contents, version)?;
            fs::write(
                contents.join("install.json"),
                serde_json::to_vec(&self.record(part, version))?,
            )?;
            // A complete, validated directory becomes visible in one rename. Other versions stay intact.
            if directory.exists() {
                fs::rename(&directory, staged.path().join("invalid"))?;
            }
            fs::rename(contents, directory)?;
        }
        self.lookup(bundle, environment)?
            .context("Managed language tools were not published")
    }

    pub(crate) fn command(
        &self,
        server: LanguageServer,
        installed: &Installed,
        args: Option<&[String]>,
        environment: Option<&Environment>,
    ) -> Result<Command> {
        let bundle = installed.bundle;
        ensure!(
            server.bundle() == Some(bundle),
            "Unexpected language tool bundle"
        );
        let checked = |part: Bundle, version: &str| {
            let recipe = part.recipe();
            ensure!(
                recipe.version == version || recipe.compatible.iter().any(|v| v == version),
                "Unexpected language tool version"
            );
            ensure!(
                self.valid(part, version),
                "Managed language tools are missing"
            );
            Ok(self.directory(part, version))
        };
        let directory = checked(bundle, &installed.version)?;
        let program = if let Some(minimum) = bundle.minimum_node() {
            self.node_program(minimum, environment)
                .context("A compatible Node.js runtime was not found")?
        } else {
            directory.join(bundle.native_executable().unwrap())
        };
        let mut command = crate::process::background(crate::paths::simplified(&program));
        match server {
            LanguageServer::Json => {
                command.arg(directory.join(JSON_SERVER));
            }
            LanguageServer::Html => {
                command.arg(directory.join(HTML_SERVER));
            }
            LanguageServer::Yaml => {
                command.arg(directory.join(YAML_SERVER));
            }
            LanguageServer::Vue => super::vue::bridge_args(&mut command, &directory),
            _ => {}
        }
        if let Some(args) = args {
            command.args(args);
        } else {
            command.args(server.default_arguments());
        }
        Ok(command)
    }
}

type Environment = HashMap<OsString, OsString>;

fn node_executable() -> &'static str {
    executable("node", "node.exe")
}

fn parse_node_version(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.strip_prefix('v').unwrap_or(version).split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(version)
}

#[derive(Deserialize)]
struct NodeProbe {
    executable: PathBuf,
    version: String,
}

fn find_node(
    path: Option<&OsStr>,
    minimum: (u32, u32, u32),
    probe: impl Fn(&Path) -> Option<NodeProbe>,
) -> Option<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    path.into_iter()
        .flat_map(env::split_paths)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(node_executable()))
        .filter(|candidate| seen.insert(candidate.clone()) && candidate.is_file())
        .find_map(|candidate| {
            let node = probe(&candidate)?;
            (node.executable.is_absolute()
                && parse_node_version(node.version.strip_prefix('v')?)? >= minimum
                && node.executable.is_file())
            .then_some(node.executable)
        })
}

fn probe_node(executable: &Path, environment: Option<&Environment>) -> Option<NodeProbe> {
    let mut command = crate::process::background(crate::paths::simplified(executable));
    if let Some(environment) = environment {
        command.env_clear().envs(environment);
    }
    let mut output = tempfile::tempfile().ok()?;
    let mut child = command
        .args([
            "--print",
            "JSON.stringify({executable:process.execPath,version:process.version})",
        ])
        .env_remove("NODE_OPTIONS")
        .stdin(Stdio::null())
        .stdout(output.try_clone().ok()?)
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                _ = child.kill();
                _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    output.rewind().ok()?;
    serde_json::from_reader(output.take(16 * 1024)).ok()
}

impl Archive {
    fn verify(&self, file: &mut File) -> Result<()> {
        file.rewind()?;
        let matches = if let Some(expected) = self.hash.strip_prefix("sha256:") {
            hex::encode(archive_digest::<Sha256>(file)?) == expected
        } else {
            let expected = self
                .hash
                .strip_prefix("sha512-")
                .context("Unsupported archive hash")?;
            STANDARD.encode(archive_digest::<Sha512>(file)?) == expected
        };
        ensure!(matches, "Archive hash mismatch for {}", self.url);
        file.rewind()?;
        Ok(())
    }

    fn unpack(&self, file: File, root: &Path) -> Result<()> {
        if self.url.ends_with(".zip") {
            #[cfg(windows)]
            {
                let mut archive = zip::ZipArchive::new(file)?;
                for (from, to) in &self.files {
                    let mut entry = archive.by_name(from.to_str().context("Invalid ZIP path")?)?;
                    write_entry(root, to, &mut entry, 0o755)?;
                }
                return Ok(());
            }
            #[cfg(not(windows))]
            anyhow::bail!("Unexpected ZIP language tool archive");
        }
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
        for entry in archive.entries()? {
            let mut entry = entry?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let path = entry.path()?;
            let to = if self.files.is_empty() {
                self.directory.join(path.strip_prefix("package")?)
            } else if let Some(to) = self.files.get(path.as_ref()) {
                to.clone()
            } else {
                continue;
            };
            let mode = entry.header().mode()?;
            write_entry(root, &to, &mut entry, mode)?;
        }
        Ok(())
    }
}

fn archive_digest<D: Digest>(file: &mut File) -> Result<sha2::digest::Output<D>> {
    let mut digest = D::new();
    let mut buffer = [0; 32 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(digest.finalize());
        }
        digest.update(&buffer[..count]);
    }
}

fn write_entry(root: &Path, path: &Path, entry: &mut impl Read, _mode: u32) -> Result<()> {
    ensure!(
        path.components()
            .all(|part| matches!(part, Component::Normal(_))),
        "Invalid archive path"
    );
    let path = root.join(path);
    fs::create_dir_all(path.parent().context("Invalid archive entry")?)?;
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    io::copy(entry, &mut file)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o644 | (_mode & 0o111)))?;
    }
    Ok(())
}

fn download(url: &str, out: &mut File, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<()> {
    ensure!(url.starts_with("https://"), "Expected an HTTPS archive URL");
    static HTTP: LazyLock<crate::network::Client> = LazyLock::new(crate::network::Client::new);
    let started = Instant::now();
    let mut url = url::Url::parse(url)?;
    for hop in 0..=10 {
        let remaining = Duration::from_secs(180)
            .checked_sub(started.elapsed())
            .context("language tool download timed out")?;
        let request = ureq::http::Request::get(url.as_str()).body(())?;
        let mut response = HTTP.send(request, remaining)?;
        let status = response.status();
        if let Some(location) = crate::network::redirect(&response) {
            ensure!(hop < 10, "too many language tool redirects");
            url = url.join(location.to_str()?)?;
            continue;
        }
        if status.is_client_error() || status.is_server_error() {
            return Err(ureq::Error::StatusCode(status.as_u16()).into());
        }
        let size = response.body().content_length();
        return copy_download(&mut response.body_mut().as_reader(), out, size, progress);
    }
    unreachable!("the last redirect returns an error")
}

fn copy_download(
    input: &mut impl Read,
    output: &mut impl Write,
    size: Option<u64>,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<()> {
    const LIMIT: u64 = 256 * 1024 * 1024;
    const INTERVAL: Duration = Duration::from_millis(150);
    ensure!(
        size.is_none_or(|size| size <= LIMIT),
        "Language tool archive exceeds the download limit"
    );
    let mut buffer = [0; 64 * 1024];
    let mut received = 0;
    let mut last_update = Instant::now();
    progress(received, size);
    loop {
        let count = match input.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            read => read?,
        };
        if count == 0 {
            progress(received, size);
            return Ok(());
        }
        received += count as u64;
        ensure!(
            received <= LIMIT,
            "Language tool archive exceeds the download limit"
        );
        output.write_all(&buffer[..count])?;
        if last_update.elapsed() >= INTERVAL {
            progress(received, size);
            last_update = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_reports_the_transfer_with_or_without_a_content_length() -> Result<()> {
        let archive = vec![0xab; 128 * 1024];
        for size in [Some(archive.len() as u64), None] {
            let mut output = Vec::new();
            let mut updates = Vec::new();
            copy_download(
                &mut archive.as_slice(),
                &mut output,
                size,
                &mut |received, size| {
                    updates.push((received, size));
                },
            )?;
            assert_eq!(output, archive);
            assert_eq!(updates.first(), Some(&(0, size)));
            assert_eq!(updates.last(), Some(&(archive.len() as u64, size)));
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn node_bundles_reuse_the_first_suitable_runtime_on_the_hosts_path() -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir()?;
        let tools = Tools::new(temp.path().join("stroll"))?;
        let old = temp.path().join("old-node");
        let supported = temp.path().join("node-bin");
        for (path, version) in [(&old, "v22.12.0"), (&supported, "v22.13.0")] {
            fs::create_dir(path)?;
            let executable = path.join("node");
            let response = serde_json::json!({"executable": executable, "version": version});
            fs::write(
                &executable,
                format!(
                    "#!/bin/sh
printf '%s\n' '{response}'
"
                ),
            )?;
            fs::set_permissions(executable, fs::Permissions::from_mode(0o755))?;
        }
        for (path, bundle, expected) in [
            (
                env::join_paths([&old, &supported])?,
                Bundle::Yaml,
                Bundle::Yaml,
            ),
            (env::join_paths([&old])?, Bundle::Yaml, Bundle::Node),
            (env::join_paths([&old])?, Bundle::Vue, Bundle::Vue),
        ] {
            let environment = Environment::from([("PATH".into(), path)]);
            let first = std::cell::Cell::new(None);
            let error = tools.install_from(
                bundle,
                Some(&environment),
                |progress| {
                    if first.get().is_none() {
                        first.set(Some(progress.bundle));
                    }
                },
                |_, _, _| anyhow::bail!("Stopped before downloading"),
            );
            assert!(format!("{:#}", error.unwrap_err()).contains("Stopped before downloading"));
            assert_eq!(first.get(), Some(expected));
        }
        Ok(())
    }

    #[test]
    fn corrupt_download_keeps_the_cache_unpublished_and_can_be_retried() -> Result<()> {
        let home = tempfile::tempdir()?;
        let tools = Tools::new(home.path().to_owned())?;
        let bundle = Bundle::TypeScript;
        let directory = tools.directory(bundle, bundle.version());
        let abandoned = directory.with_file_name(format!(
            ".{}-partial-abandoned",
            directory.file_name().unwrap().to_string_lossy()
        ));
        fs::create_dir_all(&abandoned)?;
        fs::write(abandoned.join("incomplete"), "interrupted")?;
        let error = tools.install_from(
            bundle,
            None,
            |_| {},
            |_, _, _| anyhow::bail!("network failure"),
        );
        assert!(format!("{:#}", error.unwrap_err()).contains("network failure"));
        let error = tools.install_from(
            bundle,
            None,
            |_| {},
            |_, file, _| {
                file.write_all(b"corrupt archive")?;
                Ok(())
            },
        );
        assert!(format!("{:#}", error.unwrap_err()).contains("Archive hash mismatch"));
        assert!(!abandoned.exists());
        assert!(!directory.exists());
        assert!(tools.lookup(bundle, None)?.is_none());
        Ok(())
    }

    #[test]
    #[ignore = "downloads real language tool bundles into STROLL_HOME"]
    fn install_managed_language_tool_fixtures() -> Result<()> {
        let home =
            std::env::var_os("STROLL_HOME").context("Set STROLL_HOME to a test directory")?;
        let tools = Tools::new(home.into())?;
        for bundle in [
            Bundle::TypeScript,
            Bundle::Vscode,
            Bundle::Tombi,
            Bundle::Yaml,
            Bundle::Vue,
        ] {
            let installed = tools
                .install(bundle, None, |progress| {
                    eprintln!(
                        "{} {} {}/{}",
                        bundle.id(),
                        progress.version,
                        progress.completed,
                        progress.total
                    );
                })
                .with_context(|| format!("Installing {}", bundle.id()))?;
            assert_eq!(installed.version, bundle.version());
            assert_eq!(tools.lookup(bundle, None)?, Some(installed.clone()));
            assert_eq!(
                tools.install_from(
                    bundle,
                    None,
                    |_| {},
                    |_, _, _| anyhow::bail!("Unexpected download")
                )?,
                installed,
            );
        }
        Ok(())
    }
}
