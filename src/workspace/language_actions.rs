use std::{
    cell::RefCell,
    collections::BTreeMap,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context as _, Result, anyhow, ensure};
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Disableable as _, WindowExt as _,
    button::ButtonVariants,
    h_flex,
    input::{
        CodeActionProvider, EditorState, Input, InputEvent, InputState, Rope, RopeExt, TabSize,
    },
    v_flex,
};
use lsp_types::{
    CodeAction, CodeActionDisabled, FormattingOptions, Position, PrepareRenameResponse,
    WorkspaceEdit,
};
use rust_i18n::t;
use similar::{DiffTag, TextDiff};

use crate::{
    language::LanguageServer,
    lsp::{
        self,
        actions::{self, ActionSnapshot, TextEdits},
    },
    remote::{RepoPath, Request},
    repository::{Repository, files::EntryVersion},
};

use super::{
    FileBuffer, FormatDocument, ServerProject, Workspace, WorkspaceView, action_button, toast,
};

static NEXT_PROVIDER: AtomicU64 = AtomicU64::new(1);

pub(super) struct LanguageActions {
    id: SharedString,
    source: ActionSource,
    server: Option<LanguageServer>,
    cache: Rc<RefCell<ActionCache>>,
}

#[derive(Clone)]
struct ActionSource {
    workspace: WeakEntity<Workspace>,
    document: Arc<lsp::Document>,
    path: PathBuf,
}

#[derive(Default)]
struct ActionCache {
    generation: u64,
    list: Option<ActionList>,
    preview: Option<(usize, Rc<EditPlan>)>,
}

struct ActionList {
    revision: u64,
    snapshot: ActionSnapshot,
    items: Vec<CodeAction>,
}

#[derive(Clone)]
struct TargetEditor {
    editor: Entity<EditorState>,
    revision: u64,
    text: Rope,
}

impl TargetEditor {
    fn new(editor: Entity<EditorState>, cx: &App) -> Self {
        let state = editor.read(cx);
        Self {
            revision: state.document_revision(),
            text: state.text().clone(),
            editor,
        }
    }

    fn current(&self, cx: &App) -> bool {
        self.editor.read(cx).document_revision() == self.revision
    }
}

struct EditPlan {
    repo: Option<Arc<Repository>>,
    started: Option<SystemTime>,
    targets: BTreeMap<PathBuf, TargetEdit>,
}

struct TargetEdit {
    original: Rope,
    edits: TextEdits,
    editor: Option<TargetEditor>,
    version: Option<EntryVersion>,
    buffer: Option<(FileBuffer, Option<ServerProject>)>,
}

impl LanguageActions {
    pub(super) fn new(
        workspace: WeakEntity<Workspace>,
        document: Arc<lsp::Document>,
        path: PathBuf,
        server: Option<LanguageServer>,
    ) -> Rc<Self> {
        Rc::new(Self {
            id: format!(
                "lsp-actions-{}",
                NEXT_PROVIDER.fetch_add(1, Ordering::Relaxed)
            )
            .into(),
            source: ActionSource {
                workspace,
                document,
                path,
            },
            server,
            cache: Rc::default(),
        })
    }

    fn query(
        &self,
        state: Entity<EditorState>,
        range: Range<usize>,
        probe: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<CodeAction>>> {
        let source = self.source.clone();
        let server = self.server;
        let editor = TargetEditor::new(state, cx);
        let cache = self.cache.clone();
        let generation = {
            let mut cache = cache.borrow_mut();
            if !probe {
                cache.generation += 1;
                cache.list = None;
                cache.preview = None;
            }
            cache.generation
        };
        window.spawn(cx, async move |_| {
            let (mut items, snapshot) = source
                .document
                .code_actions(editor.text.clone(), range)
                .await?;
            if server == Some(LanguageServer::Clangd) {
                items.iter_mut().for_each(actions::unpack_clangd_fix);
            }
            let revision = editor.revision;
            let capabilities = source.document.action_capabilities();
            let display = items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let disabled = item.disabled.clone().or_else(|| {
                        let check = if let Some(command) = &item.command {
                            let rename = server == Some(LanguageServer::Clangd)
                                && capabilities.rename
                                && item.edit.is_none()
                                && actions::clangd_rename(
                                    command,
                                    source.document.action_uri(),
                                    &editor.text,
                                )
                                .is_some();
                            let noop = server == Some(LanguageServer::TypeScript)
                                && actions::typescript_noop(command);
                            let tweak = server == Some(LanguageServer::Clangd)
                                && capabilities.clangd_tweak
                                && item.edit.is_none()
                                && actions::clangd_tweak(
                                    command,
                                    source.document.action_uri(),
                                    &editor.text,
                                );
                            if !rename && !noop && !tweak {
                                Err(anyhow!("This action requires a server command"))
                            } else if noop && item.edit.is_none() {
                                Err(anyhow!("This action has no text changes"))
                            } else {
                                Ok(())
                            }
                        } else if let Some(edit) = &item.edit {
                            single_file_edits(edit.clone(), &source, &editor, &snapshot).map(|_| ())
                        } else if !capabilities.resolve {
                            Err(anyhow!("This action has no text changes"))
                        } else {
                            Ok(())
                        };
                        check.err().map(|error| CodeActionDisabled {
                            reason: format!("{error:#}"),
                        })
                    });
                    CodeAction {
                        title: item.title.clone(),
                        kind: item.kind.clone(),
                        is_preferred: item.is_preferred,
                        disabled,
                        data: Some(serde_json::json!([generation, index])),
                        ..Default::default()
                    }
                })
                .collect();
            if !probe && cache.borrow().generation == generation {
                cache.borrow_mut().list = Some(ActionList {
                    revision,
                    snapshot,
                    items,
                });
            }
            Ok(display)
        })
    }

    fn use_action(
        &self,
        state: Entity<EditorState>,
        display: CodeAction,
        preview: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<String>> {
        let source = self.source.clone();
        let editor = TargetEditor::new(state, cx);
        let server = self.server;
        let cache = self.cache.clone();
        let request = (|| {
            let cache = cache.borrow();
            let (generation, index) = serde_json::from_value::<(u64, usize)>(display.data?).ok()?;
            let list = cache.list.as_ref()?;
            if cache.generation != generation || list.revision != editor.revision {
                return None;
            }
            let cached = cache
                .preview
                .as_ref()
                .filter(|(cached_index, _)| *cached_index == index)
                .map(|(_, plan)| plan.clone());
            Some((
                generation,
                index,
                list.items.get(index)?.clone(),
                list.snapshot.clone(),
                cached,
            ))
        })();
        window.spawn(cx, async move |cx| {
            let (generation, index, mut item, snapshot, cached) =
                request.ok_or_else(actions::stale)?;
            let plan = if let Some(plan) = cached {
                plan
            } else {
                if item.edit.is_none()
                    && item.command.is_none()
                    && source.document.action_capabilities().resolve
                {
                    item = source
                        .document
                        .resolve_action(editor.text.clone(), item, snapshot.version)
                        .await?;
                    if server == Some(LanguageServer::Clangd) {
                        actions::unpack_clangd_fix(&mut item);
                    }
                }
                if let Some(disabled) = item.disabled {
                    return Err(anyhow!(disabled.reason));
                }
                let clangd_command = item
                    .command
                    .as_ref()
                    .filter(|_| server == Some(LanguageServer::Clangd) && item.edit.is_none());
                let uri = source.document.action_uri();
                if let Some(params) = clangd_command
                    .and_then(|command| actions::clangd_rename(command, uri, &editor.text))
                {
                    Rc::new(
                        source
                            .rename_plan(
                                &editor,
                                params.text_document_position.position,
                                params.new_name,
                                Some(snapshot.version),
                                cx,
                            )
                            .await?,
                    )
                } else {
                    let repo = cx.update(|_, cx| source.repo(&editor, cx))??;
                    let edit = if let Some(command) = clangd_command
                        .filter(|command| actions::clangd_tweak(command, uri, &editor.text))
                    {
                        source
                            .document
                            .clangd_tweak(editor.text.clone(), command.clone(), snapshot.version)
                            .await?
                    } else {
                        ensure!(
                            item.command.is_none_or(|command| {
                                server == Some(LanguageServer::TypeScript)
                                    && actions::typescript_noop(&command)
                            }),
                            "This action requires a server command"
                        );
                        item.edit.context("This action has no text changes")?
                    };
                    Rc::new(source.single_file_plan(edit, &editor, &snapshot, repo)?)
                }
            };
            if let Err(error) = plan.check_disk(cx).await {
                cache.borrow_mut().preview = None;
                return Err(error);
            }
            if preview {
                cx.update(|_, cx| source.check(&editor, &plan, cx))??;
                let diff = plan.preview();
                if cache.borrow().generation == generation {
                    cache.borrow_mut().preview = Some((index, plan));
                }
                Ok(diff)
            } else {
                cx.update(|window, cx| source.apply(&editor, plan, window, cx))??;
                Ok(String::new())
            }
        })
    }
}

impl CodeActionProvider for LanguageActions {
    fn id(&self) -> SharedString {
        self.id.clone()
    }
    fn supports_code_actions(&self) -> bool {
        self.source.document.action_capabilities().list
    }
    fn supports_rename(&self) -> bool {
        self.source.document.action_capabilities().rename
    }
    fn supports_preview(&self) -> bool {
        true
    }
    fn probe_current_line(&self) -> bool {
        true
    }

    fn code_actions(
        &self,
        state: Entity<EditorState>,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<CodeAction>>> {
        self.query(state, range, false, window, cx)
    }

    fn probe_code_actions(
        &self,
        state: Entity<EditorState>,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<CodeAction>>> {
        self.query(state, range, true, window, cx)
    }

    fn preview_code_action(
        &self,
        state: Entity<EditorState>,
        action: CodeAction,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<String>> {
        self.use_action(state, action, true, window, cx)
    }

    fn perform_code_action(
        &self,
        state: Entity<EditorState>,
        action: CodeAction,
        _: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let task = self.use_action(state, action, false, window, cx);
        window.spawn(cx, async move |_| task.await.map(drop))
    }

    fn rename_symbol(&self, state: Entity<EditorState>, window: &mut Window, cx: &mut App) {
        let source = self.source.clone();
        window.defer(cx, move |window, cx| {
            let editor = TargetEditor::new(state, cx);
            let _ = source.workspace.clone().update(cx, |workspace, cx| {
                workspace.start_symbol_rename(source, editor, window, cx);
            });
        });
    }
}

fn single_file_edits(
    edit: WorkspaceEdit,
    source: &ActionSource,
    editor: &TargetEditor,
    snapshot: &ActionSnapshot,
) -> Result<TextEdits> {
    let uri = source.document.action_uri();
    let mut targets = actions::workspace_edits(edit, Some(uri))?;
    let target = targets
        .remove(uri)
        .context("This action has no text changes")?;
    let edits = actions::text_edits(&editor.text, target, Some(snapshot.version))?;
    ensure!(!edits.is_empty(), "This action has no text changes");
    Ok(edits)
}

impl ActionSource {
    fn current(&self, workspace: &Workspace, editor: &TargetEditor, cx: &App) -> bool {
        workspace.view == WorkspaceView::Editor
            && workspace.tabs.active() == Some(self.path.as_path())
            && workspace.repository_ui.opening.is_none()
            && !workspace.repository_ui.picker_open
            && !workspace.confirming_close
            && editor.current(cx)
            && workspace.files.get(&self.path).is_some_and(|file| {
                file.editor == editor.editor
                    && file
                        .language_server
                        .as_ref()
                        .is_some_and(|server| Arc::ptr_eq(&server.document, &self.document))
            })
    }

    fn active(&self, workspace: &Workspace, editor: &TargetEditor, cx: &App) -> bool {
        let capabilities = self.document.action_capabilities();
        self.current(workspace, editor, cx) && (capabilities.list || capabilities.rename)
    }

    fn repo(&self, editor: &TargetEditor, cx: &App) -> Result<Option<Arc<Repository>>> {
        self.workspace.read_with(cx, |workspace, cx| {
            ensure!(self.active(workspace, editor, cx), actions::stale());
            Ok(if self.path.is_absolute() {
                None
            } else {
                Some(workspace.repository.clone().ok_or_else(actions::stale)?)
            })
        })?
    }

    fn single_file_plan(
        &self,
        edit: WorkspaceEdit,
        editor: &TargetEditor,
        snapshot: &ActionSnapshot,
        repo: Option<Arc<Repository>>,
    ) -> Result<EditPlan> {
        let edits = single_file_edits(edit, self, editor, snapshot)?;
        Ok(EditPlan {
            repo,
            started: None,
            targets: BTreeMap::from([(
                self.path.clone(),
                TargetEdit {
                    original: editor.text.clone(),
                    edits,
                    editor: Some(editor.clone()),
                    version: None,
                    buffer: None,
                },
            )]),
        })
    }

    async fn rename_plan(
        &self,
        source_editor: &TargetEditor,
        position: Position,
        name: String,
        version: Option<i32>,
        cx: &mut AsyncWindowContext,
    ) -> Result<EditPlan> {
        let repo = cx.update(|_, cx| self.repo(source_editor, cx))??;
        let Some(repo) = repo else {
            let (edit, snapshot) = self
                .document
                .rename(source_editor.text.clone(), position, name, version)
                .await?;
            return self.single_file_plan(edit, source_editor, &snapshot, None);
        };
        let (store, open_at_request) = cx.update(|_, cx| {
            self.workspace
                .read_with(cx, |workspace, cx| {
                    let opened = workspace
                        .files
                        .iter()
                        .map(|(path, file)| {
                            let editor = TargetEditor::new(file.editor.clone(), cx);
                            if let Some(lsp) = &file.language_server {
                                lsp.document.changed(editor.text.clone());
                            }
                            (path.clone(), (editor, file.document.draft_baseline()))
                        })
                        .collect::<BTreeMap<_, _>>();
                    (workspace.store.clone(), opened)
                })
                .map_err(anyhow::Error::from)
        })??;
        let repo_for_clock = repo.clone();
        let started = cx
            .background_executor()
            .spawn(async move {
                if let Some(ssh) = &repo_for_clock.ssh {
                    ensure!(
                        ssh.supports_lsp_edits(),
                        "Update stroll-remote to enable cross-file LSP edits."
                    );
                    ssh.call(Request::LspEditTime)
                } else {
                    Ok(SystemTime::now())
                }
            })
            .await?;
        let (edit, snapshot) = self
            .document
            .rename(source_editor.text.clone(), position, name, version)
            .await?;
        let targets = actions::workspace_edits(edit, None)?;
        let opened = cx.update(|_, cx| {
            self.workspace.read_with(cx, |workspace, cx| {
                ensure!(self.active(workspace, source_editor, cx), actions::stale());
                let mut targets_by_uri = BTreeMap::new();
                for uri in targets.keys() {
                    let path = repo.source_path(uri).with_context(|| {
                        format!("This action changes a file outside the editable workspace: {uri}")
                    })?;
                    let existing = open_at_request.get(&path);
                    match (workspace.files.get(&path), existing) {
                        (Some(file), Some((editor, _)))
                            if file.editor == editor.editor && editor.current(cx) => {}
                        (None, None) => {}
                        _ => return Err(actions::stale()),
                    }
                    targets_by_uri.insert(uri.clone(), (path, existing.cloned()));
                }
                Ok::<_, anyhow::Error>(targets_by_uri)
            })?
        })??;
        let result = cx
            .background_executor()
            .spawn(async move {
                let mut output = BTreeMap::new();
                for (uri, edit) in targets {
                    let (path, existing) = &opened[&uri];
                    let version = target_version(&repo, path, started)?;
                    let document = super::Document::open(&repo, path, &store)?;
                    ensure!(
                        !document.conflict
                            && !document.mixed_newlines
                            && target_version(&repo, path, started)? == version,
                        actions::stale()
                    );
                    let snapshot = snapshot.documents.get(&uri);
                    let lsp_version = snapshot.map(|(version, _)| *version);
                    let disk_has_bom = snapshot.is_none() && document.has_bom();
                    let (original, editor, buffer) = if let Some((editor, baseline)) = existing {
                        ensure!(document.draft_baseline() == *baseline, actions::stale());
                        if let Some((_, text)) = snapshot {
                            ensure!(text == &editor.text, actions::stale());
                        } else {
                            ensure!(
                                editor.text == document.saved_text.as_str(),
                                actions::stale()
                            );
                        }
                        (editor.text.clone(), Some(editor.clone()), None)
                    } else {
                        ensure!(!document.restored && snapshot.is_none(), actions::stale());
                        let (buffer, project) = FileBuffer::new(document).with_project(&repo, path);
                        (buffer.text.clone(), None, Some((buffer, project)))
                    };
                    let edits = if disk_has_bom {
                        let mut disk_text = original.clone();
                        const BOM: &str = "\u{feff}";
                        disk_text.insert(0, BOM);
                        actions::text_edits(&disk_text, edit, lsp_version)?
                            .into_iter()
                            .map(|(range, replacement)| {
                                ensure!(
                                    range.start >= BOM.len(),
                                    "This action changes the file's byte order mark"
                                );
                                Ok((range.start - BOM.len()..range.end - BOM.len(), replacement))
                            })
                            .collect::<Result<_>>()?
                    } else {
                        actions::text_edits(&original, edit, lsp_version)?
                    };
                    if !edits.is_empty() {
                        ensure!(
                            output
                                .insert(
                                    path.clone(),
                                    TargetEdit {
                                        original,
                                        edits,
                                        editor,
                                        version: Some(version),
                                        buffer
                                    }
                                )
                                .is_none(),
                            "The edit lists a file more than once"
                        );
                    }
                }
                ensure!(!output.is_empty(), "This action has no text changes");
                Ok(EditPlan {
                    repo: Some(repo),
                    started: Some(started),
                    targets: output,
                })
            })
            .await?;
        Ok(result)
    }

    fn check(&self, source: &TargetEditor, plan: &EditPlan, cx: &App) -> Result<()> {
        self.workspace.read_with(cx, |workspace, cx| {
            self.check_in(workspace, source, plan, cx)
        })?
    }

    fn check_in(
        &self,
        workspace: &Workspace,
        source: &TargetEditor,
        plan: &EditPlan,
        cx: &App,
    ) -> Result<()> {
        ensure!(
            self.active(workspace, source, cx)
                && plan
                    .repo
                    .as_ref()
                    .map_or(self.path.is_absolute(), |expected| {
                        workspace
                            .repository
                            .as_ref()
                            .is_some_and(|repo| Arc::ptr_eq(repo, expected))
                    }),
            actions::stale()
        );
        for (path, target) in &plan.targets {
            ensure!(
                !workspace.saving.contains(path) && !workspace.git_edit.contains(path),
                actions::stale()
            );
            match (workspace.files.get(path), &target.editor) {
                (Some(file), Some(editor)) => ensure!(
                    file.editor == editor.editor
                        && editor.current(cx)
                        && file.editor.read(cx).is_editable()
                        && !file.document.conflict
                        && !file.document.mixed_newlines
                        && !file.missing,
                    actions::stale()
                ),
                (None, None) => {}
                _ => return Err(actions::stale()),
            }
        }
        Ok(())
    }

    fn apply(
        &self,
        source: &TargetEditor,
        plan: Rc<EditPlan>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<()> {
        self.workspace.update(cx, |workspace, cx| {
            self.check_in(workspace, source, &plan, cx)?;
            workspace.apply_language_edits(&plan, window, cx);
            Ok(())
        })?
    }
}

fn target_version(repo: &Repository, path: &Path, before: SystemTime) -> Result<EntryVersion> {
    if let Some(ssh) = &repo.ssh {
        ssh.call(Request::LspEditTarget(RepoPath::from_local(path)?, before))
    } else {
        crate::repository::files::lsp_edit_target(&repo.root, path, before)
    }
}

impl EditPlan {
    async fn check_disk(&self, cx: &mut AsyncWindowContext) -> Result<()> {
        let Some((started, repo)) = self.started.zip(self.repo.clone()) else {
            return Ok(());
        };
        let targets = self
            .targets
            .iter()
            .map(|(path, target)| (path.clone(), target.version.clone()))
            .collect::<Vec<_>>();
        cx.background_executor()
            .spawn(async move {
                for (path, version) in targets {
                    ensure!(
                        Some(target_version(&repo, &path, started)?) == version,
                        actions::stale()
                    );
                }
                Ok(())
            })
            .await
    }

    fn preview(&self) -> String {
        let mut result = String::new();
        for (path, target) in &self.targets {
            use std::fmt::Write as _;
            let _ = writeln!(result, "{}", path.display());
            for (range, replacement) in &target.edits {
                let line = target.original.offset_to_point(range.start).row + 1;
                let _ = writeln!(result, "@@ {line} @@");
                if !range.is_empty() {
                    for line in target.original.slice(range.clone()).to_string().split('\n') {
                        let _ = writeln!(result, "- {line}");
                    }
                }
                if !replacement.is_empty() {
                    for line in replacement.split('\n') {
                        let _ = writeln!(result, "+ {line}");
                    }
                }
            }
        }
        result
    }
}

fn apply_language_text_edits(
    editor: &mut EditorState,
    edits: &TextEdits,
    group: Option<u64>,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) {
    let before = editor.text().clone();
    let changes = language_edit_changes(&before, edits);
    let caret = editor.cursor();
    let scroll = (editor.visible_row_range().is_some()
        && !editor
            .range_to_bounds(&(caret..caret))
            .is_some_and(|bounds| editor.input_bounds().contains(&bounds.center())))
    .then(|| editor.scroll_offset());
    editor.edit_ranges_mapping_selection_ranges(
        edits,
        group,
        |after, selection| {
            let start = map_language_edit_offset(&before, after, &changes, selection.start, true);
            let end = if selection.is_empty() {
                start
            } else {
                map_language_edit_offset(&before, after, &changes, selection.end, false)
            };
            start..end
        },
        window,
        cx,
    );
    if let Some(scroll) = scroll {
        editor.set_scroll_offset(scroll, cx);
    }
}

// Refine the selection mapping without changing the edits applied to the buffer.
pub(super) fn language_edit_changes(
    before: &Rope,
    edits: &[(Range<usize>, String)],
) -> Vec<(Range<usize>, usize)> {
    let deadline = Instant::now() + Duration::from_millis(25);
    let mut remaining_bytes = 1024 * 1024;
    let mut changes = Vec::new();
    for (range, replacement) in edits {
        let bytes = range.len().saturating_add(replacement.len());
        if range.is_empty()
            || replacement.is_empty()
            || bytes > remaining_bytes
            || Instant::now() >= deadline
        {
            changes.push((range.clone(), replacement.len()));
            continue;
        }
        remaining_bytes -= bytes;
        let original = before.slice(range.clone()).to_string();
        let diff = TextDiff::configure()
            .deadline(deadline)
            .diff_chars(&original, replacement);
        let mut old = range.start;
        for op in diff.ops() {
            let old_end = old
                + diff.old_slices()[op.old_range()]
                    .iter()
                    .map(|part| part.len())
                    .sum::<usize>();
            if op.tag() != DiffTag::Equal {
                let inserted_len = diff.new_slices()[op.new_range()]
                    .iter()
                    .map(|part| part.len())
                    .sum::<usize>();
                changes.push((old..old_end, inserted_len));
            }
            old = old_end;
        }
    }
    changes
}

pub(super) fn map_language_edit_offset(
    before: &Rope,
    after: &Rope,
    changes: &[(Range<usize>, usize)],
    offset: usize,
    after_insert: bool,
) -> usize {
    let mut old_end = 0;
    let mut new_end = 0;
    // Validated LSP edits are sorted and disjoint.
    for (range, inserted_len) in changes {
        if offset <= range.start && !(after_insert && range.is_empty() && offset == range.start) {
            break;
        }
        let new_start = new_end + range.start - old_end;
        new_end = new_start + inserted_len;
        old_end = range.end;
        if offset < range.end {
            // A formatter can replace an entire file. Keep the relative line
            // and column inside the replacement where its text permits it.
            let old_start = before.offset_to_position(range.start);
            let old_position = before.offset_to_position(offset);
            let new_position = after.offset_to_position(new_start);
            let row = old_position.line - old_start.line;
            let column = if row == 0 {
                new_position.character + old_position.character - old_start.character
            } else {
                old_position.character
            };
            return after
                .position_to_offset(&Position::new(new_position.line + row, column))
                .min(new_end);
        }
    }
    new_end + offset - old_end
}

impl Workspace {
    pub(super) fn indent_source_newline(
        &mut self,
        editor: &Entity<EditorState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self
            .active_file()
            .filter(|file| &file.editor == editor && self.can_format_file(file, cx))
        else {
            return;
        };
        let path = file.path.clone();
        let file = self.files.get_mut(&path).unwrap();
        let (Some(newline), Some(server), Some(kind)) = (
            file.newline.take(),
            file.language_server.as_ref(),
            file.server_project
                .as_ref()
                .and_then(|project| project.newline_server()),
        ) else {
            return;
        };
        let state = editor.read(cx);
        if !newline.is_current(state) || !state.focus_handle(cx).is_focused(window) {
            return;
        }

        let editor = editor.clone();
        let document = server.document.clone();
        let request = document.newline_indentation(state.text().clone(), newline, kind);
        file.newline_task = cx.spawn_in(window, async move |workspace, cx| {
            let Ok(Some(indentation)) = request.await else {
                return;
            };
            _ = workspace.update_in(cx, |workspace, window, cx| {
                let Some(file) = workspace.active_file().filter(|file| {
                    file.editor == editor
                        && workspace.can_format_file(file, cx)
                        && file
                            .language_server
                            .as_ref()
                            .is_some_and(|server| Arc::ptr_eq(&server.document, &document))
                }) else {
                    return;
                };
                let editor = file.editor.clone();
                editor.update(cx, |editor, cx| {
                    let range = newline.indentation();
                    if newline.is_current(editor)
                        && editor.focus_handle(cx).is_focused(window)
                        && editor.text().slice(range.clone()) != indentation.as_str()
                    {
                        editor.edit_ranges(&[(range, indentation)], window, cx);
                    }
                });
            });
        });
    }

    pub(super) fn format_document(
        &mut self,
        _: &FormatDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self.active_file().filter(|file| {
            self.can_format_file(file, cx)
                && file.editor.read(cx).focus_handle(cx).is_focused(window)
                && !self.locator.open
                && !window.has_active_dialog(cx)
        }) else {
            return;
        };
        let Some(server) = &file.language_server else {
            toast::info(window, cx, t!("lsp.format_unavailable"));
            return;
        };
        let source = ActionSource {
            workspace: cx.weak_entity(),
            document: server.document.clone(),
            path: file.path.clone(),
        };
        let editor = TargetEditor::new(file.editor.clone(), cx);
        // The workspace editor uses the component's default indentation.
        let tab = TabSize::default();
        let result = source.document.format_document(
            editor.text.clone(),
            FormattingOptions {
                tab_size: tab.tab_size as u32,
                insert_spaces: !tab.hard_tabs,
                ..Default::default()
            },
        );
        self.language_format = cx.spawn_in(window, async move |this, cx| {
            let result = result.await;
            _ = this.update_in(cx, |workspace, window, cx| {
                if !source.current(workspace, &editor, cx)
                    || !workspace.can_format_file(&workspace.files[&source.path], cx)
                {
                    return;
                }
                match result {
                    Ok(Some(edits)) if !edits.is_empty() => {
                        editor.editor.update(cx, |editor, cx| {
                            apply_language_text_edits(editor, &edits, None, window, cx)
                        });
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => toast::info(window, cx, t!("lsp.format_unavailable")),
                    Err(error) => workspace.show_error(
                        error.context(t!("lsp.format_failed").into_owned()),
                        window,
                        cx,
                    ),
                }
            });
        });
    }

    fn can_format_file(&self, file: &super::OpenFile, cx: &App) -> bool {
        self.view == WorkspaceView::Editor
            && self.repository_ui.opening.is_none()
            && !self.repository_overlay_open()
            && !self.confirming_close
            && !file.document.conflict
            && !self.file_editor_locked(file)
            && file.editor.read(cx).is_editable()
            && !file.editor.read(cx).is_composing()
    }

    fn apply_language_edits(
        &mut self,
        plan: &EditPlan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = plan.targets.keys().cloned().collect::<Vec<_>>();
        let group = if paths.len() > 1 {
            match self.store.register_edit_group(&paths) {
                Ok(group) => Some(group),
                Err(error) => {
                    self.show_error(error, window, cx);
                    return;
                }
            }
        } else {
            None
        };
        for (path, target) in &plan.targets {
            if let Some((buffer, project)) = &target.buffer {
                self.install_file(path, buffer.clone(), project.clone(), window, cx);
            }
            self.remember_editor_preview(path);
            self.tabs.open_in_background(path.clone());
        }
        for (path, target) in &plan.targets {
            self.files[path].editor.update(cx, |editor, cx| {
                apply_language_text_edits(editor, &target.edits, group, window, cx);
            });
        }
        if let Some(group) = group {
            self.language_action_groups.insert(group, paths);
            if self.language_action_groups.len() > 1000 {
                self.language_action_groups.pop_first();
            }
        }
        self.capture_session(cx);
        cx.notify();
    }

    pub(super) fn replay_language_edits(
        &mut self,
        group: u64,
        undo: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = match self
            .language_action_groups
            .get(&group)
            .cloned()
            .map(|paths| Ok(Some(paths)))
            .unwrap_or_else(|| self.store.edit_group_paths(group))
        {
            Ok(Some(paths)) => paths,
            Ok(None) => return,
            Err(error) => {
                self.show_error(error, window, cx);
                return;
            }
        };
        let missing = paths
            .iter()
            .find(|path| !self.files.contains_key(*path))
            .cloned();
        if let Some(path) = missing {
            self.ensure_review_buffer(path, window, cx, move |workspace, result, window, cx| {
                match result {
                    Ok(()) => workspace.replay_language_edits(group, undo, window, cx),
                    Err(error) => workspace.show_error(error, window, cx),
                }
            });
            return;
        }
        let editors = paths
            .iter()
            .map(|path| {
                let file = self.files.get(path)?;
                let state = file.editor.read(cx);
                (state.is_editable()
                    && state.history_group(undo) == Some(group)
                    && !self.saving.contains(path)
                    && !self.git_edit.contains(path)
                    && !file.document.conflict
                    && !file.missing)
                    .then(|| file.editor.clone())
            })
            .collect::<Option<Vec<_>>>();
        let Some(editors) = editors else {
            super::toast::info(window, cx, t!("lsp.actions_history_conflict"));
            return;
        };
        for editor in editors {
            editor.update(cx, |editor, cx| {
                editor.replay_history_group(group, undo, window, cx)
            });
        }
    }

    fn start_symbol_rename(
        &mut self,
        source: ActionSource,
        editor: TargetEditor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !source.active(self, &editor, cx) {
            return;
        }
        let offset = editor.editor.read(cx).cursor();
        self.language_rename = cx.spawn_in(window, async move |this, cx| {
            let prepared = if source.document.action_capabilities().prepare_rename {
                source
                    .document
                    .prepare_rename(editor.text.clone(), offset)
                    .await
            } else {
                Ok(Some(PrepareRenameResponse::DefaultBehavior {
                    default_behavior: true,
                }))
            };
            let result =
                prepared.and_then(|prepared| rename_placeholder(&editor.text, offset, prepared));
            _ = this.update_in(cx, |workspace, window, cx| {
                if !source.active(workspace, &editor, cx)
                    || editor.editor.read(cx).cursor() != offset
                    || !editor.editor.read(cx).focus_handle(cx).is_focused(window)
                {
                    return;
                }
                match result {
                    Ok(placeholder) => {
                        let position = lsp::lsp_position(&editor.text, offset);
                        RenameDialog::open(source, editor, position, placeholder, window, cx);
                    }
                    Err(error) => workspace.show_error(error, window, cx),
                }
            });
        });
    }
}

fn rename_placeholder(
    text: &Rope,
    offset: usize,
    prepared: Option<PrepareRenameResponse>,
) -> Result<String> {
    let value = match prepared {
        Some(PrepareRenameResponse::RangeWithPlaceholder { range, placeholder }) => {
            ensure!(
                actions::byte_offset(text, range.start)
                    .zip(actions::byte_offset(text, range.end))
                    .is_some_and(|(start, end)| start < end),
                t!("lsp.rename_unavailable")
            );
            Some(placeholder)
        }
        Some(PrepareRenameResponse::Range(range)) => actions::byte_offset(text, range.start)
            .zip(actions::byte_offset(text, range.end))
            .filter(|(start, end)| start < end)
            .map(|(start, end)| text.slice(start..end).to_string()),
        Some(PrepareRenameResponse::DefaultBehavior {
            default_behavior: true,
        }) => text
            .word_range(offset)
            .map(|range| text.slice(range).to_string()),
        _ => None,
    };
    value
        .filter(|name| actions::valid_name(name))
        .context(t!("lsp.rename_unavailable").into_owned())
}

struct RenameDialog {
    source: ActionSource,
    editor: TargetEditor,
    position: Position,
    input: Entity<InputState>,
    original: String,
    name: SharedString,
    preview: Option<(SharedString, Rc<EditPlan>, SharedString)>,
    busy: bool,
    error: Option<SharedString>,
    task: Task<()>,
    _events: Subscription,
}

impl RenameDialog {
    fn open(
        source: ActionSource,
        editor: TargetEditor,
        position: Position,
        original: String,
        window: &mut Window,
        cx: &mut App,
    ) {
        use gpui_component::WindowExt as _;
        let input = cx.new(|cx| InputState::new(window, cx).default_value(original.clone()));
        let state = cx.new(|cx| {
            let events = cx.subscribe_in(
                &input,
                window,
                |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { shift, .. } => this.submit(*shift, window, cx),
                    InputEvent::Change => {
                        let name = this.input.read(cx).value();
                        if name == this.name {
                            return;
                        }
                        this.name = name;
                        this.task = Task::ready(());
                        this.busy = false;
                        this.error = None;
                        this.preview = None;
                        cx.notify();
                    }
                    _ => {}
                },
            );
            Self {
                source,
                editor,
                position,
                input: input.clone(),
                name: original.clone().into(),
                original,
                preview: None,
                busy: false,
                error: None,
                task: Task::ready(()),
                _events: events,
            }
        });
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title(t!("lsp.rename_symbol"))
                .child(state.clone())
                .width(px(500.))
                .close_button(false)
                .on_ok(|_, _, _| false)
                .on_close({
                    let state = state.clone();
                    move |_, _, cx| state.update(cx, |state, _| state.task = Task::ready(()))
                })
        });
        window.on_next_frame(move |window, cx| {
            input.update(cx, |input, cx| {
                input.select_all(window, cx);
                input.focus(window, cx);
            });
        });
    }

    fn submit(&mut self, preview: bool, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.input.read(cx).value();
        if self.busy || name.as_ref() == self.original || !actions::valid_name(&name) {
            return;
        }
        self.busy = true;
        self.error = None;
        let cached = self
            .preview
            .as_ref()
            .filter(|(value, _, _)| *value == name)
            .map(|(_, plan, _)| plan.clone());
        let source = self.source.clone();
        let editor = self.editor.clone();
        let position = self.position;
        self.task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let plan = if let Some(plan) = cached {
                    plan
                } else {
                    Rc::new(
                        source
                            .rename_plan(&editor, position, name.to_string(), None, cx)
                            .await?,
                    )
                };
                plan.check_disk(cx).await?;
                if preview {
                    cx.update(|_, cx| source.check(&editor, &plan, cx))??;
                    Ok::<_, anyhow::Error>(Some((name, plan.clone(), plan.preview().into())))
                } else {
                    cx.update(|window, cx| source.apply(&editor, plan, window, cx))??;
                    Ok(None)
                }
            }
            .await;
            _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(Some(preview)) => this.preview = Some(preview),
                    Ok(None) => {
                        use gpui_component::WindowExt as _;
                        window.close_dialog(cx);
                    }
                    Err(error) => {
                        this.preview = None;
                        this.error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            });
        });
        cx.notify();
    }
}

impl Render for RenameDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let valid = !self.busy && {
            let value = self.input.read(cx).value();
            value.as_ref() != self.original && actions::valid_name(&value)
        };
        v_flex()
            .gap_3()
            .child(
                div()
                    .debug_selector(|| "rename-input".into())
                    .child(Input::new(&self.input)),
            )
            .when_some(self.preview.as_ref(), |this, (_, _, diff)| {
                this.child(
                    div()
                        .id("rename-preview")
                        .debug_selector(|| "rename-preview".into())
                        .text_xs()
                        .font_family(cx.theme().mono_font_family.clone())
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .child(diff.clone()),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "rename-error".into())
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .when(self.busy, |this| {
                this.child(div().text_sm().child(t!("lsp.actions_loading")))
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        action_button("cancel-symbol-rename")
                            .secondary()
                            .label(t!("common.cancel"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                use gpui_component::WindowExt as _;
                                this.task = Task::ready(());
                                window.close_dialog(cx);
                            })),
                    )
                    .child(
                        action_button("preview-symbol-rename")
                            .secondary()
                            .label(t!("lsp.actions_preview"))
                            .disabled(!valid)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.submit(true, window, cx)),
                            ),
                    )
                    .child(
                        action_button("apply-symbol-rename")
                            .primary()
                            .label(t!("lsp.rename_symbol"))
                            .disabled(!valid)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.submit(false, window, cx)),
                            ),
                    ),
            )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{config::ConfigFile, repository::Store};
    use gpui_component::{
        Root,
        input::{Redo, Undo},
    };
    use std::{
        cell::Cell,
        fs,
        process::Command,
        time::{Duration, Instant},
    };

    #[::core::prelude::v1::test]
    fn language_edits_map_offsets_through_an_earlier_insert_and_unicode_replacement() {
        let before = Rope::from("let x = 😀old\nvery long\nunchanged\n");
        let after = Rope::from("// Header\nlet x = 文new\nshort\nunchanged\n");
        let original = before.to_string();
        let updated = after.to_string();
        let edits = vec![
            (0..0, "// Header\n".len()),
            (
                original.find('😀').unwrap()..original.find("\nunchanged").unwrap(),
                "文new\nshort".len(),
            ),
        ];
        let map = |offset| map_language_edit_offset(&before, &after, &edits, offset, true);
        assert_eq!(map(0), "// Header\n".len());
        assert_eq!(
            map_language_edit_offset(&before, &after, &edits, 0, false),
            0
        );
        assert_eq!(
            map(original.find("old").unwrap()),
            updated.find("new").unwrap()
        );
        assert_eq!(
            map(original.find("long").unwrap() + 2),
            updated.find("\nunchanged").unwrap()
        );
        assert_eq!(
            map(original.find("unchanged").unwrap()),
            updated.find("unchanged").unwrap()
        );
    }

    fn pump(
        visual: &mut VisualTestContext,
        step: &str,
        mut ready: impl FnMut(&mut VisualTestContext) -> bool,
    ) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            visual.executor().advance_clock(Duration::from_millis(20));
            visual.run_until_parked();
            visual.update(|window, cx| {
                window.simulate_next_frame(cx);
                window.draw(cx).clear(cx);
            });
            if ready(visual) {
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "Timed out waiting for {step}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn shortcut() -> &'static str {
        if cfg!(target_os = "macos") {
            "cmd-."
        } else {
            "ctrl-."
        }
    }

    fn test_app() -> TestAppContext {
        let app = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(cfg!(target_os = "linux")),
        );
        app.update(|cx| {
            gpui_component::init(cx);
            cx.set_reduce_motion(true);
        });
        app
    }

    fn open_fixture<'a>(
        app: &'a mut TestAppContext,
        authority: Option<&str>,
        configure: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<(
        tempfile::TempDir,
        Entity<Workspace>,
        &'a mut VisualTestContext,
    )> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repository");
        fs::create_dir(&root)?;
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()?
                .success()
        );
        let main = PathBuf::from("main.cpp");
        let other = PathBuf::from("other.cpp");
        let original = "int old = 0;\n";
        fs::write(root.join(&main), original)?;
        fs::write(root.join(&other), original)?;
        configure(&root)?;
        let config_path = fixture.path().join("config.toml");
        let script = fixture.path().join("server.py");
        fs::write(&script, include_str!("../../tests/fixtures/lsp_actions.py"))?;
        fs::write(
            &config_path,
            format!(
                "[lsp.clangd]\ncommand = 'python3 -u {}'\n",
                script.display()
            ),
        )?;
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(config_path));
        let mut workspace = None;
        let (_, visual) = app.add_window_view(|window, cx| {
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(1050.), px(650.)));
        let location = authority.map_or_else(
            || root.clone(),
            |authority| PathBuf::from(format!("ssh://{authority}{}", root.display())),
        );
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.open_repository(
                    crate::repository::Location {
                        root: location,
                        file: Some(main.clone()),
                    },
                    window,
                    cx,
                );
            })
        });
        Ok((fixture, workspace, visual))
    }

    fn workflow(authority: Option<String>) -> Result<()> {
        let mut app = test_app();
        let (fixture, workspace, visual) =
            open_fixture(&mut app, authority.as_deref(), |_| Ok(()))?;
        let root = fixture.path().join("repository");
        let main = PathBuf::from("main.cpp");
        let other = PathBuf::from("other.cpp");
        let original = "int old = 0;\n";
        pump(visual, "server startup", |visual| {
            visual.update(|_, cx| {
                workspace
                    .read(cx)
                    .active_file()
                    .and_then(|file| file.language_server.as_ref())
                    .is_some_and(|server| server.document.action_capabilities().list)
            })
        })?;
        let editor = visual.update(|window, cx| {
            let editor = workspace.read(cx).files[&main].editor.clone();
            editor.update(cx, |editor, cx| {
                editor.focus(window, cx);
                editor.set_selected_range(5..5, cx);
            });
            editor
        });
        pump(visual, "diagnostics", |visual| {
            visual.update(|_, cx| {
                editor
                    .read(cx)
                    .diagnostics()
                    .is_some_and(|items| !items.is_empty())
            })
        })?;
        // The automatic current-line query must produce a bulb without moving or
        // resizing the source, and the query uses the server's original diagnostic.
        visual.executor().advance_clock(Duration::from_millis(250));
        pump(visual, "lightbulb", |visual| {
            visual.debug_bounds("code-action-lightbulb").is_some()
        })?;
        assert!(root.join("received-diagnostic").exists());

        let bulb = visual.debug_bounds("code-action-lightbulb").unwrap();
        assert_eq!(bulb.size, size(px(22.), px(22.)));
        let source_end = visual.update(|_, cx| editor.read(cx).range_to_bounds(&(12..12)).unwrap());
        assert!(bulb.left() >= source_end.right() + px(5.));
        assert!(bulb.top() < source_end.bottom() && bulb.bottom() > source_end.top());
        visual.simulate_click(
            point(bulb.right() - px(2.), bulb.center().y),
            Modifiers::default(),
        );
        visual.update(|_, cx| assert!(editor.read(cx).code_action_menu_state().open));
        pump(visual, "code action menu", |visual| {
            visual.update(|_, cx| editor.read(cx).code_action_menu_state().items.len() == 3)
        })?;
        visual.update(|_, cx| {
            assert_eq!(
                editor.read(cx).code_action_menu_state().items[0]
                    .action
                    .title,
                "Fix current draft"
            );
        });
        let preview_button = visual.debug_bounds("preview-code-action-button-0").unwrap();
        assert_eq!(preview_button.size, size(px(22.), px(22.)));
        let selected_row = visual.debug_bounds("code-action-row-0").unwrap();
        assert!(preview_button.left() > selected_row.left());
        let right_inset = selected_row.right() - preview_button.right();
        assert!(right_inset > px(0.));
        assert_eq!(preview_button.top() - selected_row.top(), right_inset);
        assert_eq!(selected_row.bottom() - preview_button.bottom(), right_inset);
        visual.simulate_click(
            point(preview_button.right() - px(2.), preview_button.center().y),
            Modifiers::default(),
        );
        pump(visual, "quick-fix preview", |visual| {
            visual.debug_bounds("code-action-preview").is_some()
        })?;
        assert!(visual.update(|_, cx| {
            editor
                .read(cx)
                .code_action_menu_state()
                .preview
                .as_ref()
                .is_some_and(|(_, diff)| diff.contains("+ fixed"))
        }));
        assert_eq!(fs::read_to_string(root.join(&main))?, original);
        visual.simulate_keystrokes("enter");
        pump(visual, "quick-fix apply", |visual| {
            visual.update(|_, cx| editor.read(cx).text() == "int fixed = 0;\n")
        })?;
        assert_eq!(fs::read_to_string(root.join(&main))?, original);
        visual.update(|window, cx| window.dispatch_action(Box::new(Undo), cx));
        visual.update(|window, cx| {
            let editor = editor.read(cx);
            assert_eq!(editor.text(), original);
            assert_eq!(editor.selected_range(), 5..5);
            assert!(editor.focus_handle(cx).is_focused(window));
        });
        visual.update(|window, cx| window.dispatch_action(Box::new(Redo), cx));
        visual.update(|_, cx| {
            assert_eq!(editor.read(cx).text(), "int fixed = 0;\n");
            assert_eq!(editor.read(cx).selected_range(), 5..5);
        });
        visual.update(|window, cx| window.dispatch_action(Box::new(Undo), cx));

        // The local run enters via F2. The SSH run exercises the special clangd
        // action and the remote host timestamp checks.
        visual.update(|_, cx| editor.update(cx, |editor, cx| editor.set_selected_range(5..5, cx)));
        if authority.is_some() {
            visual.simulate_keystrokes(shortcut());
            pump(visual, "SSH code action menu", |visual| {
                visual.update(|_, cx| editor.read(cx).code_action_menu_state().items.len() == 3)
            })?;
            visual.simulate_keystrokes("down enter");
        } else {
            visual.simulate_keystrokes("f2");
            pump(visual, "rename dialog", |visual| {
                visual.debug_bounds("rename-input").is_some()
            })?;
            visual.simulate_input("renamed");
            visual.simulate_keystrokes("shift-enter");
            pump(visual, "rename preview", |visual| {
                visual.debug_bounds("rename-preview").is_some()
            })?;
            assert!(visual.update(|_, cx| !workspace.read(cx).files.contains_key(&other)));
            fs::write(root.join(&other), format!("{original}// external edit\n"))?;
            visual.simulate_keystrokes("enter");
            pump(visual, "rename conflict", |visual| {
                visual.debug_bounds("rename-error").is_some()
            })?;
            assert!(visual.update(|_, cx| {
                !workspace.read(cx).files.contains_key(&other) && editor.read(cx).text() == original
            }));
            fs::write(root.join(&other), original)?;
            visual.simulate_keystrokes("shift-enter");
            pump(visual, "refreshed rename preview", |visual| {
                visual.debug_bounds("rename-preview").is_some()
            })?;
            visual.simulate_keystrokes("enter");
        }
        pump(visual, "rename apply", |visual| {
            visual.update(|_, cx| {
                workspace
                    .read(cx)
                    .files
                    .get(&other)
                    .is_some_and(|file| file.editor.read(cx).text() == "int renamed = 0;\n")
            })
        })?;
        visual.update(|window, cx| {
            let workspace = workspace.read(cx);
            assert_eq!(workspace.tabs.active(), Some(main.as_path()));
            assert!(editor.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(editor.read(cx).text(), "int renamed = 0;\n");
            assert_eq!(editor.read(cx).selected_range(), 5..5);
            assert!(workspace.tab_is_dirty(&main) && workspace.tab_is_dirty(&other));
        });
        assert_eq!(fs::read_to_string(root.join(&main))?, original);
        assert_eq!(fs::read_to_string(root.join(&other))?, original);
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.files[&other].editor.update(cx, |editor, cx| {
                    editor.edit_ranges(&[(17..17, " // later".into())], window, cx);
                });
            });
            window.dispatch_action(Box::new(Undo), cx);
        });
        visual.run_until_parked();
        assert!(visual.update(|_, cx| editor.read(cx).text() == "int renamed = 0;\n"));
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.files[&other].editor.update(cx, |editor, cx| {
                    editor.replay_text_history(true, window, cx)
                });
            });
            window.dispatch_action(Box::new(Undo), cx);
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            assert_eq!(editor.read(cx).text(), original);
            assert_eq!(editor.read(cx).selected_range(), 5..5);
            assert!(editor.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(
                workspace.read(cx).files[&other].editor.read(cx).text(),
                original
            );
        });
        visual.update(|window, cx| window.dispatch_action(Box::new(Redo), cx));
        visual.run_until_parked();
        visual.update(|window, cx| {
            assert_eq!(editor.read(cx).text(), "int renamed = 0;\n");
            assert_eq!(editor.read(cx).selected_range(), 5..5);
            assert!(editor.read(cx).focus_handle(cx).is_focused(window));
            assert_eq!(
                workspace.read(cx).files[&other].editor.read(cx).text(),
                "int renamed = 0;\n"
            );
        });
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn previews_and_applies_code_actions_and_renames_as_draft_transactions() -> Result<()> {
        workflow(None)
    }

    #[::core::prelude::v1::test]
    fn lightbulb_collects_same_line_clangd_actions_and_applies_suppression_as_a_draft() -> Result<()>
    {
        const ORIGINAL: &str = "float convert(int n) {\n\treturn n + 42;\n}\n";
        const COMMENT: &str = "// NOLINT(bugprone-narrowing-conversions)";
        let mut app = test_app();
        let (fixture, workspace, visual) = open_fixture(&mut app, None, |root| {
            fs::write(root.join("main.cpp"), ORIGINAL)?;
            fs::write(root.join("suppression-mode"), "")?;
            Ok(())
        })?;
        let disk_path = fixture.path().join("repository/main.cpp");
        pump(visual, "clang-tidy diagnostics", |visual| {
            visual.update(|_, cx| {
                workspace.read(cx).active_file().is_some_and(|file| {
                    let editor = file.editor.read(cx);
                    editor.can_open_code_actions()
                        && editor.diagnostics().is_some_and(|items| !items.is_empty())
                })
            })
        })?;
        let editor = visual.update(|window, cx| {
            let editor = workspace.read(cx).active_file().unwrap().editor.clone();
            editor.update(cx, |state, cx| {
                state.focus(window, cx);
                let caret = ORIGINAL.find("n +").unwrap();
                state.set_selected_range(caret..caret, cx);
            });
            editor
        });
        pump(visual, "current line lightbulb", |visual| {
            visual.debug_bounds("code-action-lightbulb").is_some()
        })?;
        let bulb = visual.debug_bounds("code-action-lightbulb").unwrap();
        visual.simulate_click(
            point(bulb.right() - px(2.), bulb.center().y),
            Modifiers::default(),
        );
        pump(visual, "clang-tidy suppression menu", |visual| {
            visual.update(|_, cx| {
                let menu = editor.read(cx).code_action_menu_state();
                menu.open && !menu.loading
            })
        })?;
        visual.update(|window, cx| {
            editor.update(cx, |state, cx| {
                let menu = state.code_action_menu_state();
                assert_eq!(menu.items.len(), 3);
                assert_eq!(menu.items[0].action.title, "Insert an explicit cast");
                let item = menu.items[1].clone();
                assert_eq!(
                    item.action.title,
                    "Suppress bugprone-narrowing-conversions on this line"
                );
                assert!(item.action.disabled.is_none());
                assert_eq!(
                    menu.items[2].action.title,
                    "Suppress readability-magic-numbers on this line"
                );
                assert!(menu.items[2].action.disabled.is_none());
                state.preview_code_action(&item, window, cx);
            });
        });
        pump(visual, "clang-tidy suppression preview", |visual| {
            visual.update(|_, cx| {
                let state = editor.read(cx);
                assert_eq!(state.text(), ORIGINAL);
                state
                    .code_action_menu_state()
                    .preview
                    .as_ref()
                    .is_some_and(|(_, diff)| diff.contains(COMMENT))
            })
        })?;
        visual.simulate_keystrokes("down enter");
        let updated = ORIGINAL.replace("return n + 42;", &format!("return n + 42;  {COMMENT}"));
        pump(
            visual,
            "clang-tidy suppression applied to the draft",
            |visual| visual.update(|_, cx| editor.read(cx).text() == updated.as_str()),
        )?;
        assert_eq!(fs::read_to_string(&disk_path)?, ORIGINAL);
        visual.update(|window, cx| window.dispatch_action(Box::new(Undo), cx));
        visual.update(|_, cx| assert_eq!(editor.read(cx).text(), ORIGINAL));
        visual.update(|window, cx| window.dispatch_action(Box::new(Redo), cx));
        visual.update(|_, cx| assert_eq!(editor.read(cx).text(), updated.as_str()));
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn clangd_command_actions_preview_and_apply_as_undoable_draft_edits() -> Result<()> {
        const ORIGINAL: &str = "float convert(int old) { return old; }\n";
        let mut app = test_app();
        let (fixture, workspace, visual) = open_fixture(&mut app, None, |root| {
            fs::write(root.join("main.cpp"), ORIGINAL)?;
            fs::write(root.join("command-mode"), "")?;
            Ok(())
        })?;
        let root = fixture.path().join("repository");
        pump(visual, "clangd command action provider", |visual| {
            visual.update(|_, cx| {
                workspace
                    .read(cx)
                    .active_file()
                    .is_some_and(|file| file.editor.read(cx).can_open_code_actions())
            }) && root.join("unsolicited-edit-denied").exists()
        })?;
        let editor = visual.update(|window, cx| {
            let editor = workspace.read(cx).active_file().unwrap().editor.clone();
            editor.update(cx, |state, cx| {
                assert_eq!(state.text(), ORIGINAL);
                state.focus(window, cx);
                state.set_selected_range(33..33, cx);
            });
            editor
        });
        pump(visual, "clangd command action lightbulb", |visual| {
            visual.debug_bounds("code-action-lightbulb").is_some()
        })?;
        let executed = root.join("executed-tweaks");
        assert!(!executed.exists());
        for (index, replacement, preview) in [
            (0, "static_cast<float>(old)", true),
            (1, "float(old)", true),
            (1, "float(old)", false),
        ] {
            visual.simulate_keystrokes(shortcut());
            pump(visual, "clangd command action menu", |visual| {
                visual.update(|_, cx| editor.read(cx).code_action_menu_state().items.len() == 2)
            })?;
            if index == 1 {
                visual.simulate_keystrokes("down");
            }
            if preview {
                visual.update(|window, cx| {
                    editor.update(cx, |state, cx| {
                        let item = state.code_action_menu_state().items[index].clone();
                        assert!(item.action.disabled.is_none());
                        state.preview_code_action(&item, window, cx);
                    });
                });
                pump(visual, "clangd command action preview", |visual| {
                    visual.update(|_, cx| {
                        let state = editor.read(cx);
                        assert_eq!(state.text(), ORIGINAL);
                        state
                            .code_action_menu_state()
                            .preview
                            .as_ref()
                            .is_some_and(|(_, diff)| diff.contains(replacement))
                    })
                })?;
            }
            let updated = format!("float convert(int old) {{ return {replacement}; }}\n");
            visual.simulate_keystrokes("enter");
            pump(visual, "clangd command action apply", |visual| {
                visual.update(|_, cx| editor.read(cx).text() == updated.as_str())
            })?;
            assert_eq!(fs::read_to_string(root.join("main.cpp"))?, ORIGINAL);
            visual.update(|window, cx| window.dispatch_action(Box::new(Undo), cx));
            visual.update(|_, cx| assert_eq!(editor.read(cx).text(), ORIGINAL));
        }
        assert_eq!(fs::read_to_string(executed)?, "tweak\ntweak\n");
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn code_action_keeps_caret_and_selection_on_unchanged_text() -> Result<()> {
        const ORIGINAL: &str = "int old = 0;\nint payload = 1;\n";
        const UPDATED: &str = "// Header\nint fixed = 0;\nint payload = 1;\n";
        let mut app = test_app();
        let (_fixture, workspace, visual) = open_fixture(&mut app, None, |root| {
            fs::write(root.join("main.cpp"), ORIGINAL)?;
            Ok(())
        })?;
        pump(visual, "code action provider", |visual| {
            visual.update(|_, cx| {
                workspace
                    .read(cx)
                    .active_file()
                    .is_some_and(|file| file.editor.read(cx).can_open_code_actions())
            })
        })?;
        let editor =
            visual.update(|_, cx| workspace.read(cx).active_file().unwrap().editor.clone());
        let payload = |text: &str| {
            let start = text.find("payload").unwrap();
            start..start + "payload".len()
        };
        let header_len = "// Header\n".len();
        for (before, after, reversed) in [
            (0..0, header_len..header_len, false),
            (payload(ORIGINAL), payload(UPDATED), true),
        ] {
            visual.update(|window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.focus(window, cx);
                    editor.set_selected_range_with_direction(before.clone(), reversed, cx);
                });
            });
            visual.simulate_keystrokes(shortcut());
            visual.update(|_, cx| assert!(editor.read(cx).code_action_menu_state().open));
            pump(visual, "code action results", |visual| {
                visual.update(|_, cx| !editor.read(cx).code_action_menu_state().items.is_empty())
            })?;
            visual.simulate_keystrokes("enter");
            pump(visual, "whole-buffer code action", |visual| {
                visual.update(|_, cx| editor.read(cx).text() == UPDATED)
            })?;
            let assert_selection = |visual: &mut VisualTestContext, range| {
                visual.update(|_, cx| {
                    let editor = editor.read(cx);
                    assert_eq!(editor.selected_range(), range);
                    assert_eq!(editor.is_selection_reversed(), reversed);
                });
            };
            assert_selection(visual, after.clone());
            visual.update(|window, cx| window.dispatch_action(Box::new(Undo), cx));
            assert_selection(visual, before.clone());
            visual.update(|window, cx| window.dispatch_action(Box::new(Redo), cx));
            assert_selection(visual, after);
            visual.update(|window, cx| window.dispatch_action(Box::new(Undo), cx));
        }
        Ok(())
    }

    /// The loopback host must run the real helper and launch the fixture server
    /// for clangd, using the repository cwd to find ../server.py.
    #[::core::prelude::v1::test]
    #[ignore = "requires a disposable SSH host with a fixture clangd command"]
    fn code_actions_and_rename_over_ssh() -> Result<()> {
        workflow(Some(std::env::var("STROLL_TEST_LSP_SSH_AUTHORITY")?))
    }

    fn wait_for_formatting(
        visual: &mut VisualTestContext,
        workspace: &Entity<Workspace>,
    ) -> Result<()> {
        let finished = Rc::new(Cell::new(false));
        visual.update(|_, cx| {
            let task = workspace.update(cx, |workspace, _| {
                std::mem::replace(&mut workspace.language_format, Task::ready(()))
            });
            let finished = finished.clone();
            cx.spawn(async move |_| {
                task.await;
                finished.set(true);
            })
            .detach();
        });
        pump(visual, "formatting response", |_| finished.get())
    }

    fn formatting_workflow(authority: Option<&str>) -> Result<()> {
        let mut app = test_app();
        app.update(|cx| {
            cx.bind_keys([KeyBinding::new(
                "alt-shift-f",
                FormatDocument,
                Some("WorkspaceEditor > (Input && mode == editor && !Locator)"),
            )]);
        });
        let dynamic = authority.is_none();
        let (fixture, workspace, visual) = open_fixture(&mut app, authority, |root| {
            fs::write(
                root.join("format-mode"),
                if dynamic { "dynamic" } else { "static" },
            )?;
            Ok(())
        })?;
        let root = fixture.path().join("repository");
        pump(visual, "formatter session", |visual| {
            (!dynamic || root.join("register-format").exists())
                && visual.update(|_, cx| {
                    workspace
                        .read(cx)
                        .active_file()
                        .and_then(|file| file.language_server.as_ref())
                        .is_some_and(|server| server.document.action_capabilities().list)
                })
        })?;
        let editor = visual.update(|window, cx| {
            let editor = workspace.read(cx).active_file().unwrap().editor.clone();
            editor.update(cx, |editor, cx| {
                editor.focus(window, cx);
                editor.set_selected_range(13..13, cx);
            });
            editor
        });
        let original = "int old = 0;\n";
        let draft = "int old = 0;\n// 😀draft";
        let formatted = "int formatted = 0;\n// 😀note";
        visual.simulate_input("// 😀draft");
        assert!(visual.update(|_, cx| editor.read(cx).text() == draft));
        visual.simulate_keystrokes("alt-shift-f");
        wait_for_formatting(visual, &workspace)?;
        assert_eq!(
            fs::read_to_string(root.join("format-input"))
                .context("The formatter received no request")?,
            draft
        );
        assert!(visual.update(|_, cx| editor.read(cx).text() == formatted));
        assert_eq!(fs::read_to_string(root.join("main.cpp"))?, original);
        assert_eq!(fs::read_to_string(root.join("other.cpp"))?, original);
        assert!(!root.join("format-saved").exists());
        assert!(visual.update(|_, cx| {
            !workspace
                .read(cx)
                .files
                .contains_key(Path::new("other.cpp"))
        }));

        visual.update(|window, cx| window.dispatch_action(Box::new(Undo), cx));
        assert!(visual.update(|_, cx| editor.read(cx).text() == draft));
        visual.update(|window, cx| window.dispatch_action(Box::new(Redo), cx));
        assert!(visual.update(|_, cx| editor.read(cx).text() == formatted));
        visual.update(|window, cx| window.dispatch_action(Box::new(Undo), cx));

        let control = root.join("format-result");
        let revision = visual.update(|_, cx| editor.read(cx).document_revision());
        for mode in ["overlap", "invalid", "empty", "same", "error"] {
            fs::write(&control, mode)?;
            fs::remove_file(root.join("format-input"))?;
            visual.simulate_keystrokes("alt-shift-f");
            wait_for_formatting(visual, &workspace)?;
            assert_eq!(fs::read_to_string(root.join("format-input"))?, draft);
            assert!(visual.update(|_, cx| {
                editor.read(cx).text() == draft && editor.read(cx).document_revision() == revision
            }));
        }

        fs::write(&control, "delay")?;
        visual.simulate_keystrokes("alt-shift-f");
        pump(visual, "held formatting request", |_| {
            root.join("format-held").exists()
        })?;
        visual.update(|_, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_selected_range(draft.len()..draft.len(), cx)
            });
        });
        visual.simulate_input(" after");
        fs::write(root.join("format-release"), "")?;
        wait_for_formatting(visual, &workspace)?;
        let later_draft = format!("{draft} after");
        assert!(visual.update(|_, cx| editor.read(cx).text() == later_draft.as_str()));
        assert_eq!(fs::read_to_string(root.join("main.cpp"))?, original);
        assert!(!root.join("format-saved").exists());

        if dynamic {
            let hold = |visual: &mut VisualTestContext| -> Result<()> {
                fs::remove_file(root.join("format-held"))?;
                fs::remove_file(root.join("format-release"))?;
                fs::write(&control, "delay")?;
                visual.simulate_keystrokes("alt-shift-f");
                pump(visual, "held formatting request", |_| {
                    root.join("format-held").exists()
                })
            };
            hold(visual)?;
            fs::write(&control, "")?;
            visual.simulate_keystrokes("alt-shift-f");
            fs::write(root.join("format-release"), "")?;
            wait_for_formatting(visual, &workspace)?;
            let later_formatted = format!("{formatted} after");
            assert!(visual.update(|_, cx| editor.read(cx).text() == later_formatted.as_str()));
            visual.update(|window, cx| window.dispatch_action(Box::new(Undo), cx));

            hold(visual)?;
            visual.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.switch_view(WorkspaceView::Git, window, cx);
                    workspace.switch_view(WorkspaceView::Editor, window, cx);
                });
            });
            fs::write(root.join("format-release"), "")?;
            wait_for_formatting(visual, &workspace)?;
            assert!(visual.update(|_, cx| editor.read(cx).text() == later_draft.as_str()));

            hold(visual)?;
            visual.update(|_, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace
                        .files
                        .get_mut(Path::new("main.cpp"))
                        .unwrap()
                        .document
                        .conflict = true;
                    cx.notify();
                });
            });
            fs::write(root.join("format-release"), "")?;
            wait_for_formatting(visual, &workspace)?;
            assert!(visual.update(|_, cx| editor.read(cx).text() == later_draft.as_str()));
            visual.update(|_, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace
                        .files
                        .get_mut(Path::new("main.cpp"))
                        .unwrap()
                        .document
                        .conflict = false;
                    cx.notify();
                });
            });
            pump(visual, "writable editor", |visual| {
                visual.update(|window, cx| {
                    let editor = editor.read(cx);
                    editor.is_editable() && editor.focus_handle(cx).is_focused(window)
                })
            })?;
            visual.simulate_input(" // unregister");
            pump(visual, "formatter unregistration", |_| {
                root.join("unregister-format").exists()
            })?;
            fs::remove_file(root.join("format-input"))?;
            visual.simulate_keystrokes("alt-shift-f");
            wait_for_formatting(visual, &workspace)?;
            assert!(!root.join("format-input").exists());
        }
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn formats_only_the_active_draft_with_atomic_undo() -> Result<()> {
        formatting_workflow(None)
    }

    #[::core::prelude::v1::test]
    fn formatting_undo_and_redo_keep_the_selected_line_in_view() -> Result<()> {
        let mut app = test_app();
        let tail = (0..160)
            .map(|line| format!("// unchanged {line}\n"))
            .collect::<String>();
        let original = format!("int old = 0;\n// 😀draft\n{tail}");
        let formatted = format!("int formatted = 0;\n// 😀note\n{tail}");
        let (_fixture, workspace, visual) = open_fixture(&mut app, None, |root| {
            fs::write(root.join("format-mode"), "static")?;
            fs::write(root.join("main.cpp"), &original)?;
            Ok(())
        })?;
        pump(visual, "formatter session", |visual| {
            visual.update(|_, cx| {
                workspace
                    .read(cx)
                    .active_file()
                    .and_then(|file| file.language_server.as_ref())
                    .is_some_and(|server| server.document.action_capabilities().list)
            })
        })?;
        let selection = |text: &str| {
            let start = text.find("unchanged 80").unwrap();
            start..start + "unchanged".len()
        };
        let editor = visual.update(|window, cx| {
            let editor = workspace.read(cx).active_file().unwrap().editor.clone();
            editor.update(cx, |editor, cx| {
                editor.focus(window, cx);
                editor.set_selected_range_with_direction(selection(&original), true, cx);
                let top = editor.line_height().unwrap() * 65.;
                editor.set_scroll_offset(point(px(0.), -top), cx);
            });
            editor
        });
        let observe = |visual: &mut VisualTestContext, text: &str| {
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                let editor = editor.read(cx);
                let range = selection(text);
                assert_eq!(editor.text(), text);
                assert_eq!(editor.selected_range(), range);
                assert!(editor.is_selection_reversed());
                assert!(editor.focus_handle(cx).is_focused(window));
                let bounds = editor
                    .range_to_bounds(&range)
                    .expect("selection was laid out");
                let viewport = editor.input_bounds();
                assert!(bounds.top() >= viewport.top() && bounds.bottom() <= viewport.bottom());
                editor.projected_top_line()
            })
        };
        let top = observe(visual, &original);
        assert!(top > 50);
        visual.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.format_document(&FormatDocument, window, cx);
            });
        });
        wait_for_formatting(visual, &workspace)?;
        assert_eq!(observe(visual, &formatted), top);
        visual.update(|window, cx| window.dispatch_action(Box::new(Undo), cx));
        assert_eq!(observe(visual, &original), top);
        visual.update(|window, cx| window.dispatch_action(Box::new(Redo), cx));
        assert_eq!(observe(visual, &formatted), top);
        Ok(())
    }

    #[::core::prelude::v1::test]
    #[ignore = "requires a disposable SSH host with a fixture clangd command"]
    fn formats_only_the_active_draft_over_ssh() -> Result<()> {
        formatting_workflow(Some(&std::env::var("STROLL_TEST_LSP_SSH_AUTHORITY")?))
    }
}
