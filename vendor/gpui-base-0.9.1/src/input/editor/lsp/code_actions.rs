use anyhow::Result;
use gpui::{App, Bounds, Context, Entity, Pixels, SharedString, Task, Window, point, px, size};
use lsp_types::CodeAction;
use std::{ops::Range, time::Duration};

use crate::input::{
    EditorMode, EditorState, InputBaseState, RenameSymbol, RopeExt, ToggleCodeActions,
};

pub trait CodeActionProvider {
    fn id(&self) -> SharedString;

    fn supports_code_actions(&self) -> bool {
        true
    }

    fn supports_rename(&self) -> bool {
        false
    }

    /// Enables the debounced current-line query used for the inline lightbulb.
    fn probe_current_line(&self) -> bool {
        false
    }

    fn code_actions(
        &self,
        state: Entity<EditorState>,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<CodeAction>>>;

    fn probe_code_actions(
        &self,
        state: Entity<EditorState>,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<CodeAction>>> {
        self.code_actions(state, range, window, cx)
    }

    fn supports_preview(&self) -> bool {
        false
    }

    fn preview_code_action(
        &self,
        _: Entity<EditorState>,
        _: CodeAction,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<String>> {
        Task::ready(Ok(String::new()))
    }

    fn perform_code_action(
        &self,
        state: Entity<EditorState>,
        action: CodeAction,
        push_to_history: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>>;

    fn rename_symbol(&self, _: Entity<EditorState>, _: &mut Window, _: &mut App) {}
}

#[derive(Clone, Debug)]
pub struct CodeActionItem {
    pub provider_id: SharedString,
    pub action: CodeAction,
}

#[derive(Default)]
pub(crate) struct CodeActionProbe {
    key: Option<(u64, usize)>,
    available: Option<(u64, usize)>,
    task: Option<Task<()>>,
}

impl InputBaseState<EditorMode> {
    pub(crate) fn on_action_toggle_code_actions(
        &mut self,
        _: &ToggleCodeActions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_code_actions(self.selected_range(), window, cx);
    }

    pub(crate) fn on_action_rename_symbol(
        &mut self,
        _: &RenameSymbol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_editable()
            && self.ime_marked_range.is_none()
            && let Some(provider) = self
                .extras
                .lsp
                .code_action_providers
                .iter()
                .find(|provider| provider.supports_rename())
                .cloned()
        {
            self.dismiss_lsp_overlays(cx);
            provider.rename_symbol(cx.entity(), window, cx);
        }
    }

    pub fn can_open_code_actions(&self) -> bool {
        self.is_editable()
            && self
                .extras
                .lsp
                .code_action_providers
                .iter()
                .any(|provider| provider.supports_code_actions())
    }

    pub fn open_code_actions(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_open_code_actions() || self.ime_marked_range.is_some() {
            return;
        }
        let providers = self.extras.lsp.code_action_providers.clone();
        let revision = self.document_revision;
        let selection = self.selected_range();
        self.extras.context_menu_content.completion.open = false;
        let hover = self
            .hover_popover()
            .filter(|hover| {
                hover.symbol_range.start <= range.end && range.start <= hover.symbol_range.end
            })
            .cloned();
        if hover.is_none() {
            self.clear_hover_state(cx);
        }
        let menu = &mut self.extras.context_menu_content.code_action;
        menu.open = true;
        menu.range = range.clone();
        menu.hover = hover;
        menu.items.clear();
        menu.error = None;
        menu.preview = None;
        menu.loading = true;
        menu.bump();
        cx.notify();

        self.extras.context_menu_task = cx.spawn_in(window, async move |editor, cx| {
            let results = cx.update(|window, cx| {
                let Some(state) = editor.upgrade() else {
                    return Vec::new();
                };
                providers
                    .into_iter()
                    .filter(|provider| provider.supports_code_actions())
                    .map(|provider| {
                        let request =
                            provider.code_actions(state.clone(), range.clone(), window, cx);
                        (provider.id(), request)
                    })
                    .collect::<Vec<_>>()
            })?;
            let mut items = Vec::new();
            let mut error = None;
            for (provider_id, response) in results {
                match response.await {
                    Ok(actions) => items.extend(actions.into_iter().map(|action| CodeActionItem {
                        provider_id: provider_id.clone(),
                        action,
                    })),
                    Err(reason) => error = Some(format!("{reason:#}").into()),
                }
            }
            editor.update_in(cx, |editor, window, cx| {
                if !editor.is_editable()
                    || !editor.focus_handle.is_focused(window)
                    || editor.document_revision != revision
                    || editor.selected_range() != selection
                {
                    editor.dismiss_code_action_overlay(cx);
                    return;
                }
                let menu = &mut editor.extras.context_menu_content.code_action;
                if menu.open {
                    menu.items = items;
                    menu.error = error;
                    menu.loading = false;
                    menu.bump();
                    cx.notify();
                }
            })?;
            Ok(())
        });
    }

    pub(super) fn update_action_hover_documentation(&mut self) {
        let menu = &mut self.extras.context_menu_content.code_action;
        if menu.open
            && let Some(snapshot) = menu.hover.as_mut()
            && let Some(hover) = self.extras.hover_popover.as_ref()
            && snapshot.documentation_target.is_some()
            && snapshot.documentation_target == hover.documentation_target
        {
            snapshot.hover = hover.hover.clone();
            snapshot.documentation = hover.documentation.clone();
        }
    }

    pub fn code_action_can_preview(&self, item: &CodeActionItem) -> bool {
        item.action.disabled.is_none()
            && self
                .extras
                .lsp
                .code_action_providers
                .iter()
                .any(|provider| provider.id() == item.provider_id && provider.supports_preview())
    }

    pub fn perform_code_action(
        &mut self,
        item: &CodeActionItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_code_action(item, false, window, cx);
    }

    pub fn preview_code_action(
        &mut self,
        item: &CodeActionItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_code_action(item, true, window, cx);
    }

    fn run_code_action(
        &mut self,
        item: &CodeActionItem,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = &self.extras.context_menu_content.code_action;
        if !menu.open || menu.loading || !self.is_editable() || item.action.disabled.is_some() {
            return;
        }
        let Some(provider) = self
            .extras
            .lsp
            .code_action_providers
            .iter()
            .find(|provider| {
                provider.id() == item.provider_id && (!preview || provider.supports_preview())
            })
            .cloned()
        else {
            return;
        };
        let menu = &mut self.extras.context_menu_content.code_action;
        if preview
            && menu.preview.as_ref().is_some_and(|(action, _)| {
                action.provider_id == item.provider_id && action.action == item.action
            })
        {
            menu.preview = None;
            menu.bump();
            cx.notify();
            return;
        }
        let item = item.clone();
        menu.loading = true;
        menu.error = None;
        menu.preview = None;
        menu.bump();
        cx.notify();
        self.extras.context_menu_task = cx.spawn_in(window, async move |editor, cx| {
            let result = if preview {
                let request = cx.update(|window, cx| {
                    editor.upgrade().map(|editor| {
                        provider.preview_code_action(editor, item.action.clone(), window, cx)
                    })
                })?;
                let Some(request) = request else {
                    return Ok(());
                };
                request.await.map(Some)
            } else {
                let request = cx.update(|window, cx| {
                    editor.upgrade().map(|editor| {
                        provider.perform_code_action(editor, item.action.clone(), true, window, cx)
                    })
                })?;
                let Some(request) = request else {
                    return Ok(());
                };
                request.await.map(|_| None)
            };
            editor.update(cx, |editor, cx| {
                let menu = &mut editor.extras.context_menu_content.code_action;
                if !menu.open {
                    return;
                }
                menu.loading = false;
                match result {
                    Ok(Some(diff)) => menu.preview = Some((item, diff.into())),
                    Ok(None) => menu.open = false,
                    Err(error) => menu.error = Some(format!("{error:#}").into()),
                }
                menu.bump();
                cx.notify();
            })?;
            Ok(())
        });
    }

    pub fn invalidate_code_action_indicator(&mut self) {
        self.extras.lsp.code_action_probe.key = None;
        self.extras.lsp.code_action_probe.task = None;
    }

    pub fn refresh_code_action_indicator(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_open_code_actions()
            || !self.focus_handle.is_focused(window)
            || self.ime_marked_range.is_some()
        {
            self.extras.lsp.code_action_probe = CodeActionProbe::default();
            self.dismiss_code_action_overlay(cx);
            return;
        }
        if self.extras.context_menu_content.code_action.open {
            return;
        }
        let row = self.text.offset_to_point(self.cursor()).row;
        let key = (self.document_revision, row);
        if self.extras.lsp.code_action_probe.key == Some(key) {
            return;
        }
        let providers = self
            .extras
            .lsp
            .code_action_providers
            .iter()
            .filter(|provider| provider.supports_code_actions() && provider.probe_current_line())
            .cloned()
            .collect::<Vec<_>>();
        let available = self
            .extras
            .lsp
            .code_action_probe
            .available
            .filter(|previous| *previous == key);
        self.extras.lsp.code_action_probe = CodeActionProbe {
            key: Some(key),
            available,
            task: None,
        };
        if providers.is_empty() {
            return;
        }
        let range = self.code_action_line_range();
        self.extras.lsp.code_action_probe.task =
            Some(cx.spawn_in(window, async move |editor, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(180))
                    .await;
                let Ok(requests) = cx.update(|window, cx| {
                    let Some(state) = editor.upgrade() else {
                        return Vec::new();
                    };
                    if state.read(cx).extras.lsp.code_action_probe.key != Some(key) {
                        return Vec::new();
                    }
                    providers
                        .into_iter()
                        .map(|provider| {
                            provider.probe_code_actions(state.clone(), range.clone(), window, cx)
                        })
                        .collect::<Vec<_>>()
                }) else {
                    return;
                };
                let mut available = false;
                for request in requests {
                    if request
                        .await
                        .is_ok_and(|items| items.iter().any(|item| item.disabled.is_none()))
                    {
                        available = true;
                        break;
                    }
                }
                _ = editor.update(cx, |editor, cx| {
                    let probe = &mut editor.extras.lsp.code_action_probe;
                    if probe.key == Some(key) && editor.document_revision == key.0 {
                        let available = available.then_some(key);
                        if probe.available != available {
                            probe.available = available;
                            cx.notify();
                        }
                    }
                });
            }));
    }

    pub fn code_action_line_range(&self) -> Range<usize> {
        let row = self.text.offset_to_point(self.cursor()).row;
        let start = self.text.line_start_offset(row);
        let line = self.text.slice_line(row);
        let end = line.len() - usize::from(line.len() > 0 && line.byte(line.len() - 1) == b'\r');
        start..start + end
    }

    /// Available space and text baseline after the cursor's final wrapped row.
    /// Read after editor prepaint for current-frame window coordinates.
    pub fn cursor_line_end_geometry(&self) -> Option<(Bounds<Pixels>, Pixels)> {
        if !self.selected_range().is_empty() || self.is_composing() {
            return None;
        }
        let range = self.code_action_line_range();
        let row = self.text.offset_to_point(range.start).row;
        if self.display_map.is_folded_at(row) || self.extras.inline_completion.item.is_some() {
            return None;
        }
        let layout = self.last_layout.as_ref()?;
        let index = layout.visible_buffer_lines.binary_search(&row).ok()?;
        let line = &layout.lines[index];
        let end = line.position_for_index(range.len(), layout, true)?;
        let baseline = line.wrapped_lines.last()?.baseline_offset(layout.line_height);
        let viewport = self.input_bounds();
        let origin = self.last_bounds?.origin
            + point(layout.line_number_width, layout.line_top(index));
        let start = origin + end;
        if start.x < viewport.left() + layout.line_number_width
            || start.y < viewport.top()
            || start.y + layout.line_height > viewport.bottom()
        {
            return None;
        }
        let width = viewport.right() - start.x;
        (width > px(0.)).then(|| {
            (Bounds::new(start, gpui::size(width, layout.line_height)), start.y + baseline)
        })
    }

    pub fn code_action_indicator_bounds(&self, width: Pixels) -> Option<Bounds<Pixels>> {
        let range = self.code_action_line_range();
        let row = self.text.offset_to_point(range.start).row;
        let probe = &self.extras.lsp.code_action_probe;
        if probe.available != Some((self.document_revision, row))
            || !self.can_open_code_actions()
            || self.extras.context_menu_content.completion.open
            || self.display_map.is_folded_at(row)
        {
            return None;
        }
        let viewport = self.input_bounds();
        let layout = self.last_layout.as_ref()?;
        let visible_row = layout.visible_buffer_lines.binary_search(&row).ok()?;
        let line = &layout.lines[visible_row];
        let mut origin = self.last_bounds?.origin;
        origin.x += layout.line_number_width;
        origin.y += layout.line_top(visible_row);

        let caret = line.position_for_index(
            self.cursor().saturating_sub(range.start),
            layout,
            self.cursor_line_end_affinity,
        )?;
        let row_top = origin.y + caret.y;
        if row_top < viewport.top() || row_top + layout.line_height > viewport.bottom() {
            return None;
        }
        let whitespace = self
            .text
            .slice(range.clone())
            .chars()
            .take_while(|ch| matches!(ch, ' ' | '\t'))
            .map(char::len_utf8)
            .sum::<usize>();
        let start = line.position_for_index(0, layout, false)?;
        let indent = line.position_for_index(whitespace, layout, false)?;
        let end = line.position_for_index(range.len(), layout, true)?;
        let indent_slot =
            (start.y == caret.y && indent.y == caret.y && indent.x - start.x >= width + px(4.))
                .then_some(origin.x + indent.x - width - px(2.));
        let end_slot = (end.y == caret.y && self.extras.inline_completion.item.is_none())
            .then_some(origin.x + end.x + px(6.));
        let left = viewport.left() + layout.line_number_width;
        let right = viewport.right() - px(12.);
        let x = indent_slot
            .into_iter()
            .chain(end_slot)
            .find(|x| *x >= left && *x + width <= right)?;
        if viewport.size.height < width {
            return None;
        }
        let y = (row_top + (layout.line_height - width) / 2.)
            .clamp(viewport.top(), viewport.bottom() - width);
        Some(Bounds::new(point(x, y), size(width, width)))
    }
}
