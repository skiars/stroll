use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

pub(super) fn aliases(home: &Path) -> Vec<String> {
    let directory = home.join(".ssh");
    let mut visited = BTreeSet::new();
    let mut hosts = BTreeSet::new();
    read_config(
        &directory.join("config"),
        &directory,
        home,
        &mut visited,
        &mut hosts,
    );
    hosts.into_iter().collect()
}

fn read_config(
    path: &Path,
    directory: &Path,
    home: &Path,
    visited: &mut BTreeSet<PathBuf>,
    hosts: &mut BTreeSet<String>,
) {
    let Ok(path) = path.canonicalize() else {
        return;
    };
    if visited.len() >= 128 || !visited.insert(path.clone()) {
        return;
    }
    let Ok(metadata) = path.metadata() else {
        return;
    };
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return;
    }
    let Ok(source) = fs::read_to_string(path) else {
        return;
    };
    for line in source.lines() {
        let line = line.trim_start();
        let Some((key, values)) = line.split_once(|ch: char| ch.is_whitespace() || ch == '=')
        else {
            continue;
        };
        if key.eq_ignore_ascii_case("host") {
            hosts.extend(words(values).into_iter().filter(|host| {
                !host.starts_with('-')
                    && !host.is_empty()
                    && host
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            }));
        } else if key.eq_ignore_ascii_case("include") {
            for pattern in words(values) {
                let path = if let Some(relative) = pattern.strip_prefix("~/") {
                    home.join(relative)
                } else {
                    directory.join(pattern)
                };
                if let Ok(paths) = glob::glob(&path.to_string_lossy().replace('\\', "/")) {
                    for path in paths.flatten().take(128) {
                        read_config(&path, directory, home, visited, hosts);
                    }
                }
            }
        }
    }
}

fn words(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for ch in value
        .trim_start_matches(|ch: char| ch.is_whitespace() || ch == '=')
        .chars()
    {
        match ch {
            '"' => quoted = !quoted,
            '#' if !quoted && word.is_empty() => break,
            ch if ch.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            ch => word.push(ch),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_literal_aliases_in_included_configs() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let ssh = home.path().join(".ssh");
        fs::create_dir_all(ssh.join("conf.d"))?;
        fs::write(
            ssh.join("config"),
            "Host arch *.example !private\nInclude \"conf.d/*.conf\"\n",
        )?;
        fs::write(
            ssh.join("conf.d/servers.conf"),
            "Host = build arch # comment\nInclude config\n",
        )?;
        assert_eq!(aliases(home.path()), ["arch", "build"]);
        Ok(())
    }
}
