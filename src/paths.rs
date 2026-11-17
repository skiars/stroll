//! Normalize paths for display and copy operations.
//!
//! On Windows, `std::fs::canonicalize` returns extended-length paths such as `\\?\C:\...`.
//! Keep the original `PathBuf` internally to preserve long paths and special names.
//! Convert to `C:\...` or `\\server\share\...` at the UI and clipboard boundaries.

use std::path::{Path, PathBuf};

/// Convert extended-length paths to familiar forms for display and the clipboard.
pub(crate) fn display(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    if let Some(simplified) = normalized(&text) {
        return simplified;
    }
    text.into_owned()
}

/// Remove extended-length prefixes when comparing user input with paths returned by the filesystem.
pub(crate) fn simplified(path: &Path) -> PathBuf {
    #[cfg(windows)]
    if let Some(simplified) = path.to_str().and_then(normalized) {
        return PathBuf::from(simplified);
    }
    path.to_owned()
}

#[cfg_attr(not(windows), allow(dead_code))]
fn normalized(text: &str) -> Option<String> {
    let rest = text.strip_prefix(r"\\?\")?;
    if rest
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("UNC\\"))
    {
        return Some(format!(r"\\{}", &rest[4..]));
    }
    let bytes = rest.as_bytes();
    // Only convert drive-letter paths. Paths such as `\\?\Volume{...}` have no ordinary equivalent.
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        .then(|| rest.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_disk_prefix_is_removed() {
        assert_eq!(
            normalized(r"\\?\C:\stroll\src\main.rs").as_deref(),
            Some(r"C:\stroll\src\main.rs")
        );
        assert_eq!(normalized(r"\\?\c:\").as_deref(), Some(r"c:\"));
    }

    #[test]
    fn verbatim_unc_prefix_becomes_legacy_unc() {
        assert_eq!(
            normalized(r"\\?\UNC\server\share\dir").as_deref(),
            Some(r"\\server\share\dir")
        );
        assert_eq!(
            normalized(r"\\?\unc\server\share").as_deref(),
            Some(r"\\server\share")
        );
    }

    #[test]
    fn other_paths_are_unchanged() {
        assert_eq!(normalized(r"C:\repo"), None);
        assert_eq!(normalized(r"\\server\share\dir"), None);
        assert_eq!(normalized(r"\\?\Volume{00000000-0000}\dir"), None);
    }

    #[cfg(windows)]
    #[test]
    fn display_hides_the_verbatim_prefix() {
        assert_eq!(display(Path::new(r"\\?\C:\stroll")), r"C:\stroll");
        assert_eq!(display(Path::new(r"C:\stroll")), r"C:\stroll");
        assert_eq!(
            simplified(Path::new(r"\\?\C:\stroll")),
            PathBuf::from(r"C:\stroll")
        );
    }
}
