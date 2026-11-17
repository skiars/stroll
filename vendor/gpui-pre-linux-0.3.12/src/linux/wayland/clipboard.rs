use std::{
    fs::File,
    io::{ErrorKind, Write},
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
};

use calloop::{LoopHandle, PostAction};
use filedescriptor::Pipe;
use strum::IntoEnumIterator;
use wayland_client::{Connection, protocol::wl_data_offer::WlDataOffer};
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_offer_v1::ZwpPrimarySelectionOfferV1;

use crate::linux::platform::{PIPE_READ_TIMEOUT, read_fd_with_timeout};
use gpui::{ClipboardEntry, ClipboardItem, Image, ImageFormat, hash};

/// Text mime types that we'll offer to other programs.
pub(crate) const TEXT_MIME_TYPES: [&str; 3] =
    ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];
pub(crate) const FILE_LIST_MIME_TYPE: &str = "text/uri-list";
pub(crate) const FILE_CLIPBOARD_MIME_TYPES: [&str; 3] = [
    FILE_LIST_MIME_TYPE,
    "application/x-kde-cutselection",
    "x-special/gnome-copied-files",
];

fn decode_file_list(bytes: &[u8], gnome: bool, cut: bool) -> Option<ClipboardItem> {
    let mut lines = std::str::from_utf8(bytes).ok()?.lines();
    let cut = if gnome {
        match lines.next()? {
            "cut" => true,
            "copy" => false,
            _ => return None,
        }
    } else {
        cut
    };
    let paths = lines
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| url::Url::parse(line).ok()?.to_file_path().ok())
        .collect::<Option<_>>()?;
    let paths = gpui::ExternalPaths(paths);
    (!paths.paths().is_empty()).then(|| ClipboardItem::new_file_list(paths, cut))
}

fn encode_file_list(item: &ClipboardItem, mime_type: &str) -> Option<Vec<u8>> {
    let (paths, cut) = item.file_list()?;
    let gnome = match mime_type {
        "application/x-kde-cutselection" => return Some(if cut { vec![b'1'] } else { vec![b'0'] }),
        "x-special/gnome-copied-files" => true,
        FILE_LIST_MIME_TYPE => false,
        _ => return None,
    };
    let mut list = if gnome {
        if cut {
            "cut".to_owned()
        } else {
            "copy".to_owned()
        }
    } else {
        String::new()
    };
    for path in paths.paths() {
        let uri = url::Url::from_file_path(path).ok()?;
        if gnome {
            list.push('\n');
        }
        list.push_str(uri.as_str());
        if !gnome {
            list.push_str("\r\n");
        }
    }
    Some(list.into_bytes())
}

/// Text mime types that we'll accept from other programs.
pub(crate) const ALLOWED_TEXT_MIME_TYPES: [&str; 2] = ["text/plain;charset=utf-8", "UTF8_STRING"];

pub(crate) struct Clipboard {
    connection: Connection,
    loop_handle: LoopHandle<'static, ()>,
    self_mime: String,

    // Internal clipboard
    contents: Option<ClipboardItem>,
    primary_contents: Option<ClipboardItem>,

    // External clipboard
    cached_read: Option<ClipboardItem>,
    current_offer: Option<DataOffer<WlDataOffer>>,
    cached_primary_read: Option<ClipboardItem>,
    current_primary_offer: Option<DataOffer<ZwpPrimarySelectionOfferV1>>,
}

pub(crate) trait ReceiveData {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>);
}

impl ReceiveData for WlDataOffer {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }
}

impl ReceiveData for ZwpPrimarySelectionOfferV1 {
    fn receive_data(&self, mime_type: String, fd: BorrowedFd<'_>) {
        self.receive(mime_type, fd);
    }
}

#[derive(Clone, Debug)]
/// Wrapper for `WlDataOffer` and `ZwpPrimarySelectionOfferV1`, used to help track mime types.
pub(crate) struct DataOffer<T: ReceiveData> {
    pub inner: T,
    mime_types: Vec<String>,
}

impl<T: ReceiveData> DataOffer<T> {
    pub fn new(offer: T) -> Self {
        Self {
            inner: offer,
            mime_types: Vec::new(),
        }
    }

    pub fn add_mime_type(&mut self, mime_type: String) {
        self.mime_types.push(mime_type)
    }

    fn has_mime_type(&self, mime_type: &str) -> bool {
        self.mime_types.iter().any(|t| t == mime_type)
    }

    fn read_bytes(&self, connection: &Connection, mime_type: &str) -> Option<Vec<u8>> {
        let pipe = Pipe::new().unwrap();
        self.inner.receive_data(mime_type.to_string(), unsafe {
            BorrowedFd::borrow_raw(pipe.write.as_raw_fd())
        });
        let fd = pipe.read;
        drop(pipe.write);

        connection.flush().unwrap();

        match read_fd_with_timeout(fd, PIPE_READ_TIMEOUT) {
            Ok(bytes) => Some(bytes),
            Err(err) => {
                log::error!("error reading clipboard pipe: {err:?}");
                None
            }
        }
    }

    fn read_text(&self, connection: &Connection) -> Option<ClipboardItem> {
        let mime_type = self.mime_types.iter().find(|&mime_type| {
            ALLOWED_TEXT_MIME_TYPES
                .iter()
                .any(|&allowed| allowed == mime_type)
        })?;
        let bytes = self.read_bytes(connection, mime_type)?;
        let text_content = match String::from_utf8(bytes) {
            Ok(content) => content,
            Err(e) => {
                log::error!("Failed to convert clipboard content to UTF-8: {}", e);
                return None;
            }
        };

        // Normalize the text to unix line endings, otherwise
        // copying from eg: firefox inserts a lot of blank
        // lines, and that is super annoying.
        let result = text_content.replace("\r\n", "\n");
        Some(ClipboardItem::new_string(result))
    }

    fn read_image(&self, connection: &Connection) -> Option<ClipboardItem> {
        for format in ImageFormat::iter() {
            let mime_type = format.mime_type();
            if !self.has_mime_type(mime_type) {
                continue;
            }

            if let Some(bytes) = self.read_bytes(connection, mime_type) {
                let id = hash(&bytes);
                return Some(ClipboardItem {
                    entries: vec![ClipboardEntry::Image(Image { format, bytes, id })],
                });
            }
        }
        None
    }

    fn read_files(&self, connection: &Connection) -> Option<ClipboardItem> {
        let [uris, kde_cut, gnome] = FILE_CLIPBOARD_MIME_TYPES;
        if self.has_mime_type(gnome)
            && let Some(item) = self
                .read_bytes(connection, gnome)
                .and_then(|bytes| decode_file_list(&bytes, true, false))
        {
            return Some(item);
        }
        if !self.has_mime_type(uris) {
            return None;
        }
        let bytes = self.read_bytes(connection, uris)?;
        let cut = self.has_mime_type(kde_cut)
            && self
                .read_bytes(connection, kde_cut)
                .is_some_and(|bytes| bytes == b"1");
        decode_file_list(&bytes, false, cut)
    }
}

impl Clipboard {
    pub fn new(connection: Connection, loop_handle: LoopHandle<'static, ()>) -> Self {
        Self {
            connection,
            loop_handle,
            self_mime: format!("pid/{}", std::process::id()),

            contents: None,
            primary_contents: None,

            cached_read: None,
            current_offer: None,
            cached_primary_read: None,
            current_primary_offer: None,
        }
    }

    pub fn set(&mut self, item: ClipboardItem) {
        self.contents = Some(item);
    }

    pub fn set_primary(&mut self, item: ClipboardItem) {
        self.primary_contents = Some(item);
    }

    pub fn set_offer(&mut self, data_offer: Option<DataOffer<WlDataOffer>>) {
        self.cached_read = None;
        self.current_offer = data_offer;
    }

    pub fn set_primary_offer(&mut self, data_offer: Option<DataOffer<ZwpPrimarySelectionOfferV1>>) {
        self.cached_primary_read = None;
        self.current_primary_offer = data_offer;
    }

    pub fn self_mime(&self) -> String {
        self.self_mime.clone()
    }

    pub fn send(&self, mime_type: String, fd: OwnedFd) {
        if let Some(item) = &self.contents {
            let bytes = if FILE_CLIPBOARD_MIME_TYPES.contains(&mime_type.as_str()) {
                encode_file_list(item, &mime_type)
            } else {
                item.text().map(String::into_bytes)
            };
            if let Some(bytes) = bytes {
                self.send_bytes(fd, bytes);
            }
        }
    }

    pub fn send_primary(&self, _mime_type: String, fd: OwnedFd) {
        if let Some(text) = self
            .primary_contents
            .as_ref()
            .and_then(|contents| contents.text())
        {
            self.send_bytes(fd, text.as_bytes().to_owned());
        }
    }

    pub fn read(&mut self) -> Option<ClipboardItem> {
        let offer = self.current_offer.as_ref()?;
        if let Some(cached) = self.cached_read.clone() {
            return Some(cached);
        }

        if offer.has_mime_type(&self.self_mime) {
            return self.contents.clone();
        }

        let item = offer
            .read_files(&self.connection)
            .or_else(|| offer.read_text(&self.connection))
            .or_else(|| offer.read_image(&self.connection))?;

        self.cached_read = Some(item.clone());
        Some(item)
    }

    pub fn read_primary(&mut self) -> Option<ClipboardItem> {
        let offer = self.current_primary_offer.as_ref()?;
        if let Some(cached) = self.cached_primary_read.clone() {
            return Some(cached);
        }

        if offer.has_mime_type(&self.self_mime) {
            return self.primary_contents.clone();
        }

        let item = offer
            .read_text(&self.connection)
            .or_else(|| offer.read_image(&self.connection))?;

        self.cached_primary_read = Some(item.clone());
        Some(item)
    }

    pub fn send_bytes(&self, fd: OwnedFd, bytes: Vec<u8>) {
        let mut written = 0;
        self.loop_handle
            .insert_source(
                calloop::generic::Generic::new(
                    File::from(fd),
                    calloop::Interest::WRITE,
                    calloop::Mode::Level,
                ),
                move |_, file, _| {
                    let file = unsafe { file.get_mut() };
                    loop {
                        match file.write(&bytes[written..]) {
                            Ok(n) if written + n == bytes.len() => {
                                written += n;
                                break Ok(PostAction::Remove);
                            }
                            Ok(n) => written += n,
                            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                                break Ok(PostAction::Continue);
                            }
                            Err(_) => break Ok(PostAction::Remove),
                        }
                    }
                },
            )
            .unwrap();
    }
}

#[cfg(test)]
mod file_list_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn file_clipboard_payload_preserves_paths_and_the_cut_action() {
        let paths = gpui::ExternalPaths(
            ["/tmp/with space", "/tmp/音#1\nnotes.txt"]
                .into_iter()
                .map(PathBuf::from)
                .collect(),
        );
        let [uris, kde_cut, gnome] = FILE_CLIPBOARD_MIME_TYPES;
        for cut in [false, true] {
            let item = ClipboardItem::new_file_list(paths.clone(), cut);
            let marker = encode_file_list(&item, kde_cut).unwrap();
            let uri_list = encode_file_list(&item, uris).unwrap();
            let gnome_list = encode_file_list(&item, gnome).unwrap();
            assert_eq!(
                decode_file_list(&uri_list, false, marker == b"1"),
                Some(item.clone())
            );
            assert_eq!(decode_file_list(&gnome_list, true, false), Some(item));
        }
        assert!(decode_file_list(b"https://example.com", false, true).is_none());
        assert!(decode_file_list(b"link\nfile:///tmp/file", true, false).is_none());
    }
}
