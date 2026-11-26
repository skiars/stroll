use anyhow::Context as _;
use std::path::Path;

use rust_i18n::t;

use super::*;
use crate::session::SourceLocation;
use crate::workspace::{
    FindInDocument, SourcePosition, WorkspaceView, baseline_shift,
    editor_git::{HunkDirection, HunkNavigationAnchor, hunk_toolbar},
    file_name_font_size,
    image_preview::{ImageHoverSource, decode_preview, hover_image_source},
};

use gpui_component::{
    diff::DiffSide,
    input::{Editor, Position, Rope},
    menu::{DropdownMenu, PopupMenuItem},
};

#[derive(PartialEq)]
pub(super) struct CommitSummaryLayout {
    width: Pixels,
    rem: Pixels,
    scale: f32,
    font: Font,
    locale: String,
    only_matching: bool,
    has_visible_files: bool,
    whitespace: bool,
    show_details: bool,
    message: Option<Rope>,
}

pub(super) struct PendingEditorOpen {
    pub(super) path: String,
    from: Option<SourceLocation>,
}

pub(super) struct LivePatch {
    base: Option<Arc<str>>,
    version: git::FileVersion,
}

pub(super) const IMAGE_DIFF_HEIGHT: Pixels = px(128.);
const THUMBNAIL_SIZE: Size<Pixels> = size(px(160.), px(112.));
const IMAGE_DIFF_CACHE_SIZE: usize = 64;

pub(super) enum DiffImage {
    Ready(DiffThumbnail),
    Missing,
    Unsupported,
    UnsupportedHelper,
    Failed,
}

pub(super) struct DiffThumbnail {
    pub(super) image: Arc<RenderImage>,
    hover: ImageHoverSource,
}

fn image_diff_format(file: &git::ChangedFile, side: Side) -> bool {
    let (path, mode) = match side {
        Side::Old => (
            file.old_path.as_deref().unwrap_or(&file.path),
            file.old_mode.as_str(),
        ),
        Side::New => (file.path.as_str(), file.new_mode.as_str()),
    };
    mode != "120000" && crate::image_viewer::is_image(Path::new(path))
}

fn image_diff_candidate(file: &git::ChangedFile) -> bool {
    !file.conflict()
        && !file.submodule()
        && [Side::Old, Side::New]
            .into_iter()
            .any(|side| image_diff_format(file, side))
}

fn diff_images(
    backend: &git::Backend,
    repository: &Arc<Repository>,
    detail: &Arc<Changes>,
    ix: usize,
    renderer: SvgRenderer,
    scale: f32,
) -> [DiffImage; 2] {
    if !backend.supports_image_diff() {
        return [DiffImage::UnsupportedHelper, DiffImage::UnsupportedHelper];
    }
    let cache = crate::resource_cache::ResourceCache::for_desktop();
    let file = &detail.files[ix];
    [Side::Old, Side::New].map(|side| {
        let mode = match side {
            Side::Old => &file.old_mode,
            Side::New => &file.new_mode,
        };
        if mode == "000000" && detail.comparison.commit_target().is_some() {
            return DiffImage::Missing;
        }
        // Refuse symlinks on either side, including type changes.
        if !image_diff_format(file, side) {
            return DiffImage::Unsupported;
        }
        let path = snapshot_path(
            file,
            if side == Side::Old {
                DiffSide::Original
            } else {
                DiffSide::Modified
            },
        );
        match backend.image_diff_side(repository, detail, file, side, cache.as_ref()) {
            Ok(Some(bytes)) => match decode_preview(
                path,
                &bytes.0,
                renderer.clone(),
                (THUMBNAIL_SIZE.width * scale).as_f32().ceil() as u32,
                (THUMBNAIL_SIZE.height * scale).as_f32().ceil() as u32,
            ) {
                Ok(image) => {
                    let backend = backend.clone();
                    let repository = repository.clone();
                    let detail = detail.clone();
                    let cache = cache.clone();
                    let hover = ImageHoverSource::new(path.to_owned(), move || {
                        backend
                            .image_diff_side(
                                &repository,
                                &detail,
                                &detail.files[ix],
                                side,
                                cache.as_ref(),
                            )?
                            .map(|bytes| bytes.0)
                            .context(t!("git.missing_snapshot"))
                    });
                    DiffImage::Ready(DiffThumbnail { image, hover })
                }
                Err(_) => DiffImage::Failed,
            },
            Ok(None) => DiffImage::Missing,
            Err(_) => DiffImage::Failed,
        }
    })
}

fn release_diff_images(images: [DiffImage; 2], window: &mut Window) {
    for image in images {
        if let DiffImage::Ready(thumbnail) = image {
            _ = window.drop_image(thumbnail.image);
        }
    }
}

fn translated_placeholder(file: &git::ChangedFile) -> Result<DiffFile> {
    let message = if file.conflict() {
        t!("git.file_conflict")
    } else if file.submodule() {
        t!("git.submodule_changed")
    } else {
        Cow::Borrowed("")
    };
    git::placeholder(file, &message)
}

pub(super) struct FileOperations {
    version: git::FileVersion,
    staged: Option<HunkActions>,
    unstaged: Option<HunkActions>,
    staged_worktree: Vec<std::ops::Range<usize>>,
}

struct HunkActions {
    whole_file: bool,
    blocks: Vec<git::Block>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct ActionBlocks {
    pub(super) staged: Option<usize>,
    pub(super) unstaged: Option<usize>,
}

pub(super) enum HunkTarget {
    Index(usize),
    FileEdge(HunkDirection),
    Load(HunkDirection),
}

pub(super) struct HunkNavigation {
    pub(super) path: SharedString,
    pub(super) target: HunkTarget,
    pub(super) anchor: HunkNavigationAnchor,
}

impl FileOperations {
    fn patch(&self, layer: git::Layer) -> Option<&HunkActions> {
        match layer {
            git::Layer::Staged => self.staged.as_ref(),
            git::Layer::Unstaged => self.unstaged.as_ref(),
        }
    }
}

fn block_at(blocks: &[git::Block], line: usize, original: bool) -> Option<usize> {
    let range = |block: &git::Block| {
        if original {
            block.old_start..block.old_start + block.old_count
        } else {
            block.new_start..block.new_start + block.new_count
        }
    };
    let index = blocks.partition_point(|block| range(block).end <= line);
    range(blocks.get(index)?).contains(&line).then_some(index)
}

/// Project a HEAD line that survived staging into index coordinates.
fn index_line(staged: &[git::Block], line: usize) -> Option<usize> {
    let old_end = |block: &git::Block| block.old_start + block.old_count.saturating_sub(1);
    let previous = staged.partition_point(|block| old_end(block) < line);
    if staged
        .get(previous)
        .is_some_and(|block| block.old_count > 0 && block.old_start <= line)
    {
        return None;
    }
    let Some(block) = previous.checked_sub(1).and_then(|ix| staged.get(ix)) else {
        return Some(line);
    };
    let new_end = block.new_start + block.new_count.saturating_sub(1);
    Some(new_end + (line - old_end(block)))
}

impl From<git::LayerPatch> for HunkActions {
    fn from(patch: git::LayerPatch) -> Self {
        Self {
            whole_file: patch.whole_file(),
            blocks: patch.blocks,
        }
    }
}

pub(super) fn change_stat(stats: &git::ChangeStats, file: &git::ChangedFile) -> git::ChangeStat {
    stats.get(&file.path).copied().unwrap_or(git::ChangeStat {
        binary: file.binary,
        counts: file.counts,
    })
}

impl Review {
    pub(super) fn is_uncommitted(&self) -> bool {
        matches!(
            self.detail.comparison,
            Comparison::Worktree { base: None, .. }
        )
    }

    fn defer_image_patch(&self, ix: usize, stats_loading: bool) -> bool {
        let file = &self.detail.files[ix];
        let regular = |mode: &str| matches!(mode, "100644" | "100755");
        let simple = match file.code {
            'A' => file.old_mode == "000000" && regular(&file.new_mode),
            'D' => file.new_mode == "000000" && regular(&file.old_mode),
            'M' => regular(&file.new_mode) && file.old_mode == file.new_mode,
            _ => false,
        };
        if !simple || !self.image_files[ix] || self.detail.comparison.commit_target().is_none() {
            return false;
        }
        let stat = change_stat(&self.stats, file);
        // Keep text-capable formats on the patch path until Git confirms the file is binary.
        stat.binary
            || (stats_loading
                && stat.counts.is_none()
                && matches!(
                    crate::image_viewer::format(Path::new(&file.path)),
                    Some(
                        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::Webp
                    )
                ))
    }

    fn apply_stats(&mut self, mut stats: git::ChangeStats) {
        for (ix, file) in self.detail.files.iter().enumerate() {
            if file.conflict() || file.untracked() {
                stats.remove(&file.path);
            }
            // Numstat does not cover untracked files. Keep their last count until the patch arrives.
            if file.untracked()
                && !matches!(self.patches[ix], PatchState::Ready)
                && let Some(&stat) = self.stats.get(&file.path)
            {
                stats.insert(file.path.clone(), stat);
            }
        }
        self.stats = Rc::new(stats);
        self.update_totals();
        self.summary_layout = None;
    }

    fn update_totals(&mut self) {
        self.totals = self
            .detail
            .files
            .iter()
            .enumerate()
            .filter_map(|(ix, file)| {
                self.counts
                    .get(&ix)
                    .copied()
                    .or(change_stat(&self.stats, file).counts)
            })
            .fold((0, 0), |(a, d), (add, del)| (a + add, d + del));
    }

    fn reconcile_worktree(&mut self, previous: &mut Self, changed: &HashSet<String>) {
        let base_changed = self.detail.comparison != previous.detail.comparison
            || self.detail.base != previous.detail.base;
        let same_tree = self.detail.files.len() == previous.detail.files.len()
            && self
                .detail
                .files
                .iter()
                .zip(&previous.detail.files)
                .all(|(new, old)| new.path == old.path && new.old_path == old.old_path);
        if same_tree {
            *self.tree_rows.get_mut() = previous.tree_rows.get_mut().take();
        }
        for (ix, file) in self.detail.files.iter().enumerate() {
            let Some(&old) = previous.by_path.get(&file.path) else {
                continue;
            };
            let before = &previous.detail.files[old];
            self.openable[ix] = previous.openable[old];
            if file.conflict() || before.conflict() || file.submodule() || before.submodule() {
                continue;
            }
            if file.counts.is_none() && !file.binary {
                let mut stat = change_stat(&previous.stats, before);
                stat.counts = previous.counts.get(&old).copied().or(stat.counts);
                if stat.binary || stat.counts.is_some() {
                    Rc::make_mut(&mut self.stats).insert(file.path.clone(), stat);
                }
            }
            self.patch_display[ix] = std::mem::take(&mut previous.patch_display[old]);
            // Keep the last displayed content while a changed file's replacement loads.
            if self.patch_display[ix].truncated.is_some() {
                self.documents[ix] = previous.documents[old].clone();
                // These actions refer to the patch still on screen. Git verifies its version.
                if let Some(version) = previous.versions.remove(&old) {
                    self.versions.insert(ix, version);
                }
                if let Some(operations) = previous.operations.remove(&old) {
                    self.operations.insert(ix, operations);
                }
                if let Some(sources) = previous.sources.remove(&old) {
                    self.sources.insert(ix, sources);
                }
                if let Some(live) = previous.live.remove(&old) {
                    self.live.insert(ix, live);
                }
            }
            let affected = |path: &str| {
                std::path::Path::new(path)
                    .ancestors()
                    .filter_map(std::path::Path::to_str)
                    .any(|path| changed.contains(path))
            };
            let affected = affected(&file.path) || file.old_path.as_deref().is_some_and(affected);
            if base_changed
                || affected
                || file.code != before.code
                || file.worktree != before.worktree
                || file.old_path != before.old_path
                || file.old_mode != before.old_mode
                || file.new_mode != before.new_mode
            {
                continue;
            }
            if let Some(images) = previous.images.remove(&old) {
                self.images.insert(ix, images);
            }
            if matches!(previous.patches[old], PatchState::Failed) {
                self.patches[ix] = PatchState::Failed;
            }
            if !matches!(previous.patches[old], PatchState::Ready) {
                continue;
            }
            if let Some(assets) = previous.pending_assets.remove(&old) {
                self.pending_assets.insert(ix, assets);
            }
            self.patches[ix] = previous.patches[old].clone();
            if let Some(&counts) = previous.counts.get(&old) {
                Rc::make_mut(&mut self.counts).insert(ix, counts);
            }
        }
        self.update_totals();
        self.image_order
            .extend(previous.image_order.iter().filter_map(|&old| {
                let &ix = self.by_path.get(&previous.detail.files[old].path)?;
                self.images.contains_key(&ix).then_some(ix)
            }));
    }
}

// Display requests share a diff process when their options and layers match.
fn prefetch(
    backend: &git::Backend,
    detail: &Changes,
    requests: &[(usize, usize)],
    whitespace: bool,
) -> HashMap<usize, git::FileChanges> {
    let mut indices = Vec::new();
    let mut batch = Vec::new();
    for &(ix, context) in requests {
        if let Some(mode) = batchable(backend, detail, &detail.files[ix]) {
            indices.push(ix);
            batch.push((
                detail.files[ix].path.clone(),
                git::FileDiffOptions {
                    mode,
                    context: if mode == Mode::All { 0 } else { context },
                    whitespace,
                },
            ));
        }
    }
    if batch.len() < 2 {
        return HashMap::new();
    }
    backend
        .file_changes_batch(&batch)
        .map(|results| indices.into_iter().zip(results).collect())
        .unwrap_or_default()
}

fn batchable(backend: &git::Backend, detail: &Changes, file: &git::ChangedFile) -> Option<Mode> {
    if !backend.supports_git_edit() {
        return None;
    }
    let Comparison::Worktree { mode, base: None } = detail.comparison else {
        return None;
    };
    (!file.submodule() && !file.conflict()).then_some(mode)
}

pub(super) fn has_live_target(comparison: &Comparison) -> bool {
    matches!(comparison.worktree_mode(), Some(Mode::All | Mode::Unstaged))
}

pub(super) fn is_custom_worktree(comparison: &Comparison) -> bool {
    has_live_target(comparison)
        && matches!(
            comparison,
            Comparison::Versions { .. } | Comparison::Worktree { base: Some(_), .. }
        )
}

pub(super) fn aligns_historical_snapshots(comparison: &Comparison) -> bool {
    matches!(
        comparison,
        Comparison::Commit { .. }
            | Comparison::Versions { .. }
            | Comparison::Worktree { base: Some(_), .. }
    )
}

fn snapshot_worktree_path(
    comparison: &Comparison,
    file: &git::ChangedFile,
    side: DiffSide,
) -> Option<PathBuf> {
    (aligns_historical_snapshots(comparison)
        && (side == DiffSide::Original || comparison.commit_target().is_some()))
    .then(|| {
        PathBuf::from(if side == DiffSide::Original {
            file.old_path.as_deref().unwrap_or(&file.path)
        } else {
            &file.path
        })
    })
}

#[derive(Clone)]
pub(super) struct PatchAssets {
    version: Option<git::FileVersion>,
    operations: Option<git::FileDiffOptions>,
    source: Option<(SharedString, Option<git::Layer>)>,
    content: Option<[u8; 32]>,
}

impl PatchAssets {
    fn operations(
        &self,
        backend: &git::Backend,
        file: &git::ChangedFile,
    ) -> Option<FileOperations> {
        let version = self.version.as_ref()?;
        let actions = backend.file_changes(&file.path, self.operations?).ok()?;
        if &actions.version != version {
            return None;
        }
        let mut operations = FileOperations {
            version: actions.version,
            staged: None,
            unstaged: None,
            staged_worktree: Vec::new(),
        };
        for patch in actions.layers {
            let destination = match patch.layer {
                git::Layer::Staged => &mut operations.staged,
                git::Layer::Unstaged => &mut operations.unstaged,
            };
            *destination = Some(patch.into());
        }
        if let Some(staged) = operations.patch(git::Layer::Staged) {
            let unstaged = operations
                .patch(git::Layer::Unstaged)
                .map_or(&[][..], |patch| patch.blocks.as_slice());
            let project = super::super::editor_git::worktree_line;
            operations.staged_worktree = staged
                .blocks
                .iter()
                .map(|block| {
                    let start = block
                        .new_start
                        .saturating_sub(usize::from(block.new_count > 0));
                    let end = start + block.new_count;
                    project(start, unstaged, true)..project(end, unstaged, block.new_count == 0)
                })
                .collect();
        }
        Some(operations)
    }

    fn sources(
        &self,
        backend: &git::Backend,
        detail: &Changes,
        file: &git::ChangedFile,
    ) -> Vec<((SharedString, DiffSide), Rope, &'static str)> {
        let mut sources = Vec::new();
        if file.submodule() || file.conflict() || file.binary {
            return sources;
        }
        if let Some((path, layer)) = &self.source {
            for (side, diff_side, source_path) in [
                (
                    Side::Old,
                    DiffSide::Original,
                    file.old_path.as_deref().unwrap_or(&file.path),
                ),
                (Side::New, DiffSide::Modified, file.path.as_str()),
            ] {
                let snapshot = if let Some(layer) = layer {
                    backend.layer_snapshot(&file.path, *layer, side)
                } else {
                    backend.snapshot(detail, file, side)
                };
                if let Ok(git::Snapshot::Text {
                    text,
                    truncated: false,
                }) = snapshot
                {
                    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
                    let rope = Rope::from(text.as_str());
                    let language = super::super::language_for_source(Path::new(source_path), &rope);
                    sources.push(((path.clone(), diff_side), rope, language));
                }
            }
        }
        if !sources.is_empty()
            && self.version.as_ref().is_some_and(|version| {
                backend.file_version(&file.path).ok().as_ref() != Some(version)
            })
        {
            sources.clear();
        }
        sources
    }
}

pub(super) fn is_all_worktree_changes(comparison: &Comparison) -> bool {
    matches!(
        comparison,
        Comparison::Worktree {
            mode: Mode::All,
            base: None
        }
    )
}

fn review_patch(
    backend: &git::Backend,
    detail: &Changes,
    file: &git::ChangedFile,
    context: usize,
    whitespace: bool,
    prefetched: Option<git::FileChanges>,
) -> Result<(git::Patch, PatchAssets)> {
    let mode = batchable(backend, detail, file);
    let mixed = is_all_worktree_changes(&detail.comparison);
    let verified = mode.and_then(|mode| {
        prefetched
            .or_else(|| {
                backend
                    .file_changes(
                        &file.path,
                        git::FileDiffOptions {
                            mode,
                            context: if mixed { 0 } else { context },
                            whitespace,
                        },
                    )
                    .ok()
            })
            .filter(|changes| changes.version.status.as_ref() == file.worktree.as_ref())
    });
    // A draft can only use the baseline from the same source version as the displayed patch.
    let custom_version = (is_custom_worktree(&detail.comparison) && !file.binary)
        .then(|| backend.file_version(&file.path).ok())
        .flatten()
        .filter(|version| version.status.as_ref() == file.worktree.as_ref());
    let layer = (!mixed)
        .then(|| verified.as_ref()?.layers.first())
        .flatten();
    let (patch, content) = if let Some(layer) = layer {
        (git::render_patch(file, layer.patch.clone())?, None)
    } else if mixed {
        let raw = backend.review_patch_text(detail, file, context, whitespace)?;
        let content = raw.content_fingerprint();
        (git::render_patch(file, raw)?, Some(content))
    } else {
        (backend.patch(detail, file, context, whitespace)?, None)
    };
    let source = (verified.is_some() || mixed || mode.is_none())
        .then(|| (patch.document.path().clone(), layer.map(|p| p.layer)));
    let operations = mode
        .filter(|_| verified.is_some())
        .map(|mode| git::FileDiffOptions {
            mode,
            context: 0,
            whitespace,
        });
    let version = verified.map(|changes| changes.version).or_else(|| {
        custom_version.filter(|version| {
            !patch.truncated
                && !patch.document.is_binary()
                && backend.file_version(&file.path).ok().as_ref() == Some(version)
        })
    });
    Ok((
        patch,
        PatchAssets {
            version,
            operations,
            source,
            content,
        },
    ))
}

fn snapshot_path(file: &git::ChangedFile, side: DiffSide) -> &Path {
    let path = if side == DiffSide::Original {
        file.old_path.as_deref().unwrap_or(&file.path)
    } else {
        &file.path
    };
    Path::new(path)
}

fn scope_diff_document(
    mut document: DiffFile,
    workspace: &WeakEntity<Workspace>,
    cx: &App,
) -> DiffFile {
    if let Some(workspace) = workspace.upgrade() {
        let workspace = workspace.read(cx);
        for (side, path) in [
            (DiffSide::Original, document.original_path()),
            (DiffSide::Modified, document.modified_path()),
        ]
        .map(|(side, path)| (side, path.cloned()))
        {
            if let Some(language) = path.and_then(|path| {
                let path = Path::new(path.as_ref());
                workspace.source_resources(path).for_path(path)
            }) {
                document = document.with_side_language(side, language);
            }
        }
    }
    document
}

impl GitPanel {
    pub(in crate::workspace) fn refresh_file_associations(
        &mut self,
        associations: &crate::config::FileAssociations,
        repo: Option<&crate::repository::Repository>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let refresh =
            |editor: &Entity<EditorState>, path: &Path, window: &mut Window, cx: &mut App| {
                let resources = crate::language::resources::LanguageResources::for_file(repo, path);
                let language = super::super::associated_highlight(associations, resources, path)
                    .map(|(_, syntax)| syntax)
                    .or_else(|| resources.for_path(path))
                    .unwrap_or_else(|| {
                        super::super::language_for_source(path, editor.read(cx).text()).into()
                    });
                if editor.read(cx).language_name() != language {
                    editor.update(cx, |editor, cx| {
                        editor.set_highlighter(language, cx);
                        editor.ensure_highlighter_factory(
                            gpui_component::highlighter::input_highlighter_factory(),
                        );
                        editor.prepare_language(window, cx);
                    });
                }
            };
        // Projected diffs take their syntax from these snapshots and the live source editors.
        // Refresh them without replacing the diff documents or their expanded context.
        if let Some(review) = &self.review {
            for (ix, sources) in &review.sources {
                let file = &review.detail.files[*ix];
                for ((_, side), snapshot) in sources {
                    refresh(snapshot.editor(), snapshot_path(file, *side), window, cx);
                }
            }
        }
        if let Some(snapshot) = &self.snapshot
            && let Some(editor) = &snapshot.editor
        {
            let side = if snapshot.side == Side::Old {
                DiffSide::Original
            } else {
                DiffSide::Modified
            };
            refresh(
                editor,
                snapshot_path(&snapshot.detail.files[snapshot.file], side),
                window,
                cx,
            );
        }
        App::notify(cx, self.review_pane.entity_id());
        cx.notify();
    }

    pub(super) fn refresh_placeholders(&mut self, cx: &mut Context<Self>) {
        let Some(review) = &mut self.review else {
            return;
        };
        let replacements = review
            .detail
            .files
            .iter()
            .enumerate()
            .filter_map(|(ix, file)| {
                let has_mode_notice = !file.old_mode.is_empty()
                    && !file.new_mode.is_empty()
                    && file.old_mode != file.new_mode
                    && (change_stat(&review.stats, file).binary
                        || review.patch_display[ix].truncated.is_none());
                if (file.conflict() || file.submodule() || has_mode_notice)
                    && let Ok(document) = translated_placeholder(file)
                {
                    review.documents[ix] = document.clone();
                    review.patch_display[ix].content = None;
                    Some(document)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        self.diff
            .update(cx, |diff, cx| diff.replace_files(replacements, cx));
        self.editor.invalidate();
    }

    pub(super) fn load_review(
        &mut self,
        selection: Selection,
        preserve: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.load_review_at(selection, preserve, None, false, window, cx);
    }

    pub(super) fn load_review_at(
        &mut self,
        selection: Selection,
        preserve: bool,
        mut position: Option<DiffPosition>,
        restoring: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if restoring
            && selection == Selection::Worktree(Mode::All)
            && let Some(position) = &mut position
            && let Some(path) = position.path.strip_prefix('\0')
        {
            // Old sessions stored the unstaged section under a synthetic path.
            position.path = path.to_owned();
            if position.line.is_some_and(|(original, _)| original) {
                position.line = None;
                position.inset = 0.;
            }
        }
        self.load_review_with_changes(
            selection, preserve, position, restoring, None, false, window, cx,
        );
    }

    pub(in crate::workspace) fn open_blame_commit(
        &mut self,
        commit: &git::BlameLine,
        source: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = &mut self.startup_session {
            session.selection = None;
            session.diff = None;
        }
        self.history_pick.clear();
        let selection = Selection::Commit(commit.oid.clone(), None);
        let position = DiffPosition {
            path: commit.path.clone(),
            line: Some((false, commit.original)),
            inset: 0.,
            horizontal: 0.,
        };
        self.load_review_with_changes(
            selection,
            true,
            Some(position),
            false,
            None,
            source,
            window,
            cx,
        );
    }

    fn load_review_with_changes(
        &mut self,
        selection: Selection,
        preserve: bool,
        position: Option<DiffPosition>,
        restoring: bool,
        changed: Option<HashSet<String>>,
        open_source: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !preserve && self.selection.as_ref() == Some(&selection) && self.detail_error.is_none() {
            return;
        }
        let keep = preserve && self.selection == Some(selection.clone());
        let background = changed.is_some();
        let viewport = position.as_ref().map(super::diff_anchor).or_else(|| {
            (keep && changed.is_some())
                .then(|| self.editor.change_anchor(self.diff.read(cx), cx))
                .flatten()
        });
        let source = position.as_ref().filter(|_| open_source).cloned();
        let blame_navigation = preserve && position.is_some() && !restoring && changed.is_none();
        self.loading_session_diff = position;
        self.pending_file_reveal = None;
        if !keep || !background {
            self.pending_hunk_navigation = None;
            self.pending_editor_open = None;
        }
        self.pending_review_anchor = None;
        if self.selection.as_ref() != Some(&selection) {
            self.cancel_ai_commit();
        }
        self.selection = Some(selection.clone());
        if !keep {
            self.editor.invalidate();
            self.editor.clear_hidden_selections();
        }
        self.detail_loading = true;
        self.live_tasks.clear();
        if !background {
            self.confirm_restore = None;
            self.snapshot_task = Task::ready(());
            self.snapshot = None;
        }
        if !keep {
            self.detail_error = None;
        }
        self.patch_tasks = std::array::from_fn(|_| Task::ready(()));
        self.patch_loading = [false; 2];
        self.image_queue.clear();
        self.image_task = None;
        self.viewport_reads.clear();
        self.assets_task = Task::ready(());
        self.assets_loading = false;
        self.metadata_tasks = std::array::from_fn(|_| Task::ready(()));
        self.stats_loading = false;
        self.patch_requests.clear();
        self.viewport_queue.clear();
        self.prefetch_first = None;
        self.prefetch_forward = true;
        if let Some(review) = &mut self.review {
            for state in &mut review.patches {
                if matches!(state, PatchState::Loading) {
                    *state = PatchState::Pending;
                }
            }
        }
        self.pending_position = None;
        if !keep {
            if let Some(review) = self.review.take() {
                if review.is_uncommitted() {
                    self.worktree_collapsed_files = review.collapsed;
                }
                for (_, images) in review.images {
                    release_diff_images(images, window);
                }
            }
            self.diff.update(cx, |diff, cx| diff.set_files([], cx));
            self.tree_scroll = UniformListScrollHandle::new();
            self.tree_followed_file = None;
            self.show_details = false;
        }
        let restoring_commit = restoring && matches!(selection, Selection::Commit(..));
        let backend = self.backend.clone();
        let foreign = self
            .foreign_branch()
            .then(|| self.branch_oid())
            .flatten()
            .map(str::to_owned);
        let task = cx.background_spawn(async move {
            let comparison = match selection {
                Selection::Commit(oid, parent) => {
                    let commit = match backend.commit(&oid) {
                        Ok(commit) => commit,
                        Err(_) if restoring_commit && !backend.disconnected() => return Ok(None),
                        Err(error) => return Err(error),
                    };
                    let parent = parent
                        .filter(|parent| !restoring_commit || commit.parents.contains(parent))
                        .or_else(|| commit.parents.first().cloned());
                    Comparison::Commit {
                        commit: Box::new(commit),
                        parent,
                    }
                }
                Selection::CommitRange { base, newest, .. } => {
                    backend.check_revision_comparison()?;
                    Comparison::Versions {
                        base,
                        target: Some(newest),
                    }
                }
                Selection::Worktree(mode) => Comparison::Worktree {
                    mode,
                    base: foreign,
                },
                Selection::Compare { base, target, .. } => Comparison::Versions { base, target },
            };
            let (detail, needs_stats) = backend.review_files(comparison)?;
            let locale = rust_i18n::locale().to_string();
            let documents = detail
                .files
                .iter()
                .map(translated_placeholder)
                .collect::<Result<Vec<_>>>()?;
            let totals = detail
                .files
                .iter()
                .filter_map(|file| file.counts)
                .fold((0, 0), |(add, del), (a, d)| (add + a, del + d));
            Ok::<_, anyhow::Error>(Some((detail, documents, totals, locale, needs_stats)))
        });
        self.detail_task = cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, move |this, window, cx| {
                this.detail_loading = false;
                this.loading_session_diff = None;
                match result {
                    Ok(Some((detail, documents, totals, locale, needs_stats))) => {
                        this.detail_error = None;
                        let viewport = viewport.or_else(|| {
                            keep.then(|| this.diff.read(cx).viewport_anchor()).flatten()
                        });
                        let mut previous = this.review.take();
                        let active_path = previous.as_ref().and_then(|review| {
                            let active = review.active?;
                            Some(review.detail.files[active].path.as_str())
                        });
                        let by_path = detail
                            .files
                            .iter()
                            .enumerate()
                            .map(|(ix, file)| (file.path.clone(), ix))
                            .collect::<HashMap<_, _>>();
                        let active = active_path
                            .and_then(|path| by_path.get(path).copied())
                            .or_else(|| {
                                let anchor = viewport.as_ref().filter(|_| this.single_file)?;
                                by_path.get(anchor.path()).copied()
                            })
                            .or_else(|| (!keep && !detail.files.is_empty()).then_some(0));
                        let patches = detail
                            .files
                            .iter()
                            .map(|file| {
                                if file.submodule() || file.conflict() {
                                    PatchState::Ready
                                } else {
                                    PatchState::Pending
                                }
                            })
                            .collect();
                        let uncommitted =
                            matches!(detail.comparison, Comparison::Worktree { base: None, .. });
                        let directories = if uncommitted {
                            this.worktree_directories.clone()
                        } else {
                            previous
                                .as_ref()
                                .map(|review| review.directories.clone())
                                .unwrap_or_default()
                        };
                        let mut collapsed = previous.as_ref().map_or_else(
                            || {
                                if uncommitted {
                                    this.worktree_collapsed_files.clone()
                                } else {
                                    Arc::default()
                                }
                            },
                            |review| review.collapsed.clone(),
                        );
                        if uncommitted && let Some(status) = &detail.status {
                            Arc::make_mut(&mut collapsed).retain(|path| {
                                status.contains_key(path) || by_path.contains_key(path)
                            });
                        }
                        this.review = Some(Review {
                            patch_display: vec![PatchDisplay::default(); detail.files.len()],
                            image_files: detail.files.iter().map(image_diff_candidate).collect(),
                            images: HashMap::new(),
                            image_order: VecDeque::new(),
                            image_redraw_pending: false,
                            openable: vec![false; detail.files.len()],
                            detail: Arc::new(detail),
                            patches,
                            documents,
                            operations: HashMap::new(),
                            pending_assets: HashMap::new(),
                            versions: HashMap::new(),
                            sources: HashMap::new(),
                            live: HashMap::new(),
                            contexts: previous
                                .as_ref()
                                .map(|review| {
                                    review
                                        .contexts
                                        .iter()
                                        .filter_map(|(&ix, &context)| {
                                            let new = by_path.get(&review.detail.files[ix].path)?;
                                            Some((*new, context))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default(),
                            by_path: Rc::new(by_path),
                            counts: Rc::default(),
                            stats: Rc::default(),
                            totals,
                            tree_rows: Default::default(),
                            tree_motion: Default::default(),
                            summary_layout: None,
                            active,
                            directories,
                            collapsed,
                        });
                        let anchor_neighbor = viewport.as_ref().and_then(|anchor| {
                            let old = previous.as_ref()?;
                            let ix = *old.by_path.get(anchor.path())?;
                            let review = this.review.as_ref()?;
                            let visible = |file: &git::ChangedFile| {
                                review.by_path.get(&file.path).copied().filter(|&ix| {
                                    !this.only_matching
                                        || super::tree::matches(
                                            &review.detail.files[ix],
                                            &this.filter,
                                        )
                                })
                            };
                            old.detail.files[ix..]
                                .iter()
                                .find_map(visible)
                                .or_else(|| old.detail.files[..ix].iter().rev().find_map(visible))
                        });
                        if let Some(changed) = changed.as_ref() {
                            let review = this.review.as_mut().unwrap();
                            if let Some(previous) = previous.as_mut() {
                                review.reconcile_worktree(previous, changed);
                                if review.tree_rows.get_mut().is_none() {
                                    super::tree::TreeRows::preserve_scroll(
                                        previous,
                                        review,
                                        &this.filter,
                                        &this.tree_scroll,
                                    );
                                }
                            }
                            if let Some(status) = review.detail.status.as_deref() {
                                this.worktree_counts = super::worktree_counts(status);
                                if let Some(history) = this.history.as_mut() {
                                    history.changes.clone_from(status);
                                }
                            }
                        }
                        if let Some(previous) = previous {
                            for (_, images) in previous.images {
                                release_diff_images(images, window);
                            }
                        }
                        if blame_navigation
                            && let Some(path) = viewport.as_ref().map(|anchor| anchor.path())
                            && let Some(review) = this.review.as_mut()
                            && let Some(&ix) = review.by_path.get(path)
                        {
                            if this.only_matching
                                && !super::tree::matches(&review.detail.files[ix], &this.filter)
                            {
                                this.only_matching = false;
                            }
                            review.active = Some(ix);
                            Arc::make_mut(&mut review.collapsed).remove(path);
                        }
                        if locale != *rust_i18n::locale() {
                            this.refresh_placeholders(cx);
                        }
                        let revision = this.diff.read(cx).editor_row_revision();
                        this.sync_review_documents(keep, cx);
                        let projection_changed =
                            this.diff.read(cx).editor_row_revision() != revision;
                        if !keep {
                            this.reset_diff_vim(cx);
                            this.editor.restore_viewport();
                        }
                        if active.is_some() {
                            this.focus_loaded_vim_diff(window, cx);
                        }
                        this.load_review_metadata(needs_stats, window, cx);
                        if let Some(anchor) = viewport {
                            let path = anchor.path();
                            let ix = this.review.as_ref().and_then(|review| {
                                review.by_path.get(path).copied().filter(|&ix| {
                                    !this.only_matching
                                        || super::tree::matches(
                                            &review.detail.files[ix],
                                            &this.filter,
                                        )
                                })
                            });
                            if let Some(ix) = ix {
                                let review = this.review.as_ref().unwrap();
                                if changed.is_none()
                                    || projection_changed
                                    || !is_all_worktree_changes(&review.detail.comparison)
                                {
                                    this.diff.read(cx).restore_viewport_anchor(&anchor);
                                    this.editor.restore_viewport();
                                }
                                if this.snapshot.is_none()
                                    && matches!(review.patches[ix], PatchState::Pending)
                                    && !review.collapsed.contains(&review.detail.files[ix].path)
                                {
                                    if (restoring || changed.is_some() || blame_navigation)
                                        && !review.defer_image_patch(ix, this.stats_loading)
                                    {
                                        this.pending_review_anchor = Some(anchor);
                                    }
                                    this.request_visible_patch(ix, window, cx);
                                }
                            } else if this.snapshot.is_none()
                                && let Some(active) = anchor_neighbor.or(active).or_else(|| {
                                    this.review.as_ref().and_then(|review| {
                                        review.detail.files.iter().position(|file| {
                                            !this.only_matching
                                                || super::tree::matches(file, &this.filter)
                                        })
                                    })
                                })
                            {
                                this.reveal_file(active, false, window, cx);
                            }
                        }
                        this.prefetch_patches(window, cx);
                        if let Some(source) = source
                            && let Some(&ix) = this
                                .review
                                .as_ref()
                                .and_then(|review| review.by_path.get(&source.path))
                        {
                            this.open_layer_snapshot(
                                ix,
                                None,
                                Side::New,
                                source.line.map_or(1, |(_, line)| line),
                                window,
                                cx,
                            );
                        }
                    }
                    Ok(None) => {
                        this.load_review(Selection::Worktree(Mode::All), false, window, cx);
                    }
                    Err(error) => {
                        this.detail_error = Some(error.to_string());
                        if let Some(request) = this.pending_editor_open.take() {
                            this.open_review_file_in_editor(
                                &request.path,
                                request.from,
                                window,
                                cx,
                            );
                        }
                    }
                }
                if let Some(path) = this.hunk_load_path() {
                    if let Some(&ix) = this
                        .review
                        .as_ref()
                        .and_then(|review| review.by_path.get(path))
                    {
                        this.resume_hunk_navigation(ix, window, cx);
                    } else {
                        this.pending_hunk_navigation = None;
                    }
                }
                this.apply_repo_changes(window, cx);
                cx.notify();
            });
        });
        cx.notify();
    }

    fn load_review_metadata(
        &mut self,
        needs_stats: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = &self.review else { return };
        if review.detail.files.is_empty() {
            self.pending_editor_open = None;
            return;
        }
        let detail = review.detail.clone();
        let backend = self.backend.clone();
        let files = detail.clone();
        let openable = cx.background_spawn(async move { backend.openable(&files.files) });
        self.metadata_tasks[0] = cx.spawn_in(window, async move |this, cx| {
            let openable = openable.await;
            _ = this.update_in(cx, |this, window, cx| {
                if let Ok(openable) = openable
                    && let Some(review) = this
                        .review
                        .as_mut()
                        .filter(|review| review.openable != openable)
                {
                    review.openable = openable;
                    this.editor.invalidate();
                    this.preload_working_buffers(window, cx);
                    App::notify(cx, this.review_pane.entity_id());
                }
                if let Some(request) = this.pending_editor_open.take() {
                    this.open_review_file_in_editor(&request.path, request.from, window, cx);
                }
            });
        });
        if !needs_stats {
            return;
        }
        let backend = self.backend.clone();
        let stats = cx.background_spawn(async move { backend.review_stats(&detail) });
        self.stats_loading = true;
        #[cfg(test)]
        let delay = self.review_read_delay;
        self.metadata_tasks[1] = cx.spawn_in(window, async move |this, cx| {
            let result = stats.await;
            #[cfg(test)]
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            _ = this.update_in(cx, |this, window, cx| {
                this.stats_loading = false;
                if let Ok(stats) = result
                    && let Some(review) = this.review.as_mut()
                {
                    review.apply_stats(stats);
                    App::notify(cx, this.review_pane.entity_id());
                    App::notify(cx, this.tree_pane.entity_id());
                }
                if this.review.as_ref().is_some_and(|review| {
                    review.detail.comparison.commit_target().is_some()
                        && review.image_files.contains(&true)
                }) {
                    // Retry visible text patches and recover if numstat failed.
                    this.prefetch_patches(window, cx);
                }
                this.apply_repo_changes(window, cx);
            });
        });
    }

    pub(super) fn sync_documents(&mut self, cx: &mut Context<Self>) {
        self.pending_file_reveal = None;
        self.pending_hunk_navigation = None;
        self.sync_review_documents(false, cx);
    }

    fn sync_review_documents(&mut self, keep: bool, cx: &mut Context<Self>) {
        let Some(review) = &mut self.review else {
            return;
        };
        let matches_filter = |file: &git::ChangedFile| {
            !self.only_matching || super::tree::matches(file, &self.filter)
        };
        if self.single_file {
            review.active = review
                .active
                .filter(|&ix| matches_filter(&review.detail.files[ix]))
                .or_else(|| review.detail.files.iter().position(matches_filter));
        }
        let visible = review
            .documents
            .iter()
            .enumerate()
            .filter(|(ix, _)| {
                if self.single_file {
                    review.active == Some(*ix)
                } else {
                    matches_filter(&review.detail.files[*ix])
                }
            })
            .map(|(_, document)| document.clone())
            .map(|document| scope_diff_document(document, &self.workspace, cx))
            .collect::<Vec<_>>();
        let collapsed = review.collapsed.clone();
        self.diff.update(cx, |diff, cx| {
            if keep {
                diff.reconcile_files(visible, cx);
            } else {
                diff.set_files(visible, cx);
            }
            diff.restore_collapsed_files(collapsed.iter().cloned(), cx);
        });
    }

    pub(in crate::workspace) fn refresh_changed_paths(
        &mut self,
        paths: impl IntoIterator<Item = String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.refresh_commit_state(window, cx);
        if self
            .selection
            .as_ref()
            .is_some_and(Selection::includes_worktree)
        {
            self.refresh_review_paths(paths.into_iter().collect(), window, cx);
        } else {
            self.refresh(window, cx);
        }
    }

    pub(super) fn refresh_review_paths(
        &mut self,
        paths: HashSet<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(mut selection) = self.selection.clone().filter(Selection::includes_worktree) {
            if matches!(selection, Selection::Worktree(_)) && self.foreign_branch() {
                selection = Selection::Worktree(Mode::All);
            }
            self.load_review_with_changes(
                selection,
                true,
                None,
                false,
                Some(paths),
                false,
                window,
                cx,
            );
        }
    }

    pub(super) fn reveal_file(
        &mut self,
        ix: usize,
        expand: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = &mut self.review else {
            return;
        };
        let file = &review.detail.files[ix];
        let path = file.path.clone();
        let clear_filter = self.only_matching && !super::tree::matches(file, &self.filter);
        let switch_file = self.single_file && review.active != Some(ix);
        review.active = Some(ix);
        if expand {
            self.pending_editor_open = None;
            Arc::make_mut(&mut review.collapsed).remove(&path);
        }
        self.snapshot = None;
        self.snapshot_task = Task::ready(());
        self.pending_file_reveal = None;
        self.pending_hunk_navigation = None;
        self.pending_position = None;
        self.pending_review_anchor = None;
        if clear_filter {
            self.only_matching = false;
        }
        if clear_filter || switch_file {
            self.sync_documents(cx);
        }
        self.diff.update(cx, |diff, cx| {
            if expand {
                diff.set_file_collapsed(&path, false, cx);
            }
            diff.scroll_to_file(&path, cx);
        });
        if expand {
            self.request_visible_patch(ix, window, cx);
        }
        App::notify(cx, self.tree_pane.entity_id());
        cx.notify();
    }

    pub(super) fn reveal_tree_file(
        &mut self,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = &self.review else {
            return;
        };
        let Some(&ix) = review.by_path.get(path) else {
            return;
        };
        let scroll = if matches!(
            review.patches[ix],
            PatchState::Pending | PatchState::Loading
        ) && review.patch_display[ix].truncated.is_none()
        {
            FileRevealScroll::Immediate
        } else {
            FileRevealScroll::Animate
        };
        self.reveal_file(ix, true, window, cx);
        self.pending_file_reveal = Some(FileReveal {
            path: path.to_owned(),
            scroll,
        });
    }

    pub(super) fn adjacent_hunk_file(
        &self,
        from: &str,
        direction: HunkDirection,
    ) -> Option<(usize, HunkTarget)> {
        let review = self.review.as_ref().filter(|_| !self.single_file)?;
        let mut index = *review.by_path.get(from)?;
        while let Some(next) = direction.neighbor(index) {
            let file = review.detail.files.get(next)?;
            index = next;
            if self.only_matching && !super::tree::matches(file, &self.filter) {
                continue;
            }
            if review.documents[index].hunk_locations().next().is_some() {
                return Some((index, HunkTarget::FileEdge(direction)));
            }
            if matches!(
                review.patches[index],
                PatchState::Pending | PatchState::Loading
            ) && !change_stat(&review.stats, file).binary
            {
                return Some((index, HunkTarget::Load(direction)));
            }
        }
        None
    }

    pub(super) fn navigate_hunk_in_file(
        &mut self,
        path: SharedString,
        index: usize,
        anchor: HunkNavigationAnchor,
        cx: &mut Context<Self>,
    ) {
        self.reveal_hunk(
            HunkNavigation {
                path,
                target: HunkTarget::Index(index),
                anchor,
            },
            cx,
        );
    }

    pub(super) fn navigate_hunk_across_files(
        &mut self,
        from: &str,
        direction: HunkDirection,
        anchor: HunkNavigationAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((index, target)) = self.adjacent_hunk_file(from, direction) else {
            return;
        };
        let path = self.review.as_ref().unwrap().detail.files[index]
            .path
            .clone()
            .into();
        let jump = HunkNavigation {
            path,
            target,
            anchor,
        };
        if matches!(jump.target, HunkTarget::Load(_)) {
            self.pending_file_reveal = None;
            self.pending_hunk_navigation = Some(jump);
            self.request_patch(index, window, cx);
        } else {
            self.reveal_hunk(jump, cx);
        }
    }

    fn reveal_hunk(&mut self, jump: HunkNavigation, cx: &mut Context<Self>) {
        if let Some(review) = &mut self.review
            && Arc::make_mut(&mut review.collapsed).remove(jump.path.as_str())
        {
            self.diff.update(cx, |diff, cx| {
                diff.set_file_collapsed(&jump.path, false, cx)
            });
        }
        self.confirm_restore = None;
        self.pending_file_reveal = None;
        self.pending_hunk_navigation = Some(jump);
        App::notify(cx, self.review_pane.entity_id());
        App::notify(cx, self.tree_pane.entity_id());
    }

    fn hunk_load_path(&self) -> Option<&str> {
        let jump = self.pending_hunk_navigation.as_ref()?;
        matches!(jump.target, HunkTarget::Load(_)).then_some(jump.path.as_str())
    }

    fn resume_hunk_navigation(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let review = self.review.as_ref().unwrap();
        if self.hunk_load_path() != Some(review.detail.files[ix].path.as_str()) {
            return;
        }
        match review.patches[ix] {
            PatchState::Pending => {
                self.request_patch(ix, window, cx);
                return;
            }
            PatchState::Loading => return,
            PatchState::Ready | PatchState::Failed => {}
        }
        let has_hunks = review.documents[ix].hunk_locations().next().is_some();
        let Some(jump) = self.pending_hunk_navigation.take() else {
            return;
        };
        let HunkTarget::Load(direction) = jump.target else {
            return;
        };
        if has_hunks {
            self.reveal_hunk(
                HunkNavigation {
                    target: HunkTarget::FileEdge(direction),
                    ..jump
                },
                cx,
            );
        } else {
            self.navigate_hunk_across_files(jump.path.as_str(), direction, jump.anchor, window, cx);
        }
    }

    pub(super) fn openable_review_file(&self, path: &str) -> Option<usize> {
        let review = self.review.as_ref()?;
        let ix = *review.by_path.get(path)?;
        review.openable[ix].then_some(ix)
    }

    pub(super) fn open_diff_file_in_editor(
        &mut self,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_review_file_in_editor(path, None, window, cx);
    }

    pub(super) fn open_review_file_in_editor(
        &mut self,
        path: &str,
        from: Option<SourceLocation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_editor_open = None;
        if let Some(ix) = self.openable_review_file(path) {
            self.open_first_hunk_in_editor(ix, from, window, cx);
        }
    }

    fn open_first_hunk_in_editor(
        &mut self,
        ix: usize,
        from: Option<SourceLocation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = self.review.as_ref().filter(|review| review.openable[ix]) else {
            return;
        };
        if self.detail_loading
            || matches!(
                review.patches[ix],
                PatchState::Pending | PatchState::Loading
            )
        {
            self.pending_editor_open = Some(PendingEditorOpen {
                path: review.detail.files[ix].path.clone(),
                from,
            });
            self.request_patch(ix, window, cx);
            return;
        }
        let source = if matches!(review.patches[ix], PatchState::Ready) {
            let doc = &review.documents[ix];
            doc.hunk_locations()
                .next()
                .and_then(|_| {
                    let (side, line, _) = doc.patch_source_lines().next()?;
                    let position = DiffLinePosition::new(doc.path().clone(), side, line);
                    self.diff_source(&position, cx)
                })
                .map(SourcePosition::Diff)
        } else {
            None
        };
        self.open_worktree_file(ix, window, cx, move |workspace, path, window, cx| {
            if let Some(from) = from {
                workspace.open_source_jump(path, source, from, window, cx);
            } else {
                workspace.open_document_source_at(path, source, window, cx);
            }
        });
    }

    pub(super) fn open_diff_row_in_editor(
        &mut self,
        position: &DiffLinePosition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_editor_open = None;
        let Some(ix) = self.openable_review_file(position.path()) else {
            return;
        };
        let source = self.diff_source(position, cx).map(SourcePosition::Diff);
        self.open_worktree_file(ix, window, cx, move |workspace, path, window, cx| {
            workspace.open_document_source_at(path, source, window, cx);
        });
    }

    pub(super) fn open_diff_markdown(
        &mut self,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_editor_open = None;
        if let Some(ix) = self.openable_review_file(path) {
            self.open_worktree_file(ix, window, cx, Workspace::open_markdown_in_docs);
        }
    }

    pub(super) fn open_worktree_file(
        &self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
        open: impl FnOnce(&mut Workspace, PathBuf, &mut Window, &mut Context<Workspace>) + 'static,
    ) {
        let path = PathBuf::from(&self.review.as_ref().unwrap().detail.files[ix].path);
        let root = self.root.clone();
        let workspace = self.workspace.clone();
        // Switching views reads this panel to save its session.
        window.defer(cx, move |window, cx| {
            _ = workspace.update(cx, |workspace, cx| {
                if workspace.view == WorkspaceView::Git
                    && workspace
                        .repository
                        .as_ref()
                        .is_some_and(|repo| repo.root == root)
                {
                    open(workspace, path, window, cx);
                }
            });
        });
    }

    pub(super) fn prefetch_patches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preload_working_buffers(window, cx);
        self.viewport_queue.clear();
        self.image_queue.clear();
        let Some(review) = &mut self.review else {
            return;
        };
        if self.detail_loading || self.snapshot.is_some() {
            return;
        }
        let visible = {
            let mut visible = self
                .diff
                .read(cx)
                .visible_paths()
                .filter_map(|path| review.by_path.get(path.as_str()).copied());
            visible
                .next()
                .map(|first| (first, visible.last().unwrap_or(first)))
        };
        if let Some((first, last)) = visible {
            let forward = first >= self.prefetch_first.unwrap_or(first);
            if self.prefetch_first != Some(first) {
                self.prefetch_forward = forward;
                self.prefetch_first = Some(first);
            }
            let below = last + 1..(last + 5).min(review.patches.len());
            let above = (first.saturating_sub(4)..first).rev();
            let (leading, trailing): (Vec<_>, Vec<_>) = if self.prefetch_forward {
                (below.collect(), above.collect())
            } else {
                (above.collect(), below.collect())
            };
            for ix in (first..=last).chain(leading).chain(trailing) {
                let file = &review.detail.files[ix];
                if review.collapsed.contains(&file.path)
                    || (self.only_matching && !super::tree::matches(file, &self.filter))
                {
                    continue;
                }
                if matches!(review.patches[ix], PatchState::Pending)
                    && !review.defer_image_patch(ix, self.stats_loading)
                {
                    self.viewport_queue.push_back(ix);
                }
                if review.image_files[ix] {
                    if review.images.contains_key(&ix) {
                        if ix <= last && ix >= first {
                            review.image_order.retain(|&cached| cached != ix);
                            review.image_order.push_back(ix);
                        }
                    } else if !self
                        .image_task
                        .as_ref()
                        .is_some_and(|(loading, _)| *loading == ix)
                    {
                        self.image_queue.push_back(ix);
                    }
                }
            }
        }
        self.start_next_patch(window, cx);
        self.start_next_image(window, cx);
        self.start_patch_assets(window, cx);
    }

    fn start_next_image(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.image_task.is_some() || self.snapshot.is_some() {
            return;
        }
        let (Some(review), Some(ix)) = (&self.review, self.image_queue.pop_front()) else {
            return;
        };
        let detail = review.detail.clone();
        let source = detail.clone();
        let backend = self.backend.clone();
        let repository = self.repository.clone();
        let renderer = cx.svg_renderer();
        let scale = window.scale_factor();
        // Thumbnail I/O and decoding must not hold up publication of text patches.
        let load = cx
            .background_executor()
            .spawn_with_priority(Priority::Low, async move {
                diff_images(&backend, &repository, &source, ix, renderer, scale)
            });
        let task = cx.spawn_in(window, async move |this, cx| {
            let images = load.await;
            _ = this.update_in(cx, |this, window, cx| {
                let Some(review) = this
                    .review
                    .as_mut()
                    .filter(|review| Arc::ptr_eq(&review.detail, &detail))
                else {
                    return;
                };
                this.image_task = None;
                if review.image_order.len() >= IMAGE_DIFF_CACHE_SIZE
                    && let Some(old) = review.image_order.pop_front()
                    && let Some(images) = review.images.remove(&old)
                {
                    release_diff_images(images, window);
                }
                review.images.insert(ix, images);
                review.image_order.push_back(ix);
                if !review.image_redraw_pending
                    && this
                        .diff
                        .read(cx)
                        .visible_paths()
                        .any(|path| path.as_str() == detail.files[ix].path)
                {
                    review.image_redraw_pending = true;
                    let this = cx.weak_entity();
                    window.on_next_frame(move |_, cx| {
                        _ = this.update(cx, |this, cx| {
                            if let Some(review) = this
                                .review
                                .as_mut()
                                .filter(|review| Arc::ptr_eq(&review.detail, &detail))
                            {
                                review.image_redraw_pending = false;
                                App::notify(cx, this.review_pane.entity_id());
                            }
                        });
                    });
                }
                this.start_next_image(window, cx);
            });
        });
        self.image_task = Some((ix, task));
    }

    pub(super) fn request_visible_patch(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .review
            .as_ref()
            .is_some_and(|review| !review.defer_image_patch(ix, self.stats_loading))
        {
            self.request_patch(ix, window, cx);
        }
    }

    pub(super) fn request_patch(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = &self.review else { return };
        let editor_request = self
            .pending_editor_open
            .as_ref()
            .and_then(|request| review.by_path.get(&request.path))
            .copied();
        let hunk_request = self
            .hunk_load_path()
            .and_then(|path| review.by_path.get(path))
            .copied();
        if self.detail_loading
            || (editor_request != Some(ix)
                && hunk_request != Some(ix)
                && review.collapsed.contains(&review.detail.files[ix].path))
            || !matches!(review.patches[ix], PatchState::Pending)
        {
            return;
        }
        self.patch_requests.retain(|&request| {
            request != ix && (editor_request == Some(request) || hunk_request == Some(request))
        });
        self.patch_requests.push_front(ix);
        self.start_next_patch(window, cx);
    }

    fn start_next_patch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for explicit in [true, false] {
            self.start_patch_job(explicit, window, cx);
        }
    }

    fn start_patch_job(&mut self, explicit: bool, window: &mut Window, cx: &mut Context<Self>) {
        let lane = usize::from(explicit);
        if self.patch_loading[lane] || self.detail_loading {
            return;
        }
        let hunk_request = self.hunk_load_path().map(str::to_owned);
        let Some(review) = &mut self.review else {
            return;
        };
        let queue = if explicit {
            &mut self.patch_requests
        } else {
            &mut self.viewport_queue
        };
        let mut requests = Vec::new();
        while requests.len() < if explicit { 1 } else { 2 } {
            let Some(ix) = queue.pop_front() else { break };
            let path = &review.detail.files[ix].path;
            if matches!(review.patches[ix], PatchState::Pending)
                && (self
                    .pending_editor_open
                    .as_ref()
                    .is_some_and(|request| &request.path == path)
                    || hunk_request.as_ref() == Some(path)
                    || !review.collapsed.contains(path))
            {
                review.patches[ix] = PatchState::Loading;
                requests.push((ix, review.contexts.get(&ix).copied().unwrap_or(3)));
            }
        }
        if requests.is_empty() {
            return;
        }
        self.patch_loading[lane] = true;
        if !explicit {
            self.viewport_reads
                .extend(requests.iter().map(|&(ix, _)| ix));
        }
        let detail = review.detail.clone();
        let pending_detail = detail.clone();
        let backend = self.backend.clone();
        let whitespace = self.whitespace;
        #[cfg(test)]
        let (delay, executor) = (self.review_read_delay, cx.background_executor().clone());
        let (send, receive) = async_channel::bounded(requests.len());
        let worker = cx.background_spawn(async move {
            let mut prefetched = prefetch(&backend, &detail, &requests, whitespace);
            for (ix, context) in requests {
                if send.is_closed() {
                    break;
                }
                let result = review_patch(
                    &backend,
                    &detail,
                    &detail.files[ix],
                    context,
                    whitespace,
                    prefetched.remove(&ix),
                );
                #[cfg(test)]
                if !delay.is_zero() {
                    executor.timer(delay).await;
                }
                if send.try_send((ix, result)).is_err() {
                    break;
                }
            }
        });
        self.patch_tasks[lane] = cx.spawn_in(window, async move |this, cx| {
            while let Ok((ix, result)) = receive.recv().await {
                if this
                    .update_in(cx, |this, window, cx| {
                        if this
                            .review
                            .as_ref()
                            .is_some_and(|review| Arc::ptr_eq(&review.detail, &pending_detail))
                        {
                            this.publish_patch(ix, result, window, cx);
                            if !explicit {
                                this.viewport_reads.retain(|&pending| pending != ix);
                                this.apply_repo_changes(window, cx);
                            }
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
            worker.await;
            _ = this.update_in(cx, |this, window, cx| {
                this.patch_loading[lane] = false;
                if !explicit {
                    this.viewport_reads.clear();
                }
                if this.repo_changes.is_empty() {
                    this.start_next_patch(window, cx);
                } else {
                    this.start_patch_job(true, window, cx);
                    this.apply_repo_changes(window, cx);
                }
            });
        });
    }

    fn publish_patch(
        &mut self,
        ix: usize,
        result: Result<(git::Patch, PatchAssets)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = &mut self.review else {
            return;
        };
        let mut document = None;
        let mut position = None;
        let anchor = self
            .pending_review_anchor
            .take_if(|anchor| review.by_path.get(anchor.path()) == Some(&ix))
            .or_else(|| self.diff.read(cx).viewport_anchor());
        match result {
            Ok((patch, assets)) => {
                let same_content = assets.content.is_some()
                    && assets.content == review.patch_display[ix].content
                    && review.patch_display[ix].truncated == Some(patch.truncated);
                let file = &review.detail.files[ix];
                let counts = |review: &Review| {
                    review
                        .counts
                        .get(&ix)
                        .copied()
                        .or(change_stat(&review.stats, file).counts)
                        .unwrap_or_default()
                };
                let before = counts(review);
                if file.untracked() && review.stats.contains_key(&file.path) {
                    Rc::make_mut(&mut review.stats).remove(&file.path);
                }
                if let Some(counts) = patch.counts {
                    Rc::make_mut(&mut review.counts).insert(ix, counts);
                }
                let after = counts(review);
                review.totals.0 = review.totals.0 - before.0 + after.0;
                review.totals.1 = review.totals.1 - before.1 + after.1;
                // Index-only changes keep the rendered projection, including expanded context.
                if !same_content {
                    let patch = scope_diff_document(patch.document, &self.workspace, cx);
                    review.documents[ix] = patch.clone();
                    document = Some(patch);
                    review.sources.remove(&ix);
                    review.live.remove(&ix);
                }
                review.patches[ix] = PatchState::Ready;
                review.patch_display[ix] = PatchDisplay {
                    truncated: Some(patch.truncated),
                    content: assets.content,
                    error: None,
                };
                if !same_content || assets.operations.is_none() {
                    review.operations.remove(&ix);
                    self.editor.invalidate_actions();
                }
                review.versions.remove(&ix);
                if self
                    .confirm_restore
                    .as_ref()
                    .is_some_and(|(path, _)| review.by_path.get(path) == Some(&ix))
                {
                    self.confirm_restore = None;
                }
                if let Some(version) = assets.version.clone() {
                    review.versions.insert(ix, version);
                }
                review.pending_assets.insert(ix, assets);
                position = self
                    .pending_position
                    .take_if(|p| review.by_path.get(p.path().as_str()) == Some(&ix));
            }
            Err(error) => {
                review.patches[ix] = PatchState::Failed;
                let error = error.to_string();
                if review.patch_display[ix].error.as_deref() != Some(&error) {
                    review.patch_display[ix].error = Some(error);
                    self.editor.invalidate();
                }
            }
        }
        let restore = position.is_none() && document.is_some() && anchor.is_some();
        self.diff.update(cx, |diff, cx| {
            if let Some(document) = document {
                diff.replace_file(document, cx);
            }
            if let Some(position) = position {
                diff.scroll_to_line(position, cx);
            } else if restore && let Some(mut anchor) = anchor {
                diff.reconcile_viewport_anchor(&mut anchor, "");
                diff.restore_viewport_anchor(&anchor);
            }
        });
        if restore {
            self.editor.restore_viewport();
        }
        let draft = self.review.as_ref().and_then(|review| {
            if !has_live_target(&review.detail.comparison)
                || review.live.contains_key(&ix)
                || !matches!(review.patches[ix], PatchState::Ready)
            {
                return None;
            }
            let path = PathBuf::from(&review.detail.files[ix].path);
            let owner = self.workspace.upgrade()?;
            let file = owner.read(cx).files.get(&path)?;
            (file.committed != file.saved).then(|| (path, file.committed.clone()))
        });
        if let Some((path, draft)) = draft {
            self.buffer_changed(&path, draft, false, window, cx);
        }
        self.preload_working_buffers(window, cx);
        self.start_patch_assets(window, cx);
        App::notify(cx, self.review_pane.entity_id());
        App::notify(cx, self.tree_pane.entity_id());
        let file = &self.review.as_ref().unwrap().detail.files[ix];
        if let Some(request) = self
            .pending_editor_open
            .take_if(|request| request.path == file.path)
        {
            self.open_first_hunk_in_editor(ix, request.from, window, cx);
        }
        self.resume_hunk_navigation(ix, window, cx);
    }

    fn start_patch_assets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.assets_loading || self.detail_loading || self.snapshot.is_some() {
            return;
        }
        let Some(review) = &mut self.review else {
            return;
        };
        let visible = self
            .diff
            .read(cx)
            .visible_paths()
            .filter_map(|path| review.by_path.get(path.as_str()).copied());
        let Some((ix, assets)) = review.active.into_iter().chain(visible).find_map(|ix| {
            review
                .pending_assets
                .get(&ix)
                .cloned()
                .map(|assets| (ix, assets))
        }) else {
            return;
        };
        let detail = review.detail.clone();
        let pending_detail = detail.clone();
        let pending_version = assets.version.clone();
        let context = review.contexts.get(&ix).copied();
        let backend = self.backend.clone();
        let (send, receive) = async_channel::bounded(1);
        let worker = cx.background_spawn(async move {
            let file = &detail.files[ix];
            let operations = assets.operations(&backend, file);
            if send.try_send(operations).is_err() {
                return Vec::new();
            }
            assets.sources(&backend, &detail, file)
        });
        self.assets_loading = true;
        self.assets_task = cx.spawn_in(window, async move |this, cx| {
            let current = |review: &Review| {
                Arc::ptr_eq(&review.detail, &pending_detail)
                    && matches!(review.patches.get(ix), Some(PatchState::Ready))
                    && review.contexts.get(&ix).copied() == context
                    && review
                        .pending_assets
                        .get(&ix)
                        .is_some_and(|assets| assets.version == pending_version)
            };
            if let Ok(operations) = receive.recv().await {
                _ = this.update_in(cx, |this, _, cx| {
                    if let Some(review) = this.review.as_mut().filter(|review| current(review)) {
                        if let Some(operations) = operations {
                            review.operations.insert(ix, operations);
                        } else {
                            review.operations.remove(&ix);
                        }
                        this.editor.invalidate_actions();
                        App::notify(cx, this.review_pane.entity_id());
                    }
                });
            }
            let sources = worker.await;
            _ = this.update_in(cx, |this, window, cx| {
                this.assets_loading = false;
                let workspace = this.workspace.upgrade();
                let mut replacements = Vec::new();
                if let Some(review) = this.review.as_mut().filter(|review| current(review)) {
                    review.pending_assets.remove(&ix);
                    if !sources.is_empty() {
                        let editors = review.sources.entry(ix).or_default();
                        for (key, text, language) in sources {
                            let file = &review.detail.files[ix];
                            let worktree_path =
                                snapshot_worktree_path(&review.detail.comparison, file, key.1);
                            let document = &mut review.documents[ix];
                            if language == "bash" && !editors.contains_key(&key) {
                                *document = scope_diff_document(
                                    document.clone().with_side_language(key.1, language.into()),
                                    &this.workspace,
                                    cx,
                                );
                                replacements.push(document.clone());
                            }
                            let selected = workspace.as_ref().and_then(|workspace| {
                                workspace
                                    .read(cx)
                                    .path_highlight_override(snapshot_path(file, key.1))
                            });
                            editors.entry(key).or_insert_with(|| {
                                let editor = super::editor::snapshot_editor(
                                    text, language, selected, window, cx,
                                );
                                super::editor::SnapshotEditor::new(editor, worktree_path)
                            });
                        }
                        this.editor.sync_snapshots(review, cx);
                        if aligns_historical_snapshots(&review.detail.comparison) {
                            this.preload_working_buffers(window, cx);
                        }
                        App::notify(cx, this.review_pane.entity_id());
                    }
                }
                if !replacements.is_empty() {
                    this.diff
                        .update(cx, |diff, cx| diff.replace_files(replacements, cx));
                }
                this.start_patch_assets(window, cx);
            });
        });
    }

    fn reload_patch(
        &mut self,
        ix: usize,
        more_context: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = &mut self.review else {
            return;
        };
        if matches!(review.patches[ix], PatchState::Loading) {
            return;
        }
        if more_context {
            let context = review.contexts.entry(ix).or_insert(3);
            *context = (*context + 10).min(200);
        }
        self.confirm_restore = None;
        review.patches[ix] = PatchState::Pending;
        review.pending_assets.remove(&ix);
        let path = review.detail.files[ix].path.clone();
        Arc::make_mut(&mut review.collapsed).remove(&path);
        self.diff
            .update(cx, |diff, cx| diff.set_file_collapsed(&path, false, cx));
        self.request_patch(ix, window, cx);
    }

    pub(super) fn toggle_whitespace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.whitespace = !self.whitespace;
        if let Some(selection) = self.selection.clone() {
            self.load_review(selection, true, window, cx);
        }
    }

    pub(super) fn set_files_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        let Some(review) = &mut self.review else {
            return;
        };
        self.pending_hunk_navigation = None;
        let paths = Arc::make_mut(&mut review.collapsed);
        if collapsed {
            paths.extend(review.detail.files.iter().map(|file| file.path.clone()));
        } else {
            paths.retain(|path| !review.by_path.contains_key(path));
        }
        self.diff.update(cx, |diff, cx| {
            diff.set_all_files_collapsed(collapsed, cx);
            diff.invalidate_visible_files(cx);
        });
        self.confirm_restore = None;
        cx.notify();
    }

    pub(super) fn active_file(&self, cx: &App) -> Option<usize> {
        let review = self.review.as_ref()?;
        if let Some(snapshot) = &self.snapshot {
            return review
                .by_path
                .get(&snapshot.detail.files[snapshot.file].path)
                .copied();
        }
        review.active.or_else(|| {
            self.diff
                .read(cx)
                .top_file()
                .and_then(|path| review.by_path.get(path.as_str()).copied())
        })
    }

    pub(super) fn follow_review_viewport(&mut self, cx: &mut Context<Self>) {
        if self.single_file || self.snapshot.is_some() || self.pending_file_reveal.is_some() {
            return;
        }
        let Some(review) = &mut self.review else {
            return;
        };
        let active = self
            .diff
            .read(cx)
            .top_file()
            .and_then(|path| review.by_path.get(path.as_str()).copied());
        if review.active == active {
            return;
        }
        let loading_notice =
            |ix: usize| !review.image_files[ix] && review.patch_display[ix].truncated.is_none();
        let update_notice =
            review.active.is_some_and(loading_notice) || active.is_some_and(loading_notice);
        review.active = active;
        App::notify(cx, self.tree_pane.entity_id());
        if update_notice {
            App::notify(cx, self.review_pane.entity_id());
        }
    }

    pub(super) fn stage_files(
        &mut self,
        paths: Vec<String>,
        staged: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.writes_busy(cx) || !self.current_worktree() {
            return;
        }
        let Some(detail) = self.review.as_ref().map(|review| review.detail.clone()) else {
            return;
        };
        if detail.status.is_none() {
            return;
        }
        let action = if staged {
            git::FileAction::Stage
        } else {
            git::FileAction::Unstage
        };
        if paths.is_empty() {
            return;
        }
        let affected_paths = paths
            .iter()
            .flat_map(|path| {
                let old = detail.status.as_ref().and_then(|status| {
                    let file = status.get(path)?;
                    file.old_path.as_ref()
                });
                std::iter::once(path).chain(old)
            })
            .cloned()
            .collect::<Vec<_>>();
        let repo = match self.workspace.update(cx, |workspace, cx| {
            let repo = workspace
                .repository
                .clone()
                .filter(|repo| repo.root == self.root)
                .ok_or_else(|| anyhow::anyhow!(t!("git.worktree_changed")))?;
            workspace.begin_git_edit(affected_paths.iter().map(PathBuf::from), action, cx)?;
            Ok::<_, anyhow::Error>(repo)
        }) {
            Ok(Ok(repo)) => repo,
            Ok(Err(error)) => {
                super::super::toast::error(window, cx, error.to_string());
                self.stage_error = Some(error.to_string());
                cx.notify();
                return;
            }
            Err(_) => return,
        };
        self.staging = true;
        self.stage_error = None;
        self.focus.focus(window, cx);
        let backend = self.backend.clone();
        let workspace = self.workspace.clone();
        let task = cx.background_spawn(async move {
            backend.stage(detail.status.as_ref().unwrap(), &paths, staged)
        });
        self.stage_task = cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = cx.update(|window, cx| {
                _ = workspace.update(cx, |workspace, cx| {
                    if workspace
                        .repository
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &repo))
                    {
                        workspace.finish_stage_files(&affected_paths, result.is_ok(), window, cx);
                    }
                });
            });
            _ = this.update_in(cx, move |this, window, cx| {
                this.staging = false;
                this.stage_error = result.err().map(|error| error.to_string());
                if let Some(error) = this.stage_error.as_deref() {
                    super::super::toast::error(window, cx, error.to_owned());
                }
                this.refresh_changed_paths(affected_paths, window, cx);
            });
        });
        cx.notify();
    }

    pub(super) fn document_layer(&self) -> Option<git::Layer> {
        let review = self.review.as_ref()?;
        if !self.backend.supports_git_edit() {
            return None;
        }
        match review.detail.comparison {
            Comparison::Worktree {
                mode: Mode::Staged,
                base: None,
            } => Some(git::Layer::Staged),
            Comparison::Worktree {
                mode: Mode::Unstaged,
                base: None,
            } => Some(git::Layer::Unstaged),
            _ => None,
        }
    }

    pub(super) fn action_blocks(
        &self,
        path: &str,
        original: Option<usize>,
        modified: Option<usize>,
    ) -> Option<ActionBlocks> {
        let review = self.review.as_ref()?;
        let &ix = review.by_path.get(path)?;
        if review.live.contains_key(&ix) {
            return None;
        }
        let operations = review.operations.get(&ix)?;
        let staged = operations.patch(git::Layer::Staged);
        let unstaged = operations.patch(git::Layer::Unstaged);
        let direct = |patch: &HunkActions, original: Option<usize>| {
            modified
                .and_then(|line| block_at(&patch.blocks, line, false))
                .or_else(|| original.and_then(|line| block_at(&patch.blocks, line, true)))
        };
        let actions = if is_all_worktree_changes(&review.detail.comparison) {
            let staged_blocks = staged.map_or(&[][..], |patch| patch.blocks.as_slice());
            ActionBlocks {
                staged: original
                    .and_then(|line| block_at(staged_blocks, line, true))
                    .or_else(|| {
                        let line = modified?.checked_sub(1)?;
                        let ranges = &operations.staged_worktree;
                        let ix = ranges.partition_point(|range| range.end <= line);
                        ranges.get(ix)?.contains(&line).then_some(ix)
                    }),
                unstaged: unstaged.and_then(|patch| {
                    direct(
                        patch,
                        original.and_then(|line| index_line(staged_blocks, line)),
                    )
                }),
            }
        } else {
            ActionBlocks {
                staged: staged.and_then(|patch| direct(patch, original)),
                unstaged: unstaged.and_then(|patch| direct(patch, original)),
            }
        };
        (actions.staged.is_some() || actions.unstaged.is_some()).then_some(actions)
    }

    pub(super) fn hunk_actions(
        &self,
        path: &str,
        blocks: ActionBlocks,
        font_size: Pixels,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let review = self.review.as_ref()?;
        let &ix = review.by_path.get(path)?;
        let operations = review.operations.get(&ix)?;
        let status = operations.version.status.as_ref()?;
        let stale = !matches!(review.patches[ix], PatchState::Ready)
            || review.versions.get(&ix) != Some(&operations.version);
        let mut targets = Vec::new();
        let mut whole_file = false;
        for (layer, index) in [
            (git::Layer::Staged, blocks.staged),
            (git::Layer::Unstaged, blocks.unstaged),
        ] {
            let Some((patch, index)) = operations.patch(layer).zip(index) else {
                continue;
            };
            let block = patch.blocks.get(index)?;
            if !patch.whole_file && block.target.is_none() {
                continue;
            }
            whole_file |= layer == git::Layer::Unstaged && patch.whole_file;
            let target = if patch.whole_file {
                None
            } else {
                block.target.clone()
            };
            targets.extend(
                layer
                    .actions()
                    .iter()
                    .copied()
                    .filter(|action| {
                        action.available(status)
                            && (*action != git::FileAction::Restore
                                || self.backend.supports_git_operations())
                    })
                    .map(|action| (action, target.clone())),
            );
        }
        if targets.is_empty() {
            return self.whitespace.then(|| {
                div()
                    .text_size(GIT_TEXT_SIZE)
                    .child(t!("git.diff_whitespace_actions"))
                    .into_any_element()
            });
        }
        let actions = targets
            .iter()
            .map(|(action, _)| *action)
            .collect::<Vec<_>>();
        let key = (path.to_owned(), blocks.unstaged.or(blocks.staged).unwrap());
        let file_path = review.detail.files[ix].path.clone();
        let confirm = whole_file && self.confirm_restore.as_ref() == Some(&key);
        let version = operations.version.clone();
        let owner = cx.weak_entity();
        let cancel_owner = owner.clone();
        let available = |action| {
            self.workspace
                .read_with(cx, |workspace, _| {
                    (
                        workspace
                            .check_git_file(
                                std::path::Path::new(&review.detail.files[ix].path),
                                action,
                            )
                            .is_ok(),
                        stale || self.staging || !workspace.git_edit.is_empty(),
                    )
                })
                .unwrap_or((false, false))
        };
        Some(hunk_toolbar(
            &actions,
            confirm,
            whole_file,
            font_size,
            available,
            move |action, window, cx| {
                _ = owner.update(cx, |panel, cx| {
                    if whole_file
                        && action == git::FileAction::Restore
                        && panel.confirm_restore.as_ref() != Some(&key)
                    {
                        panel.confirm_restore = Some(key.clone());
                        cx.notify();
                        return;
                    }
                    panel.edit_worktree_file(
                        git::FileEdit {
                            path: file_path.clone(),
                            version: version.clone(),
                            action,
                            block: targets
                                .iter()
                                .find(|(candidate, _)| *candidate == action)
                                .and_then(|(_, target)| target.clone()),
                        },
                        window,
                        cx,
                    );
                });
            },
            move |_, cx| {
                _ = cancel_owner.update(cx, |panel, cx| {
                    panel.confirm_restore = None;
                    cx.notify();
                });
            },
            cx,
        ))
    }

    pub(super) fn edit_worktree_file(
        &mut self,
        edit: git::FileEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit_worktree_files(vec![edit], window, cx);
    }

    pub(super) fn edit_worktree_files(
        &mut self,
        edits: Vec<git::FileEdit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(action) = edits.first().map(|edit| edit.action) else {
            return;
        };
        if self.writes_busy(cx)
            || !self.current_worktree()
            || self
                .review
                .as_ref()
                .is_none_or(|review| review.detail.status.is_none())
        {
            return;
        }
        let paths = edits
            .iter()
            .map(|edit| edit.path.clone())
            .collect::<Vec<_>>();
        let repo = self.workspace.update(cx, |workspace, cx| {
            let repo = workspace
                .repository
                .clone()
                .filter(|repo| repo.root == self.root)
                .ok_or_else(|| anyhow::anyhow!(t!("git.worktree_changed")))?;
            if action == git::FileAction::Restore {
                workspace.flush_drafts(cx)?;
            }
            workspace.begin_git_edit(paths.iter().map(PathBuf::from), action, cx)?;
            Ok::<_, anyhow::Error>(repo)
        });
        let repo = match repo {
            Ok(Ok(repo)) => repo,
            Ok(Err(error)) => {
                super::super::toast::error(window, cx, error.to_string());
                self.stage_error = Some(error.to_string());
                cx.notify();
                return;
            }
            Err(_) => return,
        };
        self.confirm_restore = None;
        self.staging = true;
        self.stage_error = None;
        self.focus.focus(window, cx);
        let origin = repo.clone();
        let work_paths = paths.clone();
        let task = cx.background_spawn(async move {
            let mut completed = Vec::new();
            let total = edits.len();
            let discarding_untracked = action == git::FileAction::Restore
                && edits.iter().all(|edit| {
                    edit.version
                        .status
                        .as_ref()
                        .is_some_and(|status| status.untracked)
                });
            let result = (|| {
                if total > 1 {
                    let versions = repo.git_backend().file_versions(&work_paths);
                    for (edit, version) in edits.iter().zip(versions) {
                        anyhow::ensure!(
                            version.as_ref() == Some(&edit.version),
                            "{}: {}",
                            edit.path,
                            t!("git.worktree_changed")
                        );
                    }
                }
                for edit in edits {
                    let path = PathBuf::from(&edit.path);
                    let outcome = Workspace::run_git_edit(&repo, &path, edit).map_err(|error| {
                        if total > 1 {
                            anyhow::anyhow!("{}: {error}", path.display())
                        } else {
                            error
                        }
                    })?;
                    completed.push((path, outcome));
                }
                Ok::<_, anyhow::Error>(())
            })();
            let error = result.err().map(|error| {
                if total > 1 {
                    let key = if discarding_untracked {
                        "git.discard_batch_failed"
                    } else {
                        "git.restore_batch_failed"
                    };
                    t!(
                        key,
                        completed = completed.len(),
                        total = total,
                        error = error.to_string()
                    )
                    .into_owned()
                } else {
                    error.to_string()
                }
            });
            (completed, error)
        });
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let (completed, error) = task.await;
            let _ = cx.update(|window, cx| {
                _ = workspace.update(cx, |workspace, cx| {
                    if workspace
                        .repository
                        .as_ref()
                        .is_some_and(|repo| Arc::ptr_eq(repo, &origin))
                    {
                        let remaining = &paths[completed.len()..];
                        for (path, outcome) in completed {
                            workspace.finish_recorded_git_edit(&origin, &path, outcome, window, cx);
                        }
                        for path in remaining {
                            workspace.finish_git_edit(Path::new(path), None, false, window, cx);
                        }
                    }
                });
            });
            _ = this.update_in(cx, |panel, window, cx| {
                panel.staging = false;
                panel.stage_error = error;
                if let Some(error) = panel.stage_error.as_deref() {
                    super::super::toast::error(window, cx, error.to_owned());
                }
                panel.refresh_changed_paths(paths, window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn open_layer_snapshot(
        &mut self,
        ix: usize,
        layer: Option<git::Layer>,
        side: Side,
        line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = &self.review else { return };
        self.confirm_restore = None;
        self.viewport_queue.clear();
        self.image_queue.clear();
        self.pending_file_reveal = None;
        self.pending_hunk_navigation = None;
        self.pending_editor_open = None;
        self.snapshot = Some(Snapshot {
            detail: review.detail.clone(),
            file: ix,
            layer,
            side,
            editor: None,
            notice: None,
            loading: true,
            vim_subscriptions: None,
        });
        self.read_snapshot(side, line, window, cx);
    }

    fn read_snapshot(
        &mut self,
        side: Side,
        line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = &mut self.snapshot else {
            return;
        };
        snapshot.side = side;
        snapshot.loading = true;
        snapshot.editor = None;
        snapshot.vim_subscriptions = None;
        snapshot.notice = None;
        let detail = snapshot.detail.clone();
        let ix = snapshot.file;
        let layer = snapshot.layer;
        let backend = self.backend.clone();
        let task = cx.background_spawn(async move {
            let file = &detail.files[ix];
            if let Some(layer) = layer {
                backend.layer_snapshot(&file.path, layer, side)
            } else {
                backend.snapshot(&detail, file, side)
            }
        });
        self.snapshot_task = cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            _ = this.update_in(cx, move |this, window, cx| {
                let Some(snapshot) = &mut this.snapshot else {
                    return;
                };
                snapshot.loading = false;
                match result {
                    Ok(git::Snapshot::Text { text, truncated }) if !text.is_empty() => {
                        let file = &snapshot.detail.files[ix];
                        let path = snapshot_path(
                            file,
                            if side == Side::Old {
                                DiffSide::Original
                            } else {
                                DiffSide::Modified
                            },
                        );
                        let source = text
                            .strip_prefix('\u{feff}')
                            .unwrap_or(&text)
                            .replace("\r\n", "\n");
                        let line = line
                            .saturating_sub(1)
                            .min(source.bytes().filter(|&byte| byte == b'\n').count());
                        let source = Rope::from(source.as_str());
                        let language = this
                            .workspace
                            .upgrade()
                            .map(|workspace| {
                                let workspace = workspace.read(cx);
                                workspace
                                    .associated_highlight(path)
                                    .map(|(_, syntax)| syntax)
                                    .unwrap_or_else(|| workspace.source_language(path, &source))
                            })
                            .unwrap_or_else(|| {
                                super::super::language_for_source(path, &source).into()
                            });
                        let editor = cx.new(|cx| {
                            EditorState::new(window, cx)
                                .progressive_wrap()
                                .line_number(true)
                                .language(language)
                                .soft_wrap(this.wrap)
                                .default_file_text(source)
                        });
                        if truncated {
                            snapshot.notice = Some(t!("git.snapshot_truncated").into());
                        }
                        editor.update(cx, |editor, cx| {
                            editor.set_cursor_position(Position::new(line as u32, 0), window, cx)
                        });
                        snapshot.editor = Some(editor.clone());
                        let subscriptions = this.install_snapshot_vim(&editor, window, cx);
                        this.snapshot.as_mut().unwrap().vim_subscriptions = Some(subscriptions);
                    }
                    Ok(git::Snapshot::Text { .. }) => {
                        snapshot.notice = Some(t!("git.empty_snapshot").into())
                    }
                    Ok(git::Snapshot::Binary) => {
                        snapshot.notice = Some(t!("git.binary_snapshot").into())
                    }
                    Ok(git::Snapshot::Missing) => {
                        snapshot.notice = Some(t!("git.missing_snapshot").into())
                    }
                    Ok(git::Snapshot::Submodule) => {
                        snapshot.notice = Some(t!("git.submodule_snapshot").into())
                    }
                    Err(error) => snapshot.notice = Some(error.to_string().into()),
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    fn toggle_single_file(&mut self, cx: &mut Context<Self>) {
        let anchor = self.editor.change_anchor(self.diff.read(cx), cx);
        self.single_file = !self.single_file;
        self.pending_file_reveal = None;
        self.pending_hunk_navigation = None;
        self.sync_review_documents(true, cx);
        if let Some(anchor) = anchor {
            self.diff.read(cx).restore_viewport_anchor(&anchor);
            self.editor.restore_viewport();
        }
        cx.notify();
    }

    pub(in crate::workspace) fn toggle_wrap(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.wrap = !self.wrap;
        if let Some(editor) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.editor.as_ref())
        {
            editor.update(cx, |editor, cx| editor.set_soft_wrap(self.wrap, window, cx));
        }
        cx.notify();
    }

    pub(in crate::workspace) fn find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.search_editor(window, cx);
        self.editor.close_other_search(&state, cx);
        state.update(cx, |editor, cx| {
            editor.focus(window, cx);
            editor.open_search(false, cx);
        });
    }

    pub(super) fn search_editor(&self, window: &Window, cx: &App) -> Entity<EditorState> {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.editor.clone())
            .unwrap_or_else(|| self.editor.focused_column(window, cx).clone())
    }

    pub(super) fn jump_to_line(
        &mut self,
        position: DiffLinePosition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = &self.review else { return };
        let Some(&ix) = review.by_path.get(position.path().as_str()) else {
            return;
        };
        let loaded = review.patch_display[ix].truncated.is_some();
        self.reveal_file(ix, true, window, cx);
        if loaded {
            self.diff
                .update(cx, |diff, cx| diff.scroll_to_line(position, cx));
        } else {
            self.pending_position = Some(position);
            if self
                .review
                .as_ref()
                .is_some_and(|review| matches!(review.patches[ix], PatchState::Failed))
            {
                self.reload_patch(ix, false, window, cx);
            } else {
                self.request_patch(ix, window, cx);
            }
        }
    }

    pub(super) fn review_panel(
        &mut self,
        layout: PaneLayout,
        colors: Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let width = layout.center;
        let summary_height = if let Some(review) = &self.review
            && self.snapshot.is_none()
        {
            let layout = CommitSummaryLayout {
                width,
                rem: window.rem_size(),
                scale: window.scale_factor(),
                font: window.text_style().font(),
                locale: rust_i18n::locale().to_string(),
                only_matching: self.only_matching,
                has_visible_files: !self.diff.read(cx).files().is_empty(),
                whitespace: self.whitespace,
                show_details: self.show_details,
                message: self
                    .current_worktree()
                    .then(|| self.commit_message.read(cx).text().clone()),
            };
            if let Some((cached, height)) = &review.summary_layout
                && *cached == layout
            {
                *height
            } else {
                // Keep the projected preamble aligned with the wrapped summary and the first file.
                let height = self
                    .summary(review, cx, colors)
                    .into_any_element()
                    .layout_as_root(size(width.into(), AvailableSpace::MinContent), window, cx)
                    .height;
                self.review.as_mut().unwrap().summary_layout = Some((layout, height));
                height
            }
        } else {
            px(0.)
        };
        self.diff
            .update(cx, |diff, cx| diff.show_editor_preamble(cx));
        super::editor::ReviewEditor::prepare(self, summary_height, width, window, cx);
        let review = self.review.as_ref();
        v_flex()
            .id("git-review")
            .w_full()
            .h_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(self.review_toolbar(layout, cx, colors))
            .when_some(self.history_progress.as_ref(), |panel, progress| {
                panel.child(self.history_progress_banner(progress, colors, cx))
            })
            .when_some(self.detail_error.clone(), |panel, error| {
                panel.child(
                    h_flex()
                        .border_b_1()
                        .border_color(colors.border)
                        .px_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(message(error, colors.warning)),
                        )
                        .child(
                            action_button("git-retry-detail")
                                .secondary()
                                .small()
                                .label(t!("common.retry"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if let Some(selection) = this.selection.clone() {
                                        this.load_review(selection, true, window, cx);
                                    }
                                })),
                        ),
                )
            })
            .when_some(self.snapshot.as_ref(), |panel, snapshot| {
                panel
                    .when_some(
                        review.filter(|review| {
                            !matches!(review.detail.comparison, Comparison::Commit { .. })
                        }),
                        |panel, review| panel.child(self.summary(review, cx, colors)),
                    )
                    .child(self.snapshot_view(snapshot, cx, colors))
            })
            .when(self.snapshot.is_none(), |panel| {
                if let Some(review) = review {
                    if matches!(review.detail.comparison, Comparison::Commit { .. }) {
                        panel.child(self.diff_view(review, colors, window, cx))
                    } else if self.diff.read(cx).files().is_empty() {
                        let (label, icon) = if review.detail.files.is_empty() {
                            (t!("git.no_changes"), IconName::FileDiff)
                        } else {
                            (t!("git.no_filtered_files"), IconName::ListTree)
                        };
                        panel.child(
                            v_flex()
                                .id("git-empty-review")
                                .flex_1()
                                .min_h_0()
                                .overflow_y_scroll()
                                .child(self.summary(review, cx, colors))
                                .child(center_empty(label, icon, colors)),
                        )
                    } else {
                        panel.child(self.diff_view(review, colors, window, cx))
                    }
                } else {
                    panel.child(center_empty(
                        if self.detail_loading || self.history_loading {
                            t!("git.loading_changes")
                        } else {
                            t!("git.select_changes")
                        },
                        IconName::GitCommitHorizontal,
                        colors,
                    ))
                }
            })
    }

    fn review_toolbar(
        &self,
        layout: PaneLayout,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement {
        let compact = layout.center < px(650.);
        let PaneLayout {
            show_history,
            show_tree,
            ..
        } = layout;
        let review = self.review.as_ref();
        let has_changes = review.is_some_and(|review| !review.detail.files.is_empty());
        let can_scroll_to_top = self.snapshot.is_none()
            && review.is_some_and(|review| {
                matches!(review.detail.comparison, Comparison::Commit { .. })
                    || !self.diff.read(cx).files().is_empty()
            })
            && self.editor.is_scrolled_from_top();

        h_flex()
            .id("git-toolbar")
            .w_full()
            .debug_selector(|| "git-toolbar".into())
            .flex_shrink_0()
            .bg(colors.elevated)
            .rounded_tl(layout.history_corner)
            .h(super::GIT_TOOLBAR_HEIGHT)
            .px_2()
            .gap_1()
            .child(
                icon_control(
                    "git-toggle-history",
                    IconName::PanelLeft,
                    t!("git.history"),
                    show_history,
                )
                .debug_selector(|| "git-toggle-history".into())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_history_visible(!show_history, cx);
                })),
            )
            .when(!show_history, |row| {
                row.child(
                    icon_control(
                        "git-refresh-center",
                        IconName::RefreshCw,
                        t!("git.refresh"),
                        false,
                    )
                    .disabled(self.staging)
                    .on_click(cx.listener(|this, _, window, cx| this.refresh(window, cx))),
                )
            })
            .when_some(self.comparison_control(cx), |row, control| {
                row.child(div().min_w_0().child(control))
            })
            .child(div().flex_1().min_w_0())
            .when(has_changes, |row| {
                row.when(!compact, |row| {
                    row.child(
                        icon_control(
                            "git-diff-split",
                            IconName::Columns2,
                            t!("git.split_diff"),
                            self.split,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.split = !this.split;
                            cx.notify();
                        })),
                    )
                })
                .child(
                    icon_button(
                        "git-diff-back-to-top",
                        IconName::ArrowUp,
                        t!("git.back_to_top"),
                    )
                    .debug_selector(|| "git-diff-back-to-top".into())
                    .disabled(!can_scroll_to_top)
                    .on_click(cx.listener(|this, _, _, cx| {
                        super::editor::ReviewEditor::scroll_to_top(this, cx);
                    })),
                )
                .child(
                    icon_control(
                        "git-diff-single-file",
                        IconName::FileDiff,
                        t!("git.single_file_diff"),
                        self.single_file,
                    )
                    .debug_selector(|| "git-diff-single-file".into())
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_single_file(cx))),
                )
                .child(self.reading_menu(cx))
            })
            .when(review.is_some(), |row| {
                row.child(
                    icon_control(
                        "git-toggle-files",
                        IconName::ListTree,
                        t!("git.files_index"),
                        show_tree,
                    )
                    .debug_selector(|| "git-toggle-files".into())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_tree_visible(!show_tree, cx);
                    })),
                )
            })
    }

    fn reading_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let view = cx.weak_entity();
        let whitespace = self.whitespace;
        let split = self.split;
        let wrap = self.wrap;
        icon_button(
            "git-reading-options",
            IconName::Ellipsis,
            t!("git.review_options"),
        )
        .debug_selector(|| "git-reading-options".into())
        .dropdown_menu(move |menu, _, _| {
            let whitespace_view = view.clone();
            let split_view = view.clone();
            let wrap_view = view.clone();
            let search_view = view.clone();
            let previous = view.clone();
            let next = view.clone();
            menu.item(
                PopupMenuItem::new(t!("git.ignore_whitespace"))
                    .checked(whitespace)
                    .on_click(move |_, window, cx| {
                        _ = whitespace_view
                            .update(cx, |this, cx| this.toggle_whitespace(window, cx));
                    }),
            )
            .item(
                PopupMenuItem::new(t!("git.split_diff"))
                    .checked(split)
                    .on_click(move |_, _, cx| {
                        _ = split_view.update(cx, |this, cx| {
                            this.split = !this.split;
                            cx.notify();
                        });
                    }),
            )
            .item(
                PopupMenuItem::new(t!("git.wrap_diff"))
                    .checked(wrap)
                    .on_click(move |_, window, cx| {
                        _ = wrap_view.update(cx, |this, cx| this.toggle_wrap(window, cx));
                    }),
            )
            .separator()
            .item(
                PopupMenuItem::new(t!("git.search_diff"))
                    .icon(IconName::Search)
                    .action(Box::new(FindInDocument))
                    .on_click(move |_, window, _| {
                        // Let the menu restore focus so find can pick the active diff column.
                        let search_view = search_view.clone();
                        window.on_next_frame(move |window, cx| {
                            _ = search_view.update(cx, |this, cx| this.find(window, cx));
                        });
                    }),
            )
            .item(
                PopupMenuItem::new(t!("git.previous_hunk")).on_click(move |_, _, cx| {
                    _ = previous.update(cx, |this, cx| {
                        this.pending_file_reveal = None;
                        this.pending_hunk_navigation = None;
                        this.diff.update(cx, |diff, cx| diff.previous_change(cx))
                    });
                }),
            )
            .item(
                PopupMenuItem::new(t!("git.next_hunk")).on_click(move |_, _, cx| {
                    _ = next.update(cx, |this, cx| {
                        this.pending_file_reveal = None;
                        this.pending_hunk_navigation = None;
                        this.diff.update(cx, |diff, cx| diff.next_change(cx))
                    });
                }),
            )
        })
    }

    fn summary(&self, review: &Review, cx: &Context<Self>, colors: Palette) -> impl IntoElement {
        let counts = review.totals;
        let (title, subtitle) = match &review.detail.comparison {
            Comparison::Versions { .. } => {
                let title = match self.selection.as_ref() {
                    Some(Selection::CommitRange { count, .. }) => {
                        t!("git.commit_range_changes", count = count)
                    }
                    _ => t!("git.compare_versions"),
                };
                (title.into(), String::new())
            }
            Comparison::Commit { commit, .. } => (
                subject(commit),
                format!(
                    "{}  ·  {}",
                    commit.author,
                    display_date(&commit.committed_at)
                ),
            ),
            Comparison::Worktree { mode, base } => {
                let checkout = self
                    .history
                    .as_ref()
                    .and_then(|history| history.checkout.as_deref())
                    .map(branch_name)
                    .map_or_else(|| t!("git.detached_head"), Cow::Borrowed);
                let title = if base.is_some() {
                    t!("git.worktree_diff", branch = branch_name(&self.branch)).into_owned()
                } else {
                    t!("git.uncommitted").into()
                };
                let head = if self
                    .history
                    .as_ref()
                    .is_some_and(|history| history.head.is_some())
                {
                    Cow::Borrowed("HEAD")
                } else {
                    t!("git.empty_tree")
                };
                let baseline = match (mode, base) {
                    (_, Some(base)) => t!("git.to_worktree", base = git::short(base)).into_owned(),
                    (Mode::All, None) => t!("git.to_worktree", base = head).into_owned(),
                    (Mode::Staged, None) => t!("git.to_index", base = head).into_owned(),
                    (Mode::Unstaged, None) => t!("git.index_to_worktree").into(),
                };
                (title.into(), format!("{checkout}  ·  {baseline}"))
            }
        };
        let commit = match &review.detail.comparison {
            Comparison::Commit { commit, .. } => Some(commit),
            _ => None,
        };
        v_flex()
            .id("git-review-summary")
            .debug_selector(|| "git-review-summary".into())
            .w_full()
            .flex_shrink_0()
            .px_4()
            .py_3()
            .gap_2()
            .bg(colors.elevated)
            .when(
                self.snapshot.is_some() || self.diff.read(cx).files().is_empty(),
                |summary| summary.border_b_1().border_color(colors.border),
            )
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_semibold()
                            .text_size(px(16.))
                            .when(commit.is_none(), |title| title.truncate())
                            .child(title),
                    )
                    .when(commit.is_some(), |row| {
                        row.child(
                            icon_control(
                                "git-commit-details",
                                IconName::Info,
                                t!("git.commit_details"),
                                self.show_details,
                            )
                            .debug_selector(|| "git-commit-details".into())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_details = !this.show_details;
                                App::notify(cx, this.review_pane.entity_id());
                            })),
                        )
                    }),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .min_w_0()
                    .text_size(GIT_TEXT_SIZE)
                    .text_color(colors.muted)
                    .when(!subtitle.is_empty(), |row| {
                        row.child(div().min_w_0().truncate().child(subtitle))
                    })
                    .when_some(commit, |row, commit| {
                        let short = commit.short.clone();
                        row.child(
                            quiet_button("git-copy-sha")
                                .xsmall()
                                .label(commit.short.clone())
                                .tooltip(t!("git.copy_short_commit_id"))
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(short.clone()));
                                }),
                        )
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .child(file_count(review.detail.files.len())),
                    )
                    .child(diff_counts(counts, GIT_TEXT_SIZE, cx)),
            )
            .when(
                self.snapshot.is_none()
                    && matches!(
                        review.detail.comparison,
                        Comparison::Worktree { base: None, .. }
                    ),
                |panel| panel.child(self.commit_controls(cx)),
            )
            .when_some(commit, |panel, commit| {
                panel
                    .when(!commit.body.is_empty(), |panel| {
                        panel.child(commit_message(commit, colors, !self.show_details, cx))
                    })
                    .when(self.show_details, |panel| {
                        panel.child(self.commit_details(commit, cx, colors))
                    })
                    .when(review.detail.files.is_empty(), |panel| {
                        panel.child(
                            div()
                                .text_size(GIT_TEXT_SIZE)
                                .text_color(colors.muted)
                                .child(t!("git.no_changes")),
                        )
                    })
            })
            .when(self.only_matching, |panel| {
                panel.child(
                    div()
                        .text_size(GIT_TEXT_SIZE)
                        .text_color(colors.accent)
                        .child(
                            t!(
                                "git.shown_files",
                                shown = self.diff.read(cx).files().len(),
                                total = review.detail.files.len()
                            )
                            .into_owned(),
                        ),
                )
            })
            .when(self.whitespace, |panel| {
                panel.child(
                    div()
                        .text_size(GIT_TEXT_SIZE)
                        .text_color(colors.accent)
                        .child(t!("git.whitespace_ignored")),
                )
            })
    }

    fn commit_details(
        &self,
        commit: &Commit,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement {
        v_flex()
            .id("git-commit-details-body")
            .w_full()
            .min_h_0()
            .gap_1()
            .pt_2()
            .border_t_1()
            .border_color(colors.border)
            .text_size(GIT_TEXT_SIZE)
            .child({
                let oid = commit.oid.clone();
                quiet_button("git-copy-details-sha")
                    .xsmall()
                    .label(oid.clone())
                    .tooltip(t!("git.copy_commit_id"))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(oid.clone()))
                    })
            })
            .child(
                div().text_color(colors.muted).child(
                    t!(
                        "git.author",
                        name = commit.author,
                        email = commit.author_email,
                        date = display_date(&commit.authored_at)
                    )
                    .into_owned(),
                ),
            )
            .child(
                div().text_color(colors.muted).child(
                    t!(
                        "git.committer",
                        name = commit.committer,
                        email = commit.committer_email,
                        date = display_date(&commit.committed_at)
                    )
                    .into_owned(),
                ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .text_color(colors.muted)
                    .child(if commit.parents.is_empty() {
                        t!("git.root_commit")
                    } else {
                        t!("git.parent")
                    })
                    .children(
                        commit
                            .parents
                            .iter()
                            .cloned()
                            .enumerate()
                            .map(|(ix, parent)| {
                                quiet_button(("git-open-parent", ix))
                                    .xsmall()
                                    .label(git::short(&parent).to_owned())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.load_review(
                                            Selection::Commit(parent.clone(), None),
                                            false,
                                            window,
                                            cx,
                                        );
                                    }))
                            }),
                    ),
            )
    }

    pub(in crate::workspace) fn buffer_changed(
        &mut self,
        path: &std::path::Path,
        text: gpui_component::input::Rope,
        saved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let Some(name) = path.to_str() else { return };
        let Some(&ix) = review.by_path.get(name) else {
            return;
        };
        if self.detail_loading || !has_live_target(&review.detail.comparison) {
            return;
        }
        if saved {
            self.live_tasks.remove(path);
            self.repository_changed(
                git::watch::RepoChanges {
                    paths: [name.to_owned()].into(),
                    ..Default::default()
                },
                window,
                cx,
            );
            return;
        }
        let Some(version) = review.versions.get(&ix).cloned() else {
            return;
        };
        let base = review
            .live
            .get(&ix)
            .filter(|live| live.version == version)
            .map(|live| live.base.clone());
        let detail = review.detail.clone();
        let patch_detail = detail.clone();
        let patch_version = version.clone();
        let backend = self.backend.clone();
        let path = path.to_owned();
        let edit_path = path.clone();
        let name = name.to_owned();
        let context = review.contexts.get(&ix).copied().unwrap_or(3);
        let task = cx.spawn_in(window, async move |this, cx| {
            let executor = cx.background_executor().clone();
            executor.timer(Duration::from_millis(90)).await;
            let result = executor
                .spawn(async move {
                    let base = match base {
                        Some(base) => base,
                        None if is_all_worktree_changes(&patch_detail.comparison)
                            || is_custom_worktree(&patch_detail.comparison) =>
                        {
                            let file = &patch_detail.files[ix];
                            let base = match backend.snapshot(&patch_detail, file, Side::Old)? {
                                git::Snapshot::Text {
                                    text,
                                    truncated: false,
                                } => Some(Arc::<str>::from(
                                    text.trim_start_matches('\u{feff}').replace("\r\n", "\n"),
                                )),
                                git::Snapshot::Missing => None,
                                _ => anyhow::bail!(t!("git.cannot_edit")),
                            };
                            anyhow::ensure!(
                                backend.file_version(&name)? == version,
                                t!("git.worktree_changed")
                            );
                            base
                        }
                        None => backend.index_text(&name, &version)?.map(Arc::<str>::from),
                    };
                    let doc = git::worktree_buffer_diff(
                        &name,
                        base.as_deref(),
                        &text.to_string(),
                        context,
                    )?;
                    Ok::<_, anyhow::Error>((doc, LivePatch { base, version }))
                })
                .await;
            _ = this.update_in(cx, |this, window, cx| {
                this.live_tasks.remove(&edit_path);
                cx.notify();
                let Some(review) = this.review.as_mut().filter(|review| {
                    Arc::ptr_eq(&review.detail, &detail)
                        && review.versions.get(&ix) == Some(&patch_version)
                        && review.contexts.get(&ix).copied().unwrap_or(3) == context
                }) else {
                    return;
                };
                let (mut doc, live) = match result {
                    Ok(result) => result,
                    Err(error) => {
                        super::super::toast::error(window, cx, error.to_string());
                        this.stage_error = Some(error.to_string());
                        cx.notify();
                        return;
                    }
                };
                doc = scope_diff_document(doc, &this.workspace, cx);
                this.editor.invalidate_live_source(edit_path.clone());
                if let Some(base) = &live.base {
                    let sources = review.sources.entry(ix).or_default();
                    let key = (doc.path().clone(), DiffSide::Original);
                    if !sources.contains_key(&key) {
                        sources.insert(key, {
                            let source = Rope::from(base.as_ref());
                            let worktree_path = snapshot_worktree_path(
                                &detail.comparison,
                                &detail.files[ix],
                                DiffSide::Original,
                            );
                            let language = super::super::language_for_source(
                                worktree_path.as_deref().unwrap_or(&edit_path),
                                &source,
                            );
                            let selected = this.workspace.upgrade().and_then(|workspace| {
                                workspace.read(cx).path_highlight_override(snapshot_path(
                                    &detail.files[ix],
                                    DiffSide::Original,
                                ))
                            });
                            super::editor::SnapshotEditor::new(
                                super::editor::snapshot_editor(
                                    source, language, selected, window, cx,
                                ),
                                worktree_path,
                            )
                        });
                    }
                }
                review.live.insert(ix, live);
                this.editor.invalidate_actions();
                review.documents[ix] = doc.clone();
                review.patch_display[ix].content = None;
                this.diff.update(cx, |diff, cx| diff.replace_file(doc, cx));
                App::notify(cx, this.review_pane.entity_id());
            });
        });
        self.live_tasks.insert(path, task);
    }

    pub(super) fn has_image_diff(&self, path: &str) -> bool {
        let Some(review) = &self.review else {
            return false;
        };
        !review.collapsed.contains(path)
            && review
                .by_path
                .get(path)
                .is_some_and(|&ix| review.image_files[ix])
    }

    pub(super) fn image_diff_row(
        &self,
        path: &SharedString,
        width: Pixels,
        rem: Pixels,
        colors: Palette,
    ) -> Option<AnyElement> {
        let review = self.review.as_ref()?;
        let &ix = review.by_path.get(path.as_str())?;
        if !review.image_files[ix] || review.collapsed.contains(path.as_str()) {
            return None;
        }
        let images = review.images.get(&ix);
        let padding = rem;
        let gap = rem * 0.75;
        let arrow_size = px(16.);
        let width =
            ((width - 2. * (padding + gap) - arrow_size) / 2.).clamp(px(0.), THUMBNAIL_SIZE.width);
        let top = (IMAGE_DIFF_HEIGHT - THUMBNAIL_SIZE.height) / 2.;
        let arrow_x = padding + width + gap;
        let slots = [padding, arrow_x + arrow_size + gap]
            .map(|x| Bounds::new(point(x, top), size(width, THUMBNAIL_SIZE.height)));
        let mut thumbnails = [None, None];
        let [before, after] = [
            (0usize, "before", t!("git.before")),
            (1, "after", t!("git.after")),
        ]
        .map(|(index, name, label)| {
            let slot = slots[index];
            let preview = div()
                .id(("git-image-thumbnail", index))
                .debug_selector(|| format!("git-image-thumbnail:{name}:{path}"))
                .aria_label(label)
                .absolute()
                .top(slot.top())
                .left(slot.left())
                .w(slot.size.width)
                .h(slot.size.height);
            let status = match images.map(|sides| &sides[index]) {
                Some(DiffImage::Ready(thumbnail)) => {
                    let fitted = ObjectFit::Contain.get_bounds(slot, thumbnail.image.size(0));
                    thumbnails[index] = Some((thumbnail.image.clone(), fitted));
                    return hover_image_source(
                        preview,
                        Some(self.editor.image_hover.clone()),
                        thumbnail.hover.clone(),
                    );
                }
                Some(DiffImage::Missing) => t!("git.missing_snapshot"),
                Some(DiffImage::Unsupported) => t!("image.unsupported"),
                Some(DiffImage::Failed) => t!("image.open_failed"),
                Some(DiffImage::UnsupportedHelper) => t!("git.binary_diff"),
                None => t!("image.loading"),
            };
            preview
                .flex()
                .items_center()
                .justify_center()
                .child(div().px_2().text_xs().child(status))
        });
        Some(
            div()
                .id((ElementId::from("git-image-diff"), path.clone()))
                .debug_selector(|| format!("git-image-diff:{path}"))
                .relative()
                .h(IMAGE_DIFF_HEIGHT)
                .w_full()
                .text_color(colors.muted)
                .child(before)
                .child(after)
                .child(
                    div()
                        .absolute()
                        .left(arrow_x)
                        .top((IMAGE_DIFF_HEIGHT - arrow_size) / 2.)
                        .debug_selector(|| format!("git-image-arrow:{path}"))
                        .child(icon(
                            IconName::ArrowRight,
                            colors.muted,
                            arrow_size.as_f32(),
                        )),
                )
                .when(thumbnails.iter().any(Option::is_some), |row| {
                    row.child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, (), window, _| {
                                for (slot, thumbnail) in slots.into_iter().zip(thumbnails) {
                                    if let Some((image, fitted)) = thumbnail {
                                        let _ = window.paint_image(
                                            Bounds::new(bounds.origin + slot.origin, slot.size),
                                            Bounds::new(bounds.origin + fitted.origin, fitted.size),
                                            Corners::default(),
                                            image,
                                            0,
                                            false,
                                        );
                                    }
                                }
                            },
                        )
                        .absolute()
                        .inset_0(),
                    )
                })
                .into_any_element(),
        )
    }

    pub(super) fn review_notice(&self, path: &str, cx: &App) -> Option<SharedString> {
        let review = self.review.as_ref()?;
        let &ix = review.by_path.get(path)?;
        let file = &review.detail.files[ix];
        let document = &review.documents[ix];
        let display = &review.patch_display[ix];
        let label = if file.conflict() {
            t!("git.file_conflict")
        } else if file.submodule() {
            t!("git.submodule_changed")
        } else if change_stat(&review.stats, file).binary || document.is_binary() {
            if review.image_files[ix] {
                return None;
            }
            t!("git.binary_diff")
        } else {
            match display.truncated {
                None if display.error.is_some() => return display.error.clone().map(Into::into),
                None if !review.image_files[ix] && self.active_file(cx) == Some(ix) => {
                    t!("git.loading_changes")
                }
                None => return None,
                Some(true) => return None,
                Some(false)
                    if !file.old_mode.is_empty()
                        && !file.new_mode.is_empty()
                        && file.old_mode != file.new_mode =>
                {
                    t!("git.mode_changed", old = file.old_mode, new = file.new_mode)
                }
                Some(false) => t!("git.no_text_diff"),
            }
        };
        Some(label.into())
    }

    fn diff_view(
        &self,
        review: &Review,
        colors: Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cx.weak_entity();
        let detail = review.detail.clone();
        let stage_allowed = detail.status.is_some() && self.current_worktree();
        let stage_busy = self.writes_busy(cx);
        let by_path = review.by_path.clone();
        let selected = self.diff.read(cx).selected_lines();
        let (font_size, line_height, editor_font) = self
            .workspace
            .read_with(cx, |workspace, _| {
                let editor = &workspace.config.current().editor;
                (
                    px(editor.font_size as f32),
                    editor.line_height as f32,
                    workspace.editor_font.clone(),
                )
            })
            .unwrap_or_else(|_| {
                (
                    cx.theme().mono_font_size,
                    crate::config::Editor::default().line_height as f32,
                    None,
                )
            });
        let header = move |path: &str, text_progress: f32, cx: &mut App| {
            let Some(&ix) = by_path.get(path) else {
                return div().into_any_element();
            };
            let file = &detail.files[ix];
            let Some((display, counts, can_open)) = view
                .read_with(cx, |this, _| {
                    let review = this.review.as_ref()?;
                    if !Arc::ptr_eq(&review.detail, &detail) {
                        return None;
                    }
                    Some((
                        review.patch_display[ix].clone(),
                        review
                            .counts
                            .get(&ix)
                            .copied()
                            .or(change_stat(&review.stats, file).counts),
                        review.openable[ix],
                    ))
                })
                .ok()
                .flatten()
            else {
                return div().into_any_element();
            };
            let view_for_menu = view.clone();
            let failed = display.error.is_some();
            let path = file.path.clone();
            let title = file
                .old_path
                .as_ref()
                .filter(|old| *old != &path)
                .map_or_else(|| path.clone(), |old| format!("{old} → {path}"));
            let state = file.worktree.clone();
            let show_stage = stage_allowed && state.as_ref().is_some_and(|state| !state.conflict);
            let editor_view = view.clone();
            let editor_path = path.clone();
            let editor_selector = format!("git-open-file-in-editor:{path}");
            let menu_selector = format!("git-file-menu:{path}");
            let menu_detail = detail.clone();
            let line = selected
                .as_ref()
                .filter(|range| range.path().as_str() == path)
                .map(|range| (range.side(), range.start()));
            let notice = match display.truncated {
                Some(_) if failed => Some(t!("git.patch_read_failed")),
                Some(true) => Some(t!("git.truncated")),
                _ => None,
            };
            let file_name_size = file_name_font_size(cx);
            h_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .text_size(px(file_name_size))
                .child(status_badge(file, colors, file_name_size, cx))
                .child(super::editor::file_header_name(
                    title,
                    colors,
                    text_progress,
                    cx.theme().mode.is_dark(),
                ))
                .when_some(notice, |row, label| {
                    row.child(div().text_color(colors.warning).child(label))
                })
                .when_some(counts, |row, counts| {
                    row.child(diff_counts(counts, GIT_TEXT_SIZE, cx))
                })
                .child(
                    compact_icon_button(
                        ("git-open-file-in-editor", ix),
                        IconName::Pencil,
                        t!("git.open_in_editor"),
                    )
                    .debug_selector(move || editor_selector)
                    .tooltip(if can_open {
                        t!("git.open_in_editor")
                    } else {
                        t!("git.missing_worktree_file")
                    })
                    .disabled(!can_open)
                    .on_click(move |_, window, cx| {
                        _ = editor_view.update(cx, |panel, cx| {
                            panel.open_diff_file_in_editor(&editor_path, window, cx);
                        });
                    }),
                )
                .child(
                    compact_icon_button(
                        ("git-file-menu", ix),
                        IconName::Ellipsis,
                        t!("git.file_actions"),
                    )
                    .debug_selector(move || menu_selector)
                    .tooltip(
                        display
                            .error
                            .filter(|_| display.truncated.is_some())
                            .unwrap_or_else(|| t!("git.file_actions").into()),
                    )
                    .dropdown_menu(move |mut menu, _, cx| {
                        let path_copy = path.clone();
                        menu = menu.item(PopupMenuItem::new(t!("git.copy_path")).on_click(
                            move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(path_copy.clone()));
                            },
                        ));
                        for (side, label) in [
                            (Side::Old, t!("git.view_before")),
                            (Side::New, t!("git.view_after")),
                        ] {
                            let view = view_for_menu.clone();
                            let line = line
                                .filter(|(diff_side, _)| {
                                    (*diff_side == DiffSide::Original) == (side == Side::Old)
                                })
                                .map_or(1, |(_, line)| line);
                            menu = menu.item(PopupMenuItem::new(label).on_click(
                                move |_, window, cx| {
                                    _ = view.update(cx, |this, cx| {
                                        this.open_layer_snapshot(
                                            ix,
                                            this.document_layer(),
                                            side,
                                            line,
                                            window,
                                            cx,
                                        )
                                    });
                                },
                            ));
                        }
                        let view = view_for_menu.clone();
                        menu = menu.item(PopupMenuItem::new(t!("git.more_context")).on_click(
                            move |_, window, cx| {
                                _ = view
                                    .update(cx, |this, cx| this.reload_patch(ix, true, window, cx));
                            },
                        ));
                        if failed {
                            let view = view_for_menu.clone();
                            menu = menu.item(PopupMenuItem::new(t!("git.retry_patch")).on_click(
                                move |_, window, cx| {
                                    _ = view.update(cx, |this, cx| {
                                        this.reload_patch(ix, false, window, cx)
                                    });
                                },
                            ));
                        }
                        if show_stage && let Some(state) = &state {
                            menu = menu.separator();
                            for (staged, label, allowed) in [
                                (
                                    true,
                                    t!("git.stage_file"),
                                    state.untracked || state.unstaged,
                                ),
                                (false, t!("git.unstage"), state.staged),
                            ] {
                                let view = view_for_menu.clone();
                                let path = path.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(label)
                                        .icon(if staged {
                                            IconName::Plus
                                        } else {
                                            IconName::Minus
                                        })
                                        .disabled(!allowed || stage_busy)
                                        .on_click(move |_, window, cx| {
                                            _ = view.update(cx, |this, cx| {
                                                this.stage_files(
                                                    vec![path.clone()],
                                                    staged,
                                                    window,
                                                    cx,
                                                )
                                            });
                                        }),
                                );
                            }
                            for item in super::tree::worktree_restore_items(
                                &view_for_menu,
                                &menu_detail,
                                &[ix],
                                &path,
                                false,
                                cx,
                            ) {
                                menu = menu.item(item);
                            }
                        }
                        menu
                    }),
                )
                .into_any_element()
        };
        self.editor.render(
            self,
            font_size,
            line_height,
            editor_font,
            colors,
            stage_allowed && self.backend.supports_git_edit(),
            header,
            move |panel, cx| {
                panel
                    .review
                    .as_ref()
                    .map(|review| panel.summary(review, cx, colors).into_any_element())
            },
            window,
            cx,
        )
    }

    fn snapshot_view(
        &self,
        snapshot: &Snapshot,
        cx: &Context<Self>,
        colors: Palette,
    ) -> impl IntoElement {
        let file = &snapshot.detail.files[snapshot.file];
        let path = if snapshot.side == Side::Old {
            file.old_path.as_ref().unwrap_or(&file.path)
        } else {
            &file.path
        };
        v_flex()
            .id("git-snapshot")
            .debug_selector(|| "git-snapshot".into())
            .flex_1()
            .min_h_0()
            .w_full()
            .child(
                h_flex()
                    .w_full()
                    .h(px(42.))
                    .flex_shrink_0()
                    .gap_2()
                    .px_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        quiet_button("git-back-to-diff")
                            .small()
                            .icon(IconName::ArrowLeft)
                            .label(t!("git.back_to_changes"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.snapshot = None;
                                this.snapshot_task = Task::ready(());
                                this.prefetch_patches(window, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .font_semibold()
                            .child(path.clone()),
                    )
                    .child(
                        h_flex()
                            .gap(px(2.))
                            .p(px(2.))
                            .rounded(px(7.))
                            .bg(colors.soft)
                            .children(
                                [(Side::Old, t!("git.before")), (Side::New, t!("git.after"))].map(
                                    |(side, label)| {
                                        quiet_button(match side {
                                            Side::Old => "snapshot-before",
                                            Side::New => "snapshot-after",
                                        })
                                        .small()
                                        .label(label)
                                        .selected(snapshot.side == side)
                                        .toggled(snapshot.side == side)
                                        .on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                if this
                                                    .snapshot
                                                    .as_ref()
                                                    .is_some_and(|current| current.side != side)
                                                {
                                                    this.read_snapshot(side, 1, window, cx);
                                                }
                                            }),
                                        )
                                    },
                                ),
                            ),
                    )
                    .when_some(snapshot.editor.clone(), |row, editor| {
                        row.child(
                            icon_control(
                                "git-copy-snapshot",
                                IconName::Copy,
                                t!("git.copy_snapshot"),
                                false,
                            )
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    editor.read(cx).text().to_string(),
                                ))
                            }),
                        )
                    }),
            )
            .when(snapshot.loading, |panel| {
                panel.child(message(t!("git.loading_snapshot"), colors.muted))
            })
            .when_some(snapshot.notice.clone(), |panel, notice| {
                panel.child(message(notice, colors.muted))
            })
            .when_some(snapshot.editor.as_ref(), |panel, editor| {
                panel.child(
                    div().flex_1().min_h_0().w_full().child(
                        Editor::new(editor)
                            .readonly(true)
                            .bordered(false)
                            .size_full(),
                    ),
                )
            })
    }
}

const MESSAGE_PREVIEW_LINES: usize = 8;

fn commit_message(commit: &Commit, colors: Palette, preview: bool, cx: &App) -> impl IntoElement {
    let line_height = px((text_base_size(cx) * MESSAGE_LINE_HEIGHT).round());
    div()
        .id((
            ElementId::from(if preview {
                "git-commit-message"
            } else {
                "git-full-commit-message"
            }),
            SharedString::from(commit.oid.clone()),
        ))
        .debug_selector(|| {
            if preview {
                "git-commit-message"
            } else {
                "git-full-commit-message"
            }
            .into()
        })
        .w_full()
        .text_size(px(text_base_size(cx)))
        .line_height(relative(MESSAGE_LINE_HEIGHT))
        .text_color(colors.ink)
        .when(preview, |body| {
            body.line_clamp(MESSAGE_PREVIEW_LINES)
                .text_ellipsis()
                .max_h(line_height * MESSAGE_PREVIEW_LINES)
        })
        .child(commit.body.clone())
}

pub(super) fn display_date(date: &str) -> String {
    match date.get(..19) {
        Some(value) => format!(
            "{}{}",
            value.replace('T', " "),
            date.get(19..)
                .map_or(String::new(), |zone| format!(" {zone}"))
        ),
        None => date.to_owned(),
    }
}

fn center_empty(
    label: impl Into<SharedString>,
    glyph: IconName,
    colors: Palette,
) -> impl IntoElement {
    v_flex()
        .flex_1()
        .min_h_0()
        .w_full()
        .items_center()
        .justify_center()
        .gap_3()
        .text_color(colors.muted)
        .child(icon(glyph, colors.accent, 26.))
        .child(label.into())
}

pub(super) fn diff_counts(
    (add, del): (usize, usize),
    font_size: Pixels,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .flex_shrink_0()
        .gap_1()
        .text_size(font_size)
        .child(
            div()
                .text_color(cx.theme().success)
                .child(format!("+{add}")),
        )
        .child(div().text_color(cx.theme().danger).child(format!("−{del}")))
}

pub(super) fn status_badge(
    file: &git::ChangedFile,
    colors: Palette,
    reference: f32,
    cx: &App,
) -> Div {
    let color = super::status_color(file.code, colors, cx);
    div()
        .w(px(18.))
        .h(px(18.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .text_size(GIT_BADGE_SIZE)
        .font_medium()
        .text_center()
        .bg(color.opacity(0.13))
        .text_color(color)
        .child(
            div()
                .relative()
                .top(baseline_shift(cx, GIT_BADGE_SIZE.as_f32(), reference))
                .child(file.code.to_string()),
        )
}

#[cfg(all(test, target_os = "linux"))]
#[path = "tests/review_sync_tests.rs"]
mod review_sync_tests;
