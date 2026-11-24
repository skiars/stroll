use super::*;
use gpui_component::input::{BufferPoint, RopeExt as _};
use std::cmp::Ordering;

const SCROLLBAR_EDGE: Pixels = px(18.);
// Wheel interpolation can start after the first scheduled frame.
const SETTLED_FRAMES: u8 = 4;

#[derive(Default)]
pub(super) struct ScrollFollow {
    active: Option<SourceScroll>,
    frame_pending: bool,
    dragging_source: bool,
    source_contact: bool,
    ignore_source_contact: bool,
    realign: bool,
}

struct SourceScroll {
    editor: EntityId,
    revision: u64,
    viewport: Size<Pixels>,
    line_height: Option<Pixels>,
    y: Pixels,
    direction: Ordering,
    reversing_to: Option<Ordering>,
    last_anchor: Option<(EntityId, usize)>,
    navigation: Option<WeakEntity<docs::DocReader>>,
    sample_pending: bool,
    still_frames: u8,
}

impl SourceScroll {
    fn cancel_navigation(&mut self, cx: &mut App) {
        if let Some(reader) = self.navigation.take() {
            _ = reader.update(cx, |reader, cx| reader.cancel_editor_scroll_navigation(cx));
        }
    }
}

fn source_block_progress(
    editor: &EditorState,
    span: (usize, usize),
    top_offset: usize,
    inset: Pixels,
) -> Option<f32> {
    let text = editor.text();
    let first = editor.projected_line_bounds(text.offset_to_point(span.0).row)?;
    let last = editor.projected_line_bounds(text.offset_to_point(span.1 - 1).row)?;
    let position = text.offset_to_point(top_offset);
    let line = editor.projected_line_bounds(position.row)?;
    let map = editor.display_map();
    let rows = map.buffer_line_to_display_row_range(position.row)?;
    let row = map
        .buffer_pos_to_display_pos(BufferPoint::new(position.row, position.column))
        .row;
    let height = editor.line_height()?;
    let span_height = last.bottom() - first.top();
    // Inline editor decorations can give a row nonstandard geometry.
    if !rows.contains(&row)
        || (line.size.height - height * rows.len() as f32).abs() > px(0.5)
        || span_height <= px(0.)
        || span_height > editor.input_bounds().size.height
    {
        return None;
    }
    let top = line.top() + height * (row - rows.start) as f32 + inset;
    Some(((top - first.top()) / span_height).clamp(0., 1.))
}

impl Workspace {
    pub(super) fn markdown_scroll_input(source: bool, cx: &Context<Self>) -> impl IntoElement {
        let workspace = cx.weak_entity();
        canvas(
            |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
            move |_, hitbox, window, _| {
                let wheel = workspace.clone();
                let scroll_hitbox = hitbox.id;
                window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
                    if phase.capture() && scroll_hitbox.should_handle_scroll(window) {
                        _ = wheel.update(cx, |this, cx| {
                            this.markdown_scroll_wheel(source, event, window, cx);
                        });
                    }
                });
                let drag = workspace.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    if phase.capture()
                        && event.button == MouseButton::Left
                        && hitbox.bounds.right() - event.position.x <= SCROLLBAR_EDGE
                        && hitbox.is_hovered_at(event.position, window)
                    {
                        _ = drag.update(cx, |this, cx| {
                            if source {
                                this.editor_previews.scroll_follow.dragging_source = true;
                                this.begin_markdown_scroll_follow(window, cx);
                            } else {
                                this.pause_markdown_scroll_follow(cx);
                            }
                        });
                    }
                });
                if source {
                    let drag = workspace.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                        if phase.capture() && event.dragging() {
                            _ = drag.update(cx, |this, cx| {
                                if this.editor_previews.scroll_follow.dragging_source {
                                    this.begin_markdown_scroll_follow(window, cx);
                                }
                            });
                        }
                    });
                    let drag = workspace.clone();
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                        if phase.capture() && event.button == MouseButton::Left {
                            _ = drag.update(cx, |this, _| {
                                this.editor_previews.scroll_follow.dragging_source = false;
                            });
                        }
                    });
                }
            },
        )
        .absolute()
        .inset_0()
    }

    fn markdown_scroll_wheel(
        &mut self,
        source: bool,
        event: &ScrollEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let follow = &mut self.editor_previews.scroll_follow;
        if source && !event.is_wheel() {
            match event.touch_phase {
                TouchPhase::Started => {
                    follow.source_contact = true;
                    follow.ignore_source_contact = false;
                }
                TouchPhase::Cancelled => {
                    self.reset_markdown_scroll_follow(cx);
                    self.editor_previews.scroll_follow.source_contact = false;
                    return;
                }
                TouchPhase::Ended => return,
                TouchPhase::Moved if follow.ignore_source_contact => return,
                TouchPhase::Moved => {
                    follow.source_contact |= event.source != ScrollSource::Pointer;
                }
            }
        }
        let delta = event.delta.pixel_delta(px(1.));
        if event.modifiers.shift || delta.y.abs() <= delta.x.abs() {
            return;
        }
        if source {
            let direction = if delta.y < px(0.) {
                Ordering::Greater
            } else {
                Ordering::Less
            };
            if let Some(active) = &mut self.editor_previews.scroll_follow.active
                && active.direction != Ordering::Equal
                && active.reversing_to.unwrap_or(active.direction) != direction
            {
                active.cancel_navigation(cx);
                active.sample_pending = false;
                active.reversing_to = Some(direction);
            }
            self.begin_markdown_scroll_follow(window, cx);
        } else {
            self.pause_markdown_scroll_follow(cx);
        }
    }

    pub(super) fn begin_markdown_scroll_follow(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view != WorkspaceView::Editor
            || self.active_editor_preview_mode() != EditorPreviewMode::Split
            || self.terminal_maximized()
            || self.locator.open
            || self.repository_overlay_open()
            || self.confirming_close
        {
            return;
        }
        let Some(file) = self.active_file() else {
            return;
        };
        let editor_id = file.editor.entity_id();
        let editor = file.editor.read(cx);
        let revision = editor.document_revision();
        let viewport = editor.input_bounds().size;
        let line_height = editor.line_height();
        let follow = &mut self.editor_previews.scroll_follow;
        if let Some(active) = &mut follow.active
            && active.editor == editor_id
            && active.revision == revision
            && active.viewport == viewport
            && active.line_height == line_height
        {
            active.still_frames = 0;
        } else {
            let y = editor.scroll_offset().y;
            if let Some(mut previous) = follow.active.take() {
                previous.cancel_navigation(cx);
            }
            follow.active = Some(SourceScroll {
                editor: editor_id,
                revision,
                viewport,
                line_height,
                y,
                direction: Ordering::Equal,
                reversing_to: None,
                last_anchor: None,
                navigation: None,
                sample_pending: false,
                still_frames: 0,
            });
        }
        self.queue_markdown_scroll_frame(window, cx);
    }

    pub(in crate::workspace) fn pause_markdown_scroll_follow(&mut self, cx: &mut Context<Self>) {
        self.reset_markdown_scroll_follow(cx);
        self.editor_previews.scroll_follow.realign = true;
        if let Some(reader) = self.active_markdown_preview() {
            reader.update(cx, |reader, cx| reader.cancel_editor_scroll_follow(cx));
        }
    }

    pub(in crate::workspace) fn reset_markdown_scroll_follow(&mut self, cx: &mut App) {
        let follow = &mut self.editor_previews.scroll_follow;
        follow.ignore_source_contact |= follow.source_contact;
        follow.dragging_source = false;
        follow.realign = false;
        if let Some(mut active) = follow.active.take() {
            active.cancel_navigation(cx);
        }
    }

    fn queue_markdown_scroll_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let follow = &mut self.editor_previews.scroll_follow;
        if !std::mem::replace(&mut follow.frame_pending, true) {
            cx.on_next_frame(window, |this, window, cx| {
                this.follow_markdown_scroll_frame(window, cx)
            });
        }
    }

    fn follow_markdown_scroll_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let follow = &mut self.editor_previews.scroll_follow;
        follow.frame_pending = false;
        let Some(mut active) = follow.active.take() else {
            return;
        };
        if self.view != WorkspaceView::Editor
            || self.active_editor_preview_mode() != EditorPreviewMode::Split
            || self.terminal_maximized()
            || self.editor_previews.split_drag.is_some()
        {
            active.cancel_navigation(cx);
            return;
        }
        let Some(file) = self
            .active_file()
            .filter(|file| file.editor.entity_id() == active.editor)
        else {
            active.cancel_navigation(cx);
            return;
        };
        let editor = file.editor.clone();
        let path = file.path.clone();
        let (y, top) = {
            let editor = editor.read(cx);
            if active.revision != editor.document_revision()
                || active.viewport != editor.input_bounds().size
                || active.line_height != editor.line_height()
            {
                active.cancel_navigation(cx);
                return;
            }
            (
                editor.scroll_offset().y,
                editor
                    .viewport_anchor()
                    .map(|(top, inset)| (editor.text().position_to_offset(&top), inset)),
            )
        };
        if top.is_none() {
            active.cancel_navigation(cx);
            return;
        }
        if y == active.y {
            active.still_frames = active.still_frames.saturating_add(1);
        } else {
            let direction = if y < active.y {
                Ordering::Greater
            } else {
                Ordering::Less
            };
            if active.direction != Ordering::Equal && active.direction != direction {
                active.cancel_navigation(cx);
            }
            active.direction = direction;
            active.still_frames = 0;
            active.y = y;
            if active
                .reversing_to
                .is_none_or(|expected| expected == direction)
            {
                active.reversing_to = None;
                active.sample_pending = true;
            }
        }

        if self
            .editor_previews
            .markdown
            .get(&path)
            .is_some_and(|preview| preview.reader_revision == Some(active.revision))
            && let Some(reader) = self.active_markdown_preview().cloned()
        {
            if active
                .navigation
                .as_ref()
                .is_some_and(|old| old.entity_id() != reader.entity_id())
            {
                active.cancel_navigation(cx);
            }
            let navigating = reader.read(cx).editor_scroll_is_navigating(cx);
            if !navigating {
                active.navigation = None;
            }
            if active.sample_pending
                && let Some((offset, inset)) = top
            {
                self.apply_markdown_scroll_sample(
                    &mut active,
                    &editor,
                    &reader,
                    offset,
                    inset,
                    navigating,
                    cx,
                );
            }
        } else {
            active.cancel_navigation(cx);
        }
        if active.navigation.is_some() || active.still_frames < SETTLED_FRAMES {
            self.editor_previews.scroll_follow.active = Some(active);
            self.queue_markdown_scroll_frame(window, cx);
        }
    }

    fn apply_markdown_scroll_sample(
        &mut self,
        active: &mut SourceScroll,
        editor: &Entity<EditorState>,
        reader: &Entity<docs::DocReader>,
        offset: usize,
        inset: Pixels,
        navigating: bool,
        cx: &mut Context<Self>,
    ) {
        let follow = &mut self.editor_previews.scroll_follow;
        let Some(anchor) = reader.read(cx).editor_scroll_anchor(offset, cx) else {
            // A gap supplies no new target. Let an already confirmed reveal
            // finish unless the source reverses or the reader takes over.
            active.sample_pending = false;
            follow.realign = true;
            return;
        };
        if navigating && active.navigation.is_none() {
            return;
        }
        let animate = cx.smooth_scrolling() && !cx.reduce_motion();
        let id = reader.entity_id();
        if animate
            && !follow.realign
            && let Some(fraction) =
                source_block_progress(editor.read(cx), anchor.span, offset, inset)
            && let Some(delta) = reader.read(cx).editor_scroll_delta(anchor, fraction, cx)
        {
            let moves_with_source = delta.partial_cmp(&px(0.)) == Some(active.direction);
            if navigating {
                if moves_with_source
                    && reader.update(cx, |reader, cx| {
                        reader.retarget_editor_scroll_delta(delta, cx)
                    })
                {
                    active.sample_pending = false;
                    active.last_anchor = Some((id, anchor.source));
                    return;
                }
            } else {
                active.sample_pending = false;
                active.last_anchor = Some((id, anchor.source));
                if moves_with_source {
                    reader.update(cx, |reader, cx| reader.apply_editor_scroll_delta(delta, cx));
                }
                return;
            }
        }
        if follow.realign
            || active.last_anchor.is_none_or(|(previous, last)| {
                id != previous || anchor.source.cmp(&last) == active.direction
            })
        {
            follow.realign = false;
            active.last_anchor = Some((id, anchor.source));
            active.sample_pending = false;
            reader.update(cx, |reader, cx| {
                reader.apply_editor_scroll_anchor(anchor, animate, cx)
            });
            if animate && reader.read(cx).editor_scroll_is_navigating(cx) {
                active.navigation = Some(reader.downgrade());
            }
        } else if !navigating {
            active.sample_pending = false;
        }
    }
}
