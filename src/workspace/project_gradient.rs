use std::{cell::Cell, rc::Rc};

use gpui::{
    Background, Bounds, ColorSpace, ContentMask, Corners, Hsla, IntoElement, PaintQuad,
    ParentElement, Pixels, Styled, Window, canvas, div, fill, linear_color_stop, linear_gradient,
    point, px, rgb, size,
};
use gpui_component::{Colorize as _, ThemeMode, resizable::HandleEdge, window_corner_radii};
use sha2::{Digest, Sha256};

use super::{DARK_PANE_DIVIDER, PANE_RESIZE_GUTTER, PANE_SHADOW_OPACITY, Palette, pane_edge};
use crate::repository::Repository;

pub(super) const EDITOR_CORNER: Pixels = px(12.);
const PANE_DEPTH: Pixels = px(120.);

// Each palette stores the two light stops followed by the two dark stops.
const PROJECT_PALETTES: [[[u32; 4]; 8]; 5] = [
    // Blue and violet.
    [
        [0xe1d5f7, 0xd8e3f8, 0x42365b, 0x2e3a53],
        [0xd9defc, 0xd3ecf3, 0x353955, 0x1f3e45],
        [0xe2e0fa, 0xf1e3f7, 0x3b3852, 0x423349],
        [0xd0e2fe, 0xe6e3fd, 0x2a3b56, 0x3b3651],
        [0xeadbf7, 0xd6f1fb, 0x43344f, 0x1f3f4b],
        [0xe5dffd, 0xfbdde3, 0x3e3753, 0x4c2e36],
        [0xd3ddf6, 0xd4e8fc, 0x313a54, 0x263c50],
        [0xdde4fc, 0xf7e1f3, 0x343a52, 0x483245],
    ],
    // Teal and blue.
    [
        [0xccebe8, 0xd7e6f8, 0x234842, 0x2b3b54],
        [0xc0e8e0, 0xdaf2f6, 0x12423b, 0x213e42],
        [0xc5e6ef, 0xd3e8ff, 0x1a404a, 0x253b52],
        [0xcee8f8, 0xd6f2e9, 0x223e4e, 0x204138],
        [0xc5e8ed, 0xd3ece8, 0x174045, 0x21403c],
        [0xd8eafc, 0xdcf1f6, 0x2a3c4f, 0x233d44],
        [0xc5ebf1, 0xd8eedf, 0x144147, 0x274030],
        [0xd3efee, 0xe7e5ff, 0x204040, 0x3a3555],
    ],
    // Apricot and gold.
    [
        [0xf5e1ca, 0xf1dfec, 0x4b3a2d, 0x443249],
        [0xf0e4c0, 0xfee5d0, 0x463a13, 0x4b321c],
        [0xf9dac8, 0xf6ebd4, 0x513422, 0x44381e],
        [0xfcd6d0, 0xefeace, 0x55312d, 0x413b1b],
        [0xf3e0c4, 0xeaeed3, 0x4a381a, 0x3b3d20],
        [0xf6d6c9, 0xfedddd, 0x523327, 0x4e2e2f],
        [0xfcddde, 0xffe5cc, 0x513335, 0x4c3419],
        [0xf0e7c8, 0xfddfd9, 0x433a19, 0x4f322c],
    ],
    // Green and sage.
    [
        [0xe0ebcf, 0xd9ebe4, 0x394632, 0x2d4243],
        [0xe6e7c2, 0xd8f0e0, 0x3d3d16, 0x243f2f],
        [0xe1e5c9, 0xf6e9d2, 0x3a3d20, 0x45371e],
        [0xd4e8d1, 0xd5edf8, 0x2c4029, 0x223e4a],
        [0xd2e8cd, 0xeaecca, 0x2b4026, 0x3c3d18],
        [0xccead7, 0xe0efda, 0x214130, 0x2e3e27],
        [0xe4edce, 0xd1f0f5, 0x363e1e, 0x1a4045],
        [0xd5eddd, 0xd0f1e8, 0x274031, 0x1a4139],
    ],
    // Pink and mauve.
    [
        [0xf4dce4, 0xebe1f7, 0x4e3345, 0x3d3554],
        [0xfad6dd, 0xf5e2f3, 0x513038, 0x453143],
        [0xead7e5, 0xf3e9f7, 0x493544, 0x403445],
        [0xfedde8, 0xffe4d9, 0x51323e, 0x4f3327],
        [0xefd9f3, 0xdee9ff, 0x48324c, 0x303b53],
        [0xf5d3e3, 0xfedede, 0x513141, 0x4e2e30],
        [0xe7dbf2, 0xf4e3f4, 0x43364e, 0x443245],
        [0xf9dde8, 0xd2eeff, 0x4d323e, 0x1f3e51],
    ],
];

#[derive(Clone, Copy)]
pub(super) struct ProjectGradient {
    start: Hsla,
    middle: Hsla,
    neutral: Hsla,
}

impl ProjectGradient {
    pub(super) fn for_repository(repo: &Repository, mode: ThemeMode, colors: Palette) -> Self {
        // Linked worktrees share their Git directory. SSH locators include the host.
        let [light_start, light_middle, dark_start, dark_middle] =
            palette_for_identity(repo.common_dir.as_os_str().as_encoded_bytes());
        let (start, middle) = if mode.is_dark() {
            (dark_start, dark_middle)
        } else {
            (light_start, light_middle)
        };
        Self {
            start: rgb(start).into(),
            middle: rgb(middle).into(),
            neutral: if mode.is_dark() {
                colors.titlebar
            } else {
                colors.titlebar.mix_oklab(colors.surface, 0.58)
            },
        }
    }

    pub(super) fn controls(mut colors: Palette, mode: ThemeMode) -> Palette {
        let (muted, accent) = if mode.is_dark() {
            (0xb6b7be, 0xa8b7ff)
        } else {
            colors.nav_track = colors.ink.opacity(0.06);
            colors.nav_hover = Some(Hsla::white().opacity(0.30));
            (0x525c70, 0x314dcc)
        };
        colors.muted = rgb(muted).into();
        colors.accent = rgb(accent).into();
        colors
    }

    pub(super) fn titlebar(self, height: Pixels) -> impl IntoElement {
        canvas(
            |_, _, _| (),
            move |bounds, _, window, cx| {
                self.paint_horizontal(bounds, window_corner_radii(window, cx), window);
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .w_full()
        .h(height)
    }

    fn paint_horizontal(self, bounds: Bounds<Pixels>, radii: Corners<Pixels>, window: &mut Window) {
        const MIDPOINT: f32 = 0.34;
        const NEUTRAL_POINT: f32 = 0.78;
        let split = point(bounds.left() + bounds.size.width * MIDPOINT, bounds.top());
        let left = Bounds::from_corners(bounds.origin, point(split.x, bounds.bottom()));
        let right = Bounds::from_corners(split, bounds.bottom_right());
        for (bounds, corner_radii, from, to, end) in [
            (
                left,
                Corners {
                    top_left: radii.top_left,
                    ..Corners::default()
                },
                self.start,
                self.middle,
                1.,
            ),
            (
                right,
                Corners {
                    top_right: radii.top_right,
                    ..Corners::default()
                },
                self.middle,
                self.neutral,
                (NEUTRAL_POINT - MIDPOINT) / (1. - MIDPOINT),
            ),
        ] {
            let background =
                linear_gradient(90., linear_color_stop(from, 0.), linear_color_stop(to, end))
                    .color_space(ColorSpace::Oklab);
            window.paint_quad(PaintQuad {
                corner_radii,
                ..fill(bounds, background)
            });
        }
    }
}

fn palette_for_identity(identity: &[u8]) -> [u32; 4] {
    let hash = Sha256::digest(identity);
    // The first byte keeps the original family assignment. The second picks a variant.
    let family = &PROJECT_PALETTES[usize::from(hash[0]) % PROJECT_PALETTES.len()];
    family[usize::from(hash[1]) % family.len()]
}

#[derive(Clone)]
pub(super) struct PaneGradient {
    colors: ProjectGradient,
    fade: Background,
    // Both the tree and sticky rows use these window coordinates.
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl PaneGradient {
    pub(super) fn new(colors: ProjectGradient, base: Hsla) -> Self {
        Self {
            colors,
            fade: linear_gradient(
                180.,
                linear_color_stop(base.opacity(0.), 0.),
                linear_color_stop(base, 1.),
            ),
            bounds: Rc::new(Cell::new(None)),
        }
    }

    pub(super) fn background(self, width: Pixels, corner: Pixels) -> impl IntoElement {
        let layout = self.bounds.clone();
        canvas(
            move |bounds, _, _| layout.set(Some(bounds)),
            move |bounds, _, window, _| {
                self.paint(Bounds::new(bounds.origin, size(width, PANE_DEPTH)), window);
                if corner > px(0.) {
                    // Extend behind the rounded corner without tinting the content below it.
                    self.paint(
                        Bounds::new(
                            point(bounds.left() + width, bounds.top()),
                            size(corner, corner),
                        ),
                        window,
                    );
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .w_full()
        .h(PANE_DEPTH)
    }

    pub(super) fn paint(&self, region: Bounds<Pixels>, window: &mut Window) {
        let Some(bounds) = self
            .bounds
            .get()
            .filter(|bounds| bounds.intersects(&region))
        else {
            return;
        };
        window.with_content_mask(
            Some(ContentMask {
                bounds: bounds.intersect(&region),
            }),
            |window| {
                self.colors
                    .paint_horizontal(bounds, Corners::default(), window);
                window.paint_quad(fill(bounds, self.fade));
            },
        );
    }
}

pub(super) fn pane_divider(top: Pixels, color: Hsla, mode: ThemeMode) -> impl IntoElement {
    const TRANSITION: Pixels = px(32.);
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .right_0()
        .w(PANE_RESIZE_GUTTER)
        .child(pane_edge(HandleEdge::Trailing, PANE_DEPTH, mode))
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    let line = px(1.);
                    let line_x = bounds.right() - line;
                    let blend_y = bounds.bottom() - TRANSITION;
                    let upper_line = Bounds::from_corners(
                        point(line_x, bounds.top()),
                        point(bounds.right(), blend_y),
                    );
                    paint_edge_fade(window, upper_line, color.opacity(0.), color, px(24.));
                    let blend_line = Bounds::new(point(line_x, blend_y), size(line, TRANSITION));
                    let end = if mode.is_dark() {
                        rgb(DARK_PANE_DIVIDER).into()
                    } else {
                        color.opacity(0.)
                    };
                    paint_edge_fade(window, blend_line, color, end, TRANSITION);

                    if !mode.is_dark() {
                        // Sample the same horizontal falloff as the shadow below, while
                        // bringing its opacity up vertically over the end of the project fade.
                        let shadow_width = bounds.size.width - line;
                        let columns = f32::from(shadow_width * window.scale_factor())
                            .round()
                            .max(1.);
                        let column_width = shadow_width / columns;
                        let clear = Hsla::black().opacity(0.);
                        for index in 0..columns as usize {
                            let offset = index as f32;
                            let column = Bounds::new(
                                point(bounds.left() + column_width * offset, blend_y),
                                size(column_width, TRANSITION),
                            );
                            let alpha = PANE_SHADOW_OPACITY * (offset + 0.5) / columns;
                            paint_edge_fade(window, column, clear, clear.alpha(alpha), TRANSITION);
                        }
                    }
                },
            )
            .absolute()
            .top(top)
            .w_full()
            .h(PANE_DEPTH - top),
        )
}

fn paint_edge_fade(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    from: Hsla,
    to: Hsla,
    fade: Pixels,
) {
    window.paint_quad(fill(
        bounds,
        linear_gradient(
            180.,
            linear_color_stop(from, 0.),
            linear_color_stop(to, (fade / bounds.size.height).min(1.)),
        ),
    ));
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn every_project_palette_is_reachable_in_its_original_family() {
        let mut reached = PROJECT_PALETTES.map(|_| HashSet::new());
        for index in 0..1024 {
            let identity = format!("/projects/repository-{index}/.git");
            let hash = Sha256::digest(identity.as_bytes());
            let original_family = usize::from(hash[0]) % PROJECT_PALETTES.len();
            reached[original_family].insert(palette_for_identity(identity.as_bytes()));
        }

        let mut light = HashSet::new();
        let mut dark = HashSet::new();
        for (family, reached) in PROJECT_PALETTES.into_iter().zip(reached) {
            assert_eq!(reached, HashSet::from(family));
            for [light_start, light_middle, dark_start, dark_middle] in family {
                assert!(light.insert([light_start, light_middle]));
                assert!(dark.insert([dark_start, dark_middle]));
            }
        }
    }
}
