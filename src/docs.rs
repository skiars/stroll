use rust_i18n::t;

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    ops::Range,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
#[cfg(test)]
use core::prelude::v1::test;
use futures_channel::oneshot;
use gpui::{prelude::FluentBuilder as _, *};
use gpui_base::{CheckboxIndicator, CheckboxIndicatorColors, CheckboxState};
use gpui_component::{
    ActiveTheme, Colorize as _, Selectable as _, StyledExt,
    button::Button,
    h_flex,
    input::{DocumentFind, Escape, SelectAll},
    scroll::ScrollbarViewport as _,
    text::{
        InlineElement, InlineRenderContext, MarkdownExtensions, MarkdownNode, MarkdownParseContext,
        MarkdownPlugin, RangeHighlight, TextView, TextViewState, TextViewStyle, markdown_ast::Node,
    },
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit_assets::IconName;
use percent_encoding::percent_decode_str;
use url::Url;

use crate::{
    ai::{DocumentTranslations, document_prose_keys},
    doc_graphics::{self, GraphicKind, Graphics},
    paths,
    repository::{
        Entry, Repository, decode_markdown,
        resources::{RemoteResources, read_local_resource},
    },
    resource_cache::ResourceCache,
    session::{DocumentLinePosition, DocumentLocation, DocumentPosition},
    workspace::{
        Palette, PaneDrag, Workspace, compact_icon_button, icon_button, pane_scrollbar,
        panel_resize, scroll_viewport_background,
    },
};

mod context_menu;
mod markdown_alerts;
mod online_images;

pub(crate) use markdown_alerts::strip_markers as strip_alert_markers;

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
    editor_scroll_spans: Vec<Range<usize>>,
    prose_keys: Arc<HashMap<usize, String>>,
    html_anchors: HashMap<SharedString, usize>,
    extensions: MarkdownExtensions,
    graphics: Arc<Graphics>,
    remote: Option<RemoteResources>,
}

#[derive(Clone, Copy)]
pub(crate) struct EditorScrollAnchor {
    pub(crate) source: usize,
    pub(crate) span: (usize, usize),
    block: usize,
    line: Option<usize>,
}

impl PreparedDocument {
    pub(crate) fn from_editor(source: String, remote: Option<RemoteResources>) -> Result<Self> {
        let mut document = Self::prepare(source, true)?;
        document.remote = remote;
        Ok(document)
    }

    pub(crate) fn from_source(source: String, remote: Option<RemoteResources>) -> Result<Self> {
        let mut document = Self::new(source)?;
        document.remote = remote;
        Ok(document)
    }

    pub(crate) fn has_fragment(&self, fragment: &str) -> bool {
        self.html_anchors.contains_key(fragment)
            || heading_index(&self.headings, fragment).is_some()
    }

    pub(crate) fn title(&self) -> Option<&str> {
        self.headings
            .first()
            .map(|heading| heading.title.trim())
            .filter(|title| !title.is_empty())
    }

    pub fn new(source: String) -> Result<Self> {
        Self::prepare(source, false)
    }

    fn prepare(source: String, editor_preview: bool) -> Result<Self> {
        let ast = parse_markdown(&source)?;
        let headings = headings(&ast);
        let editor_scroll_spans = if editor_preview {
            ast.children()
                .into_iter()
                .flatten()
                .filter(|node| {
                    matches!(node, Node::Heading(_) | Node::Paragraph(_))
                        && scrollable_inline_text(node)
                })
                .filter_map(|node| {
                    node.position()
                        .map(|span| span.start.offset..span.end.offset)
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut graphics = Graphics::default();
        let mut task_cells = HashMap::new();
        visit(&ast, &mut |node| {
            for kind in [
                GraphicKind::Diagram,
                GraphicKind::DisplayMath,
                GraphicKind::InlineMath,
            ] {
                if let Some(source) = kind.source(node)
                    && let Some(position) = node.position()
                {
                    graphics.prepare(kind, source, position.start.offset);
                }
            }
            if let Node::Table(table) = node {
                collect_task_cells(table, &source, &mut task_cells);
            }
        });
        let graphics = Arc::new(graphics);
        let html_anchors = html_anchors(&ast, &source);
        let prose_keys = Arc::new(document_prose_keys(ast, &source));
        let extensions = markdown_alerts::register(doc_graphics::extensions(graphics.clone()))
            .blockquote_left_rule(markdown_alerts::RAIL_WIDTH)
            .plugin(TaskTable(task_cells));
        Ok(Self {
            source: source.into(),
            headings,
            prose_keys,
            editor_scroll_spans,
            html_anchors,
            extensions,
            graphics,
            remote: None,
        })
    }
}

// Only follow text whose display order comes directly from its source.
// In particular, a paragraph containing an image or formula can reflow after paint.
fn scrollable_inline_text(node: &Node) -> bool {
    match node {
        Node::Text(_) | Node::InlineCode(_) | Node::Break(_) => true,
        Node::Heading(_)
        | Node::Paragraph(_)
        | Node::Emphasis(_)
        | Node::Strong(_)
        | Node::Delete(_)
        | Node::Link(_)
        | Node::LinkReference(_) => node
            .children()
            .is_some_and(|children| children.iter().all(scrollable_inline_text)),
        _ => false,
    }
}

pub(crate) fn parse_markdown(source: &str) -> Result<Node> {
    markdown_alerts::parser()
        .parse_mdast(source)
        .map_err(|error| anyhow::anyhow!("{error}"))
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

fn html_anchors(ast: &Node, source: &str) -> HashMap<SharedString, usize> {
    let mut anchors = HashMap::new();
    visit(ast, &mut |node| {
        if let Node::Html(html) = node
            && let Some(position) = &html.position
        {
            let ids = gpui_base::text::html_ids(&html.value);
            if ids.is_empty() {
                return;
            }
            let raw = &source[position.start.offset..position.end.offset];
            let mut starts = vec![position.start.offset];
            starts.extend(
                raw.match_indices(['\n', '\r'])
                    .filter_map(|(offset, ending)| {
                        let next = offset + 1;
                        (ending != "\r" || !raw[next..].starts_with('\n'))
                            .then_some(position.start.offset + next)
                    }),
            );
            for (id, line) in ids {
                let offset = starts
                    .get(line - 1)
                    .copied()
                    .unwrap_or(position.start.offset);
                anchors.entry(id).or_insert(offset);
            }
        }
    });
    anchors
}

pub(crate) fn fragment_source_offset(source: &str, fragment: &str) -> Option<usize> {
    let ast = parse_markdown(source).ok()?;
    if let Some(&offset) = html_anchors(&ast, source).get(fragment) {
        return Some(offset);
    }
    let headings = headings(&ast);
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

    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InlineElement> {
        let checked = node.data::<bool>().copied().unwrap_or(false);
        let state = if checked {
            CheckboxState::Checked
        } else {
            CheckboxState::Unchecked
        };
        let theme = cx.theme();
        let colors = CheckboxIndicatorColors {
            border: theme.muted_foreground,
            accent: theme.accent,
            mark: theme.button_primary_foreground,
        };
        let size = context.font_size();
        let baseline = CheckboxIndicator::text_baseline(window, &context.text_style().font(), size);
        Some(
            InlineElement::new(CheckboxIndicator::web(
                size,
                state,
                colors,
                false,
                if checked { 1. } else { 0. },
                None,
            ))
            .with_baseline(baseline),
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

enum DocumentFragment {
    Heading(usize),
    HtmlId(String),
}

pub struct DocReader {
    typography: crate::config::Docs,
    root: PathBuf,
    pub path: PathBuf,
    document: PreparedDocument,
    displayed_images: Entity<online_images::DisplayedImages>,
    image_thumbnails: Entity<ImageThumbnails>,
    image_subscription: Subscription,
    text: Entity<TextViewState>,
    translation: Option<Arc<DocumentTranslations>>,
    search: Entity<DocumentFind>,
    vim_find_origin: Option<(String, ListOffset)>,
    workspace: WeakEntity<Workspace>,
    _subscriptions: [Subscription; 2],
    show_outline: Option<bool>,
    outline_width: Pixels,
    standalone: bool,
    editor_preview: bool,
    outline_was_visible: bool,
    outline_scroll: ScrollHandle,
    selected_heading: Option<usize>,
    body_position: Option<ListOffset>,
    fragment_jump_block: Option<usize>,
    pending_fragment: Option<(DocumentFragment, bool)>,
    pending_source_location: Option<(Range<usize>, bool)>,
    history_source_offset: Option<usize>,
    history_fragment: Option<String>,
    pending_position: Option<DocumentPosition>,
    restoring_line: bool,
    navigate_saved_position: bool,
}

impl DocReader {
    pub(crate) fn document_title(&self) -> Option<&str> {
        self.document.title()
    }

    pub(crate) fn source_text(&self) -> SharedString {
        self.document.source.clone()
    }

    pub(crate) fn set_translation(
        &mut self,
        translation: Option<Arc<DocumentTranslations>>,
        cx: &mut Context<Self>,
    ) {
        if match (&self.translation, &translation) {
            (Some(old), Some(new)) => Arc::ptr_eq(old, new),
            (None, None) => true,
            _ => false,
        } {
            return;
        }
        self.translation = translation;
        self.text
            .update(cx, TextViewState::invalidate_inline_layout);
        cx.notify();
    }

    pub(crate) fn toolbar_actions(
        &self,
        reader: &Entity<Self>,
        translation: Div,
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
                    .debug_selector(|| "document-find".into())
                    .selected(self.search.read(cx).is_open())
                    .toggled(self.search.read(cx).is_open())
                    .on_click({
                        let reader = reader.clone();
                        move |_, window, cx| {
                            reader.update(cx, |doc, cx| doc.toggle_find(window, cx));
                        }
                    }),
            )
            .child(translation)
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

    pub(crate) fn set_editor_preview(&mut self, standalone: bool) {
        self.editor_preview = true;
        self.standalone = standalone;
        self.show_outline = Some(false);
    }

    pub(crate) fn document_parsed(&self, cx: &App) -> bool {
        self.text.read(cx).rendered_text().source() == self.document.source.as_ref()
    }

    pub(crate) fn editor_scroll_anchor(
        &self,
        offset: usize,
        cx: &App,
    ) -> Option<EditorScrollAnchor> {
        let spans = &self.document.editor_scroll_spans;
        let span = spans.get(spans.partition_point(|span| span.end <= offset))?;
        let source = self.document.source.as_str();
        if offset < span.start
            && !source
                .get(offset..span.start)?
                .bytes()
                .all(|byte| matches!(byte, b' ' | b'\t'))
        {
            return None;
        }
        if self.pending_position.is_some()
            || self.pending_fragment.is_some()
            || self.pending_source_location.is_some()
        {
            return None;
        }
        let text = self.text.read(cx);
        let list = text.list_state();
        if list.viewport_bounds().size.height <= px(0.) {
            return None;
        }
        let block = text.block_for_source_offset(span.start)?;
        if text.source_offset_for_block(block)? != span.start {
            return None;
        }
        if offset <= span.start
            || self
                .document
                .headings
                .iter()
                .any(|heading| heading.source == *span)
        {
            return Some(EditorScrollAnchor {
                source: span.start,
                span: (span.start, span.end),
                block,
                line: None,
            });
        }

        let tail = source.get(offset..span.end)?;
        let end = tail.find('\n').unwrap_or(tail.len()) + offset;
        let range = text.rendered_text().range_for_source(offset..end)?;
        Some(EditorScrollAnchor {
            source: offset,
            span: (span.start, span.end),
            block,
            line: Some(range.start),
        })
    }

    pub(crate) fn editor_scroll_is_navigating(&self, cx: &App) -> bool {
        let text = self.text.read(cx);
        text.has_pending_reveal() || text.list_state().is_navigating()
    }

    pub(crate) fn editor_scroll_delta(
        &self,
        anchor: EditorScrollAnchor,
        fraction: f32,
        cx: &App,
    ) -> Option<Pixels> {
        let text = self.text.read(cx);
        let list = text.list_state();
        let bounds = list.bounds_for_item(anchor.block)?;
        let viewport = list.viewport_bounds();
        if !bounds.intersects(&viewport)
            || bounds.size.height <= px(0.)
            || bounds.size.height > viewport.size.height
        {
            return None;
        }
        Some(bounds.top() - viewport.top() + bounds.size.height * fraction)
    }

    pub(crate) fn apply_editor_scroll_delta(&self, delta: Pixels, cx: &mut Context<Self>) {
        let list = self.text.read(cx).list_state();
        let position = -list.scroll_px_offset_for_scrollbar().y;
        let remaining = (list.max_offset_for_scrollbar().y - position).max(px(0.));
        let delta = delta.clamp(-position.max(px(0.)), remaining);
        if delta.abs() > px(0.01) {
            list.scroll_by(delta);
            cx.notify();
        }
    }

    pub(crate) fn retarget_editor_scroll_delta(
        &self,
        delta: Pixels,
        cx: &mut Context<Self>,
    ) -> bool {
        let text = self.text.read(cx);
        let list = text.list_state();
        if text.has_pending_reveal() {
            return false;
        }
        let destination = list.navigation_scroll_top();
        let Some(bounds) = list.bounds_for_item(destination.item_ix) else {
            return false;
        };
        let target = delta - list.scroll_px_offset_for_scrollbar().y;
        if target < px(0.) || target > list.max_offset_for_scrollbar().y {
            return false;
        }
        let previous = bounds.top() - list.viewport_bounds().top() + destination.offset_in_item;
        let change = delta - previous;
        if change.abs() > px(0.5) {
            list.navigate_by(change);
            cx.notify();
        }
        true
    }

    pub(crate) fn apply_editor_scroll_anchor(
        &self,
        anchor: EditorScrollAnchor,
        animate: bool,
        cx: &mut Context<Self>,
    ) {
        self.text.update(cx, |text, cx| {
            let line = if animate && anchor.line.is_none() {
                text.rendered_text()
                    .range_for_source(anchor.span.0..anchor.span.1)
                    .map(|range| range.start)
            } else {
                anchor.line
            };
            if let Some(line) = line {
                _ = text.scroll_to_range(line..line, Some(px(0.)), animate, cx);
            } else {
                let position = ListOffset {
                    item_ix: anchor.block,
                    offset_in_item: px(0.),
                };
                if text.has_pending_reveal() || text.list_state().logical_scroll_top() != position {
                    text.cancel_scroll_navigation();
                    text.scroll_to_block(position, animate);
                    cx.notify();
                }
            }
        });
    }

    #[cfg(test)]
    pub(crate) fn editor_scroll_pixels(&self, cx: &App) -> Pixels {
        -self
            .text
            .read(cx)
            .list_state()
            .scroll_px_offset_for_scrollbar()
            .y
    }

    #[cfg(test)]
    pub(crate) fn editor_scroll_destination(&self, cx: &App) -> Option<usize> {
        let text = self.text.read(cx);
        let list = text.list_state();
        list.is_navigating()
            .then(|| text.source_offset_for_block(list.navigation_scroll_top().item_ix))
            .flatten()
    }

    pub(crate) fn cancel_editor_scroll_navigation(&self, cx: &mut Context<Self>) {
        self.text
            .update(cx, |text, _| text.cancel_scroll_navigation());
    }

    pub(crate) fn cancel_editor_scroll_follow(&mut self, cx: &mut Context<Self>) {
        self.restore_position(None);
        self.cancel_editor_scroll_navigation(cx);
    }

    pub(crate) fn wait_for_document_parse(
        &self,
        cx: &mut App,
    ) -> Option<(Subscription, oneshot::Receiver<()>)> {
        if self.document_parsed(cx) {
            return None;
        }
        let source = self.document.source.clone();
        let (ready, parsed) = oneshot::channel();
        let mut ready = Some(ready);
        let subscription = cx.observe(&self.text, move |text, cx| {
            if text.read(cx).rendered_text().source() == source.as_ref()
                && let Some(ready) = ready.take()
            {
                _ = ready.send(());
            }
        });
        Some((subscription, parsed))
    }

    pub(crate) fn inherit_document(
        &mut self,
        previous: &Entity<Self>,
        remap: impl Fn(usize) -> (usize, bool),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = previous.read(cx);
        let text = self.text.read(cx);
        let position = previous
            .session_position(cx)
            .or(previous.pending_position)
            .map(|mut position| {
                if let Some(offset) = position.source_offset {
                    let (offset, deleted) = remap(offset);
                    position.source_offset = Some(offset);
                    if deleted {
                        let preceding = text.block_for_source_offset(offset);
                        let next = preceding.map_or(0, |block| block + 1);
                        if preceding
                            .is_none_or(|block| text.source_offset_for_block(block) != Some(offset))
                            && let Some(start) = (next..text.list_state().item_count())
                                .find_map(|block| text.source_offset_for_block(block))
                        {
                            position.source_offset = Some(start);
                        }
                        position.inset = 0.;
                    }
                }
                position.line = position.line.and_then(|mut line| {
                    let (offset, deleted) = remap(line.source_offset);
                    line.source_offset = offset;
                    (!deleted).then_some(line)
                });
                position
            });
        let focused = previous.text.read(cx).focus_handle().is_focused(window);
        let display = previous.displayed_images.clone();
        self.image_thumbnails = previous.image_thumbnails.clone();
        self.document
            .graphics
            .reuse_ready(&previous.document.graphics, |offset| {
                let (offset, deleted) = remap(offset);
                (!deleted).then_some(offset)
            });
        display.update(cx, |display, _| display.clear_block_tracking());
        self.image_subscription = cx.observe(&display, Self::refresh_image_layout);
        self.displayed_images = display;

        if let Some(position) = position {
            // Seed the initial layout at the old block before restoring the exact line.
            self.text.update(cx, |text, _| {
                let item_ix = position
                    .source_offset
                    .and_then(|offset| text.block_for_source_offset(offset))
                    .unwrap_or(position.block);
                text.scroll_to_block(
                    ListOffset {
                        item_ix,
                        offset_in_item: px(position.inset),
                    },
                    false,
                );
            });
            self.restore_position(Some(position));
        }
        if focused {
            self.focus_content(window, cx);
        }
    }

    fn follow_preview_link(&mut self, target: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Destination::Local {
            path,
            fragment: Some(fragment),
        }) = destination(&self.path, target)
            && path == self.path
        {
            if !self.jump_to_fragment_with_motion(&fragment, true, cx) {
                _ = self.workspace.update(cx, |workspace, cx| {
                    workspace.preview_fragment_missing(&fragment, window, cx);
                });
            }
            return;
        }
        let workspace = self.workspace.clone();
        let path = self.path.clone();
        let target = target.to_owned();
        window.defer(cx, move |window, cx| {
            _ = workspace.update(cx, |workspace, cx| {
                workspace.follow_editor_preview_link(&path, &target, window, cx);
            });
        });
    }

    pub(crate) fn refresh_language(&mut self, _: &mut Window, cx: &mut Context<Self>) {
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
            .scrollable_content_width(px(860.), rems(1.25))
            .track_viewport_line()
        });
        text.read(cx).list_state().set_pixel_aligned_scroll(true);
        text.read(cx).list_state().set_scroll_past_end(true);
        let search = DocumentFind::for_document(text.clone(), window, cx);
        let text_subscription = cx.observe(&text, |this: &mut Self, _, cx| this.reveal_pending(cx));
        let search_subscription = cx.observe(&search, |this: &mut Self, search, cx| {
            let search = search.read(cx);
            if !search.is_open() {
                this.vim_find_origin = None;
            }
            if search.has_matches() {
                this.fragment_jump_block = None;
            }
            cx.notify();
        });
        let displayed_images = online_images::DisplayedImages::new(cx);
        let image_subscription = cx.observe(&displayed_images, Self::refresh_image_layout);
        document.graphics.observe(cx.weak_entity());
        let mut reader = Self {
            typography: crate::config::Docs::default(),
            root,
            path,
            document,
            displayed_images,
            image_thumbnails: ImageThumbnails::new(cx),
            image_subscription,
            text,
            translation: None,
            search,
            vim_find_origin: None,
            workspace,
            _subscriptions: [text_subscription, search_subscription],
            show_outline: None,
            outline_width: px(195.),
            standalone: false,
            editor_preview: false,
            outline_was_visible: false,
            outline_scroll: ScrollHandle::new(),
            selected_heading: None,
            body_position: None,
            fragment_jump_block: None,
            pending_fragment: None,
            pending_source_location: None,
            history_source_offset: None,
            history_fragment: None,
            pending_position: None,
            restoring_line: false,
            navigate_saved_position: false,
        };
        if let Some(fragment) = fragment {
            reader.jump_to_fragment_with_motion(&fragment, false, cx);
        }
        reader
    }

    pub(crate) fn release_online_images(&mut self, cx: &mut Context<Self>) {
        self.pause_ssh_images(true, cx);
        self.text
            .update(cx, |text, _| text.cancel_scroll_navigation());
        self.displayed_images = online_images::DisplayedImages::new(cx);
        self.image_subscription = cx.observe(&self.displayed_images, Self::refresh_image_layout);
        self.image_thumbnails = ImageThumbnails::new(cx);
        cx.notify();
    }

    pub(crate) fn pause_ssh_images(&self, pause: bool, cx: &App) {
        self.displayed_images.read(cx).pause_ssh(pause);
    }

    fn refresh_image_layout(
        &mut self,
        display: Entity<online_images::DisplayedImages>,
        cx: &mut Context<Self>,
    ) {
        let blocks = display.update(cx, |images, _| images.take_changed_blocks());
        self.refresh_inline_resources(blocks, cx);
    }

    pub(crate) fn refresh_graphic_layout(&mut self, offsets: &[usize], cx: &mut Context<Self>) {
        let text = self.text.read(cx);
        let blocks = offsets
            .iter()
            .filter_map(|offset| text.block_for_source_offset(*offset))
            .collect();
        self.refresh_inline_resources(blocks, cx);
    }

    fn refresh_inline_resources(&mut self, blocks: HashSet<usize>, cx: &mut Context<Self>) {
        if blocks.is_empty() {
            return;
        }
        let text = self.text.read(cx);
        let top = text.list_state().logical_scroll_top();
        let position = if blocks.contains(&top.item_ix)
            && self.fragment_jump_block.is_none()
            && !text.has_pending_reveal()
        {
            self.session_position(cx)
        } else {
            None
        };
        if let Some(position) = position.filter(|position| position.line.is_some()) {
            self.restore_position(Some(position));
        }
        self.text
            .update(cx, |text, cx| text.invalidate_inline_blocks(blocks, cx));
        cx.notify();
    }

    pub(crate) fn history_location(&self, cx: &App) -> DocumentLocation {
        DocumentLocation {
            position: self.pending_position.or_else(|| {
                if self.fragment_jump_block.is_some() {
                    return None;
                }
                let text = self.text.read(cx);
                if !text.has_pending_reveal()
                    && let Some(position) = self.session_position(cx)
                {
                    return Some(position);
                }
                self.history_source_offset.map(|offset| DocumentPosition {
                    source_offset: Some(offset),
                    block: text.block_for_source_offset(offset).unwrap_or_default(),
                    inset: 0.,
                    line: Some(DocumentLinePosition {
                        source_offset: offset,
                        viewport_inset: 0.,
                    }),
                })
            }),
            fragment: self.history_fragment.clone(),
        }
    }

    pub(crate) fn replay_history_location(
        &mut self,
        location: DocumentLocation,
        animate: bool,
        cx: &mut Context<Self>,
    ) {
        self.pending_source_location = None;
        self.history_source_offset = None;
        if let Some(position) = location.position {
            self.pending_fragment = None;
            self.history_fragment = location.fragment;
            self.restore_position(Some(position));
            self.navigate_saved_position = animate;
        } else if !location
            .fragment
            .as_deref()
            .is_some_and(|fragment| self.jump_to_fragment_with_motion(fragment, animate, cx))
        {
            self.pending_fragment = None;
            self.history_fragment = None;
            self.restore_position(Some(DocumentPosition {
                source_offset: None,
                block: 0,
                inset: 0.,
                line: None,
            }));
            self.navigate_saved_position = animate;
        }
        cx.notify();
    }

    pub(crate) fn focus_content(&self, window: &mut Window, cx: &mut App) {
        self.text.read(cx).focus_handle().clone().focus(window, cx);
    }

    pub(crate) fn content_focused(&self, window: &Window, cx: &App) -> bool {
        self.text.read(cx).focus_handle().is_focused(window)
    }

    fn scroll_or_jump(&mut self, key: char, cx: &mut Context<Self>) -> bool {
        match key {
            'j' | 'k' => {
                let direction = if key == 'j' { 1. } else { -1. };
                let list = self.text.read(cx).list_state();
                list.navigate_by(px(self.typography.body_font_size as f32
                    * 1.72
                    * 3.
                    * direction));
                cx.notify();
            }
            'h' | 'l' => {
                let position = self.selected_heading;
                let index = if key == 'h' {
                    position.and_then(|index| index.checked_sub(1))
                } else {
                    let text = self.text.read(cx);
                    let before_first = self.document.headings.first().is_some_and(|heading| {
                        text.block_for_source_offset(heading.source.start)
                            .is_some_and(|first| {
                                text.list_state().logical_scroll_top().item_ix < first
                            })
                    });
                    Some(if before_first {
                        0
                    } else {
                        position.map_or(0, |index| index + 1)
                    })
                };
                if let Some(index) = index.filter(|index| *index < self.document.headings.len()) {
                    self.jump_to_heading(index, true, cx);
                }
            }
            _ => return false,
        }
        true
    }

    pub(crate) fn vim_key(
        &mut self,
        stroke: &Keystroke,
        key: Option<char>,
        top: bool,
        modal: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if top {
            self.text
                .read(cx)
                .list_state()
                .scroll_to(ListOffset::default());
        } else if stroke.modifiers
            == (Modifiers {
                control: true,
                ..Default::default()
            })
            && matches!(stroke.key.as_str(), "d" | "u")
        {
            let list = self.text.read(cx).list_state();
            let direction = if stroke.key == "d" { 0.5 } else { -0.5 };
            list.navigate_by(list.viewport_bounds().size.height * direction);
        } else {
            match key {
                Some('G') => self.text.read(cx).list_state().scroll_to_end(),
                Some('/') => {
                    if modal {
                        let query = self.search.read(cx).retained_query().to_owned();
                        let top = self.text.read(cx).list_state().logical_scroll_top();
                        self.vim_find_origin = Some((query, top));
                    }
                    self.find(window, cx);
                }
                Some('n' | 'N') => self.step_find_match(key == Some('N'), cx),
                Some(key) => return self.scroll_or_jump(key, cx),
                None => return false,
            }
        }
        cx.notify();
        true
    }

    pub(crate) fn step_find_match(&mut self, previous: bool, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.step(previous, cx));
    }

    pub(crate) fn session_position(&self, cx: &App) -> Option<DocumentPosition> {
        if self.pending_position.is_some()
            || self.pending_fragment.is_some()
            || self.pending_source_location.is_some()
        {
            return None;
        }
        let text = self.text.read(cx);
        let list = text.list_state();
        if list.item_count() == 0 || list.is_navigating() {
            return None;
        }
        let top = list.logical_scroll_top();
        Some(DocumentPosition {
            source_offset: text.source_offset_for_block(top.item_ix),
            block: top.item_ix,
            inset: top.offset_in_item.into(),
            line: text
                .viewport_line()
                .map(|(source_offset, inset)| DocumentLinePosition {
                    source_offset,
                    viewport_inset: inset.into(),
                }),
        })
    }

    pub(crate) fn restore_position(&mut self, position: Option<DocumentPosition>) {
        if position.is_some() {
            self.fragment_jump_block = None;
        }
        self.pending_position = position;
        self.restoring_line = false;
        self.navigate_saved_position = false;
    }

    fn apply_saved_position(&mut self, cx: &mut Context<Self>) {
        let text = self.text.read(cx);
        let list = text.list_state();
        if text.rendered_text().source() != self.document.source.as_ref() {
            return;
        }
        let Some(position) = self.pending_position else {
            return;
        };
        if self.restoring_line {
            if !text.has_pending_reveal() && !list.is_navigating() {
                self.restore_position(None);
            }
            cx.notify();
            return;
        }
        let block = position
            .source_offset
            .and_then(|offset| text.block_for_source_offset(offset))
            .unwrap_or(position.block);
        let anchor = ListOffset {
            item_ix: block,
            offset_in_item: px(position.inset),
        };
        let animate = self.navigate_saved_position
            && cx.smooth_scrolling()
            && !cx.reduce_motion()
            && list.viewport_bounds().size.height > px(0.);
        if let Some(line) = position.line {
            let rendered = text.rendered_text();
            let offset = line.source_offset;
            if let Some(character) = rendered
                .source()
                .get(offset..)
                .and_then(|rest| rest.chars().next())
                && let Some(range) =
                    rendered.range_for_source(offset..offset + character.len_utf8())
            {
                self.restoring_line = self.text.update(cx, |text, cx| {
                    text.scroll_to_range(range, Some(px(line.viewport_inset)), animate, cx)
                        .is_ok()
                });
            }
        }
        if !self.restoring_line {
            self.text.update(cx, |text, cx| {
                text.scroll_to_block(anchor, animate);
                cx.notify();
            });
            if animate {
                self.restoring_line = true;
            } else {
                self.pending_position = None;
            }
        }
        cx.notify();
    }

    pub(crate) fn set_typography(
        &mut self,
        typography: crate::config::Docs,
        cx: &mut Context<Self>,
    ) {
        if self.typography != typography {
            if self.fragment_jump_block.is_none()
                && let Some(position) = self.session_position(cx)
            {
                self.restore_position(Some(position));
            }
            self.typography = typography;
        }
        cx.notify();
    }

    pub(crate) fn finish_vim_find(
        &mut self,
        commit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((query, top)) = &self.vim_find_origin else {
            return false;
        };
        let restore = (!commit).then_some(query.as_str());
        if !self.search.update(cx, |search, cx| {
            search.finish_modal_find(restore, window, cx)
        }) {
            return false;
        }
        if !commit {
            self.text.update(cx, |text, _| {
                text.cancel_scroll_navigation();
                text.list_state().scroll_to(*top);
            });
        }
        self.vim_find_origin = None;
        cx.notify();
        true
    }

    pub fn toggle_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.toggle(window, cx));
    }

    pub(crate) fn find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text.read(cx);
        let query = if text.focus_handle().contains_focused(window, cx) {
            text.selected_text()
        } else {
            String::new()
        };
        let search = self.search.read(cx);
        if query.is_empty() && search.is_open() {
            search.focus_handle(cx).focus(window, cx);
            window.dispatch_action(Box::new(SelectAll), cx);
        } else {
            self.search
                .update(cx, |search, cx| search.open_with_query(&query, window, cx));
        }
    }

    pub(crate) fn locate_source(
        &mut self,
        range: Range<usize>,
        fingerprint: [u8; 32],
        animate: bool,
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
        self.restore_position(None);
        self.history_fragment = None;
        self.history_source_offset = Some(source.start);
        self.fragment_jump_block = None;
        self.pending_fragment = None;
        self.pending_source_location = Some((source, animate));
        self.reveal_pending(cx);
    }

    fn reveal_source_range(&mut self, source: Range<usize>, animate: bool, cx: &mut Context<Self>) {
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
                    _ = text.scroll_to_range(range.start..range.start, None, animate, cx);
                }
            } else if let Some(range) = rendered.range_for_source(source) {
                _ = text.set_range_highlights([RangeHighlight::new(range.clone(), accent)], cx);
                _ = text.scroll_to_range(range, None, animate, cx);
            }
        });
    }

    pub fn jump_to_fragment(&mut self, fragment: &str, cx: &mut Context<Self>) -> bool {
        self.jump_to_fragment_with_motion(fragment, true, cx)
    }

    pub(crate) fn jump_to_fragment_with_motion(
        &mut self,
        fragment: &str,
        animate: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.document.html_anchors.contains_key(fragment) {
            self.start_fragment_jump(DocumentFragment::HtmlId(fragment.to_owned()), animate, cx);
            true
        } else if let Some(index) = heading_index(&self.document.headings, fragment) {
            self.jump_to_heading(index, animate, cx);
            true
        } else {
            false
        }
    }

    fn jump_to_heading(&mut self, index: usize, animate: bool, cx: &mut Context<Self>) {
        self.select_heading(index);
        self.start_fragment_jump(DocumentFragment::Heading(index), animate, cx);
    }

    fn start_fragment_jump(
        &mut self,
        fragment: DocumentFragment,
        animate: bool,
        cx: &mut Context<Self>,
    ) {
        self.restore_position(None);
        self.fragment_jump_block = None;
        self.pending_source_location = None;
        self.history_fragment = Some(match &fragment {
            DocumentFragment::Heading(index) => self.document.headings[*index].slug.clone(),
            DocumentFragment::HtmlId(id) => id.clone(),
        });
        self.pending_fragment = Some((fragment, animate));
        self.reveal_pending(cx);
        cx.notify();
    }

    fn reveal_pending(&mut self, cx: &mut Context<Self>) {
        if self.pending_fragment.is_none() && self.pending_source_location.is_none() {
            return;
        }
        let text = self.text.read(cx);
        let rendered = text.rendered_text();
        if rendered.source() != self.document.source.as_ref() {
            return;
        }
        let Some((fragment, animate)) = self.pending_fragment.take() else {
            if let Some((source, animate)) = self.pending_source_location.take() {
                self.reveal_source_range(source, animate, cx);
            }
            return;
        };
        match fragment {
            DocumentFragment::Heading(index) => {
                let source = self.document.headings[index].source.clone();
                if let Some(range) = rendered.range_for_source(source.clone()) {
                    self.fragment_jump_block = text.block_for_source_offset(source.start);
                    self.text.update(cx, |text, cx| {
                        _ = text.scroll_to_range(
                            range.start..range.start,
                            Some(px(12.)),
                            animate,
                            cx,
                        );
                    });
                }
            }
            DocumentFragment::HtmlId(id) => {
                let source = self.document.html_anchors[id.as_str()];
                let heading = self
                    .document
                    .headings
                    .partition_point(|heading| heading.source.start <= source)
                    .saturating_sub(1);
                let block = self.text.update(cx, |text, cx| {
                    text.scroll_to_html_id(&id, px(12.), animate, cx)
                        .or_else(|| {
                            let block = text.block_for_source_offset(source)?;
                            text.scroll_to_block(
                                ListOffset {
                                    item_ix: block,
                                    offset_in_item: px(0.),
                                },
                                animate,
                            );
                            cx.notify();
                            Some(block)
                        })
                });
                if !self.document.headings.is_empty() {
                    self.select_heading(heading);
                    self.fragment_jump_block = block;
                }
            }
        }
    }

    fn select_heading(&mut self, index: usize) {
        self.selected_heading = Some(index);
        self.outline_scroll.scroll_to_item(index);
    }

    fn sync_outline(&mut self, cx: &mut Context<Self>) {
        if self.pending_fragment.is_some() || self.pending_position.is_some() {
            return;
        }
        let text = self.text.read(cx);
        let list = text.list_state();
        let top = list.logical_scroll_top();
        if list.is_navigating() || list.item_count() == 0 || self.document.headings.is_empty() {
            return;
        }
        let previous = self.body_position.replace(top);
        // An explicit reveal can place a heading anywhere within the visible page.
        // Keep the clicked heading until the reader moves away from that position.
        if let Some(target) = self.fragment_jump_block.take() {
            if list
                .bounds_for_item(target)
                .is_some_and(|bounds| list.viewport_bounds().intersects(&bounds))
            {
                return;
            }
            if text.has_pending_reveal() {
                self.fragment_jump_block = Some(target);
                return;
            }
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
                    .debug_selector(move || format!("document-heading-{index}"))
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
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let workspace = this.workspace.clone();
                        if workspace.upgrade().is_some() {
                            let path = this.path.clone();
                            let slug = this.document.headings[index].slug.clone();
                            window.defer(cx, move |window, cx| {
                                _ = workspace.update(cx, |workspace, cx| {
                                    workspace.navigate_document(path, Some(slug), window, cx);
                                });
                            });
                        } else {
                            this.jump_to_heading(index, true, cx);
                        }
                    }))
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
        if self.pending_position.is_some() && !self.restoring_line {
            self.apply_saved_position(cx);
        }
        if self.pending_position.is_some() {
            cx.on_next_frame(window, |this, _, cx| this.apply_saved_position(cx));
        }
        let theme = cx.theme();
        let colors = Palette::for_mode(theme.mode);
        let dark = theme.mode == gpui_component::ThemeMode::Dark;
        let body_foreground = Hsla::from(rgb(if dark { 0xc5c8d0 } else { 0x24262b }));
        let quote_border = if dark {
            rgb(0x404550).into()
        } else {
            colors.surface
        };
        let translation_foreground = body_foreground.mix_oklab(colors.muted, 0.8);
        let reader = cx.weak_entity();
        let code_font = crate::workspace::ui_code_font(cx);
        let inline_code = StyleRefinement::default().font_family(code_font.clone());
        let images = self.images();
        let mut table = StyleRefinement::default().rounded(px(9.));
        table.overflow.x = Some(Overflow::Scroll);
        let mut code_block = StyleRefinement::default()
            .rounded(px(9.))
            .border_0()
            .p(px(12.))
            .line_height(relative(1.5))
            .font_family(code_font.clone())
            .text_size(px(self.typography.code_block_font_size as f32));
        code_block.overflow.x = Some(Overflow::Scroll);
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
        .table_head(
            StyleRefinement::default()
                .bg(colors.hover)
                .text_color(colors.ink),
        )
        // Tables keep only their row rules: a transparent cell border color
        // leaves the column dividers painted by the row slack invisible.
        .table_cell(StyleRefinement::default().border_color(theme.transparent))
        .code_block(code_block);
        let code_languages = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).document_repository())
            .map_or_else(
                || crate::language::resources::LanguageResources::installed().code_languages(),
                |repo| repo.language_resources().code_languages(),
            );
        let follow_link = {
            let reader = reader.clone();
            let workspace = self.workspace.clone();
            let path = self.path.clone();
            let editor_preview = self.editor_preview;
            move |target: &SharedString, event: &ClickEvent, window: &mut Window, cx: &mut App| {
                if event.is_right_click() {
                    context_menu::show(
                        reader.clone(),
                        Some(target.clone()),
                        None,
                        event.position(),
                        window,
                        cx,
                    );
                    return;
                }
                if matches!(event, ClickEvent::Mouse(click) if matches!(click.up.button, MouseButton::Navigate(_)))
                {
                    return;
                }
                let target = target.clone();
                let reader = reader.clone();
                let workspace = workspace.clone();
                let path = path.clone();
                window.defer(cx, move |window, cx| {
                    if editor_preview {
                        _ = reader.update(cx, |reader, cx| {
                            reader.follow_preview_link(&target, window, cx);
                        });
                    } else {
                        _ = workspace.update(cx, |workspace, cx| {
                            workspace.follow_document_link(&path, &target, window, cx);
                        });
                    }
                });
            }
        };
        let mut extensions = self.document.extensions.clone();
        if let Some(translations) = &self.translation {
            let translations = translations.clone();
            let prose = self.document.prose_keys.clone();
            let follow_link = follow_link.clone();
            let typography = self.typography.body_font_size;
            let style = style.clone();
            let inline_code = inline_code.clone();
            extensions = extensions.prose_suffix(move |offset, _, _| {
                let markdown = translations.get(prose.get(&offset)?)?;
                Some(
                    div()
                        .pt(rems(0.25))
                        .min_w_0()
                        .debug_selector(move || format!("document-translated-prose:{offset}"))
                        .child(
                            TextView::markdown(("document-translation", offset), markdown.clone())
                                .style(style.clone())
                                .text_color(translation_foreground)
                                .text_size(px(typography as f32))
                                .font_weight(FontWeight::NORMAL)
                                .line_height(relative(1.72))
                                .inline_code_style(inline_code.clone())
                                .on_link_click(follow_link.clone()),
                        ),
                )
            });
        }
        let body = TextView::new(&self.text)
            .code_languages(code_languages)
            .scrollable(true)
            .wheel_line_height(px(30.))
            .style(style)
            .inline_code_style(inline_code)
            .blockquote_style(StyleRefinement::default().border_color(quote_border))
            .text_size(px(self.typography.body_font_size as f32))
            .line_height(relative(1.72))
            .text_color(body_foreground)
            .flex_1()
            .h_full()
            .min_h_0()
            .w_full()
            .markdown_extensions(extensions)
            .image_source_with_block({
                let images = images.clone();
                let thumbnails = self.image_thumbnails.clone();
                move |uri, block| {
                    images
                        .source(uri, block)
                        .map(|source| source.fit_to_display(&thumbnails))
                        .unwrap_or_else(|| {
                            ImageSource::Custom(Arc::new(|_, _| {
                                Some(Err(
                                    anyhow::anyhow!("Unsupported document image source").into()
                                ))
                            }))
                        })
                }
            })
            .on_image_click({
                let reader = reader.clone();
                move |uri, _, window, cx| {
                    _ = reader.update(cx, |reader, cx| reader.preview_image(uri, window, cx));
                }
            })
            .on_image_context_menu({
                let reader = reader.clone();
                move |uri, link, event, window, cx| {
                    context_menu::show(
                        reader.clone(),
                        link.cloned(),
                        Some(uri.clone()),
                        event.position(),
                        window,
                        cx,
                    );
                }
            })
            .link_tooltip(|target, window, cx| Tooltip::new(target.clone()).build(window, cx))
            .on_link_click(follow_link)
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
        let body = div()
            .relative()
            .flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .min_h_0()
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                let text = this.text.read(cx);
                if event.keystroke.modifiers.modified() || !text.focus_handle().is_focused(window) {
                    return;
                }
                if let Some(key) = event.keystroke.key.chars().next()
                    && event.keystroke.key.len() == 1
                    && this.scroll_or_jump(key, cx)
                {
                    window.prevent_default();
                    cx.stop_propagation();
                    return;
                }
                let direction = match event.keystroke.key.as_str() {
                    "pageup" => -1.,
                    "pagedown" => 1.,
                    _ => return,
                };
                let list = this.text.read(cx).list_state();
                list.navigate_by(list.viewport_bounds().size.height * (direction * 0.9));
                cx.stop_propagation();
                cx.notify();
            }))
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
                                reader.fragment_jump_block.is_some()
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
                    .child(body)
                    .when(show_outline, |el| el.child(self.outline(cx, colors))),
            )
    }
}

fn copy_button(code: SharedString) -> Button {
    compact_icon_button("copy-code", IconName::Copy, t!("docs.copy_code"))
        .debug_selector(|| "docs-copy-code".into())
        .on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(code.to_string()));
        })
}

pub(crate) fn read_for_reload(repo: Option<&Repository>, path: &Path) -> Result<Option<String>> {
    if let Some(repo) = repo.filter(|_| !path.is_absolute()) {
        return repo
            .bytes_if_present(path)?
            .map(decode_markdown)
            .transpose();
    }
    match read_local_resource(path, false, None) {
        Ok(bytes) => decode_markdown(bytes).map(Some),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn load_local(path: &Path) -> Result<PreparedDocument> {
    let bytes = read_local_resource(path, false, ResourceCache::for_desktop().as_ref())?;
    PreparedDocument::new(decode_markdown(bytes)?)
}

pub fn load(repo: &Repository, path: &Path) -> Result<PreparedDocument> {
    anyhow::ensure!(is_markdown(path), t!("docs.select_markdown"));
    let remote = (!path.is_absolute())
        .then(|| repo.remote_resources())
        .flatten();
    let cache = ResourceCache::for_desktop();
    let read = || {
        if let Some(remote) = &remote {
            remote.read(path, false, cache.as_ref())
        } else {
            let target = if path.is_absolute() {
                path.to_owned()
            } else {
                repo.root.join(path)
            };
            let target = target.canonicalize()?;
            anyhow::ensure!(target.is_file(), t!("storage.regular_file"));
            read_local_resource(&target, false, cache.as_ref())
        }
    };
    let source = read()
        .and_then(decode_markdown)
        .with_context(|| t!("docs.read_failed", path = paths::display(path)).into_owned())?;
    PreparedDocument::from_source(source, remote)
}

#[cfg(test)]
impl DocReader {
    pub(crate) fn block_bounds(&self, index: usize, cx: &App) -> Bounds<Pixels> {
        self.text
            .read(cx)
            .list_state()
            .bounds_for_item(index)
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn markdown_alerts_keep_body_offsets_and_fall_back_to_quotes(cx: &mut TestAppContext) {
        let source = concat!(
            "> [!nOtE]\n> **First paragraph**\n>\n> - Another item\n",
            "\n> [!OTHER]\n> Unknown type\n",
            "\n> > [!TIP]\n> > Nested quote\n",
            "\n> [!WARNING]No separator\n> Ordinary quote\n",
            "\n> \\[!CAUTION]\n> Escaped marker\n",
            "\n> [!WARNING] **Prepare** `config.toml` [guide][setup]\r\n> Alert body.\r\n",
            "\r\n> [!TIP] Title without a body\r\n",
            "\n> [!IMPORTANT] *Open emphasis\n> Body ends with punctuation*\n",
            "\n> [!NOTE] Lazy continuation example\nUnquoted body text.\n",
            "\n[setup]: ./setup.md\n",
        );
        let paragraph = source.find("First paragraph").unwrap();
        let paragraph_range = paragraph..paragraph + "First paragraph".len();
        let marker = source.find("[!nOtE]").unwrap();
        let title_source = "**Prepare** `config.toml` [guide][setup]";
        let title_start = source.find(title_source).unwrap();
        let title_range = title_start..title_start + title_source.len();
        let warning_marker = source.find("[!WARNING] **").unwrap();
        let body_start = source.find("Alert body.").unwrap();

        for document in [
            PreparedDocument::new(source.into()).unwrap(),
            PreparedDocument::from_editor(source.into(), None).unwrap(),
        ] {
            assert_eq!(document.prose_keys[&(paragraph - 2)], "**First paragraph**");
            assert!(document.prose_keys[&title_start].contains("Prepare"));
            assert_eq!(document.prose_keys[&body_start], "Alert body&#46;");
            let state = cx.update(|cx| {
                cx.new(|cx| {
                    TextViewState::markdown_with_extensions(
                        &document.source,
                        document.extensions,
                        cx,
                    )
                })
            });
            state.read_with(cx, |state, _| {
                let rendered = state.rendered_text();
                assert_eq!(rendered.source(), source);
                assert!(
                    rendered
                        .range_for_source(marker..marker + "[!nOtE]".len())
                        .is_none()
                );
                let range = rendered.range_for_source(paragraph_range.clone()).unwrap();
                assert_eq!(&rendered.as_str()[range], "First paragraph");
                assert!(
                    rendered
                        .range_for_source(warning_marker..warning_marker + "[!WARNING]".len())
                        .is_none()
                );
                let title = rendered.range_for_source(title_range.clone()).unwrap();
                assert_eq!(&rendered.as_str()[title], "Prepare config.toml guide");
                let body = rendered
                    .range_for_source(body_start..body_start + "Alert body.".len())
                    .unwrap();
                assert_eq!(&rendered.as_str()[body], "Alert body.");
                for text in [
                    "Another item",
                    "[!OTHER]",
                    "[!TIP]",
                    "[!WARNING]No separator",
                    "[!CAUTION]",
                    "Title without a body",
                    "*Open emphasis\nBody ends with punctuation*",
                    "Lazy continuation example\nUnquoted body text.",
                ] {
                    assert!(rendered.as_str().contains(text), "{text}: {rendered:?}");
                }
            });
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn markdown_alerts_and_quotes_fit_their_painted_content() -> Result<()> {
        use gpui_component::ThemeMode;
        use gpui_wgpu::WgpuHeadlessRenderer;

        const SOURCE: &str = concat!(
            "Opening paragraph.\n",
            "\n> A quote with enough words to wrap in a narrow preview.\n>\n> A second paragraph.\n",
            "\n> [!NOTE] Read `setup.md`\n> Read the full chapter before moving on to the next steps.\n>\n> Keep reading.\n",
            "\n> [!TIP]\n> Preview the changes.\n",
            "\n> [!IMPORTANT]\n> Add the images.\n",
            "\n> [!WARNING] Check the links from the new location before sharing\n",
            "\n> [!CAUTION]\n> Review the screenshots.\n",
            "\nEnd of document.\n",
        );

        let mut app = HeadlessAppContext::with_platform(
            gpui_platform::current_platform(true).text_system(),
            Arc::new(gpui_kit_assets::AllAssets),
            || Ok(Some(Box::new(WgpuHeadlessRenderer::new()?))),
        );
        let mut wide_title_height = None;
        for (mode, width, scale, colors) in [
            (
                ThemeMode::Light,
                580.,
                1.,
                [0xd7dce8, 0x365ac3, 0x18794e, 0x7547bd, 0x8c5c11, 0xb03c34],
            ),
            (
                ThemeMode::Dark,
                330.,
                2.,
                [0x404550, 0x8ea2ff, 0x65c994, 0xbd9cf6, 0xd3a95f, 0xe3837a],
            ),
        ] {
            app.update(|cx| {
                gpui_component::init(cx);
                crate::workspace::apply_theme(mode, cx);
            });
            let window = app.open_window(size(px(width), px(790.)), |window, cx| {
                cx.new(|cx| {
                    DocReader::new(
                        PathBuf::new(),
                        "alerts.md".into(),
                        PreparedDocument::new(SOURCE.into()).unwrap(),
                        None,
                        WeakEntity::new_invalid(),
                        window,
                        cx,
                    )
                })
            })?;
            app.simulate_scale_factor_change(window.into(), scale)?;
            app.run_until_parked();
            let read_rails = |window: &mut Window, cx: &mut App| {
                window.draw(cx).clear(cx);
                let painted = window.painted_quads();
                let width = 3. * window.scale_factor();
                colors.map(|color| {
                    let mut matching = painted.iter().filter(|quad| {
                        quad.bounds.size.width.as_f32() == width
                            && quad.background.as_solid() == Some(rgb(color).into())
                    });
                    let rail = matching
                        .next()
                        .unwrap_or_else(|| {
                            let narrow: Vec<_> = painted
                                .iter()
                                .filter(|quad| quad.bounds.size.width.as_f32() <= width)
                                .map(|quad| (quad.bounds, quad.background.as_solid()))
                                .collect();
                            panic!("{mode:?}: missing three-pixel rail for {color:#x}: {narrow:?}");
                        })
                        .bounds;
                    assert!(matching.next().is_none(), "duplicate rail for {color:#x}");
                    rail
                })
            };
            let rails = app.update_window(window.into(), |_, window, cx| read_rails(window, cx))?;
            let title_height = rails[4].size.height.as_f32() / scale;
            if let Some(wide) = wide_title_height {
                assert!(
                    title_height - wide > 12.,
                    "the title wraps in the narrow pane"
                );
            } else {
                wide_title_height = Some(title_height);
            }
            let screenshot = app.capture_screenshot(window.into())?;
            assert!(
                rails
                    .windows(2)
                    .all(|pair| pair[0].bottom() < pair[1].top())
            );

            for (index, rail) in rails.into_iter().enumerate() {
                let x = rail.left().as_f32() as u32;
                let top = rail.top().as_f32() as u32;
                let bottom = rail.bottom().as_f32() as u32;
                let background = *screenshot.get_pixel(x - 1, top);
                let probe = (4. * scale) as u32;
                let left = x + (10. * scale) as u32;
                let right = screenshot.width() - (40. * scale) as u32;
                let ink: Vec<_> = (top - probe..bottom + probe)
                    .filter(|&y| (left..right).any(|x| screenshot.get_pixel(x, y) != &background))
                    .collect();
                let first = *ink.first().expect("painted quote or alert");
                let last = *ink.last().unwrap() + 1;
                assert!(
                    first.abs_diff(top) <= probe / 2 && last.abs_diff(bottom) <= probe / 2,
                    "{mode:?} block {index}: rail {top}..{bottom}, content {first}..{last}",
                );
            }

            app.update_window(window.into(), |reader, window, cx| {
                let reader = reader.downcast::<DocReader>().unwrap();
                let text = reader.read(cx).text.clone();
                let sentence = "Review the screenshots.";
                let source = SOURCE.find(sentence).unwrap();
                text.update(cx, |state, cx| {
                    let range = state
                        .rendered_text()
                        .range_for_source(source..source + sentence.len())
                        .unwrap();
                    state
                        .set_range_highlights([RangeHighlight::new(range, gpui::red())], cx)
                        .unwrap();
                });
                assert_eq!(read_rails(window, cx), rails);
                assert!(
                    window
                        .painted_quads()
                        .iter()
                        .any(|quad| quad.background.as_solid() == Some(gpui::red()))
                );
            })?;
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn document_translation_preserves_inline_code_appearance_and_prose_contrast() -> Result<()> {
        let mut app = HeadlessAppContext::with_platform(
            gpui_platform::current_platform(true).text_system(),
            Arc::new(gpui_kit_assets::AllAssets),
            || Ok(Some(Box::new(gpui_wgpu::WgpuHeadlessRenderer::new()?))),
        );
        for mode in [
            gpui_component::ThemeMode::Light,
            gpui_component::ThemeMode::Dark,
        ] {
            app.update(|cx| {
                gpui_component::init(cx);
                crate::workspace::apply_theme(mode, cx);
            });
            let window = app.open_window(size(px(580.), px(200.)), |window, cx| {
                let document =
                    PreparedDocument::new("Original prose with `value`.".into()).unwrap();
                let source = document.prose_keys[&0].clone();
                cx.new(|cx| {
                    let mut reader = DocReader::new(
                        PathBuf::new(),
                        "translation.md".into(),
                        document,
                        None,
                        WeakEntity::new_invalid(),
                        window,
                        cx,
                    );
                    reader.set_translation(
                        Some(Arc::new(HashMap::from([(
                            source,
                            "Translated prose with `value`.".into(),
                        )]))),
                        cx,
                    );
                    reader
                })
            })?;
            app.run_until_parked();
            let prose = app.update_window(window.into(), |_, window, cx| {
                window.set_scale_factor(2.);
                window.draw(cx).clear(cx);
                let mut code: Vec<_> = window
                    .painted_quads()
                    .into_iter()
                    .filter(|quad| quad.background.as_solid() == Some(Palette::for_mode(mode).soft))
                    .map(|quad| quad.bounds)
                    .collect();
                code.sort_by(|a, b| a.top().as_f32().total_cmp(&b.top().as_f32()));
                let [original, translated] = code.as_slice() else {
                    panic!("{mode:?}: expected two inline code backgrounds, got {code:?}");
                };
                assert!(original.bottom() <= translated.top());
                assert!((original.size.width - translated.size.width).as_f32().abs() <= 1.);
                assert!(
                    (original.size.height - translated.size.height)
                        .as_f32()
                        .abs()
                        <= 1.
                );
                *translated
            })?;

            let image = app.capture_screenshot(window.into())?;
            let expected = match mode {
                gpui_component::ThemeMode::Light => [0x31u8, 0x34, 0x3c],
                gpui_component::ThemeMode::Dark => [0xbeu8, 0xc1, 0xc9],
            };
            assert!(
                image.enumerate_pixels().any(|(x, y, pixel)| {
                    (x as f32) < prose.left().as_f32()
                        && (prose.top().as_f32()..prose.bottom().as_f32()).contains(&(y as f32))
                        && pixel.0[..3]
                            .iter()
                            .zip(expected)
                            .all(|(channel, expected)| channel.abs_diff(expected) <= 3)
                }),
                "{mode:?}: translated prose should paint close to the body foreground"
            );
        }
        Ok(())
    }

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

    #[test]
    fn finds_explicit_html_ids_with_exact_case_and_decoded_attributes() {
        let source = "# Title\n\n<span ID='Part&amp;One'>Label</span>\n\n<!-- <p id='comment'> -->\n\n```html\n<img id='example'>\n```";
        let document = PreparedDocument::new(source.into()).unwrap();
        assert!(document.has_fragment("Part&One"));
        assert!(document.has_fragment("title"));
        let table = "<table>\n<tr><td id='cell'>Target</td></tr>\n</table>";
        assert_eq!(fragment_source_offset(table, "cell"), table.find("<tr>"));
        for missing in ["part&one", "comment", "example"] {
            assert!(!document.has_fragment(missing));
        }
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn markdown_html_cell_jump_uses_the_target_row_within_the_table(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let rows = (0..40)
            .map(|index| format!("<tr><td id='Row-{index}'>Item {index}</td></tr>"))
            .collect::<Vec<_>>()
            .join("\n");
        let source = format!(
            "[Jump](#Row-20)\n\n<table><tbody>\n{rows}\n</tbody></table>\n\n{}",
            "Following paragraph.\n\n".repeat(25),
        );
        let (reader, visual) = cx.add_window_view(|window, cx| {
            DocReader::new(
                PathBuf::new(),
                "README.md".into(),
                PreparedDocument::new(source).unwrap(),
                None,
                WeakEntity::new_invalid(),
                window,
                cx,
            )
        });
        visual.simulate_resize(size(px(850.), px(430.)));
        draw_scroll_test_frame(visual);
        let reveal = |visual: &mut VisualTestContext, id: &str| {
            visual.update(|_, cx| {
                reader.update(cx, |reader, cx| {
                    assert!(reader.jump_to_fragment_with_motion(id, false, cx));
                });
            });
            draw_scroll_test_frame(visual);
            visual.update(|_, cx| {
                let reader = reader.read(cx);
                let text = reader.text.read(cx);
                assert!(!text.has_pending_reveal());
                assert_eq!(reader.history_fragment.as_deref(), Some(id));
                let list = text.list_state();
                let table = list.bounds_for_item(1).unwrap();
                table.top() - list.viewport_bounds().top()
            })
        };
        let first = reveal(visual, "Row-0");
        let middle = reveal(visual, "Row-20");
        assert!(first.abs() < px(30.));
        assert!(middle < first - px(450.));
    }

    #[cfg(target_os = "linux")]
    fn draw_scroll_test_frame(visual: &mut VisualTestContext) {
        for _ in 0..4 {
            visual.run_until_parked();
            visual.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
            });
        }
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn ssh_document_paints_before_images_arrive_and_keeps_its_reading_position(
        cx: &mut TestAppContext,
    ) {
        use std::{
            fs,
            process::Command,
            thread,
            time::{Duration, Instant},
        };

        use crate::remote::{SshSession, SshTarget};

        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let read_log = root.join("image-requests");
        let release = root.join("release-image");
        let source = format!(
            "# Ready before images\n\n![Delayed](visible.svg)\n\nReading anchor\n\n{}\n![Below](offscreen.svg)\n",
            "Another paragraph.\n\n".repeat(80),
        );
        fs::write(root.join("README.md"), &source).unwrap();
        fs::write(
            root.join("visible.svg"),
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="120"><rect width="120" height="120" fill="red"/></svg>"#,
        )
        .unwrap();

        // Speak the same framed protocol as the remote helper and hold image replies.
        let mut helper = Command::new("python3");
        helper
            .arg("-u")
            .arg("-c")
            .arg(
                r#"
import base64
import json
from pathlib import Path
import struct
import sys
import time

root = Path(sys.argv[1])
reader = sys.stdin.buffer
writer = sys.stdout.buffer
while header := reader.read(4):
    call = json.loads(reader.read(struct.unpack(">I", header)[0]))
    request = call["request"]
    if "Hello" in request:
        reply = {"Ok": request["Hello"]}
    elif "OpenWorkspace" in request:
        reply = {"Ok": {
            "identity": "document-fixture", "root": str(root), "common_dir": str(root / ".git"),
            "name": "documents", "branch": "main", "children": {},
        }}
    elif "Read" in request:
        path, asset = request["Read"]
        if asset:
            with (root / "image-requests").open("a") as log:
                log.write(path + "\n")
            deadline = time.monotonic() + 10
            while not (root / "release-image").is_file() and time.monotonic() < deadline:
                time.sleep(.005)
        try:
            reply = {"Ok": base64.b64encode((root / path).read_bytes()).decode()}
        except OSError:
            reply = {"Err": "Missing fixture image"}
    else:
        reply = {"Err": "Unexpected fixture request"}
    data = json.dumps({"id": call["id"], "result": reply}).encode()
    writer.write(struct.pack(">I", len(data)) + data)
    writer.flush()
"#,
            )
            .arg(root);
        let target = SshTarget {
            host: "fixture".into(),
            port: None,
            path: root.to_owned(),
        };
        let session = SshSession::connect_test_helper(target, helper).unwrap();
        let repo = Repository::open_ssh(session, root.to_owned()).unwrap();
        let document = load(&repo, Path::new("README.md")).unwrap();
        assert!(
            !read_log.exists(),
            "document preparation reads only Markdown"
        );

        cx.update(gpui_component::init);
        let (reader, visual) = cx.add_window_view(|window, cx| {
            DocReader::new(
                repo.root,
                "README.md".into(),
                document,
                None,
                WeakEntity::new_invalid(),
                window,
                cx,
            )
        });
        visual.executor().allow_parking();
        visual.simulate_resize(size(px(640.), px(260.)));
        let until = |visual: &mut VisualTestContext,
                     ready: &mut dyn FnMut(&mut VisualTestContext) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                draw_scroll_test_frame(visual);
                if ready(visual) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for document resource"
                );
                thread::sleep(Duration::from_millis(5));
            }
        };
        until(visual, &mut |_| read_log.exists());
        visual.update(|_, cx| {
            let text = reader.read(cx).text.read(cx);
            assert!(
                text.viewport_line().is_some(),
                "the first frame painted text"
            );
        });
        assert_eq!(fs::read_to_string(&read_log).unwrap(), "visible.svg\n");
        assert!(!release.exists());

        let list = visual.update(|_, cx| {
            let text = reader.read(cx).text.read(cx);
            let list = text.list_state().clone();
            list.scroll_to(ListOffset {
                item_ix: text
                    .block_for_source_offset(source.find("Reading anchor").unwrap())
                    .unwrap(),
                offset_in_item: px(0.),
            });
            list
        });
        draw_scroll_test_frame(visual);
        let reading_line = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| reader.read(cx).text.read(cx).viewport_line().unwrap())
        };
        let before = reading_line(visual);
        let old_height = list.max_offset_for_scrollbar().y;
        fs::write(&release, "").unwrap();
        until(visual, &mut |_| {
            list.max_offset_for_scrollbar().y > old_height + px(40.)
        });
        assert_eq!(reading_line(visual), before);
        assert_eq!(fs::read_to_string(&read_log).unwrap(), "visible.svg\n");
    }

    #[cfg(target_os = "linux")]
    fn document_scroll_thumb(
        visual: &mut VisualTestContext,
        axis: Axis,
        viewport: Bounds<Pixels>,
    ) -> Option<Bounds<Pixels>> {
        visual.update(|window, _| {
            let scale = window.scale_factor();
            window
                .painted_quads()
                .into_iter()
                .map(|quad| {
                    let to_pixels = |value: ScaledPixels| px(value.as_f32() / scale);
                    Bounds::new(
                        point(to_pixels(quad.bounds.left()), to_pixels(quad.bounds.top())),
                        size(
                            to_pixels(quad.bounds.size.width),
                            to_pixels(quad.bounds.size.height),
                        ),
                    )
                })
                .find(|bounds| match axis {
                    Axis::Horizontal => {
                        (px(5.9)..=px(8.1)).contains(&bounds.size.height)
                            && bounds.size.width >= px(18.)
                            && viewport.contains(&bounds.center())
                    }
                    Axis::Vertical => {
                        (bounds.size.width - px(6.)).abs() < px(0.1)
                            && bounds.size.height >= px(18.)
                            && (viewport.right() - bounds.right()).abs() < px(16.)
                    }
                })
        })
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn markdown_reader_scrolls_from_both_margins_and_paints_the_thumb_at_the_pane_edge(
        cx: &mut TestAppContext,
    ) {
        let source = (0..40)
            .map(|index| format!("## Section {index}\n\n{}", "Short paragraph.\n\n".repeat(5)))
            .collect::<String>();
        let (reader, visual) = cx.add_window_view(|window, cx| {
            gpui_component::init(cx);
            cx.set_reduce_motion(true);
            cx.set_smooth_scrolling(false);
            let mut reader = DocReader::new(
                PathBuf::new(),
                PathBuf::from("README.md"),
                PreparedDocument::new(source).unwrap(),
                None,
                WeakEntity::new_invalid(),
                window,
                cx,
            );
            reader.set_outline_layout(px(220.), Some(true), false);
            reader
        });
        visual.simulate_resize(size(px(1500.), px(540.)));
        draw_scroll_test_frame(visual);

        let (list, outline) = visual.update(|_, cx| {
            let reader = reader.read(cx);
            (
                reader.text.read(cx).list_state().clone(),
                reader.outline_scroll.clone(),
            )
        });
        let viewport = list.viewport_bounds();
        assert!(viewport.size.width > px(1100.), "{viewport:?}");
        assert!(outline.bounds().left() - viewport.right() < px(16.));
        let thumb = document_scroll_thumb(visual, Axis::Vertical, viewport)
            .expect("the thumb is on the right edge of the reading pane");

        let y = viewport.center().y;
        for x in [viewport.left() + px(36.), viewport.right() - px(36.)] {
            let before = list.scroll_px_offset_for_scrollbar().y;
            visual.simulate_event(ScrollEvent {
                position: point(x, y),
                delta: ScrollDelta::Lines(point(0., -2.)),
                ..Default::default()
            });
            draw_scroll_test_frame(visual);
            assert_eq!(list.scroll_px_offset_for_scrollbar().y, before - px(60.));
        }

        let moved_thumb = document_scroll_thumb(visual, Axis::Vertical, viewport).unwrap();
        assert!(moved_thumb.top() > thumb.top());
        let before = list.scroll_px_offset_for_scrollbar().y;
        visual.simulate_mouse_move(moved_thumb.center(), None, Modifiers::default());
        visual.simulate_mouse_down(
            moved_thumb.center(),
            MouseButton::Left,
            Modifiers::default(),
        );
        let drag_to = moved_thumb.center() + point(px(0.), px(50.));
        visual.simulate_mouse_move(drag_to, MouseButton::Left, Modifiers::default());
        visual.simulate_mouse_up(drag_to, MouseButton::Left, Modifiers::default());
        draw_scroll_test_frame(visual);
        assert!(list.scroll_px_offset_for_scrollbar().y < before);
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn markdown_wide_blocks_route_vertical_input_to_the_reader_and_scroll_horizontally(
        cx: &mut TestAppContext,
    ) {
        let diagram = include_str!("../tests/fixtures/mermaid-layout.md")
            .split_once("## Wide horizontal chain\n\n")
            .unwrap()
            .1
            .split_once("\n\n## ")
            .unwrap()
            .0
            .replacen(
                "```mermaid\n",
                "```mermaid\n---\nconfig:\n  layout: elk\n---\n",
                1,
            );
        let formula = format!("$$\n{}z\n$$", "x+".repeat(110));
        let table = format!(
            "|{}|\n|{}|\n|{}|",
            " Column header |".repeat(27).trim_end_matches('|'),
            "---|".repeat(27).trim_end_matches('|'),
            " entry |".repeat(27).trim_end_matches('|'),
        );
        let code = format!("```text\n{}\n```", "0123456789".repeat(40));
        for (kind, block) in [
            ("Mermaid", diagram.as_str()),
            ("formula", formula.as_str()),
            ("table", table.as_str()),
            ("code", code.as_str()),
        ] {
            let preceding = "Before the block.\n\n".repeat(3);
            let source = format!(
                "{preceding}{block}\n\n{}",
                "Short paragraph.\n\n".repeat(100)
            );
            let (reader, visual) = cx.add_window_view(|window, cx| {
                gpui_component::init(cx);
                cx.set_reduce_motion(true);
                cx.set_smooth_scrolling(false);
                DocReader::new(
                    PathBuf::new(),
                    PathBuf::from("README.md"),
                    PreparedDocument::new(source).unwrap(),
                    None,
                    WeakEntity::new_invalid(),
                    window,
                    cx,
                )
            });
            visual.simulate_resize(size(px(530.), px(540.)));
            draw_scroll_test_frame(visual);
            let list = visual.update(|_, cx| reader.read(cx).text.read(cx).list_state().clone());
            let viewport = list.viewport_bounds();
            let position = list.bounds_for_item(3).unwrap().center();
            let page_y = || list.scroll_px_offset_for_scrollbar().y;
            let thumb = |visual: &mut VisualTestContext| {
                document_scroll_thumb(visual, Axis::Horizontal, viewport).unwrap_or_else(|| {
                    panic!(
                        "{kind}: missing horizontal thumb at page offset {:?}",
                        page_y()
                    )
                })
            };
            let send = |visual: &mut VisualTestContext, source, phase, delta, shift| {
                visual.simulate_event(ScrollEvent {
                    position,
                    delta,
                    source,
                    touch_phase: phase,
                    modifiers: Modifiers {
                        shift,
                        ..Default::default()
                    },
                });
                draw_scroll_test_frame(visual);
            };

            let touchpad = |visual: &mut VisualTestContext, phase, x, y| {
                send(
                    visual,
                    ScrollSource::Touchpad,
                    phase,
                    ScrollDelta::Pixels(point(px(x), px(y))),
                    false,
                );
            };
            let shift_wheel = |visual: &mut VisualTestContext, x, y| {
                send(
                    visual,
                    ScrollSource::Pointer,
                    TouchPhase::Moved,
                    ScrollDelta::Lines(point(x, y)),
                    true,
                );
            };

            let start = thumb(visual);
            touchpad(visual, TouchPhase::Started, -62., -19.);
            let horizontal = thumb(visual);
            assert!(
                horizontal.left() > start.left(),
                "{kind}: touchpad horizontal"
            );
            touchpad(visual, TouchPhase::Moved, -16., -80.);
            assert_eq!(page_y(), px(0.), "{kind}: keep the initial horizontal axis");
            assert!(
                thumb(visual).left() > horizontal.left(),
                "{kind}: horizontal continuation"
            );
            touchpad(visual, TouchPhase::Ended, 0., 0.);

            let before = thumb(visual);
            shift_wheel(visual, 0., -5.);
            assert!(
                thumb(visual).left() > before.left(),
                "{kind}: Shift-wheel on Y"
            );
            let before = thumb(visual);
            shift_wheel(visual, 5., 0.);
            assert!(
                thumb(visual).left() < before.left(),
                "{kind}: native Shift-wheel on X"
            );
            assert_eq!(page_y(), px(0.), "{kind}: Shift-wheel only moves the block");

            let drag_from = thumb(visual).center();
            visual.simulate_mouse_move(drag_from, None, Modifiers::default());
            visual.simulate_mouse_down(drag_from, MouseButton::Left, Modifiers::default());
            let drag_to = point(drag_from.x + px(40.), drag_from.y);
            visual.simulate_mouse_move(drag_to, MouseButton::Left, Modifiers::default());
            visual.simulate_mouse_up(drag_to, MouseButton::Left, Modifiers::default());
            draw_scroll_test_frame(visual);
            assert!(
                thumb(visual).center().x > drag_from.x,
                "{kind}: horizontal thumb drag"
            );
            assert_eq!(page_y(), px(0.), "{kind}: thumb drag only moves the block");

            shift_wheel(visual, 0., -1000.);
            let block_x = thumb(visual).left();
            touchpad(visual, TouchPhase::Started, -80., -19.);
            touchpad(visual, TouchPhase::Moved, 0., -60.);
            touchpad(visual, TouchPhase::Ended, 0., 0.);
            assert_eq!(
                page_y(),
                px(0.),
                "{kind}: horizontal boundary never moves the page"
            );
            assert_eq!(
                thumb(visual).left(),
                block_x,
                "{kind}: horizontal boundary stays clamped"
            );

            for (source, phase, delta, distance) in [
                (
                    ScrollSource::Pointer,
                    TouchPhase::Moved,
                    ScrollDelta::Lines(point(0., -1.)),
                    px(30.),
                ),
                (
                    ScrollSource::PreciseWheel,
                    TouchPhase::Moved,
                    ScrollDelta::Pixels(point(px(0.), px(-17.))),
                    px(17.),
                ),
                (
                    ScrollSource::Touchpad,
                    TouchPhase::Started,
                    ScrollDelta::Pixels(point(px(-6.), px(-24.))),
                    px(24.),
                ),
            ] {
                list.set_offset_from_scrollbar(point(px(0.), px(0.)));
                draw_scroll_test_frame(visual);
                send(visual, source, phase, delta, false);
                assert_eq!(
                    page_y(),
                    -distance,
                    "{kind}: vertical {source:?} belongs to the page"
                );
                assert_eq!(
                    thumb(visual).left(),
                    block_x,
                    "{kind}: vertical input never moves the block"
                );
            }
        }
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
            let bounds = text.list_state().bounds_for_item(heading).unwrap();
            assert!(
                (bounds.top() - body.top()).abs() < px(28.),
                "{bounds:?} {body:?}"
            );
        });

        visual.update(|_, cx| {
            reader.update(cx, |reader, cx| {
                reader.restore_position(Some(DocumentPosition {
                    source_offset: Some(reader.document.headings[20].source.start),
                    block: 0,
                    inset: 0.,
                    line: None,
                }));
                cx.notify();
            });
        });
        draw(visual);
        assert_eq!(selected(visual), 20);
        assert!(outline.intersects(&row(20)));

        let list = visual.update(|window, cx| {
            cx.set_smooth_scrolling(true);
            cx.set_reduce_motion(false);
            let text = reader.read(cx).text.read(cx);
            let list = text.list_state().clone();
            text.focus_handle().clone().focus(window, cx);
            list
        });
        let prior_page = list.logical_scroll_top();
        visual.simulate_keystrokes("pagedown");
        draw(visual);
        assert_eq!(list.logical_scroll_top(), prior_page);
        assert!(list.is_navigating());
        crate::workspace::finish_scroll_navigation(visual);
        assert!(list.logical_scroll_top().item_ix > prior_page.item_ix);
        visual.update(|_, cx| cx.set_smooth_scrolling(false));

        visual.update(|window, cx| reader.update(cx, |reader, cx| reader.toggle_find(window, cx)));
        draw(visual);
        visual.simulate_input("Section 34");
        draw(visual);
        // Reveal preserves the minimum body scroll, so the previous section may
        // still own the reading line with the match visible at the bottom.
        let after_find = selected(visual);
        assert!((33..=34).contains(&after_find));
        assert!(outline.intersects(&row(after_find)));

        // Reissuing Find from the body and from the query retains the current search.
        visual.update(|window, cx| {
            reader
                .read(cx)
                .text
                .read(cx)
                .focus_handle()
                .clone()
                .focus(window, cx);
        });
        for _ in 0..2 {
            visual.update(|window, cx| reader.update(cx, |reader, cx| reader.find(window, cx)));
            draw(visual);
            visual.update(|window, cx| {
                let reader = reader.read(cx);
                assert!(reader.search.read(cx).has_matches());
                assert!(reader.search.read(cx).focus_handle(cx).is_focused(window));
            });
            assert_eq!(selected(visual), after_find);
        }

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

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn final_nested_heading_aligns_at_top_with_half_screen_of_normal_end_scroll(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let source = format!(
            "{}> Quoted text.\n>\n> ## Final heading\n>\n> Last line.",
            "Paragraph.\n\n".repeat(50)
        );
        let final_block = source.find("> Quoted").unwrap();
        let (reader, visual) = cx.add_window_view(|window, cx| {
            DocReader::new(
                PathBuf::new(),
                "README.md".into(),
                PreparedDocument::new(source).unwrap(),
                None,
                WeakEntity::new_invalid(),
                window,
                cx,
            )
        });
        let draw = |visual: &mut VisualTestContext| {
            for _ in 0..4 {
                visual.run_until_parked();
                visual.update(|window, cx| window.draw(cx).clear(cx));
            }
        };
        draw(visual);
        let list = visual.update(|_, cx| reader.read(cx).text.read(cx).list_state().clone());
        let initial_max = list.max_offset_for_scrollbar().y;
        list.set_offset_from_scrollbar(point(px(0.), -initial_max));
        visual.update(|_, cx| reader.update(cx, |_, cx| cx.notify()));
        draw(visual);
        let last = list.bounds_for_item(list.item_count() - 1).unwrap();
        let viewport = list.viewport_bounds();
        assert!((last.bottom() - viewport.center().y).abs() <= px(1.));
        visual.update(|_, cx| {
            reader.update(cx, |reader, cx| {
                assert!(reader.jump_to_fragment("final-heading", cx));
            })
        });
        draw(visual);
        let position = visual.update(|_, cx| reader.read(cx).session_position(cx).unwrap());
        assert_eq!(position.source_offset, Some(final_block));
        assert!(
            position.inset > 12.,
            "a nested title must align within its containing block"
        );
        assert!(list.max_offset_for_scrollbar().y > initial_max);
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn wrapped_reading_line_survives_history_and_reopening_at_new_width_and_font(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let source = (0..220)
            .map(|ix| format!("Topic {ix:03} has a café and a `unique marker`. "))
            .collect::<String>();
        let build = |window: &mut Window, cx: &mut Context<DocReader>| {
            DocReader::new(
                PathBuf::new(),
                "README.md".into(),
                PreparedDocument::new(source.clone()).unwrap(),
                None,
                WeakEntity::new_invalid(),
                window,
                cx,
            )
        };
        let draw = |visual: &mut VisualTestContext| {
            for _ in 0..8 {
                visual.run_until_parked();
                visual.update(|window, cx| {
                    window.simulate_next_frame(cx);
                    window.draw(cx).clear(cx);
                });
            }
        };
        let scroll_by =
            |visual: &mut VisualTestContext, reader: &Entity<DocReader>, delta: Pixels| {
                visual.update(|_, cx| {
                    reader.update(cx, |reader, cx| {
                        let list = reader.text.read(cx).list_state();
                        let y = list.scroll_px_offset_for_scrollbar().y - delta;
                        list.set_offset_from_scrollbar(point(px(0.), y));
                        cx.notify();
                    })
                });
                draw(visual);
            };

        let (reader, visual) = cx.add_window_view(build);
        visual.simulate_resize(size(px(520.), px(420.)));
        draw(visual);
        scroll_by(visual, &reader, px(716.));
        let old = visual.update(|_, cx| reader.read(cx).session_position(cx).unwrap());
        let line = old.line.expect("visible wrapped line");
        assert_eq!(old.block, 0);
        assert!(line.source_offset > 500);

        let confirm_anchor = |visual: &mut VisualTestContext, reader: &Entity<DocReader>| {
            let placed =
                visual.update(|_, cx| reader.read(cx).session_position(cx).unwrap().line.unwrap());
            assert!(
                (placed.viewport_inset - line.viewport_inset).abs() <= 1.5,
                "expected inset {line:?}, got {placed:?}"
            );
            scroll_by(visual, reader, px(42.));
            let following =
                visual.update(|_, cx| reader.read(cx).session_position(cx).unwrap().line.unwrap());
            assert!(
                placed.source_offset <= line.source_offset
                    && line.source_offset < following.source_offset,
                "expected the original character {line:?} in the top row between {placed:?} and {following:?}"
            );
        };
        let typography = crate::config::Docs {
            body_font_size: 23.,
            ..Default::default()
        };
        visual.simulate_resize(size(px(760.), px(420.)));
        visual.update(|_, cx| {
            reader.update(cx, |reader, cx| {
                reader.set_typography(typography, cx);
                reader.replay_history_location(
                    DocumentLocation {
                        position: Some(old),
                        fragment: None,
                    },
                    true,
                    cx,
                );
            })
        });
        draw(visual);
        confirm_anchor(visual, &reader);

        let (reopened, visual) = cx.add_window_view(build);
        visual.simulate_resize(size(px(760.), px(420.)));
        // Reopening a document restores the same serialized reading point as history.
        let saved = serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
        visual.update(|_, cx| {
            reopened.update(cx, |reader, cx| {
                reader.set_typography(typography, cx);
                reader.restore_position(Some(saved));
            })
        });
        draw(visual);
        confirm_anchor(visual, &reopened);
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn mermaid_reader_reflows_and_restores_its_horizontal_layout(cx: &mut TestAppContext) {
        let (reader, visual) = cx.add_window_view(|window, cx| {
            gpui_component::init(cx);
            DocReader::new(
                PathBuf::new(),
                "review.md".into(),
                PreparedDocument::new(
                    "# Workflow\n\n```mermaid\nflowchart LR\nA[Receive] --> B[Review] --> C[Approve] --> D[Record] --> E[Publish] --> F[Notify]\n```\n".into(),
                )
                .unwrap(),
                None,
                WeakEntity::new_invalid(),
                window,
                cx,
            )
        });
        let mut heights = Vec::new();
        for width in [860., 470., 860., 470.] {
            visual.simulate_resize(size(px(width), px(900.)));
            for _ in 0..6 {
                draw_scroll_test_frame(visual);
            }
            heights.push(visual.update(|_, cx| {
                reader
                    .read(cx)
                    .text
                    .read(cx)
                    .list_state()
                    .bounds_for_item(1)
                    .unwrap()
                    .size
                    .height
            }));
        }
        assert!(heights[1] > heights[0] + px(120.), "{heights:?}");
        assert!(heights[1] > px(440.), "{heights:?}");
        assert_eq!(heights[0], heights[2]);
        assert_eq!(heights[1], heights[3]);
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn mermaid_reader_scrolls_each_wide_diagram_to_its_own_edges(cx: &mut TestAppContext) {
        let fixture = include_str!("../tests/fixtures/mermaid-layout.md");
        let (_, chain) = fixture.split_once("## Wide horizontal chain\n\n").unwrap();
        let (chain, _) = chain.split_once("\n\n## ").unwrap();
        let wide = chain.replacen(
            "flowchart LR",
            "---\nconfig:\n  layout: elk\n---\nflowchart LR",
            1,
        );
        let shorter = wide.replace("    F --> G[Update catalog]\n", "");
        let source = format!("# Wide diagrams\n\n{wide}\n\n## Second diagram\n\n{shorter}\n");
        let (reader, visual) = cx.add_window_view(|window, cx| {
            gpui_component::init(cx);
            cx.set_reduce_motion(true);
            cx.set_smooth_scrolling(false);
            DocReader::new(
                PathBuf::new(),
                "mermaid-layout.md".into(),
                PreparedDocument::new(source).unwrap(),
                None,
                WeakEntity::new_invalid(),
                window,
                cx,
            )
        });
        visual.simulate_resize(size(px(530.), px(420.)));
        draw_scroll_test_frame(visual);
        visual
            .executor()
            .advance_clock(std::time::Duration::from_millis(100));
        draw_scroll_test_frame(visual);
        let raster_jobs = visual.update(|_, cx| svg_raster_stats(cx).0);
        assert!(raster_jobs >= 2);
        let list = visual.update(|_, cx| reader.read(cx).text.read(cx).list_state().clone());
        let blocks = [1, 3].map(|index| list.bounds_for_item(index).unwrap());
        let thumb = |visual: &mut VisualTestContext, index: usize| {
            document_scroll_thumb(visual, Axis::Horizontal, blocks[index]).unwrap()
        };
        let scroll = |visual: &mut VisualTestContext, index: usize, lines: f32| {
            visual.simulate_event(ScrollEvent {
                position: blocks[index].center(),
                delta: ScrollDelta::Lines(point(0., lines)),
                source: ScrollSource::Pointer,
                touch_phase: TouchPhase::Moved,
                modifiers: Modifiers {
                    shift: true,
                    ..Default::default()
                },
            });
            draw_scroll_test_frame(visual);
        };

        let initial = [thumb(visual, 0), thumb(visual, 1)];
        assert!(initial[0].size.width < initial[1].size.width);
        let mut expected = initial;
        for index in [0, 1] {
            scroll(visual, index, -1000.);
            let end = thumb(visual, index);
            assert!(end.left() > initial[index].left() + px(15.));
            let left_inset = initial[index].left() - blocks[index].left();
            let right_inset = blocks[index].right() - end.right();
            assert!(
                (left_inset - right_inset).abs() <= px(1.),
                "{end:?}, {:?}",
                blocks[index]
            );
            assert_eq!(thumb(visual, 1 - index), expected[1 - index]);
            expected[index] = end;
            scroll(visual, index, -1000.);
            assert_eq!(thumb(visual, index), end);
        }
        for index in [0, 1] {
            scroll(visual, index, 1000.);
            assert_eq!(thumb(visual, index), initial[index]);
            assert_eq!(thumb(visual, 1 - index), expected[1 - index]);
            expected[index] = initial[index];
        }
        assert_eq!(list.scroll_px_offset_for_scrollbar().y, px(0.));

        let mut frames = Vec::new();
        for frame in 0..80 {
            let started = std::time::Instant::now();
            visual.simulate_event(ScrollEvent {
                position: blocks[frame % 2].center(),
                delta: ScrollDelta::Pixels(point(
                    px(if frame % 20 < 10 { -31.5 } else { 31.5 }),
                    px(0.),
                )),
                source: ScrollSource::Pointer,
                touch_phase: TouchPhase::Moved,
                modifiers: Modifiers::default(),
            });
            visual.update(|window, cx| window.draw(cx).clear(cx));
            frames.push(started.elapsed());
            if frame == 8 {
                assert!(thumb(visual, 0).left() > initial[0].left());
            }
            visual
                .executor()
                .advance_clock(std::time::Duration::from_millis(16));
        }
        draw_scroll_test_frame(visual);
        assert_eq!(visual.update(|_, cx| svg_raster_stats(cx).0), raster_jobs);
        frames.sort_unstable();
        println!(
            "Mermaid horizontal scroll: {} frames, draw P95 {:.3} ms, 0 new rasters",
            frames.len(),
            frames[75].as_secs_f64() * 1000.
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mermaid_reader_renders_light_and_dark_offscreen() -> Result<()> {
        use gpui_component::ThemeMode;
        use gpui_wgpu::WgpuHeadlessRenderer;

        let mut app = HeadlessAppContext::with_platform(
            gpui_platform::current_platform(true).text_system(),
            Arc::new(gpui_kit_assets::AllAssets),
            || Ok(Some(Box::new(WgpuHeadlessRenderer::new()?))),
        );
        let output = std::env::var_os("STROLL_MERMAID_SCREENSHOTS").map(PathBuf::from);
        let fixture = include_str!("../tests/fixtures/mermaid-layout.md");
        let (flowcharts, _) = fixture.split_once("\n## Wide horizontal chain").unwrap();
        let (_, state) = fixture
            .split_once("## State diagram with parallel transitions")
            .unwrap();
        let (state, _) = state.split_once("\n## Sequence diagram").unwrap();
        let state = format!("# State diagram with parallel transitions{state}");
        for (mode, name, node_color) in [
            (ThemeMode::Light, "light", [244, 246, 251]),
            (ThemeMode::Dark, "dark", [39, 43, 56]),
        ] {
            app.update(|cx| {
                gpui_component::init(cx);
                crate::workspace::apply_theme(mode, cx);
            });
            for (case, source, width, height) in [
                ("layout", flowcharts, 690., 1330.),
                ("state", state.as_str(), 520., 350.),
            ] {
                let window = app.open_window(size(px(width), px(height)), |window, cx| {
                    cx.new(|cx| {
                        DocReader::new(
                            PathBuf::new(),
                            "mermaid-layout.md".into(),
                            PreparedDocument::new(source.into()).unwrap(),
                            None,
                            WeakEntity::new_invalid(),
                            window,
                            cx,
                        )
                    })
                })?;
                for _ in 0..8 {
                    app.run_until_parked();
                    app.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))?;
                }
                let screenshot = app.capture_screenshot(window.into())?;
                if let Some(directory) = &output {
                    screenshot.save(directory.join(format!("mermaid-{case}-{name}.png")))?;
                }
                let node_pixels = screenshot
                    .pixels()
                    .filter(|pixel| {
                        pixel.0[..3]
                            .iter()
                            .zip(node_color)
                            .all(|(&actual, expected)| actual.abs_diff(expected) <= 2)
                    })
                    .count();
                assert!(node_pixels > 1500, "{case} {name}: {node_pixels}");

                if case == "state" {
                    let overflows = app.update_window(window.into(), |_, window, _| {
                        let scale = window.scale_factor();
                        window.painted_quads().iter().any(|quad| {
                            let size = quad.bounds.size;
                            (5.9..=8.1).contains(&(size.height.as_f32() / scale))
                                && size.width.as_f32() / scale >= 18.
                        })
                    })?;
                    assert!(
                        !overflows,
                        "{case} {name}: expected wrapping to fit the graph"
                    );
                }
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn document_svg_rasters_match_display_scale_offscreen() -> Result<()> {
        use gpui_component::ThemeMode;
        use gpui_wgpu::WgpuHeadlessRenderer;
        use std::{
            thread,
            time::{Duration, Instant},
        };

        let mut app = HeadlessAppContext::with_platform(
            gpui_platform::current_platform(true).text_system(),
            Arc::new(gpui_kit_assets::AllAssets),
            || Ok(Some(Box::new(WgpuHeadlessRenderer::new()?))),
        );
        app.update(|cx| {
            gpui_component::init(cx);
            crate::workspace::apply_theme(ThemeMode::Light, cx);
        });
        let fixture = include_str!("../tests/fixtures/mermaid-layout.md");
        let (_, chart) = fixture.split_once("```mermaid").unwrap();
        let (chart, _) = chart.split_once("```").unwrap();
        let source = format!(
            "# Mermaid\n\n```mermaid{chart}```\n\n## SVG image\n\n<img src=\"tall-image.svg\" alt=\"SVG with labels\" width=\"344\" />\n",
        );
        let output = std::env::var_os("STROLL_DOCUMENT_DPI_SCREENSHOTS").map(PathBuf::from);

        for (name, dpi, svg_width) in [("100", 1., 344), ("125", 1.25, 430), ("200", 2., 688)] {
            let window = app.open_window(size(px(570.), px(1230.)), |window, cx| {
                cx.new(|cx| {
                    DocReader::new(
                        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                            .join("tests/fixtures/markdown-scroll"),
                        "comparison.md".into(),
                        PreparedDocument::new(source.clone()).unwrap(),
                        None,
                        WeakEntity::new_invalid(),
                        window,
                        cx,
                    )
                })
            })?;
            app.simulate_scale_factor_change(window.into(), dpi)?;

            let start = Instant::now();
            loop {
                app.run_until_parked();
                app.advance_clock(Duration::from_millis(16));
                let images = app.update_window(window.into(), |_, window, cx| {
                    window.draw(cx).clear(cx);
                    window.painted_images().to_vec()
                })?;
                if images.len() == 2
                    && images.iter().all(|image| {
                        image.bounds.size.width.0 == image.tile.bounds.size.width.0 as f32
                            && image.bounds.size.height.0 == image.tile.bounds.size.height.0 as f32
                    })
                {
                    assert!(
                        images
                            .iter()
                            .any(|image| image.tile.bounds.size.width.0 == svg_width)
                    );
                    break;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "{dpi}: {images:?}"
                );
                thread::sleep(Duration::from_millis(5));
            }
            let screenshot = app.capture_screenshot(window.into())?;
            assert!((screenshot.width() as f32 - 570. * dpi).abs() <= 1.);
            assert!((screenshot.height() as f32 - 1230. * dpi).abs() <= 1.);
            assert!(
                screenshot
                    .pixels()
                    .any(|pixel| pixel.0[..3] == [0x27, 0x53, 0x6b])
            );
            assert!(
                screenshot
                    .pixels()
                    .any(|pixel| pixel.0[..3] == [244, 246, 251])
            );
            if let Some(output) = &output {
                screenshot.save(output.join(format!("document-{name}.png")))?;
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn document_svg_rasters_wait_for_resize_to_settle() -> Result<()> {
        use std::{
            thread,
            time::{Duration, Instant},
        };

        let fixture = include_str!("../tests/fixtures/mermaid-layout.md");
        let (_, chart) = fixture.split_once("```mermaid").unwrap();
        let (chart, _) = chart.split_once("```").unwrap();
        let source = format!(
            "# Mermaid\n\n```mermaid{chart}```\n\n## SVG image\n\n<img src=\"tall-image.svg\" alt=\"SVG with labels\" style=\"width: 80%\" />\n",
        );
        let mut app = HeadlessAppContext::with_platform(
            gpui_platform::current_platform(true).text_system(),
            Arc::new(gpui_kit_assets::AllAssets),
            || Ok(None),
        );
        let window = app
            .open_window(size(px(530.), px(900.)), |window, cx| {
                gpui_component::init(cx);
                cx.new(|cx| {
                    DocReader::new(
                        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                            .join("tests/fixtures/markdown-scroll"),
                        "comparison.md".into(),
                        PreparedDocument::new(source).unwrap(),
                        None,
                        WeakEntity::new_invalid(),
                        window,
                        cx,
                    )
                })
            })?
            .into();
        app.simulate_scale_factor_change(window, 1.25)?;

        let draw = |app: &mut HeadlessAppContext, width: Option<f32>| {
            app.update_window(window, |_, window, cx| {
                if let Some(width) = width {
                    window.resize(size(px(width), px(900.)));
                    window.bounds_changed(cx);
                }
                window.draw(cx).clear(cx);
                let mut images = window.painted_images().to_vec();
                images.sort_by(|a, b| a.bounds.top().0.total_cmp(&b.bounds.top().0));
                images
            })
        };
        let exact = |images: &[PolychromeSprite]| {
            images.len() == 2
                && images.iter().all(|image| {
                    image.bounds.size.width.0 == image.tile.bounds.size.width.0 as f32
                        && image.bounds.size.height.0 == image.tile.bounds.size.height.0 as f32
                })
        };
        let settle = |app: &mut HeadlessAppContext| -> Result<Vec<PolychromeSprite>> {
            let started = Instant::now();
            loop {
                app.run_until_parked();
                app.advance_clock(Duration::from_millis(100));
                let images = draw(app, None)?;
                if exact(&images) {
                    return Ok(images);
                }
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "SVG rasters did not settle: {images:?}"
                );
                thread::sleep(Duration::from_millis(5));
            }
        };
        settle(&mut app)?;

        // Measure with real font metrics since Mermaid's fitting interval
        // depends on the intrinsic width of its labels.
        let mut trace = Vec::<(f32, [f32; 2])>::new();
        let mut drag_widths = None;
        for width in (350..=830).step_by(30).map(|width| width as f32) {
            draw(&mut app, Some(width))?;
            let images = draw(&mut app, None)?;
            if images.len() != 2 {
                continue;
            }
            let widths = [images[0].bounds.size.width.0, images[1].bounds.size.width.0];
            if let Some((old_width, old_images)) = trace.last()
                && width - old_width == 30.
                && widths[0] - old_images[0] >= 24.
                && widths[1] - old_images[1] >= 18.
            {
                drag_widths = Some((old_width + 3., width - 3.));
                break;
            }
            trace.push((width, widths));
        }
        let (start_width, end_width) =
            drag_widths.unwrap_or_else(|| panic!("no resize range shared by both SVGs: {trace:?}"));
        draw(&mut app, Some(start_width))?;
        let initial = settle(&mut app)?;
        let warm = app.update(|cx| svg_raster_stats(cx).0);
        let mut timings = Vec::new();
        for frame in 1..=60 {
            let started = Instant::now();
            let width = start_width + (end_width - start_width) * frame as f32 / 60.;
            let images = draw(&mut app, Some(width))?;
            timings.push(started.elapsed());
            assert_eq!(images.len(), 2, "blank raster during resize");
            app.advance_clock(Duration::from_millis(16));
            while app.background_executor().tick() {}
        }
        assert_eq!(app.update(|cx| svg_raster_stats(cx).0), warm);
        assert!(!exact(&draw(&mut app, None)?));

        let final_images = settle(&mut app)?;
        let changed = final_images
            .iter()
            .zip(&initial)
            .filter(|(after, before)| after.tile.bounds.size != before.tile.bounds.size)
            .count();
        let (completed, bytes, _) = app.update(|cx| svg_raster_stats(cx));
        assert_eq!(changed, 2, "both types of SVG should resize");
        assert_eq!(completed - warm, changed);
        assert!(bytes <= 64 * 1024 * 1024);
        timings.sort_unstable();
        println!(
            "Continuous resize: {} frames, draw P95 {:.3} ms, 0 raster jobs during drag, {changed} after settling",
            timings.len(),
            timings[56].as_secs_f64() * 1000.
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn display_math_is_centered_and_scales_with_document_typography() -> Result<()> {
        use gpui_component::ThemeMode;
        use gpui_wgpu::WgpuHeadlessRenderer;

        let mut app = HeadlessAppContext::with_platform(
            gpui_platform::current_platform(true).text_system(),
            Arc::new(gpui_kit_assets::AllAssets),
            || Ok(Some(Box::new(WgpuHeadlessRenderer::new()?))),
        );
        app.update(|cx| {
            gpui_component::init(cx);
            crate::workspace::apply_theme(ThemeMode::Light, cx);
        });
        let width = 640.;
        let window = app.open_window(size(px(width), px(240.)), |window, cx| {
            cx.new(|cx| {
                DocReader::new(
                    PathBuf::new(),
                    "math.md".into(),
                    PreparedDocument::new(
                        r"$$
\frac{\sqrt{2\pi}}{1+x^2}=\sum_{n=1}^{\infty}n^{-2}
$$"
                        .into(),
                    )
                    .unwrap(),
                    None,
                    WeakEntity::new_invalid(),
                    window,
                    cx,
                )
            })
        })?;
        let mut expected_width = None;
        for (mode, font_size, dpi) in [
            (ThemeMode::Light, 16., 1.),
            (ThemeMode::Light, 16., 1.5),
            (ThemeMode::Dark, 24., 1.5),
        ] {
            app.update(|cx| crate::workspace::apply_theme(mode, cx));
            app.update_window(window.into(), |reader, window, cx| {
                window.set_scale_factor(dpi);
                reader
                    .downcast::<DocReader>()
                    .unwrap()
                    .update(cx, |reader, cx| {
                        reader.set_typography(
                            crate::config::Docs {
                                body_font_size: font_size,
                                ..Default::default()
                            },
                            cx,
                        );
                    });
            })?;
            for _ in 0..5 {
                app.run_until_parked();
                app.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))?;
            }
            let screenshot = app.capture_screenshot(window.into())?;
            if let Some(directory) = std::env::var_os("STROLL_THEME_SCREENSHOTS") {
                screenshot.save(
                    PathBuf::from(directory).join(format!("math-{mode:?}-{font_size}-{dpi}.png")),
                )?;
            }
            // RaTeX ink has a known dark or light base color. Select pixels
            // near that color to account for the antialiased outline.
            let mut left = u32::MAX;
            let mut right = 0;
            for (x, y, pixel) in screenshot.enumerate_pixels() {
                if x as f32 >= width * dpi || y as f32 >= 120. * dpi {
                    continue;
                }
                let is_ink = match mode {
                    ThemeMode::Light => pixel[0] < 100 && pixel[3] > 200,
                    ThemeMode::Dark => pixel[0] > 160 && pixel[3] > 200,
                };
                if is_ink {
                    left = left.min(x);
                    right = right.max(x);
                }
            }
            assert!(right > left, "the formula paints ink");
            let center = (left + right) as f32 / 2. / dpi;
            assert!((center - width / 2.).abs() < 6., "formula center {center}");
            let ink_width = (right - left) as f32 / dpi;
            let baseline = *expected_width.get_or_insert(ink_width);
            let expected = baseline * font_size as f32 / 16.;
            assert!(
                (ink_width - expected).abs() < 3.,
                "ink width {ink_width}, expected {expected}"
            );
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn markdown_find_seeds_from_rendered_selection_and_reselects_the_query(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let (reader, visual) = cx.add_window_view(|window, cx| {
            DocReader::new(
                PathBuf::new(),
                PathBuf::from("README.md"),
                PreparedDocument::new("**formattedNeedle**".into()).unwrap(),
                None,
                WeakEntity::new_invalid(),
                window,
                cx,
            )
        });
        let draw = |visual: &mut VisualTestContext| {
            visual.run_until_parked();
            visual.update(|window, cx| window.draw(cx).clear(cx));
        };
        let find = |visual: &mut VisualTestContext| {
            visual.update(|window, cx| reader.update(cx, |reader, cx| reader.find(window, cx)));
            draw(visual);
        };
        let assert_selected_query = |visual: &mut VisualTestContext, expected: &str| {
            visual.update(|window, cx| {
                assert!(
                    reader
                        .read(cx)
                        .search
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)
                );
                cx.write_to_clipboard(ClipboardItem::new_string("clipboard sentinel".into()));
            });
            visual.simulate_keystrokes("ctrl-c");
            visual.update(|_, cx| {
                assert_eq!(
                    cx.read_from_clipboard()
                        .and_then(|item| item.text())
                        .as_deref(),
                    Some(expected)
                );
            });
        };
        draw(visual);
        visual.update(|window, cx| {
            reader.read(cx).text.clone().update(cx, |text, cx| {
                text.focus_handle().clone().focus(window, cx);
                text.select_all(cx);
            });
        });
        find(visual);
        assert_selected_query(visual, "formattedNeedle");
        visual.update(|_, cx| assert!(reader.read(cx).search.read(cx).has_matches()));
        visual.simulate_input("missing");
        find(visual);
        assert_selected_query(visual, "missing");
        visual.update(|window, cx| {
            reader.read(cx).text.clone().update(cx, |text, cx| {
                text.focus_handle().clone().focus(window, cx);
                text.clear_selection(cx);
            });
        });
        find(visual);
        assert_selected_query(visual, "missing");
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn markdown_find_wraps_to_a_match_beyond_the_passive_highlight_limit(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let mut source = format!("{}\n\n", "hitcount ".repeat(25)).repeat(200);
        source.push_str(&"Unmatched paragraph.\n\n".repeat(40));
        let last_match = source.len();
        source.push_str("hitcount LastAnchor.\n");
        let (reader, visual) = cx.add_window_view(|window, cx| {
            DocReader::new(
                PathBuf::new(),
                PathBuf::from("README.md"),
                PreparedDocument::new(source).unwrap(),
                None,
                WeakEntity::new_invalid(),
                window,
                cx,
            )
        });
        visual.simulate_resize(size(px(1000.), px(430.)));
        let draw = |visual: &mut VisualTestContext| {
            for _ in 0..4 {
                visual.run_until_parked();
                visual.update(|window, cx| {
                    window.simulate_next_frame(cx);
                    window.draw(cx).clear(cx);
                });
            }
        };
        let last_match_is_visible = |visual: &mut VisualTestContext| {
            visual.update(|_, cx| {
                let text = reader.read(cx).text.read(cx);
                let block = text.block_for_source_offset(last_match).unwrap();
                text.list_state()
                    .bounds_for_item(block)
                    .is_some_and(|bounds| text.bounds().intersects(&bounds))
            })
        };
        draw(visual);
        visual.update(|window, cx| reader.update(cx, |reader, cx| reader.toggle_find(window, cx)));
        draw(visual);
        visual.simulate_input("hitcount");
        draw(visual);
        assert!(!last_match_is_visible(visual));
        visual.simulate_keystrokes("shift-enter");
        draw(visual);
        assert!(last_match_is_visible(visual));
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn markdown_list_and_table_checkboxes_follow_document_typography(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::workspace::apply_theme(gpui_component::ThemeMode::Light, cx);
        });
        let (reader, visual) = cx.add_window_view(|window, cx| {
            DocReader::new(
                PathBuf::new(),
                "tasks.md".into(),
                PreparedDocument::new(
                    "- [x] Checked item\n\n| Status | Task |\n| --- | --- |\n| [x] | Table item |"
                        .into(),
                )
                .unwrap(),
                None,
                WeakEntity::new_invalid(),
                window,
                cx,
            )
        });
        visual.simulate_resize(size(px(640.), px(420.)));
        for body_font_size in [16., 23.] {
            visual.update(|_, cx| {
                reader.update(cx, |reader, cx| {
                    reader.set_typography(
                        crate::config::Docs {
                            body_font_size,
                            ..Default::default()
                        },
                        cx,
                    );
                })
            });
            draw_scroll_test_frame(visual);
            visual.update(|window, cx| {
                let accent = cx.theme().accent;
                let scale = window.scale_factor();
                let mut boxes: Vec<_> = window
                    .painted_quads()
                    .into_iter()
                    .filter(|quad| quad.background.as_solid() == Some(accent))
                    .map(|quad| quad.bounds)
                    .collect();
                boxes.sort_by(|a, b| a.top().as_f32().total_cmp(&b.top().as_f32()));
                let [list, table] = boxes.as_slice() else {
                    panic!("expected two filled task indicators, got {boxes:?}");
                };
                // The table uses the reader's smaller table text size.
                for (bounds, expected) in
                    [(list, body_font_size), (table, body_font_size * 14. / 15.)]
                {
                    let width = f64::from(bounds.size.width.as_f32() / scale);
                    let height = f64::from(bounds.size.height.as_f32() / scale);
                    assert!(
                        (width - expected).abs() <= 0.75 && (height - expected).abs() <= 0.75,
                        "the task indicator follows its text size ({expected}px): {width}×{height}"
                    );
                }
            });
        }
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
