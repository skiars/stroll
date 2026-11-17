use std::{path::PathBuf, sync::Arc};

use gpui::{App, AppContext as _, Task, WeakEntity, Window};
use gpui_component::input::{DefinitionProvider, EditorState, Rope, RopeExt as _};
use lsp_types::{LocationLink, Range};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use url::Url;

use crate::{
    docs::{self, Destination},
    lsp::Document,
    repository::Repository,
};

#[derive(Clone)]
pub(super) struct MarkdownLinks {
    pub path: PathBuf,
    pub repository: Option<Arc<Repository>>,
    pub server: Option<Arc<Document>>,
    pub editor: WeakEntity<EditorState>,
}

impl MarkdownLinks {
    fn file_link(&self, text: &Rope, offset: usize) -> Option<LocationLink> {
        let (range, target) = docs::inline_link_at(&text.to_string(), offset)?;
        let Destination::Local { path, fragment } = docs::destination(&self.path, &target)? else {
            return None;
        };
        let mut uri = if path.is_absolute() {
            Url::from_file_path(path).ok()?
        } else {
            let repository = self.repository.as_ref()?;
            match &repository.ssh {
                Some(ssh) => {
                    let relative = crate::remote::RepoPath::from_local(&path).ok()?;
                    let root = ssh.target().path.to_str()?.trim_end_matches('/');
                    Url::parse(
                        &crate::remote::file_uri(&format!("{root}/{}", relative.as_str())).ok()?,
                    )
                    .ok()?
                }
                None => Url::from_file_path(repository.root.join(path)).ok()?,
            }
        };
        if let Some(fragment) = fragment {
            uri.set_fragment(Some(
                &utf8_percent_encode(&fragment, NON_ALPHANUMERIC).to_string(),
            ));
        }
        Some(LocationLink {
            origin_selection_range: Some(Range::new(
                text.offset_to_position(range.start),
                text.offset_to_position(range.end),
            )),
            target_uri: uri.as_str().parse().ok()?,
            target_range: Range::default(),
            target_selection_range: Range::default(),
        })
    }
}

impl DefinitionProvider for MarkdownLinks {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Vec<LocationLink>>> {
        let links = self.clone();
        let server = self.server.clone().map(|server| {
            let version = server.synced_version();
            (server, version)
        });
        let editor = self.editor.clone();
        let text = text.clone();
        let local = {
            let text = text.clone();
            cx.background_spawn(async move { links.file_link(&text, offset) })
        };
        window.spawn(cx, async move |cx| {
            if let Some(link) = local.await {
                return Ok(vec![link]);
            }
            let Some((server, version)) = server else {
                return Ok(Vec::new());
            };
            if !cx.update(|_, cx| {
                editor
                    .upgrade()
                    .is_some_and(|editor| editor.read(cx).text() == &text)
            })? {
                return Ok(Vec::new());
            }
            Ok(server.editor_definitions(text, offset, Some(version)).await)
        })
    }
}
