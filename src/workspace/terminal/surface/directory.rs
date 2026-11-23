use std::path::PathBuf;

use alacritty_terminal::vte::{Parser, Perform};
use percent_encoding::percent_decode;

#[derive(Default)]
pub(super) struct DirectoryReport {
    parser: Parser,
    directory: ReportedDirectory,
}

impl DirectoryReport {
    pub(super) fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.directory, bytes);
    }

    pub(super) fn path(&self) -> Option<&PathBuf> {
        self.directory.0.as_ref()
    }
}

#[derive(Default)]
struct ReportedDirectory(Option<PathBuf>);

impl Perform for ReportedDirectory {
    fn osc_dispatch(&mut self, params: &[&[u8]], _: bool) {
        let [b"7", uri] = params else { return };
        let Some((host, path)) = std::str::from_utf8(uri)
            .ok()
            .and_then(|uri| uri.strip_prefix("file://"))
            .and_then(|uri| uri.split_once('/'))
        else {
            return;
        };
        let Ok(path) = percent_decode(path.as_bytes()).decode_utf8() else {
            return;
        };
        if host.contains(['\\', ':', '@']) || path.contains('\0') {
            return;
        }
        let local_host = host.is_empty()
            || host.eq_ignore_ascii_case("localhost")
            || std::env::var("COMPUTERNAME").is_ok_and(|name| host.eq_ignore_ascii_case(&name));
        let path = if local_host {
            PathBuf::from(path.as_ref())
        } else {
            PathBuf::from(format!("//{host}/{path}"))
        };
        if path.is_absolute() {
            self.0 = Some(path);
        }
    }
}
