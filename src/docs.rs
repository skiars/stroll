use rust_i18n::t;

use std::{
    collections::{BTreeSet, HashMap},
    ops::Range,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context as _, Result};
#[cfg(test)]
use core::prelude::v1::test;
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme, Selectable as _, StyledExt,
    button::Button,
    h_flex,
    input::{DocumentFind, Escape},
    scroll::ScrollbarViewport as _,
    text::{
        MarkdownExtensions, MarkdownNode, MarkdownParseContext, MarkdownPlugin, RangeHighlight,
        TextView, TextViewState, TextViewStyle, data_url_image, markdown_ast::Node,
    },
    v_flex,
};
use gpui_kit_assets::IconName;
use percent_encoding::percent_decode_str;
use url::Url;

use crate::{
    doc_graphics::{self, GraphicKind, Graphics},
    paths,
    repository::{Entry, Repository},
    session::DocumentPosition,
    workspace::{
        Palette, PaneDrag, Workspace, compact_icon_button, icon, icon_button, pane_scrollbar,
        panel_resize, scroll_viewport_background,
    },
};

mod online_images;

pub fn is_markdown(path: &Path) -> bool {
    crate::repository::is_markdown(path)
}

pub(crate) fn is_root_readme(path: &Path) -> bool {
    path.parent()
        .is_some_and(|parent| parent.as_os_str().is_empty())
        && path
            .file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("readme"))
}

#[derive(Default)]
pub struct DocumentTree {
    pub children: HashMap<PathBuf, Vec<Entry>>,
    pub initial: Option<PathBuf>,
}

impl DocumentTree {
    pub fn from_repo(repo: &Repository) -> Self {
        let mut parents = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for entry in repo
            .children
            .values()
            .flatten()
            .filter(|entry| !entry.directory && !entry.symlink && is_markdown(&entry.path))
        {
            paths.insert(entry.path.clone());
            parents.extend(
                entry
                    .path
                    .ancestors()
                    .skip(1)
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(Path::to_owned),
            );
        }
        let children = repo
            .children
            .iter()
            .filter_map(|(path, children)| {
                let selected: Vec<_> = children
                    .iter()
                    .filter(|entry| {
                        if entry.directory {
                            parents.contains(&entry.path)
                        } else {
                            paths.contains(&entry.path)
                        }
                    })
                    .cloned()
                    .collect();
                (!selected.is_empty()).then(|| (path.clone(), selected))
            })
            .collect();
        let initial = paths
            .iter()
            .find(|path| {
                path.parent()
                    .is_some_and(|parent| parent.as_os_str().is_empty())
                    && path
                        .file_stem()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.eq_ignore_ascii_case("readme"))
            })
            .or_else(|| paths.first())
            .cloned();
        Self { children, initial }
    }

    pub(crate) fn from_paths(paths: Vec<PathBuf>) -> Self {
        let paths: BTreeSet<_> = paths.into_iter().collect();
        let initial = paths
            .iter()
            .find(|path| is_root_readme(path))
            .or_else(|| paths.first())
            .cloned();
        let mut children = HashMap::<PathBuf, Vec<Entry>>::new();
        let mut directories = BTreeSet::new();
        for path in paths {
            for parent in path
                .ancestors()
                .skip(1)
                .take_while(|path| !path.as_os_str().is_empty())
            {
                if !directories.insert(parent.to_owned()) {
                    break;
                }
                children
                    .entry(parent.parent().unwrap().to_owned())
                    .or_default()
                    .push(Entry {
                        path: parent.to_owned(),
                        directory: true,
                        symlink: false,
                        ignored: false,
                    });
            }
            children
                .entry(path.parent().unwrap().to_owned())
                .or_default()
                .push(Entry {
                    path,
                    directory: false,
                    symlink: false,
                    ignored: false,
                });
        }
        for entries in children.values_mut() {
            entries.sort_by(|a, b| (!a.directory, &a.path).cmp(&(!b.directory, &b.path)));
        }
        Self { children, initial }
    }
}

struct Heading {
    title: SharedString,
    slug: String,
    depth: u8,
    source: Range<usize>,
}

pub struct PreparedDocument {
    source: SharedString,
    headings: Vec<Heading>,
    extensions: MarkdownExtensions,
    images: Arc<HashMap<String, Arc<gpui::Image>>>,
}

impl PreparedDocument {
    pub(crate) fn title(&self) -> Option<&str> {
        self.headings
            .first()
            .map(|heading| heading.title.trim())
            .filter(|title| !title.is_empty())
    }

    pub fn new(source: String) -> Result<Self> {
        let ast = parse_markdown(&source)?;
        let headings = headings(&ast);
        let mut graphics = Graphics::default();
        let mut task_cells = HashMap::new();
        visit(&ast, &mut |node| {
            for kind in [
                GraphicKind::Diagram,
                GraphicKind::DisplayMath,
                GraphicKind::InlineMath,
            ] {
                if let Some(source) = kind.source(node) {
                    graphics.prepare(kind, source);
                }
            }
            if let Node::Table(table) = node {
                collect_task_cells(table, &source, &mut task_cells);
            }
        });
        Ok(Self {
            source: source.into(),
            headings,
            images: Arc::default(),
            extensions: doc_graphics::extensions(Arc::new(graphics)).plugin(TaskTable(task_cells)),
        })
    }
}

fn parse_markdown(source: &str) -> Result<Node> {
    let mut options = markdown::ParseOptions::gfm();
    options.constructs.math_flow = true;
    options.constructs.math_text = true;
    options.constructs.frontmatter = true;
    markdown::to_mdast(source, &options).map_err(|error| anyhow::anyhow!("{error}"))
}

fn headings(ast: &Node) -> Vec<Heading> {
    let mut headings = Vec::new();
    let mut used = BTreeSet::new();
    visit(ast, &mut |node| {
        let Node::Heading(heading) = node else { return };
        let Some(position) = &heading.position else {
            return;
        };
        let title: String = heading.children.iter().map(ToString::to_string).collect();
        let base = slug(&title);
        let base = if base.is_empty() {
            format!("section-{}", headings.len() + 1)
        } else {
            base
        };
        let mut slug = base.clone();
        let mut suffix = 1;
        while !used.insert(slug.clone()) {
            slug = format!("{base}-{suffix}");
            suffix += 1;
        }
        headings.push(Heading {
            title: title.into(),
            slug,
            depth: heading.depth,
            source: position.start.offset..position.end.offset,
        });
    });
    headings
}

fn heading_index(headings: &[Heading], fragment: &str) -> Option<usize> {
    let fragment = fragment.strip_prefix("md-").unwrap_or(fragment);
    headings
        .iter()
        .position(|heading| heading.slug == fragment)
        .or_else(|| {
            let normalized = slug(fragment);
            headings
                .iter()
                .position(|heading| heading.slug == normalized)
        })
}

pub(crate) fn heading_source_offset(source: &str, fragment: &str) -> Option<usize> {
    let headings = headings(&parse_markdown(source).ok()?);
    Some(headings[heading_index(&headings, fragment)?].source.start)
}

pub(crate) fn inline_link_at(source: &str, offset: usize) -> Option<(Range<usize>, String)> {
    let ast = parse_markdown(source).ok()?;
    let mut result = None;
    visit(&ast, &mut |node| {
        if let Node::Link(link) = node
            && let Some(position) = &link.position
        {
            let range = position.start.offset..position.end.offset;
            if range.contains(&offset) {
                result = Some((range, link.url.clone()));
            }
        }
    });
    result
}

fn collect_task_cells(
    table: &markdown::mdast::Table,
    source: &str,
    tasks: &mut HashMap<usize, bool>,
) {
    for row in table.children.iter().skip(1) {
        let Some(Node::TableCell(cell)) = row.children().and_then(|cells| cells.first()) else {
            continue;
        };
        let [Node::Text(text)] = cell.children.as_slice() else {
            continue;
        };
        if let Some(position) = &text.position
            && let Some(raw) = source.get(position.start.offset..position.end.offset)
        {
            match raw {
                "[ ]" => {
                    tasks.insert(position.start.offset, false);
                }
                "[x]" | "[X]" => {
                    tasks.insert(position.start.offset, true);
                }
                _ => {}
            }
        }
    }
}

struct TaskTable(HashMap<usize, bool>);

impl MarkdownPlugin for TaskTable {
    fn name(&self) -> &str {
        "table-task"
    }

    fn parse(&self, node: &Node, context: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let Node::Text(text) = node else { return None };
        let start = text.position.as_ref()?.start.offset + context.offset();
        let checked = *self.0.get(&start)?;
        Some(
            MarkdownNode::new("table-task", checked)
                .text(text.value.clone())
                .markdown(text.value.clone())
                .accessibility_label(if checked {
                    t!("docs.checked")
                } else {
                    t!("docs.unchecked")
                }),
        )
    }

    fn render(&self, node: &MarkdownNode, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let checked = node.data::<bool>().copied().unwrap_or(false);
        let color = if checked {
            cx.theme().accent
        } else {
            cx.theme().muted_foreground
        };
        icon(
            if checked {
                IconName::SquareCheck
            } else {
                IconName::Square
            },
            color,
            14.,
        )
    }
}

fn visit(node: &Node, f: &mut impl FnMut(&Node)) {
    f(node);
    if let Some(children) = node.children() {
        for child in children {
            visit(child, f);
        }
    }
}

fn slug(value: &str) -> String {
    let mut output = String::new();
    for ch in value.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() || ch == '_' {
            output.push(ch);
        } else if (ch.is_whitespace() || ch == '-') && !output.is_empty() && !output.ends_with('-')
        {
            output.push('-');
        }
    }
    output.trim_end_matches('-').to_owned()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Destination {
    Local {
        path: PathBuf,
        fragment: Option<String>,
    },
    External(Url),
}

pub fn destination(current: &Path, target: &str) -> Option<Destination> {
    if target.starts_with("//") {
        return Url::parse(&format!("https:{target}"))
            .ok()
            .map(Destination::External);
    }
    if let Ok(url) = Url::parse(target) {
        return matches!(url.scheme(), "http" | "https" | "mailto")
            .then_some(Destination::External(url));
    }
    let (raw_path, raw_fragment) = target
        .split_once('#')
        .map_or((target, None), |(path, fragment)| (path, Some(fragment)));
    let raw_path = raw_path.split_once('?').map_or(raw_path, |(path, _)| path);
    let decoded = percent_decode_str(raw_path).decode_utf8().ok()?;
    if decoded.contains(['\\', '\0']) {
        return None;
    }
    let path = if decoded.is_empty() {
        current.to_owned()
    } else {
        let (mut path, relative) = if let Some(rooted) = decoded.strip_prefix('/') {
            if current.is_absolute() {
                return None;
            }
            (PathBuf::new(), rooted)
        } else {
            (current.parent()?.to_owned(), decoded.as_ref())
        };
        for component in Path::new(relative).components() {
            match component {
                Component::Normal(name) if name != ".git" => path.push(name),
                Component::CurDir => {}
                Component::ParentDir if path.pop() => {}
                _ => return None,
            }
        }
        path
    };
    let fragment = raw_fragment
        .and_then(|value| percent_decode_str(value).decode_utf8().ok())
        .filter(|value| !value.is_empty())
        .map(|value| value.into_owned());
    Some(Destination::Local { path, fragment })
}

pub(crate) fn line_fragment(fragment: &str) -> Option<u32> {
    fragment
        .strip_prefix(['L', 'l'])?
        .parse::<u32>()
        .ok()
        .map(|line| line.saturating_sub(1))
}

fn local_image_source(root: &Path, current: &Path, uri: &str) -> Option<ImageSource> {
    let Destination::Local { path, .. } = destination(current, uri)? else {
        return None;
    };
    let target = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    let image = target.canonicalize().ok()?;
    (image.is_file()
        && image.components().all(|part| part.as_os_str() != ".git")
        && image
            .metadata()
            .is_ok_and(|metadata| metadata.len() <= 16 * 1024 * 1024))
    .then(|| image.into())
}

fn empty_image_source() -> ImageSource {
    // A missing local image has no fallback source.
    ImageSource::Custom(Arc::new(|_, _| None))
}

fn is_data_url(uri: &str) -> bool {
    uri.get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
}

type EmbeddedImages = Arc<Mutex<HashMap<SharedString, Option<Arc<Image>>>>>;

pub struct DocReader {
    typography: crate::config::Docs,
    root: PathBuf,
    pub path: PathBuf,
    document: PreparedDocument,
    online_images: Entity<online_images::DisplayedImages>,
    image_thumbnails: Entity<ImageThumbnails>,
    embedded_images: EmbeddedImages,
    text: Entity<TextViewState>,
    search: Entity<DocumentFind>,
    workspace: WeakEntity<Workspace>,
    _subscriptions: [Subscription; 2],
    show_outline: Option<bool>,
    outline_width: Pixels,
    standalone: bool,
    outline_was_visible: bool,
    outline_scroll: ScrollHandle,
    selected_heading: Option<usize>,
    body_position: Option<ListOffset>,
    heading_jump_block: Option<usize>,
    pending_heading: Option<usize>,
    pending_position: Option<DocumentPosition>,
}

impl DocReader {
    pub(crate) fn source_text(&self) -> SharedString {
        self.document.source.clone()
    }

    pub(crate) fn toolbar_actions(
        &self,
        reader: &Entity<Self>,
        window: &Window,
        cx: &App,
    ) -> impl IntoElement + use<> {
        let show_outline = self.outline_visible(window);
        let path = self.path.clone();
        h_flex()
            .flex_shrink_0()
            .gap_1()
            .child(
                icon_button("document-find", IconName::Search, t!("docs.search"))
                    .selected(self.search.read(cx).is_open())
                    .toggled(self.search.read(cx).is_open())
                    .on_click({
                        let reader = reader.clone();
                        move |_, window, cx| {
                            reader.update(cx, |doc, cx| doc.toggle_find(window, cx));
                        }
                    }),
            )
            .child(
                icon_button(
                    "document-outline-toggle",
                    IconName::List,
                    t!("docs.document_outline"),
                )
                .selected(show_outline)
                .toggled(show_outline)
                .on_click({
                    let reader = reader.clone();
                    move |_, _, cx| {
                        reader.update(cx, |doc, cx| {
                            doc.show_outline = Some(!show_outline);
                            _ = doc.workspace.update(cx, |workspace, _| {
                                workspace.outline_visibility = doc.show_outline;
                            });
                            cx.notify();
                        });
                    }
                }),
            )
            .child(
                icon_button(
                    "document-source",
                    IconName::FileCode,
                    t!("docs.open_source"),
                )
                .on_click({
                    let workspace = self.workspace.clone();
                    move |_, window, cx| {
                        _ = workspace.update(cx, |workspace, cx| {
                            workspace.open_document_source(path.clone(), window, cx);
                        });
                    }
                }),
            )
    }

    fn outline_visible(&self, window: &Window) -> bool {
        self.show_outline
            .unwrap_or(self.standalone || window.viewport_size().width >= px(1000.))
            && !self.document.headings.is_empty()
    }

    pub(crate) fn set_outline_layout(
        &mut self,
        width: Pixels,
        visible: Option<bool>,
        standalone: bool,
    ) {
        self.outline_width = width;
        self.show_outline = visible;
        self.standalone = standalone;
    }

    pub(crate) fn refresh_language(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |search, cx| {
            search.set_placeholder(t!("docs.find"), window, cx);
        });
        cx.notify();
    }

    pub fn new(
        root: PathBuf,
        path: PathBuf,
        document: PreparedDocument,
        fragment: Option<String>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let text = cx.new(|cx| {
            TextViewState::markdown_with_extensions(
                &document.source,
                document.extensions.clone(),
                cx,
            )
            .scrollable(true)
        });
        text.read(cx).list_state().set_pixel_aligned_scroll(true);
        let search = DocumentFind::for_document(text.clone(), window, cx);
        search.update(cx, |search, cx| {
            search.set_placeholder(t!("docs.find"), window, cx)
        });
        let text_subscription = cx.observe(&text, |this: &mut Self, _, cx| this.reveal_pending(cx));
        let search_subscription = cx.observe(&search, |this: &mut Self, search, cx| {
            if search.read(cx).has_matches() {
                this.heading_jump_block = None;
            }
            cx.notify();
        });
        let mut reader = Self {
            typography: crate::config::Docs::default(),
            root,
            path,
            document,
            online_images: online_images::DisplayedImages::new(cx),
            image_thumbnails: ImageThumbnails::new(cx),
            embedded_images: Arc::default(),
            text,
            search,
            workspace,
            _subscriptions: [text_subscription, search_subscription],
            show_outline: None,
            outline_width: px(195.),
            standalone: false,
            outline_was_visible: false,
            outline_scroll: ScrollHandle::new(),
            selected_heading: None,
            body_position: None,
            heading_jump_block: None,
            pending_heading: None,
            pending_position: None,
        };
        if let Some(fragment) = fragment {
            reader.jump_to_fragment(&fragment, cx);
        }
        reader
    }

    pub(crate) fn release_online_images(&mut self, cx: &mut Context<Self>) {
        self.online_images = online_images::DisplayedImages::new(cx);
        self.image_thumbnails = ImageThumbnails::new(cx);
        cx.notify();
    }

    pub(crate) fn session_position(&self, cx: &App) -> Option<DocumentPosition> {
        if self.pending_position.is_some() || self.pending_heading.is_some() {
            return None;
        }
        let text = self.text.read(cx);
        let list = text.list_state();
        if list.item_count() == 0 {
            return None;
        }
        let top = list.logical_scroll_top();
        Some(DocumentPosition {
            source_offset: text.source_offset_for_block(top.item_ix),
            block: top.item_ix,
            inset: top.offset_in_item.into(),
        })
    }

    pub(crate) fn restore_position(&mut self, position: Option<DocumentPosition>) {
        if position.is_some() {
            self.heading_jump_block = None;
        }
        self.pending_position = position;
    }

    fn apply_saved_position(&mut self, cx: &mut Context<Self>) {
        let text = self.text.read(cx);
        let list = text.list_state();
        if list.item_count() == 0 {
            return;
        }
        if let Some(position) = self.pending_position.take() {
            let block = position
                .source_offset
                .and_then(|offset| text.block_for_source_offset(offset))
                .unwrap_or(position.block)
                .min(list.item_count() - 1);
            list.scroll_to(ListOffset {
                item_ix: block,
                offset_in_item: px(position.inset),
            });
            cx.notify();
        }
    }

    pub(crate) fn set_typography(
        &mut self,
        typography: crate::config::Docs,
        cx: &mut Context<Self>,
    ) {
        self.typography = typography;
        cx.notify();
    }

    pub fn toggle_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.toggle(window, cx));
    }

    pub(crate) fn locate_source(
        &mut self,
        range: Range<usize>,
        fingerprint: [u8; 32],
        cx: &mut Context<Self>,
    ) {
        let source = self.document.source.as_str();
        let normalized = source.replace("\r\n", "\n");
        if crate::locator::fingerprint(&normalized) != fingerprint {
            return;
        }
        let to_source_offset = |offset| {
            let mut relative = 0;
            for (index, byte) in source.bytes().enumerate() {
                if byte == b'\r' && source.as_bytes().get(index + 1) == Some(&b'\n') {
                    continue;
                }
                if relative == offset {
                    return index;
                }
                relative += 1;
            }
            source.len()
        };
        let source = to_source_offset(range.start)..to_source_offset(range.end);
        self.heading_jump_block = None;
        let accent = cx.theme().accent.alpha(0.48);
        self.text.update(cx, |text, cx| {
            let rendered = text.rendered_text();
            if source.is_empty() {
                let start = source.start;
                let end = rendered.source()[start..]
                    .chars()
                    .next()
                    .map_or(start, |character| start + character.len_utf8());
                let range = rendered.range_for_source(start..end).or_else(|| {
                    let line_start = rendered.source()[..start]
                        .rfind('\n')
                        .map_or(0, |newline| newline + 1);
                    let line_end = rendered.source()[start..]
                        .find('\n')
                        .map_or(rendered.source().len(), |newline| start + newline);
                    rendered.range_for_source(line_start..line_end)
                });
                if let Some(range) = range {
                    _ = text.reveal_range(range.start..range.start, cx);
                }
            } else if let Some(range) = rendered.range_for_source(source) {
                _ = text.set_range_highlights([RangeHighlight::new(range.clone(), accent)], cx);
                _ = text.reveal_range(range, cx);
            }
        });
    }

    pub fn jump_to_fragment(&mut self, fragment: &str, cx: &mut Context<Self>) -> bool {
        let Some(index) = heading_index(&self.document.headings, fragment) else {
            return false;
        };
        self.jump_to_heading(index, cx);
        true
    }

    fn jump_to_heading(&mut self, index: usize, cx: &mut Context<Self>) {
        self.pending_position = None;
        self.heading_jump_block = None;
        self.select_heading(index);
        self.pending_heading = Some(index);
        self.reveal_pending(cx);
        cx.notify();
    }

    fn reveal_pending(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.pending_heading else {
            return;
        };
        let text = self.text.read(cx);
        let rendered = text.rendered_text();
        if rendered.source() != self.document.source.as_ref() {
            return;
        }
        self.pending_heading = None;
        if let Some(range) = rendered.range_for_source(self.document.headings[index].source.clone())
        {
            self.heading_jump_block =
                text.block_for_source_offset(self.document.headings[index].source.start);
            self.text.update(cx, |text, cx| {
                _ = text.reveal_range(range.start..range.start, cx);
            });
        }
    }

    fn select_heading(&mut self, index: usize) {
        self.selected_heading = Some(index);
        self.outline_scroll.scroll_to_item(index);
    }

    fn sync_outline(&mut self, cx: &mut Context<Self>) {
        if self.pending_heading.is_some() || self.pending_position.is_some() {
            return;
        }
        let text = self.text.read(cx);
        let list = text.list_state();
        let top = list.logical_scroll_top();
        if list.item_count() == 0 || self.document.headings.is_empty() {
            return;
        }
        let previous = self.body_position.replace(top);
        // An explicit reveal can place a heading anywhere within the visible page.
        // Keep the clicked heading until the reader moves away from that position.
        if let Some(target) = self.heading_jump_block {
            if list
                .bounds_for_item(target)
                .is_some_and(|bounds| list.viewport_bounds().intersects(&bounds))
            {
                self.heading_jump_block = None;
            }
            return;
        }
        if previous == Some(top) {
            return;
        }
        let reading_line = list.viewport_bounds().top() + px(16.);
        let mut block = top.item_ix;
        while list
            .bounds_for_item(block + 1)
            .is_some_and(|bounds| bounds.top() <= reading_line)
        {
            block += 1;
        }
        let source = (0..=block)
            .rev()
            .find_map(|ix| text.source_offset_for_block(ix))
            .unwrap_or_default();
        let index = self
            .document
            .headings
            .partition_point(|heading| heading.source.start <= source)
            .saturating_sub(1);
        if self.selected_heading != Some(index) {
            self.select_heading(index);
            cx.notify();
        }
    }

    fn outline(&self, cx: &Context<Self>, colors: Palette) -> impl IntoElement {
        let headings = self
            .document
            .headings
            .iter()
            .enumerate()
            .map(|(index, heading)| {
                div()
                    .id(("heading", index))
                    .flex_shrink_0()
                    .px_2()
                    .py_1()
                    .pl(px(8. + 8. * f32::from(heading.depth.saturating_sub(2))))
                    .text_size(px(12.))
                    .line_height(relative(1.45))
                    .text_color(colors.muted)
                    .rounded_md()
                    .when(self.selected_heading == Some(index), |el| {
                        el.text_color(colors.accent).bg(colors.accent.opacity(0.12))
                    })
                    .cursor_pointer()
                    .hover(|el| el.bg(colors.hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.jump_to_heading(index, cx)))
                    .child(heading.title.clone())
            });
        v_flex()
            .id("document-outline")
            .h_full()
            .min_h_0()
            .relative()
            .w(self.outline_width)
            .max_w(relative(0.45))
            .flex_shrink_0()
            .pl_2()
            .pt_3()
            .child(panel_resize(
                "document-outline-resize",
                gpui_component::resizable::HandleEdge::Leading,
                px(0.),
            ))
            .child(
                div()
                    .flex_shrink_0()
                    .px_2()
                    .pb_2()
                    .text_size(px(12.))
                    .font_semibold()
                    .text_color(colors.muted)
                    .child(t!("docs.outline")),
            )
            .child(
                v_flex()
                    .id("document-outline-list")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .pr_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.outline_scroll)
                    .scrollbar_overlay(pane_scrollbar(&self.outline_scroll))
                    .children(headings),
            )
    }
}

impl Render for DocReader {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.pending_position.is_some() {
            cx.on_next_frame(window, |this, _, cx| this.apply_saved_position(cx));
        }
        let theme = cx.theme();
        let colors = Palette::for_mode(theme.mode);
        let dark = theme.mode == gpui_component::ThemeMode::Dark;
        let workspace = self.workspace.clone();
        let code_font = crate::workspace::ui_code_font(cx);
        let source_path = self.path.clone();
        let image_path = self.path.clone();
        let images = self.document.images.clone();
        let online_images = self.online_images.clone();
        let root = self.root.clone();
        let remote = !self.path.is_absolute()
            && root.to_str().is_some_and(|root| root.starts_with("ssh://"));
        let resolve_image = {
            Arc::new(move |uri: &str| -> Option<ImageSource> {
                if remote {
                    images.get(uri).cloned().map(ImageSource::Image)
                } else {
                    local_image_source(&root, &image_path, uri)
                }
            })
        };
        let embedded_image = {
            let images = self.standalone.then(|| self.embedded_images.clone());
            move |uri: &SharedString| -> Option<ImageSource> {
                images
                    .as_ref()?
                    .lock()
                    .unwrap()
                    .entry(uri.clone())
                    .or_insert_with(|| data_url_image(uri.as_ref()))
                    .clone()
                    .map(ImageSource::Image)
            }
        };
        let mut table = StyleRefinement::default().rounded(px(9.));
        table.overflow.x = Some(Overflow::Scroll);
        let style = TextViewStyle {
            heading_base_font_size: px(self.typography.body_font_size as f32),
            highlight_theme: theme.highlight_theme.clone(),
            inline_code: HighlightStyle {
                background_color: Some(colors.soft),
                ..HighlightStyle::default()
            },
            is_dark: dark,
            ..TextViewStyle::default()
        }
        .paragraph_gap(rems(0.9))
        .heading_font_size(|level, base| {
            base * match level {
                1 => 1.30,
                2 => 1.125,
                3 => 1.03,
                _ => 1.,
            }
        })
        .table(table)
        // Tables keep only their row rules: a transparent cell border color
        // leaves the column dividers painted by the row slack invisible.
        .table_cell(StyleRefinement::default().border_color(theme.transparent))
        .code_block(
            StyleRefinement::default()
                .rounded(px(9.))
                .border_0()
                .p(px(12.))
                .line_height(relative(1.5))
                .font_family(code_font.clone())
                .text_size(px(self.typography.code_block_font_size as f32)),
        );
        let code_languages = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).document_repository())
            .map_or_else(
                || crate::language_resources::LanguageResources::installed().code_languages(),
                |repo| repo.language_resources().code_languages(),
            );
        let body = TextView::new(&self.text)
            .code_languages(code_languages)
            .scrollable(true)
            .wheel_line_height(px(30.))
            .style(style)
            .inline_code_style(StyleRefinement::default().font_family(code_font))
            .text_size(px(self.typography.body_font_size as f32))
            .line_height(relative(1.72))
            .text_color(if dark { rgb(0xc5c8d0) } else { rgb(0x24262b) })
            .flex_1()
            .h_full()
            .min_h_0()
            .w_full()
            .max_w(px(860.))
            .px_5()
            .markdown_extensions(self.document.extensions.clone())
            .image_source({
                let resolve_image = resolve_image.clone();
                let thumbnails = self.image_thumbnails.clone();
                let embedded_image = embedded_image.clone();
                move |uri| {
                    let source = if is_data_url(uri.as_ref()) {
                        embedded_image(uri)
                    } else {
                        online_images::source(uri.as_ref(), &online_images)
                            .or_else(|| resolve_image(uri.as_ref()))
                    };
                    source
                        .map(|source| source.fit_to_display(&thumbnails))
                        .unwrap_or_else(empty_image_source)
                }
            })
            .on_image_click({
                let resolve_image = resolve_image.clone();
                let workspace = workspace.clone();
                move |uri, _, window, cx| {
                    let (source, title) = if is_data_url(uri.as_ref()) {
                        (
                            embedded_image(uri),
                            t!("image.embedded").into_owned().into(),
                        )
                    } else {
                        (
                            online_images::preview_source(uri.as_ref(), cx)
                                .or_else(|| resolve_image(uri.as_ref())),
                            uri.clone(),
                        )
                    };
                    let Some(source) = source else { return };
                    _ = workspace.update(cx, |workspace, cx| {
                        workspace.open_image_preview(source, title, window, cx)
                    });
                }
            })
            .on_link_click({
                let workspace = workspace.clone();
                let path = source_path.clone();
                move |target, _, window, cx| {
                    let workspace = workspace.clone();
                    let path = path.clone();
                    let target = target.clone();
                    window.defer(cx, move |window, cx| {
                        _ = workspace.update(cx, |workspace, cx| {
                            workspace.follow_document_link(&path, &target, window, cx)
                        });
                    });
                }
            })
            .code_block_actions(|code, _, _| {
                let code = code.code();
                copy_button(code)
            });
        let show_outline = self.outline_visible(window);
        if show_outline && !self.outline_was_visible {
            // The scroll handle learns the viewport bounds on the first layout.
            cx.on_next_frame(window, |this, window, cx| {
                if this.outline_visible(window)
                    && let Some(index) = this.selected_heading
                {
                    this.outline_scroll.scroll_to_item(index);
                    cx.notify();
                }
            });
        }
        self.outline_was_visible = show_outline;
        let list = self.text.read(cx).list_state().clone();
        let reader = cx.weak_entity();
        let body = div()
            .relative()
            .flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .min_h_0()
            .max_w(px(860.))
            .when(colors.bg.is_opaque(), |viewport| {
                viewport
                    .partial_scroll(move |_| list.painted_scroll_offset())
                    .child(scroll_viewport_background(colors.bg))
            })
            .child(body)
            .when(!self.document.headings.is_empty(), |viewport| {
                viewport.child(
                    canvas(
                        |_, _, _| (),
                        move |_, _, window, cx| {
                            let report = reader.read_with(cx, |reader, cx| {
                                reader.heading_jump_block.is_some()
                                    || reader.body_position
                                        != Some(
                                            reader.text.read(cx).list_state().logical_scroll_top(),
                                        )
                            });
                            if report.unwrap_or(false) {
                                window.defer(cx, move |_, cx| {
                                    _ = reader.update(cx, Self::sync_outline);
                                });
                            }
                        },
                    )
                    .absolute()
                    .size_full(),
                )
            });
        v_flex()
            .w_full()
            .h_full()
            .min_h_0()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .when(self.search.read(cx).is_open(), |el| {
                el.key_context("DocumentFind")
                    .on_action(cx.listener(|this, _: &Escape, window, cx| {
                        this.search
                            .update(cx, |search, cx| search.dismiss(window, cx));
                    }))
                    .child(self.search.clone())
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .w_full()
                    .id("document-panes")
                    .on_drag_move(cx.listener(|this, event: &DragMoveEvent<PaneDrag>, _, cx| {
                        if event.drag(cx).edge != gpui_component::resizable::HandleEdge::Leading {
                            return;
                        }
                        let maximum = event.bounds.size.width * 0.45;
                        let width =
                            event.bounds.right() - event.event.position.x + event.drag(cx).offset();
                        this.outline_width = width.clamp(px(120.).min(maximum), maximum);
                        _ = this.workspace.update(cx, |workspace, _| {
                            workspace.outline_width = this.outline_width;
                        });
                        cx.notify();
                    }))
                    .items_start()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .justify_center()
                            .child(body),
                    )
                    .when(show_outline, |el| el.child(self.outline(cx, colors))),
            )
    }
}

fn copy_button(code: SharedString) -> Button {
    compact_icon_button("copy-code", IconName::Copy, t!("docs.copy_code")).on_click(
        move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(code.to_string())),
    )
}

pub(crate) fn load_local(path: &Path) -> Result<PreparedDocument> {
    crate::repository::read_markdown_file(path).and_then(PreparedDocument::new)
}

pub fn load(repo: &Repository, path: &Path) -> Result<PreparedDocument> {
    anyhow::ensure!(is_markdown(path), t!("docs.select_markdown"));
    let remote = !path.is_absolute() && repo.ssh.is_some();
    let read = || {
        if remote {
            repo.read_markdown(path)
        } else {
            let target = if path.is_absolute() {
                path.to_owned()
            } else {
                repo.root.join(path)
            };
            let target = target.canonicalize()?;
            anyhow::ensure!(target.is_file(), t!("storage.regular_file"));
            crate::repository::read_markdown_file(&target)
        }
    };
    let source =
        read().with_context(|| t!("docs.read_failed", path = paths::display(path)).into_owned())?;
    let mut document = PreparedDocument::new(source.clone())?;
    if remote {
        let ast = markdown::to_mdast(&source, &markdown::ParseOptions::gfm())
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let mut urls = BTreeSet::new();
        visit(&ast, &mut |node| match node {
            Node::Image(image) => {
                urls.insert(image.url.clone());
            }
            Node::Definition(definition) => {
                urls.insert(definition.url.clone());
            }
            _ => {}
        });
        let mut budget = 32 * 1024 * 1024;
        for uri in urls.into_iter().take(128) {
            let Some(Destination::Local { path: image, .. }) = destination(path, &uri) else {
                continue;
            };
            let Some(format) = crate::image_viewer::format(&image) else {
                continue;
            };
            if let Ok(bytes) = repo.bytes(&image, true) {
                if bytes.len() > budget {
                    break;
                }
                budget -= bytes.len();
                Arc::make_mut(&mut document.images)
                    .insert(uri, Arc::new(gpui::Image::from_bytes(format, bytes)));
            }
        }
    }
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outlines_nested_unicode_headings_without_parsing_code_as_headings() {
        let doc = PreparedDocument::new(
            "# 中文 & **Code**\n\n## 中文 & Code\n\n```sh\n# ignored\n```\n".into(),
        )
        .unwrap();
        assert_eq!(
            doc.headings
                .iter()
                .map(|h| h.slug.as_str())
                .collect::<Vec<_>>(),
            ["中文-code", "中文-code-1"]
        );
        assert_eq!(
            &doc.source[doc.headings[1].source.clone()],
            "## 中文 & Code"
        );
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn outline_follows_the_body_and_can_be_scrolled_independently(cx: &mut TestAppContext) {
        fn draw(visual: &mut VisualTestContext) {
            for _ in 0..4 {
                visual.run_until_parked();
                visual.update(|window, cx| {
                    window.simulate_next_frame(cx);
                    window.draw(cx).clear(cx);
                });
            }
        }

        fn wheel(position: Point<Pixels>, y: f32) -> ScrollEvent {
            ScrollEvent {
                position,
                delta: ScrollDelta::Pixels(point(px(0.), px(y))),
                modifiers: Modifiers::default(),
                touch_phase: TouchPhase::Moved,
                source: ScrollSource::Touchpad,
            }
        }

        cx.update(gpui_component::init);
        let source = (0..40)
            .map(|index| format!("## Section {index}\n\n{}\n", "Body text.\n\n".repeat(12)))
            .collect::<String>();
        let (reader, visual) = cx.add_window_view(|window, cx| {
            gpui_component::Theme::set_scrollbar_mode(
                gpui_component::scroll::ScrollbarMode::Always,
                cx,
            );
            DocReader::new(
                PathBuf::new(),
                PathBuf::from("README.md"),
                PreparedDocument::new(source).unwrap(),
                Some("section-28".into()),
                WeakEntity::new_invalid(),
                window,
                cx,
            )
        });
        visual.simulate_resize(size(px(1200.), px(430.)));
        draw(visual);

        let scroll = visual.update(|_, cx| reader.read(cx).outline_scroll.clone());
        let outline = scroll.bounds();
        let selected = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| reader.read(cx).selected_heading.unwrap())
        };
        let row = |index| {
            let mut bounds = scroll.bounds_for_item(index).unwrap();
            bounds.origin += scroll.offset();
            bounds
        };
        assert_eq!(selected(visual), 28);
        assert!(outline.intersects(&row(28)));
        let body = visual.update(|_, cx| reader.read(cx).text.read(cx).bounds());
        visual.simulate_event(wheel(body.center(), -2100.));
        draw(visual);
        let active = selected(visual);
        assert!(active > 28, "the body wheel advances the active heading");
        let active_row = row(active);
        assert!(active_row.top() >= outline.top() && active_row.bottom() <= outline.bottom());

        let thumb = visual.update(|window, _| {
            let scale = window.scale_factor();
            window
                .painted_quads()
                .iter()
                .find(|quad| {
                    let bounds = quad.bounds;
                    (bounds.size.width.as_f32() / scale - 4.).abs() < 0.1
                        && (bounds.right().as_f32() / scale - outline.right().as_f32()).abs() < 10.
                        && bounds.size.height.as_f32() / scale > 18.
                })
                .map(|quad| {
                    let center = quad.bounds.center();
                    point(px(center.x.as_f32() / scale), px(center.y.as_f32() / scale))
                })
                .expect("the outline paints a shared thin scrollbar thumb")
        });
        let top = point(thumb.x, outline.top() + px(2.));
        visual.simulate_mouse_move(thumb, None, Modifiers::default());
        visual.simulate_mouse_down(thumb, MouseButton::Left, Modifiers::default());
        visual.simulate_mouse_move(top, MouseButton::Left, Modifiers::default());
        visual.simulate_mouse_up(top, MouseButton::Left, Modifiers::default());
        draw(visual);
        assert_eq!(selected(visual), active);
        assert!(outline.intersects(&row(0)));
        assert!(!outline.intersects(&row(active)));

        let target = row(2).center();
        visual.simulate_click(target, Modifiers::default());
        draw(visual);
        assert_eq!(selected(visual), 2);
        visual.update(|_, cx| {
            let text = reader.read(cx).text.read(cx);
            let heading = text
                .block_for_source_offset(reader.read(cx).document.headings[2].source.start)
                .unwrap();
            assert!(body.intersects(&text.list_state().bounds_for_item(heading).unwrap()));
        });

        visual.update(|_, cx| {
            reader.update(cx, |reader, cx| {
                reader.restore_position(Some(DocumentPosition {
                    source_offset: Some(reader.document.headings[20].source.start),
                    block: 0,
                    inset: 0.,
                }));
                cx.notify();
            });
        });
        draw(visual);
        assert_eq!(selected(visual), 20);
        assert!(outline.intersects(&row(20)));

        visual.update(|window, cx| reader.update(cx, |reader, cx| reader.toggle_find(window, cx)));
        draw(visual);
        visual.simulate_input("Section 34");
        draw(visual);
        // Reveal preserves the minimum body scroll, so the previous section may
        // still own the reading line with the match visible at the bottom.
        let after_find = selected(visual);
        assert!((33..=34).contains(&after_find));
        assert!(outline.intersects(&row(after_find)));

        let assert_closed = |visual: &mut VisualTestContext| {
            visual.update(|window, cx| {
                let reader = reader.read(cx);
                assert!(!reader.search.read(cx).is_open());
                assert!(reader.text.read(cx).focus_handle().is_focused(window));
            });
        };
        visual.simulate_keystrokes("escape");
        draw(visual);
        assert_closed(visual);

        visual.update(|window, cx| reader.update(cx, |reader, cx| reader.toggle_find(window, cx)));
        draw(visual);
        visual.update(|window, cx| {
            let reader = reader.read(cx);
            assert!(reader.search.read(cx).has_matches());
            reader
                .text
                .read(cx)
                .focus_handle()
                .clone()
                .focus(window, cx);
        });
        visual.simulate_keystrokes("escape");
        draw(visual);
        assert_closed(visual);
    }

    #[test]
    fn only_authored_first_column_body_markers_become_tasks() {
        let source = "| [x] | status |\n| --- | --- |\n| [X] | [x] |\n| [ ] | open |\n| \\[x\\] | escaped |\n";
        let Node::Root(root) = markdown::to_mdast(source, &markdown::ParseOptions::gfm()).unwrap()
        else {
            panic!("expected a root");
        };
        let Node::Table(table) = &root.children[0] else {
            panic!("expected a table");
        };
        let mut tasks = HashMap::new();
        collect_task_cells(table, source, &mut tasks);
        assert_eq!(tasks.len(), 2);
        assert!(tasks[&source.find("[X]").unwrap()]);
        assert!(!tasks[&source.find("[ ]").unwrap()]);
    }
}
