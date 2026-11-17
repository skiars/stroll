use gpui::{Bounds, PartialFrame, Primitive, ScaledPixels, point, size};

use crate::WgpuAtlas;

const TILE: u32 = 128;
// 合并一对小矩形最多多画一块的像素，并限制整帧的额外面积。
const MERGE_PIXELS: u64 = (TILE * TILE) as u64;

// Track the actual ordered paint of each reusable tile. A phase change, overlay,
// or edit dirties only the tiles it touches, without guessing from scroll state.
pub(crate) struct ScrollPaint<'a> {
    pub paints: Vec<&'a Primitive>,
    pub atlas: Vec<Option<u64>>,
}

#[derive(Default)]
pub(crate) struct ScrollTiles {
    current: Vec<Vec<usize>>,
    shifted: Vec<Vec<usize>>,
    previous: Vec<bool>,
}

impl<'a> ScrollPaint<'a> {
    pub fn new(scene: &'a PartialFrame, atlas: Option<&WgpuAtlas>) -> Self {
        let mut paints = scene.scroll_primitives().collect::<Vec<_>>();
        paints.sort_by_key(|p| PartialFrame::scroll_paint_order(p));
        let atlas = atlas.map_or_else(Vec::new, |atlas| {
            let mut generations = Vec::new();
            atlas.tile_generations(
                paints.iter().filter_map(|p| p.scroll_tile()),
                &mut generations,
            );
            let mut generations = generations.into_iter();
            paints
                .iter()
                .map(|p| p.scroll_tile().and_then(|_| generations.next().flatten()))
                .collect()
        });
        Self { paints, atlas }
    }
}

impl ScrollTiles {
    fn index(
        &mut self,
        current: &ScrollPaint<'_>,
        previous: &ScrollPaint<'_>,
        rect: [u32; 4],
        dy: i32,
    ) {
        let [x, y, width, height] = rect;
        let cols = width.div_ceil(TILE);
        let count = (cols * height.div_ceil(TILE)) as usize;
        self.current.resize_with(count, Vec::new);
        self.shifted.resize_with(count, Vec::new);
        self.current.iter_mut().for_each(Vec::clear);
        self.shifted.iter_mut().for_each(Vec::clear);
        self.previous.resize(count, false);
        self.previous.fill(false);

        let viewport = device_rect(rect);
        let tiles = |bounds: Bounds<ScaledPixels>| {
            let bounds = bounds.intersect(&viewport);
            let col = ((bounds.left().0 as u32).saturating_sub(x)) / TILE;
            let row = ((bounds.top().0 as u32).saturating_sub(y)) / TILE;
            let end_col = ((bounds.right().0.ceil() as u32).saturating_sub(x)).div_ceil(TILE);
            let end_row = ((bounds.bottom().0.ceil() as u32).saturating_sub(y)).div_ceil(TILE);
            (row..if bounds.is_empty() { row } else { end_row })
                .flat_map(move |r| (col..end_col).map(move |c| (r * cols + c) as usize))
        };
        for (index, paint) in current.paints.iter().enumerate() {
            for tile in tiles(paint.scroll_bounds(ScaledPixels(0.))) {
                self.current[tile].push(index);
            }
        }
        for (index, paint) in previous.paints.iter().enumerate() {
            let mut bounds = paint.scroll_bounds(ScaledPixels(0.));
            // 第一项是覆盖整个视口的不透明底色，不计作需要搬移的内容。
            if index > 0 {
                for tile in tiles(bounds) {
                    self.previous[tile] = true;
                }
            }
            bounds.origin.y += ScaledPixels(dy as f32);
            for tile in tiles(bounds) {
                self.shifted[tile].push(index);
            }
        }
    }

    pub fn damage(
        &mut self,
        current: &ScrollPaint<'_>,
        previous: &ScrollPaint<'_>,
        old_atlas: &[Option<u64>],
        rect: [u32; 4],
        dy: i32,
    ) -> (Vec<[u32; 4]>, Option<[u32; 4]>, u64) {
        let shift = ScaledPixels(dy as f32);
        self.index(current, previous, rect, dy);
        let [x, y, width, height] = rect;
        let available = if dy < 0 {
            [x, y, width, height - dy.unsigned_abs()]
        } else {
            [x, y + dy as u32, width, height - dy as u32]
        };
        let available = device_rect(available);
        let mut damage = Vec::<[u32; 4]>::new();
        let mut reused_pixels = 0;
        let mut copy_left = x + width;
        let mut copy_top = y + height;
        let mut copy_right = x;
        let mut copy_bottom = y;
        let cols = width.div_ceil(TILE) as usize;
        for (tile_index, (now, old)) in self.current.iter().zip(&self.shifted).enumerate() {
            let offset_x = (tile_index % cols) as u32 * TILE;
            let offset_y = (tile_index / cols) as u32 * TILE;
            let rect = [
                x + offset_x,
                y + offset_y,
                TILE.min(width - offset_x),
                TILE.min(height - offset_y),
            ];
            let bounds = device_rect(rect);
            let same = bounds.intersect(&available) == bounds
                && now.len() == old.len()
                && now.iter().zip(old).all(|(&new, &old)| {
                    current.atlas[new] == old_atlas[old]
                        && previous.paints[old].same_scrolled_paint(
                            current.paints[new],
                            shift,
                            bounds,
                        )
                });
            if same {
                reused_pixels += u64::from(rect[2]) * u64::from(rect[3]);
                // Empty background can stay in place. Bound the copy to
                // tiles containing paint in either the old or new position.
                if now.len() > 1 || self.previous[tile_index] {
                    copy_left = copy_left.min(rect[0]);
                    copy_top = copy_top.min(rect[1]);
                    copy_right = copy_right.max(rect[0] + rect[2]);
                    copy_bottom = copy_bottom.max(rect[1] + rect[3]);
                }
            } else if let Some(last) = damage.last_mut()
                && last[1] == rect[1]
                && last[0] + last[2] == rect[0]
            {
                last[2] += rect[2];
            } else {
                damage.push(rect);
            }
        }
        // Merge matching spans on adjacent rows; preserve disjoint scissors.
        let mut merged = Vec::<[u32; 4]>::new();
        for rect in damage {
            if let Some(last) = merged.iter_mut().rev().find(|last| {
                last[0] == rect[0] && last[2] == rect[2] && last[1] + last[3] == rect[1]
            }) {
                last[3] += rect[3];
            } else {
                merged.push(rect);
            }
        }
        let copy = (copy_right > copy_left && copy_bottom > copy_top).then(|| {
            [
                copy_left,
                copy_top,
                copy_right - copy_left,
                copy_bottom - copy_top,
            ]
        });
        coalesce_damage(&mut merged);
        (merged, copy, reused_pixels)
    }
}

fn coalesce_damage(rects: &mut Vec<[u32; 4]>) {
    let mut budget = 2 * MERGE_PIXELS;
    let pixels = |[_, _, w, h]: [u32; 4]| u64::from(w) * u64::from(h);
    // 控制搜索成本。高度碎片化的帧由上层决定是否整块重画。
    while (2..=12).contains(&rects.len()) {
        let mut best: Option<(u64, usize, usize, [u32; 4])> = None;
        for (a, &[x, y, w, h]) in rects.iter().enumerate() {
            for (b, &[other_x, other_y, other_w, other_h]) in rects.iter().enumerate().skip(a + 1) {
                let left = x.min(other_x);
                let top = y.min(other_y);
                let merged = [
                    left,
                    top,
                    (x + w).max(other_x + other_w) - left,
                    (y + h).max(other_y + other_h) - top,
                ];
                let excess = pixels(merged) - pixels(rects[a]) - pixels(rects[b]);
                if excess > budget.min(MERGE_PIXELS)
                    || best.as_ref().is_some_and(|(cost, ..)| excess >= *cost)
                {
                    continue;
                }
                // 保持所有裁剪区互不重叠，透明图元只混合一次。
                let bounds = device_rect(merged);
                if rects
                    .iter()
                    .enumerate()
                    .any(|(i, &r)| i != a && i != b && bounds.intersects(&device_rect(r)))
                {
                    continue;
                }
                best = Some((excess, a, b, merged));
            }
        }
        let Some((excess, a, b, merged)) = best else {
            break;
        };
        rects[a] = merged;
        rects.swap_remove(b);
        budget -= excess;
    }
}

pub(crate) fn device_rect([x, y, width, height]: [u32; 4]) -> Bounds<ScaledPixels> {
    Bounds::new(
        point(ScaledPixels(x as f32), ScaledPixels(y as f32)),
        size(ScaledPixels(width as f32), ScaledPixels(height as f32)),
    )
}
