use std::collections::HashMap;

use gpui::*;
use gpui_component::ActiveTheme;

use crate::terminal::{Cell, Rgb};

use super::Session;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Appearance {
    pub(super) foreground: Rgb,
    pub(super) background: Rgb,
    pub(super) ansi: [Rgb; 16],
    pub(super) dark: bool,
}

impl Appearance {
    pub(super) fn new(dark: bool) -> Self {
        let (foreground, background, ansi) = if dark {
            (
                0xd9dee8,
                0x17191f,
                [
                    0x7e899b, 0xe68a8c, 0x9ac78d, 0xd7b978, 0x8fb2e2, 0xbd9fd7, 0x86c5cc, 0xc4cad5,
                    0x969fac, 0xf1a2a3, 0xb0d8a2, 0xe5cb92, 0xa5c6ef, 0xd1b4e6, 0x9dd8de, 0xe3e7ee,
                ],
            )
        } else {
            (
                0x303540,
                0xffffff,
                [
                    0x252a34, 0xb43b43, 0x28734a, 0x875f20, 0x3566b8, 0x8955a2, 0x227681, 0x596273,
                    0x676f7d, 0xc43943, 0x278047, 0x94651b, 0x386bc4, 0x9553af, 0x247985, 0x384150,
                ],
            )
        };
        Self {
            foreground: foreground.into(),
            background: background.into(),
            ansi: ansi.map(Rgb::from),
            dark,
        }
    }

    fn selection(self, focused: bool) -> (Rgb, Rgb) {
        match (self.dark, focused) {
            (true, true) => (0xe3e7ee.into(), 0x35465e.into()),
            (true, false) => (0xe3e7ee.into(), 0x303943.into()),
            (false, true) => (0x252a34.into(), 0xdce5f5.into()),
            (false, false) => (0x252a34.into(), 0xe8ebf0.into()),
        }
    }
}

impl Session {
    pub(super) fn paint_grid(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focused = window.is_window_active() && self.focus.is_focused(window);
        let handles_text = focused
            && matches!(
                self.status,
                super::Status::Starting | super::Status::Running
            );
        let ime_bounds = if handles_text {
            window.handle_input(
                &self.focus,
                ElementInputHandler::new(bounds, cx.entity()),
                cx,
            );
            self.ime_bounds()
        } else {
            None
        };
        if self.ime_anchor != ime_bounds {
            self.ime_anchor = ime_bounds;
            if ime_bounds.is_some() {
                window.invalidate_character_coordinates();
            }
        }
        let Some(appearance) = self.appearance else {
            return;
        };
        let geometry = self.geometry;
        if geometry.rows == 0 {
            return;
        }
        let frame = &self.frame;
        let (base, font_size) = Self::grid_font(window);
        let physical_scale = window.scale_factor();
        let font_id = window.text_system().resolve_font(&base);
        let ascent = window.text_system().ascent(font_id, font_size);
        let descent = window.text_system().descent(font_id, font_size).abs();
        let baseline = (geometry.cell.height + ascent - descent) / 2.;
        let blink_visible = (self.blink.elapsed().as_millis() / 600).is_multiple_of(2);
        let visible_cursor = frame.cursor.filter(|cursor| cursor.visible);
        let cursor = visible_cursor
            .filter(|cursor| !focused || !cursor.blinking || blink_visible || self.composing());
        if focused && visible_cursor.is_some_and(|cursor| cursor.blinking) {
            self.schedule_blink(cx);
        }
        if self.dragging_selection
            && self
                .emulator
                .as_ref()
                .is_some_and(|e| e.selection_autoscroll())
        {
            self.schedule_autoscroll(cx);
        }
        let frame = &self.frame;
        let hovered_link = self.hovered_link();
        let link_color = cx.theme().link;
        let (selection_fg, selection_bg) = appearance.selection(focused);
        if frame.background != appearance.background {
            window.paint_quad(fill(bounds, rgb(frame.background.hex())));
        }
        let mut contrast_cache = HashMap::new();
        let mut shaped_cells = HashMap::new();
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (y, row) in frame.rows.iter().enumerate() {
                let mut background_run: Option<(usize, usize, Rgb)> = None;
                for (x, (cell, text)) in row.iter().enumerate() {
                    if cell.width == 0 {
                        continue;
                    }
                    let cells = usize::from(cell.width);
                    let mut fg = cell.foreground;
                    let mut bg = cell.background;
                    if cell.flags & Cell::INVERSE != 0 {
                        (fg, bg) = (bg, fg);
                    }
                    if cell.selected {
                        (fg, bg) = (selection_fg, selection_bg);
                    }
                    let block_cursor = cursor.filter(|cursor| {
                        focused && cursor.x == x && cursor.y == y && cursor.style == 1
                    });
                    if let Some(cursor) = block_cursor {
                        (fg, bg) = (frame.background, cursor.color);
                    }
                    let width = geometry.cell.width * cells as f32;
                    let origin = geometry.origin(x, y);
                    let cell_bounds = Bounds::new(origin, size(width, geometry.cell.height));
                    let paint_bg = bg != frame.background;
                    if let Some((start, end, color)) = background_run
                        && (end != x || color != bg || !paint_bg)
                    {
                        let bounds = Bounds::new(
                            geometry.origin(start, y),
                            size(
                                geometry.cell.width * (end - start) as f32,
                                geometry.cell.height,
                            ),
                        );
                        window.paint_quad(fill(bounds, rgb(color.hex())));
                        background_run = None;
                    }
                    if paint_bg {
                        // Paint this cell before its glyph. Blank backgrounds can be combined.
                        if text.is_empty()
                            && cell.underline_style() == 0
                            && cell.flags & Cell::STRIKE == 0
                        {
                            let start = background_run.map_or(x, |(start, _, _)| start);
                            background_run = Some((start, x + cells, bg));
                            continue;
                        }
                        if let Some((start, end, color)) = background_run.take() {
                            let bounds = Bounds::new(
                                geometry.origin(start, y),
                                size(
                                    geometry.cell.width * (end - start) as f32,
                                    geometry.cell.height,
                                ),
                            );
                            window.paint_quad(fill(bounds, rgb(color.hex())));
                        }
                        window.paint_quad(fill(cell_bounds, rgb(bg.hex())));
                    }
                    if cell.flags & Cell::INVISIBLE != 0
                        || (text.is_empty()
                            && cell.underline_style() == 0
                            && cell.flags & Cell::STRIKE == 0)
                    {
                        continue;
                    }
                    let text = if text.is_empty() { " " } else { text.as_str() };
                    let faint =
                        cell.flags & Cell::FAINT != 0 && !cell.selected && block_cursor.is_none();
                    let graphic = text
                        .chars()
                        .all(|ch| ch.is_whitespace() || ('\u{2500}'..='\u{259f}').contains(&ch));
                    if !graphic && block_cursor.is_none() {
                        fg = *contrast_cache
                            .entry((fg, bg, faint))
                            .or_insert_with(|| readable(fg, bg, faint));
                    }
                    let line = shaped_cells
                        .entry((text, *cell, fg, faint))
                        .or_insert_with(|| {
                            let mut face = base.clone();
                            face.weight =
                                match (cell.flags & Cell::BOLD != 0, faint && physical_scale >= 2.)
                                {
                                    (true, true) => FontWeight::SEMIBOLD,
                                    (true, false) => FontWeight::BOLD,
                                    (false, true) => FontWeight::LIGHT,
                                    (false, false) => FontWeight::NORMAL,
                                };
                            if cell.flags & Cell::ITALIC != 0 {
                                face.style = FontStyle::Italic;
                            }
                            let fg = rgb(fg.hex()).into();
                            let underline = cell.underline_style();
                            let run = TextRun {
                                len: text.len(),
                                font: face,
                                color: fg,
                                background_color: None,
                                underline: (underline != 0).then_some(UnderlineStyle {
                                    thickness: px(1.),
                                    color: Some(if cell.has_underline {
                                        rgb(cell.underline.hex()).into()
                                    } else {
                                        fg
                                    }),
                                    wavy: underline == 3,
                                    pattern: Default::default(),
                                }),
                                strikethrough: (cell.flags & Cell::STRIKE != 0).then_some(
                                    StrikethroughStyle {
                                        thickness: px(1.),
                                        color: Some(fg),
                                    },
                                ),
                            };
                            window.text_system().shape_line(
                                SharedString::from(text.to_owned()),
                                font_size,
                                &[run],
                                None,
                            )
                        });
                    // Keep every glyph on the same grid and baseline as neighboring cells,
                    // regardless of fallback fonts, adjacent text, and block cursor blinking.
                    let natural_baseline = (geometry.cell.height + line.ascent - line.descent) / 2.;
                    let draw_at = point(origin.x, origin.y + baseline - natural_baseline);
                    window.with_content_mask(
                        Some(ContentMask {
                            bounds: cell_bounds,
                        }),
                        |window| {
                            _ = line.paint(
                                draw_at,
                                geometry.cell.height,
                                TextAlign::Left,
                                None,
                                window,
                                cx,
                            );
                        },
                    );
                }
                if let Some((start, end, color)) = background_run {
                    let bounds = Bounds::new(
                        geometry.origin(start, y),
                        size(
                            geometry.cell.width * (end - start) as f32,
                            geometry.cell.height,
                        ),
                    );
                    window.paint_quad(fill(bounds, rgb(color.hex())));
                }
                if let Some(columns) = hovered_link.and_then(|link| link.columns(y, row.len())) {
                    let mut origin = geometry.origin(columns.start, y);
                    origin.y += (baseline + px(2.)).min(geometry.cell.height - px(1.));
                    window.paint_quad(fill(
                        Bounds::new(
                            origin,
                            size(geometry.cell.width * columns.len() as f32, px(1.)),
                        ),
                        link_color,
                    ));
                }
            }
            if let Some(cursor) = cursor {
                let origin = geometry.origin(cursor.x, cursor.y);
                let wide = frame
                    .rows
                    .get(cursor.y)
                    .and_then(|row| row.get(cursor.x))
                    .map_or(1., |(cell, _)| cell.width.max(1) as f32);
                let cursor_bounds = Bounds::new(
                    origin,
                    size(geometry.cell.width * wide, geometry.cell.height),
                );
                let color = rgb(cursor.color.hex());
                if !focused || cursor.style == 3 {
                    window.paint_quad(outline(cursor_bounds, color, BorderStyle::default()));
                } else if cursor.style == 0 {
                    window.paint_quad(fill(
                        Bounds::new(origin, size(px(1.), geometry.cell.height)),
                        color,
                    ));
                } else if cursor.style == 2 {
                    window.paint_quad(fill(
                        Bounds::new(
                            point(origin.x, cursor_bounds.bottom() - px(1.)),
                            size(cursor_bounds.size.width, px(1.)),
                        ),
                        color,
                    ));
                }
            }
            if let Some(anchor) = ime_bounds.filter(|_| self.composing()) {
                let text = SharedString::from(self.marked.clone());
                let run = TextRun {
                    len: text.len(),
                    font: base,
                    color: rgb(frame.foreground.hex()).into(),
                    background_color: Some(rgb(selection_bg.hex()).into()),
                    underline: Some(UnderlineStyle {
                        thickness: px(1.),
                        color: None,
                        wavy: false,
                        pattern: Default::default(),
                    }),
                    strikethrough: None,
                };
                let preedit = window
                    .text_system()
                    .shape_line(text, font_size, &[run], None);
                _ = preedit.paint(
                    anchor.origin,
                    geometry.cell.height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
        });
    }

    fn schedule_blink(&mut self, cx: &Context<Self>) {
        if self.blink_task.is_some() {
            return;
        }
        let delay = 600 - self.blink.elapsed().as_millis() % 600;
        self.blink_task = Some(cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(delay as u64))
                .await;
            _ = view.update(cx, |this, cx| {
                this.blink_task = None;
                cx.notify();
            });
        }));
    }

    fn schedule_autoscroll(&mut self, cx: &Context<Self>) {
        if self.selection_task.is_some() {
            return;
        }
        self.selection_task = Some(cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(35))
                .await;
            _ = view.update(cx, |this, cx| {
                this.selection_task = None;
                if this.dragging_selection {
                    this.select(
                        crate::terminal::SelectionAction::AutoScroll,
                        this.selection_pointer,
                        0,
                        cx,
                    );
                }
            });
        }));
    }
}

fn readable(fg: Rgb, bg: Rgb, faint: bool) -> Rgb {
    let start = if faint { 128 } else { 256 };
    for step in start..=256 {
        let color = mix(fg, bg, step);
        if contrast(color, bg) >= 4.5 {
            return color;
        }
    }
    let black = Rgb(0, 0, 0);
    let white = Rgb(255, 255, 255);
    let target = if contrast(black, bg) > contrast(white, bg) {
        black
    } else {
        white
    };
    for step in 1..=256 {
        let color = mix(target, fg, step);
        if contrast(color, bg) >= 4.5 {
            return color;
        }
    }
    target
}

fn mix(fg: Rgb, bg: Rgb, alpha: u32) -> Rgb {
    let channel =
        |a: u8, b: u8| ((u32::from(a) * alpha + u32::from(b) * (256 - alpha) + 128) / 256) as u8;
    Rgb(
        channel(fg.0, bg.0),
        channel(fg.1, bg.1),
        channel(fg.2, bg.2),
    )
}

fn contrast(a: Rgb, b: Rgb) -> f32 {
    let luminance = |rgb: Rgb| {
        let linear = |channel: u8| {
            let channel = f32::from(channel) / 255.;
            if channel <= 0.04045 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(rgb.0) + 0.7152 * linear(rgb.1) + 0.0722 * linear(rgb.2)
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}
