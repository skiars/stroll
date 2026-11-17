use gpui::SharedString;

/// Built-in aliases for editing defaults when no exact grammar name is registered.
/// Availability of a parser does not change what a language name means.
pub(crate) fn language_name(name: &str) -> SharedString {
    let name = name.to_lowercase();
    match name.as_str() {
        "plain" => "text".into(),
        "plaintext" => "text".into(),
        "jsonc" => "json".into(),
        "sh" => "bash".into(),
        "c++" => "cpp".into(),
        "cs" => "csharp".into(),
        "scss" => "css".into(),
        "ex" => "elixir".into(),
        "js" => "javascript".into(),
        "kt" => "kotlin".into(),
        "kts" => "kotlin".into(),
        "ktm" => "kotlin".into(),
        "makefile" => "make".into(),
        "md" => "markdown".into(),
        "mdx" => "markdown".into(),
        "markdown-inline" => "markdown_inline".into(),
        "php3" => "php".into(),
        "php4" => "php".into(),
        "php5" => "php".into(),
        "phtml" => "php".into(),
        "protobuf" => "proto".into(),
        "py" => "python".into(),
        "rb" => "ruby".into(),
        "rs" => "rust".into(),
        "ts" => "typescript".into(),
        "yml" => "yaml".into(),
        "pyi" => "python".into(),
        _ => name.into(),
    }
}

/// Recognizes Bash filenames and script interpreter declarations.
pub fn is_bash_source(name: &str, first_line: Option<&str>) -> bool {
    if matches!(
        name.to_ascii_lowercase().as_str(),
        ".bashrc"
            | ".bash_profile"
            | ".bash_login"
            | ".bash_logout"
            | ".bash_aliases"
            | "bashrc"
            | "bash.bashrc"
    ) || name.rsplit_once('.').is_some_and(|(_, suffix)| {
        suffix.eq_ignore_ascii_case("sh") || suffix.eq_ignore_ascii_case("bash")
    }) {
        return true;
    }
    if std::path::Path::new(name).extension().is_some() {
        return false;
    }
    let Some(line) = first_line.and_then(|line| line.strip_prefix("#!")) else {
        return false;
    };
    let mut words = line.split_ascii_whitespace();
    match words.next().and_then(|path| path.rsplit('/').next()) {
        Some("bash") => true,
        Some("env") => match words.next() {
            Some("bash") => true,
            Some("-S" | "--split-string") => words.next() == Some("bash"),
            _ => false,
        },
        _ => false,
    }
}
