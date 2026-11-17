use std::{path::Path, sync::LazyLock};

use regex::Regex;

pub(crate) fn allowed_path(path: &str) -> bool {
    static COMPONENT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
        r"(?i)^(?:\.{1,2}|\.git|\.aws|\.ssh|\.kube|\.docker|\.npmrc|\.pypirc|\.netrc|\.git-credentials|\.env(?:\..*)?|credentials(?:\..*)?|secrets?(?:\..*)?|id_(?:rsa|dsa|ecdsa|ed25519)(?:\..*)?)$",
    ).unwrap()
    });
    static EXTENSION: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\.(?:pem|key|p12|pfx|keystore)$").unwrap());
    !path.is_empty()
        && path.encode_utf16().count() <= 4096
        && !path.starts_with('/')
        && !path.contains(['\\', '\0'])
        && !path.split('/').any(|part| COMPONENT.is_match(part))
        && !EXTENSION.is_match(path)
}

pub(crate) fn allowed_hover_source(root: &Path, path: &Path) -> bool {
    fn allowed(path: &Path) -> bool {
        path.components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
            && path
                .to_str()
                .is_some_and(|path| allowed_path(&path.replace(std::path::MAIN_SEPARATOR, "/")))
    }

    allowed(path)
        && root.canonicalize().ok().is_some_and(|root| {
            root.join(path).canonicalize().ok().is_some_and(|resolved| {
                resolved.is_file() && resolved.strip_prefix(&root).is_ok_and(allowed)
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_web_credential_paths_and_resolved_targets() -> anyhow::Result<()> {
        for path in [
            ".ENV.local",
            "nested/.ssh/config",
            "nested/Secret.json",
            "some.pem",
            ".git/config",
            "../source.rs",
            "/source.rs",
            "nested\\source.rs",
        ] {
            assert!(!allowed_path(path), "{path}");
        }
        assert!(allowed_path("src/source.rs"));
        let fixture = tempfile::tempdir()?;
        std::fs::write(fixture.path().join("source.rs"), "")?;
        assert!(allowed_hover_source(fixture.path(), Path::new("source.rs")));
        #[cfg(unix)]
        {
            std::fs::write(fixture.path().join(".env"), "")?;
            std::os::unix::fs::symlink(".env", fixture.path().join("alias.rs"))?;
            assert!(!allowed_hover_source(fixture.path(), Path::new("alias.rs")));
        }
        Ok(())
    }
}
