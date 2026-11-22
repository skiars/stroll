use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use anyhow::{Context as _, Result, ensure};
use rust_i18n::t;
use serde::{Deserialize, Serialize};

use super::{MAX_ASSET_BYTES, MAX_FILE_BYTES, Repository, files::EntryVersion, resolve_asset};
use crate::remote::{Bytes, RepoPath};

const MAX_RESOURCE_CHECKS: usize = 128;
const RESOURCE_CHUNK_BYTES: usize = 32 * 1024;

#[derive(Deserialize, Serialize)]
pub(crate) struct ResourceCheck {
    path: RepoPath,
    asset: bool,
    version: EntryVersion,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum ResourceStatus {
    Unchanged,
    Changed,
    Unavailable(String),
}

#[derive(Deserialize, Serialize)]
pub(crate) enum ResourceRead {
    Unchanged,
    Changed {
        version: Option<EntryVersion>,
        bytes: Bytes,
    },
}

#[derive(Deserialize, Serialize)]
pub(crate) enum ResourceChunk {
    Unchanged,
    Part {
        version: EntryVersion,
        size: u64,
        bytes: Bytes,
    },
}

impl Repository {
    pub(crate) fn read_resource(
        &self,
        path: &Path,
        asset: bool,
        known_version: Option<&EntryVersion>,
    ) -> Result<ResourceRead> {
        let file = self.open_resource(path, asset)?;
        if known_version.is_some() && file.version.as_ref() == known_version {
            return Ok(ResourceRead::Unchanged);
        }
        let (bytes, version) = file.read()?;
        Ok(ResourceRead::Changed {
            version,
            bytes: Bytes(bytes),
        })
    }

    pub(crate) fn check_resources(
        &self,
        checks: Vec<ResourceCheck>,
    ) -> Result<Vec<ResourceStatus>> {
        ensure!(
            checks.len() <= MAX_RESOURCE_CHECKS,
            "Too many resource checks"
        );
        Ok(checks
            .into_iter()
            .map(
                |check| match self.open_resource(check.path.as_path(), check.asset) {
                    Ok(file) if file.version.as_ref() == Some(&check.version) => {
                        ResourceStatus::Unchanged
                    }
                    Ok(_) => ResourceStatus::Changed,
                    Err(error) => ResourceStatus::Unavailable(format!("{error:#}")),
                },
            )
            .collect())
    }

    pub(crate) fn read_resource_chunk(
        &self,
        path: &Path,
        asset: bool,
        offset: u64,
        known_version: Option<&EntryVersion>,
    ) -> Result<ResourceChunk> {
        let mut resource = self.open_resource(path, asset)?;
        let version = resource
            .version
            .context("Cannot determine resource version")?;
        if offset == 0 && known_version == Some(&version) {
            return Ok(ResourceChunk::Unchanged);
        }
        ensure!(
            offset == 0 || known_version == Some(&version),
            t!("workspace.tree_entry_changed")
        );
        let size = resource.size;
        ensure!(
            offset <= size,
            "Resource offset is past the end of the file"
        );
        let length = (size - offset).min(RESOURCE_CHUNK_BYTES as u64) as usize;
        let mut bytes = vec![0; length];
        resource.file.seek(SeekFrom::Start(offset))?;
        resource.file.read_exact(&mut bytes)?;
        ensure!(
            EntryVersion::from_metadata(&resource.file.metadata()?) == version,
            t!("workspace.tree_entry_changed")
        );
        Ok(ResourceChunk::Part {
            version,
            size,
            bytes: Bytes(bytes),
        })
    }

    fn open_resource(&self, path: &Path, asset: bool) -> Result<ResourceFile> {
        let target = if asset {
            resolve_asset(&self.root, path)?
        } else {
            self.resolve(path)?
        };
        ResourceFile::open(&target, asset)
    }
}

struct ResourceFile {
    file: File,
    asset: bool,
    size: u64,
    version: Option<EntryVersion>,
}

impl ResourceFile {
    fn open(path: &Path, asset: bool) -> Result<Self> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), t!("storage.regular_file"));
        check_size(metadata.len(), asset)?;
        Ok(Self {
            file,
            asset,
            size: metadata.len(),
            version: metadata
                .modified()
                .ok()
                .map(|_| EntryVersion::from_metadata(&metadata)),
        })
    }

    fn read(self) -> Result<(Vec<u8>, Option<EntryVersion>)> {
        let mut bytes = Vec::new();
        (&self.file)
            .take(limit(self.asset) + 1)
            .read_to_end(&mut bytes)?;
        check_size(bytes.len() as u64, self.asset)?;
        let version = self.version.filter(|version| {
            self.file.metadata().is_ok_and(|metadata| {
                metadata.len() == bytes.len() as u64
                    && &EntryVersion::from_metadata(&metadata) == version
            })
        });
        Ok((bytes, version))
    }
}

fn limit(asset: bool) -> u64 {
    if asset {
        MAX_ASSET_BYTES
    } else {
        MAX_FILE_BYTES
    }
}

fn check_size(bytes: u64, asset: bool) -> Result<()> {
    if asset {
        ensure!(bytes <= MAX_ASSET_BYTES, t!("storage.image_limit"));
    } else {
        ensure!(bytes <= MAX_FILE_BYTES, t!("storage.legacy_file_limit"));
    }
    Ok(())
}

#[cfg(feature = "desktop")]
pub(crate) use desktop::{RemoteResources, read_local_resource};

#[cfg(feature = "desktop")]
mod desktop {
    use super::*;
    use crate::{
        remote::{Request, SshSession},
        repository::read_remote_bytes,
        resource_cache::{ResourceCache, SourceKey},
    };
    use std::path::PathBuf;

    pub(crate) fn read_local_resource(
        path: &Path,
        asset: bool,
        cache: Option<&ResourceCache>,
    ) -> Result<Vec<u8>> {
        let file = ResourceFile::open(path, asset)?;
        let key = SourceKey::new("local", &[path.as_os_str().as_encoded_bytes()]);
        if let Some(cache) = cache
            && let Some(entry) = cache.source::<EntryVersion>(key)
            && file.version.as_ref() == Some(&entry.version)
            && let Some(bytes) = cache.read_content(&entry.hash, limit(asset))
        {
            return Ok(bytes);
        }
        let (bytes, version) = file.read()?;
        if let (Some(cache), Some(version)) = (cache, version) {
            let _ = cache.store(key, &version, &bytes);
        }
        Ok(bytes)
    }

    #[derive(Clone)]
    pub(crate) struct RemoteResources {
        pub(crate) ssh: SshSession,
        scope: Vec<u8>,
        conditional_read: bool,
        chunked_read: bool,
    }

    impl Repository {
        pub(crate) fn remote_resources(&self) -> Option<RemoteResources> {
            Some(RemoteResources {
                ssh: self.ssh.clone()?,
                scope: self.storage_key().to_owned(),
                conditional_read: self.remote_resource_read,
                chunked_read: self.remote_resource_chunks,
            })
        }
    }

    impl RemoteResources {
        fn source_key(&self, path: &RepoPath, asset: bool) -> SourceKey {
            SourceKey::new(
                "ssh",
                &[&self.scope, path.as_str().as_bytes(), &[asset.into()]],
            )
        }

        pub(crate) fn check_cached_assets(
            &self,
            paths: &[PathBuf],
            cache: &ResourceCache,
        ) -> Vec<Option<Result<Vec<u8>>>> {
            let mut results: Vec<_> = paths.iter().map(|_| None).collect();
            if !self.conditional_read {
                return results;
            }
            let mut selected = Vec::new();
            let mut checks = Vec::new();
            for (index, path) in paths.iter().enumerate().take(MAX_RESOURCE_CHECKS) {
                if let Ok(path) = RepoPath::from_local(path)
                    && let Some(entry) = cache.source::<EntryVersion>(self.source_key(&path, true))
                {
                    checks.push(ResourceCheck {
                        path,
                        asset: true,
                        version: entry.version,
                    });
                    selected.push((index, entry.hash));
                }
            }
            if checks.len() >= 2 {
                let statuses = {
                    let _priority = self.ssh.prioritize_resources();
                    self.ssh
                        .call::<Vec<ResourceStatus>>(Request::CheckResources(checks))
                };
                let Ok(statuses) = statuses else {
                    return results;
                };
                if statuses.len() != selected.len() {
                    return results;
                }
                for ((index, hash), status) in selected.into_iter().zip(statuses) {
                    results[index] = match status {
                        ResourceStatus::Unchanged => {
                            cache.read_content(&hash, MAX_ASSET_BYTES).map(Ok)
                        }
                        ResourceStatus::Changed => None,
                        ResourceStatus::Unavailable(error) => Some(Err(anyhow::anyhow!(error))),
                    };
                }
            }
            results
        }

        pub(crate) fn read(
            &self,
            path: &Path,
            asset: bool,
            cache: Option<&ResourceCache>,
        ) -> Result<Vec<u8>> {
            self.read_while(path, asset, cache, || true)?
                .context("Resource read ended before completion")
        }

        pub(crate) fn read_while(
            &self,
            path: &Path,
            asset: bool,
            cache: Option<&ResourceCache>,
            wanted: impl Fn() -> bool,
        ) -> Result<Option<Vec<u8>>> {
            if !wanted() {
                return Ok(None);
            }
            let _priority = (!asset).then(|| self.ssh.prioritize_resources());
            if self.chunked_read {
                return self.read_chunks(path, asset, cache, &wanted);
            }
            let bytes = self.read_whole(path, asset, cache)?;
            Ok(wanted().then_some(bytes))
        }

        fn read_chunks(
            &self,
            path: &Path,
            asset: bool,
            cache: Option<&ResourceCache>,
            wanted: &impl Fn() -> bool,
        ) -> Result<Option<Vec<u8>>> {
            let path = RepoPath::from_local(path)?;
            let key = self.source_key(&path, asset);
            let mut cached = cache.and_then(|cache| cache.source::<EntryVersion>(key));
            if asset && let Some(entry) = cached.take() {
                let statuses = {
                    let _validation = self.ssh.prioritize_resources();
                    self.ssh
                        .call::<Vec<ResourceStatus>>(Request::CheckResources(vec![ResourceCheck {
                            path: path.clone(),
                            asset,
                            version: entry.version,
                        }]))
                };
                if !wanted() {
                    return Ok(None);
                }
                match statuses?.as_slice() {
                    [ResourceStatus::Unchanged] => {
                        if let Some(bytes) =
                            cache.and_then(|cache| cache.read_content(&entry.hash, limit(asset)))
                        {
                            return Ok(wanted().then_some(bytes));
                        }
                    }
                    [ResourceStatus::Changed] => {}
                    [ResourceStatus::Unavailable(error)] => anyhow::bail!("{error}"),
                    _ => anyhow::bail!("Invalid SSH resource check response"),
                }
            }
            let mut result = Vec::new();
            let mut expected: Option<(EntryVersion, u64)> = None;
            while wanted() {
                let offset = result.len() as u64;
                let request = Request::ReadResourceChunk {
                    path: path.clone(),
                    asset,
                    offset,
                    version: expected
                        .as_ref()
                        .map(|(version, _)| version.clone())
                        .or_else(|| cached.as_ref().map(|entry| entry.version.clone())),
                };
                let reply = if asset {
                    self.ssh.image_chunk(request, wanted)
                } else {
                    self.ssh.call(request).map(Some)
                };
                if !wanted() {
                    return Ok(None);
                }
                let Some(reply) = reply? else { return Ok(None) };
                match reply {
                    ResourceChunk::Unchanged => {
                        let entry = cached
                            .take()
                            .context("Unexpected unchanged resource chunk")?;
                        if let Some(bytes) =
                            cache.and_then(|cache| cache.read_content(&entry.hash, limit(asset)))
                        {
                            return Ok(wanted().then_some(bytes));
                        }
                    }
                    ResourceChunk::Part {
                        version,
                        size,
                        bytes,
                    } => {
                        check_size(size, asset)?;
                        ensure!(
                            offset <= size
                                && bytes.0.len() as u64
                                    == (size - offset).min(RESOURCE_CHUNK_BYTES as u64),
                            "Invalid SSH resource chunk length"
                        );
                        if let Some((expected_version, expected_size)) = &expected {
                            ensure!(
                                &version == expected_version && size == *expected_size,
                                t!("workspace.tree_entry_changed")
                            );
                        }
                        result.extend(bytes.0);
                        if result.len() as u64 == size {
                            if let Some(cache) = cache {
                                let _ = cache.store(key, &version, &result);
                            }
                            return Ok(wanted().then_some(result));
                        }
                        expected = Some((version, size));
                        cached = None;
                    }
                }
            }
            Ok(None)
        }

        fn read_whole(
            &self,
            path: &Path,
            asset: bool,
            cache: Option<&ResourceCache>,
        ) -> Result<Vec<u8>> {
            let Some(cache) = cache.filter(|_| self.conditional_read) else {
                return read_remote_bytes(&self.ssh, path, asset);
            };
            let path = RepoPath::from_local(path)?;
            let key = self.source_key(&path, asset);
            let entry = cache.source::<EntryVersion>(key);
            let mut response = self.ssh.call(Request::ReadResource {
                path: path.clone(),
                asset,
                version: entry.as_ref().map(|entry| entry.version.clone()),
            })?;
            if matches!(response, ResourceRead::Unchanged) {
                if let Some(bytes) =
                    entry.and_then(|entry| cache.read_content(&entry.hash, limit(asset)))
                {
                    return Ok(bytes);
                }
                response = self.ssh.call(Request::ReadResource {
                    path,
                    asset,
                    version: None,
                })?;
            }
            let ResourceRead::Changed { version, bytes } = response else {
                anyhow::bail!("Expected SSH resource contents");
            };
            check_size(bytes.0.len() as u64, asset)?;
            if let Some(version) = version {
                let _ = cache.store(key, &version, &bytes.0);
            }
            Ok(bytes.0)
        }
    }

    #[cfg(all(test, target_os = "linux"))]
    #[test]
    fn ssh_resource_reuses_disk_content_across_connections() -> Result<()> {
        use std::{fs, path::PathBuf, process::Command};

        use crate::remote::SshTarget;

        let fixture = tempfile::tempdir()?;
        let worktree = fixture.path();
        let directory = worktree.join("cache");
        let path = Path::new("README.md");
        let original = b"# First version\n";
        let changed = b"# Latest version\n";
        let assets = [PathBuf::from("one.svg"), PathBuf::from("two.svg")];
        let large = Path::new("large.png");
        let large_bytes = vec![42; RESOURCE_CHUNK_BYTES * 2 + 7];
        fs::write(worktree.join(path), original)?;
        fs::write(worktree.join(large), &large_bytes)?;
        for asset in &assets {
            fs::write(worktree.join(asset), original)?;
        }

        let connect = |chunks: bool| -> Result<RemoteResources> {
            let mut helper = Command::new("python3");
            helper
                .arg("-u")
                .arg("-c")
                .arg(
                    r#"
import base64
import json
from pathlib import Path
import struct
import sys

root = Path(sys.argv[1])
input = sys.stdin.buffer
output = sys.stdout.buffer
while header := input.read(4):
    call = json.loads(input.read(struct.unpack(">I", header)[0]))
    request = call["request"]
    if "Hello" in request:
        reply = {"Ok": request["Hello"]}
    elif "OpenWorkspace" in request:
        reply = {"Ok": {
            "identity": str(root), "root": str(root), "common_dir": str(root / ".git"),
            "name": "resource-test", "branch": "main", "children": {}, "resource_read": True,
            "resource_chunks": sys.argv[2] == "true",
        }}
    elif "ReadResource" in request:
        read = request["ReadResource"]
        try:
            path = root / read["path"]
            version = str(path.stat().st_ctime_ns)
            if version == read["version"]:
                reply = {"Ok": "Unchanged"}
                event = "unchanged"
            else:
                data = base64.b64encode(path.read_bytes()).decode()
                reply = {"Ok": {"Changed": {"version": version, "bytes": data}}}
                event = "contents"
        except FileNotFoundError:
            reply = {"Err": "Missing resource"}
            event = "missing"
        with (root / "reads").open("a") as log:
            log.write(event + "\n")
    elif "ReadResourceChunk" in request:
        read = request["ReadResourceChunk"]
        path = root / read["path"]
        stat = path.stat()
        version = str(stat.st_ctime_ns)
        offset = read["offset"]
        assert offset == 0 or read["version"] == version
        with path.open("rb") as file:
            file.seek(offset)
            data = base64.b64encode(file.read(int(sys.argv[3]))).decode()
        reply = {"Ok": {"Part": {"version": version, "size": stat.st_size, "bytes": data}}}
        with (root / "offsets").open("a") as log:
            log.write(str(offset) + "\n")
    elif "CheckResources" in request:
        checks = request["CheckResources"]
        assert len(checks) == 2 and all(check["asset"] for check in checks)
        reply = {"Ok": ["Unchanged" if str((root / check["path"]).stat().st_ctime_ns)
                        == check["version"] else "Changed" for check in checks]}
        with (root / "reads").open("a") as log:
            log.write("batch\n")
    else:
        reply = {"Err": "Unexpected resource request"}
    data = json.dumps({"id": call["id"], "result": reply}).encode()
    output.write(struct.pack(">I", len(data)) + data)
    output.flush()
"#,
                )
                .arg(worktree)
                .arg(chunks.to_string())
                .arg(RESOURCE_CHUNK_BYTES.to_string());
            let session = SshSession::connect_test_helper(
                SshTarget {
                    host: "fixture".into(),
                    port: None,
                    path: worktree.to_owned(),
                },
                helper,
            )?;
            Ok(Repository::open_ssh(session, PathBuf::from(worktree))?
                .remote_resources()
                .unwrap())
        };

        let cache = ResourceCache::new(directory.clone());
        assert_eq!(
            read_local_resource(&worktree.join(path), false, Some(&cache))?,
            original
        );
        assert_eq!(connect(false)?.read(path, false, Some(&cache))?, original);
        for asset in &assets {
            assert_eq!(connect(false)?.read(asset, true, Some(&cache))?, original);
        }
        assert_eq!(cache.count_contents(), 1);
        drop(cache);

        let cache = ResourceCache::new(directory);
        let remote = connect(false)?;
        assert_eq!(remote.read(path, false, Some(&cache))?, original);
        fs::write(worktree.join(&assets[1]), changed)?;
        let mut checked = remote.check_cached_assets(&assets, &cache).into_iter();
        assert_eq!(checked.next().flatten().unwrap()?, original);
        assert!(checked.next().flatten().is_none());
        assert_eq!(remote.read(&assets[1], true, Some(&cache))?, changed);
        fs::write(worktree.join(path), changed)?;
        assert_eq!(remote.read(path, false, Some(&cache))?, changed);
        fs::remove_file(worktree.join(path))?;
        assert!(remote.read(path, false, Some(&cache)).is_err());
        assert_eq!(
            fs::read_to_string(worktree.join("reads"))?,
            "contents\ncontents\ncontents\nunchanged\nbatch\ncontents\ncontents\nmissing\n"
        );

        let modern = connect(true)?;
        let offsets = worktree.join("offsets");
        assert!(
            modern
                .read_while(large, true, Some(&cache), || !offsets.exists())?
                .is_none()
        );
        assert_eq!(fs::read_to_string(&offsets)?, "0\n");
        let key = modern.source_key(&RepoPath::from_local(large)?, true);
        assert!(cache.source::<EntryVersion>(key).is_none());
        assert_eq!(modern.read(large, true, Some(&cache))?, large_bytes);
        assert_eq!(
            fs::read_to_string(&offsets)?,
            format!(
                "0\n0\n{RESOURCE_CHUNK_BYTES}\n{}\n",
                RESOURCE_CHUNK_BYTES * 2
            )
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use super::*;

    #[test]
    fn versioned_reads_validate_current_files_and_assets() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().canonicalize()?;
        assert!(
            Command::new("git")
                .arg("init")
                .arg("-q")
                .current_dir(&root)
                .status()?
                .success()
        );
        let path = Path::new("README.md");
        fs::write(root.join(path), "first")?;
        let stable = Path::new("diagram.svg");
        fs::write(root.join(stable), "<svg/>")?;
        let repo = Repository::open_root(root.clone())?;
        let ResourceRead::Changed {
            version: Some(version),
            bytes,
        } = repo.read_resource(path, false, None)?
        else {
            panic!("expected version and document bytes");
        };
        assert_eq!(bytes.0, b"first");
        let ResourceRead::Changed {
            version: Some(stable_version),
            ..
        } = repo.read_resource(stable, true, None)?
        else {
            panic!("expected a versioned asset");
        };
        assert!(matches!(
            repo.read_resource(path, false, Some(&version))?,
            ResourceRead::Unchanged
        ));
        assert!(matches!(
            repo.read_resource(path, true, Some(&version))?,
            ResourceRead::Unchanged
        ));

        let original = File::open(root.join(path))?;
        let modified = original.metadata()?.modified()?;
        let staged = root.join("replacement");
        fs::write(&staged, "later")?;
        fs::OpenOptions::new()
            .write(true)
            .open(&staged)?
            .set_times(fs::FileTimes::new().set_modified(modified))?;
        drop(original);
        fs::remove_file(root.join(path))?;
        fs::rename(staged, root.join(path))?;
        let ResourceRead::Changed { bytes, .. } =
            repo.read_resource(path, false, Some(&version))?
        else {
            panic!("file replacement must invalidate the old version");
        };
        assert_eq!(bytes.0, b"later");
        let status = repo.check_resources(vec![
            ResourceCheck {
                path: RepoPath::from_local(stable)?,
                asset: true,
                version: stable_version,
            },
            ResourceCheck {
                path: RepoPath::from_local(path)?,
                asset: false,
                version: version.clone(),
            },
            ResourceCheck {
                path: RepoPath::from_local(Path::new("missing.svg"))?,
                asset: true,
                version: version.clone(),
            },
        ])?;
        assert!(matches!(
            status.as_slice(),
            [
                ResourceStatus::Unchanged,
                ResourceStatus::Changed,
                ResourceStatus::Unavailable(_)
            ]
        ));
        fs::remove_file(root.join(path))?;
        assert!(repo.read_resource(path, false, Some(&version)).is_err());
        assert!(
            repo.read_resource(Path::new("../README.md"), true, Some(&version))
                .is_err()
        );
        let asset_bytes = vec![12; RESOURCE_CHUNK_BYTES + 7];
        fs::write(root.join(stable), &asset_bytes)?;
        let ResourceChunk::Part {
            bytes,
            version,
            size,
        } = repo.read_resource_chunk(stable, true, 0, None)?
        else {
            panic!("expected first asset chunk");
        };
        assert_eq!(size, asset_bytes.len() as u64);
        assert_eq!(bytes.0, asset_bytes[..RESOURCE_CHUNK_BYTES]);
        assert!(matches!(
            repo.read_resource_chunk(stable, true, 0, Some(&version))?,
            ResourceChunk::Unchanged
        ));
        let ResourceChunk::Part { bytes, .. } =
            repo.read_resource_chunk(stable, true, RESOURCE_CHUNK_BYTES as u64, Some(&version))?
        else {
            panic!("expected last asset chunk");
        };
        assert_eq!(bytes.0, asset_bytes[RESOURCE_CHUNK_BYTES..]);
        fs::write(root.join(stable), "changed")?;
        assert!(
            repo.read_resource_chunk(stable, true, RESOURCE_CHUNK_BYTES as u64, Some(&version))
                .is_err()
        );
        Ok(())
    }
}
