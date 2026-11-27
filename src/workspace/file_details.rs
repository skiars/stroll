use crate::repository::{FileDetails, FileReadIssue, MAX_FILE_BYTES, file_details};

use super::*;

pub(super) enum EditorLoad {
    Text(FileBuffer, Option<ServerProject>),
    Details(FileInformation),
}

#[derive(Clone)]
pub(super) struct FileInformation {
    pub(super) issue: FileReadIssue,
    pub(super) metadata: Option<FileDetails>,
}

impl EditorLoad {
    pub(super) fn read(repo: Option<&Repository>, path: &Path, store: &Store) -> Result<Self> {
        let document = match repo {
            Some(repo) => Document::open(repo, path, store),
            None => Document::open_local(path, store),
        };
        let document = match document {
            Ok(document) => document,
            Err(error) => {
                let Some(&issue) = error.downcast_ref::<FileReadIssue>() else {
                    return Err(error);
                };
                match Document::recover_uneditable(repo, path, store, issue)? {
                    Some(draft) => draft,
                    None => {
                        return Ok(Self::Details(FileInformation {
                            issue,
                            metadata: file_details(repo, path).ok(),
                        }));
                    }
                }
            }
        };
        let buffer = FileBuffer::new(document);
        let (buffer, project) = match repo {
            Some(repo) => buffer.with_project(repo, path),
            None => (buffer, ServerProject::for_local_file(path)),
        };
        Ok(Self::Text(buffer, project))
    }
}

pub(super) fn reason(issue: FileReadIssue) -> SharedString {
    match issue {
        FileReadIssue::Binary => t!("file_details.binary"),
        FileReadIssue::UnsupportedEncoding => t!("file_details.unsupported"),
        FileReadIssue::Utf16 => t!("file_details.encoding", encoding = "UTF-16"),
        FileReadIssue::Utf32 => t!("file_details.encoding", encoding = "UTF-32"),
        FileReadIssue::TooLarge(_) => t!(
            "file_details.too_large",
            limit = MAX_FILE_BYTES / (1024 * 1024)
        ),
        FileReadIssue::PermissionDenied => t!("file_details.permission"),
    }
    .into()
}

pub(super) fn encoding(issue: FileReadIssue) -> Option<&'static str> {
    match issue {
        FileReadIssue::Utf16 => Some("UTF-16"),
        FileReadIssue::Utf32 => Some("UTF-32"),
        _ => None,
    }
}

impl Workspace {
    pub(super) fn install_editor_load(
        &mut self,
        path: &Path,
        load: EditorLoad,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match load {
            EditorLoad::Text(buffer, project) => {
                if !self.files.contains_key(path) {
                    self.install_file(path, buffer, project, window, cx);
                }
            }
            EditorLoad::Details(details) if !self.files.contains_key(path) => {
                let format = match details.issue {
                    FileReadIssue::Binary => Some(file_icons::KnownFormat::Binary),
                    FileReadIssue::Utf16 | FileReadIssue::Utf32 => {
                        Some(file_icons::KnownFormat::Text)
                    }
                    _ => None,
                };
                let formats = Rc::make_mut(&mut self.file_formats);
                if let Some(format) = format {
                    formats.insert(path.to_owned(), format);
                } else {
                    formats.remove(path);
                }
                self.file_information.insert(path.to_owned(), details);
            }
            EditorLoad::Details(_) => {}
        }
    }

    pub(super) fn active_file_information(&self) -> Option<(&Path, &FileInformation)> {
        let path = self.tabs.active()?;
        Some((path, self.file_information.get(path)?))
    }

    pub(super) fn file_information_content(
        &self,
        path: &Path,
        details: &FileInformation,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement {
        let full_path = self
            .repository
            .as_ref()
            .map_or_else(|| path.to_owned(), |repo| repo.root.join(path));
        let display = paths::display(&full_path);
        let local = path.is_absolute()
            || self
                .repository
                .as_ref()
                .is_none_or(|repo| repo.ssh.is_none());
        let size = match details.issue {
            FileReadIssue::TooLarge(size) => {
                Some(size.max(details.metadata.as_ref().map_or(0, |m| m.size)))
            }
            _ => details.metadata.as_ref().map(|metadata| metadata.size),
        };
        let modified = details
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.modified);
        let row = |label: SharedString, value: SharedString| {
            h_flex()
                .items_start()
                .gap_3()
                .child(
                    div()
                        .w(px(110.))
                        .flex_shrink_0()
                        .text_color(colors.muted)
                        .child(label),
                )
                .child(div().min_w_0().flex_1().child(value))
        };
        let retry_path = path.to_owned();
        let manager = self.system_files.clone();
        div()
            .id("editor-file-information")
            .debug_selector(|| "editor-file-information".into())
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .max_w(px(680.))
                    .mx_auto()
                    .p_6()
                    .gap_4()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_start()
                            .child(file_type_icon(
                                self.repository.as_deref(),
                                &self.config.file_associations,
                                path,
                                self.file_formats.get(path).copied(),
                                colors,
                                22.,
                            ))
                            .child(
                                div()
                                    .min_w_0()
                                    .font_semibold()
                                    .text_lg()
                                    .child(file_name(path).to_owned()),
                            ),
                    )
                    .child(
                        div()
                            .debug_selector(|| "editor-file-information-reason".into())
                            .text_color(colors.muted)
                            .child(reason(details.issue)),
                    )
                    .child(
                        v_flex()
                            .border_t_1()
                            .border_color(colors.border)
                            .pt_3()
                            .gap_2()
                            .child(row(t!("hover.path").into(), display.clone().into()))
                            .when_some(size, |list, size| {
                                list.child(row(
                                    t!("hover.size").into(),
                                    t!("hover.bytes", count = size).into(),
                                ))
                            })
                            .when_some(modified, |list, modified| {
                                list.child(row(
                                    t!("hover.modified").into(),
                                    hover_info::modified_label(modified).into(),
                                ))
                            })
                            .when_some(encoding(details.issue), |list, encoding| {
                                list.child(row(t!("hover.encoding").into(), encoding.into()))
                            }),
                    )
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                quiet_button("details-copy-path")
                                    .small()
                                    .icon(IconName::Copy)
                                    .label(t!("tabs.copy_absolute_path"))
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            display.clone(),
                                        ))
                                    }),
                            )
                            .when(local && manager.can_reveal(), |actions| {
                                actions.child(
                                    quiet_button("details-reveal")
                                        .small()
                                        .icon(IconName::FolderOpen)
                                        .label(manager.manager_label(false))
                                        .on_click(move |_, window, cx| {
                                            manager.clone().open(
                                                full_path.clone(),
                                                true,
                                                window,
                                                cx,
                                            )
                                        }),
                                )
                            })
                            .child(
                                quiet_button("details-retry")
                                    .small()
                                    .label(t!("file_details.retry"))
                                    .disabled(self.loading_file.is_some())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_file(
                                            retry_path.clone(),
                                            this.tabs.is_preview(&retry_path),
                                            window,
                                            cx,
                                        );
                                    })),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::FileReadIssue::{Binary, TooLarge, UnsupportedEncoding, Utf16};

    #[::core::prelude::v1::test]
    fn editor_load_keeps_drafts_and_distinguishes_content_failures() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::new(temp.path().join("state"))?;
        let root = temp.path().canonicalize()?;
        let binary = root.join("program");
        let encoded = root.join("encoded.txt");
        let unknown = root.join("unknown");
        let oversized = root.join("oversized.txt");
        fs::write(&binary, b"\x7fELF\0sample")?;
        fs::write(&encoded, b"\xff\xfeA\0")?;
        fs::write(&unknown, b"\xffABC")?;
        fs::File::create(&oversized)?.set_len(MAX_FILE_BYTES + 1)?;
        for (path, expected) in [
            (&binary, Binary),
            (&encoded, Utf16),
            (&unknown, UnsupportedEncoding),
            (&oversized, TooLarge(MAX_FILE_BYTES + 1)),
        ] {
            let EditorLoad::Details(details) = EditorLoad::read(None, path, &store)? else {
                panic!("expected file details for {}", path.display());
            };
            assert_eq!(details.issue, expected);
            assert_eq!(details.metadata.unwrap().size, path.metadata()?.len());
        }
        assert!(EditorLoad::read(None, &root.join("missing"), &store).is_err());
        store.update_editor_draft(None, &binary, Some([1; 32]), Some("unsaved draft"))?;
        let EditorLoad::Text(buffer, _) = EditorLoad::read(None, &binary, &store)? else {
            panic!("expected the recovered draft");
        };
        assert_eq!(buffer.document.text, "unsaved draft");
        assert_eq!(buffer.document.disk_issue, Some(Binary));
        assert!(!buffer.document.missing);
        Ok(())
    }
}
