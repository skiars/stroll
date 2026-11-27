use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

pub(super) fn local_path(path: &Path, directory: Option<&Path>, shell: &OsStr) -> Option<String> {
    let path = crate::paths::simplified(path);
    let directory = directory.map(crate::paths::simplified);
    let path = directory
        .as_deref()
        .filter(|directory| directory.is_absolute())
        .and_then(|directory| relative(&path, directory))
        .unwrap_or(path);
    let shell = Path::new(shell).file_stem()?.to_str()?.to_ascii_lowercase();
    let path = path.to_str()?;
    if path.chars().any(char::is_control) {
        return None;
    }
    match shell.as_str() {
        "cmd" => {
            // Percent expansion and delayed expansion can occur inside double quotes.
            (!path.contains(['%', '!', '"'])).then(|| format!("\"{path}\""))
        }
        "pwsh" | "powershell" => {
            let mut word = String::from("'");
            for character in path.chars() {
                if matches!(character, '\'' | '‘' | '’' | '‚' | '‛') {
                    word.push(character);
                }
                word.push(character);
            }
            word.push('\'');
            Some(word)
        }
        "sh" | "bash" | "ash" | "dash" | "zsh" | "fish" | "ksh" | "mksh" | "yash" => {
            if cfg!(windows) {
                unix_word(&path.replace('\\', "/"))
            } else {
                unix_word(path)
            }
        }
        _ => plain_word(path).then(|| path.to_owned()),
    }
}

pub(super) fn remote_path(root: &Path, entry: &str, directory: Option<&Path>) -> Option<String> {
    let root = root.to_str()?.trim_end_matches('/');
    let path = format!("{root}/{entry}");
    let relative = directory
        .filter(|dir| {
            dir.to_str()
                .is_some_and(|dir| dir.starts_with('/') && !(cfg!(windows) && dir.contains('\\')))
        })
        .and_then(|dir| relative(Path::new(&path), dir));
    if let Some(relative) = relative.as_deref().and_then(Path::to_str) {
        if cfg!(windows) {
            return unix_word(&relative.replace('\\', "/"));
        }
        return unix_word(relative);
    }
    unix_word(&path)
}

fn relative(path: &Path, directory: &Path) -> Option<PathBuf> {
    let relative = crate::paths::relative(path, directory)?;
    if relative.to_str()?.starts_with('-') {
        Some(Path::new(".").join(relative))
    } else {
        Some(relative)
    }
}

fn plain_word(path: &str) -> bool {
    !path.starts_with('-')
        && path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_./-:".contains(&byte))
}

fn unix_word(path: &str) -> Option<String> {
    if path.chars().any(char::is_control) {
        return None;
    }
    if plain_word(path) {
        return Some(path.to_owned());
    }
    // Escape quotes and backslashes outside single-quoted spans. Both POSIX shells and
    // Fish accept this form, including when an SSH helper chooses the login shell.
    let mut word = String::from("'");
    for character in path.chars() {
        match character {
            '\'' => word.push_str("'\\''"),
            '\\' => word.push_str("'\\\\'"),
            _ => word.push(character),
        }
    }
    word.push('\'');
    Some(word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths_follow_the_terminal_directory() {
        let root = Path::new("/work/repo");
        let dir = |value| Some(Path::new(value));
        assert_eq!(
            remote_path(root, "README.md", dir("/work/repo/src")),
            Some("../README.md".into())
        );
        assert_eq!(
            remote_path(root, "-n", dir("/work/repo")),
            Some("./-n".into())
        );
        assert_eq!(
            remote_path(root, "src", dir("/work/repo/src")),
            Some(".".into())
        );
        assert_eq!(
            remote_path(root, "README.md", None),
            Some("/work/repo/README.md".into())
        );
        assert_eq!(remote_path(root, "a\nb", None), None);
    }

    #[test]
    fn path_metacharacters_form_one_shell_word() {
        let path = "a b/$项目'\\name";
        let word = unix_word(path).unwrap();
        assert_eq!(shlex::split(&word), Some(vec![path.into()]));
        assert_eq!(unix_word("a\u{1b}b"), None);
        assert_eq!(local_path(Path::new("a%name"), None, "cmd".as_ref()), None);
        assert_eq!(
            local_path(Path::new("O'Neil"), None, "pwsh".as_ref()),
            Some("'O''Neil'".into())
        );
    }
}
