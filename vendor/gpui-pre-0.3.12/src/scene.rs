// todo("windows"): remove
#![cfg_attr(windows, allow(dead_code))]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    AtlasTextureId, AtlasTile, Background, Bounds, ContentMask, Corners, Edges, Hsla, Pixels,
    Point, Radians, ScaledPixels, Size, bounds_tree::BoundsTree, point,
};
use std::{
    fmt::Debug,
    iter::Peekable,
    ops::{Add, Range, Sub},
    slice,
};

#[allow(non_camel_case_types, unused)]
#[expect(missing_docs)]
pub type PathVertex_ScaledPixels = PathVertex<ScaledPixels>;

#[expect(missing_docs)]
pub type DrawOrder = u32;

/// A boolean stored as a `u32` so that GPU-facing structs contain no
/// compiler-inserted padding bytes, which would be undefined behavior to
/// reinterpret as `&[u8]` when writing instance buffers. Guaranteed to be
/// `0` or `1` by construction; shaders read it as a `u32`/`uint`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct PaddedBool32(u32);

impl From<bool> for PaddedBool32 {
    fn from(value: bool) -> Self {
        PaddedBool32(value as u32)
    }
}

#[derive(Default)]
#[expect(missing_docs)]
pub struct Scene {
    pub(crate) paint_operations: Vec<PaintOperation>,
    primitive_bounds: BoundsTree<ScaledPixels>,
    layer_stack: Vec<(Bounds<ScaledPixels>, DrawOrder)>,
    pub shadows: Vec<Shadow>,
    pub quads: Vec<Quad>,
    pub paths: Vec<Path<ScaledPixels>>,
    pub underlines: Vec<Underline>,
    pub monochrome_sprites: Vec<MonochromeSprite>,
    pub subpixel_sprites: Vec<SubpixelSprite>,
    pub polychrome_sprites: Vec<PolychromeSprite>,
    pub surfaces: Vec<PaintSurface>,
    pub backdrop_blurs: Vec<BackdropBlur>,
    partial_regions: Vec<PartialRegion>,
    partial_order_offset: DrawOrder,
    max_order: DrawOrder,
}

#[expect(missing_docs)]
impl Scene {
    pub fn clear(&mut self) {
        self.paint_operations.clear();
        self.primitive_bounds.clear();
        self.layer_stack.clear();
        self.paths.clear();
        self.shadows.clear();
        self.quads.clear();
        self.underlines.clear();
        self.monochrome_sprites.clear();
        self.subpixel_sprites.clear();
        self.polychrome_sprites.clear();
        self.surfaces.clear();
        self.backdrop_blurs.clear();
        self.partial_regions.clear();
        self.partial_order_offset = 0;
        self.max_order = 0;
    }

    pub fn len(&self) -> usize {
        self.paint_operations.len()
    }

    pub fn push_layer(&mut self, bounds: Bounds<ScaledPixels>) {
        let order = self.primitive_bounds.insert(bounds) + self.partial_order_offset;
        self.max_order = self.max_order.max(order);
        self.layer_stack.push((bounds, order));
        self.paint_operations
            .push(PaintOperation::StartLayer(bounds));
    }

    pub fn pop_layer(&mut self) {
        self.layer_stack.pop();
        self.paint_operations.push(PaintOperation::EndLayer);
    }

    pub fn insert_primitive(&mut self, primitive: impl Into<Primitive>) {
        let mut primitive = primitive.into();
        let clipped_bounds = primitive
            .bounds()
            .intersect(&primitive.content_mask().bounds);

        if clipped_bounds.is_empty() {
            return;
        }

        let order = self
            .layer_stack
            .last()
            .map(|&(_, order)| order)
            .unwrap_or_else(|| {
                self.primitive_bounds.insert(clipped_bounds) + self.partial_order_offset
            });
        let order = order.max(self.partial_order_offset + 1);
        self.max_order = self.max_order.max(order);
        match &mut primitive {
            Primitive::Shadow(shadow) => {
                shadow.order = order;
                self.shadows.push(*shadow);
            }
            Primitive::Quad(quad) => {
                quad.order = order;
                self.quads.push(*quad);
            }
            Primitive::Path(path) => {
                path.order = order;
                path.id = PathId(self.paths.len());
                self.paths.push(path.clone());
            }
            Primitive::Underline(underline) => {
                underline.order = order;
                self.underlines.push(*underline);
            }
            Primitive::MonochromeSprite(sprite) => {
                sprite.order = order;
                self.monochrome_sprites.push(*sprite);
            }
            Primitive::SubpixelSprite(sprite) => {
                sprite.order = order;
                self.subpixel_sprites.push(*sprite);
            }
            Primitive::PolychromeSprite(sprite) => {
                sprite.order = order;
                self.polychrome_sprites.push(*sprite);
            }
            Primitive::Surface(surface) => {
                surface.order = order;
                self.surfaces.push(surface.clone());
            }
        }
        self.paint_operations
            .push(PaintOperation::Primitive(primitive));
    }

    /// Insert a filter after the primitives already present in its paint region.
    pub fn insert_backdrop_blur(&mut self, mut blur: BackdropBlur) {
        let clipped = blur.bounds.intersect(&blur.content_mask.bounds);
        if clipped.is_empty() {
            return;
        }
        blur.order = self.primitive_bounds.insert(clipped) + self.partial_order_offset;
        self.max_order = self.max_order.max(blur.order);
        // A backdrop is a paint barrier even inside an explicit layer.
        for (bounds, order) in &mut self.layer_stack {
            *order = self.primitive_bounds.insert(*bounds) + self.partial_order_offset;
            self.max_order = self.max_order.max(*order);
        }
        self.backdrop_blurs.push(blur);
        self.paint_operations
            .push(PaintOperation::BackdropBlur(blur));
    }

    pub fn replay(&mut self, range: Range<usize>, prev_scene: &Scene) {
        for operation in &prev_scene.paint_operations[range] {
            match operation {
                PaintOperation::Primitive(primitive) => self.insert_primitive(primitive.clone()),
                PaintOperation::BackdropBlur(blur) => self.insert_backdrop_blur(*blur),
                PaintOperation::StartLayer(bounds) => self.push_layer(*bounds),
                PaintOperation::EndLayer => self.pop_layer(),
                PaintOperation::BeginPartial { bounds, scroll } => {
                    self.begin_scroll_region(*bounds, *scroll)
                }
                PaintOperation::EndPartial => self.end_partial_region(),
            }
        }
    }

    #[doc(hidden)]
    pub fn begin_partial_region(&mut self, bounds: Bounds<ScaledPixels>) {
        self.begin_scroll_region(bounds, None);
    }

    #[doc(hidden)]
    pub fn begin_scroll_region(
        &mut self,
        bounds: Bounds<ScaledPixels>,
        scroll: Option<ScaledPixels>,
    ) {
        self.partial_order_offset = self.max_order;
        for (_, order) in &mut self.layer_stack {
            *order = self.partial_order_offset + 1;
        }
        self.partial_regions.push(PartialRegion {
            bounds,
            scroll,
            order: self.partial_order_offset + 1,
            end_order: 0,
            start: self.paint_operations.len(),
            end: None,
        });
        self.paint_operations
            .push(PaintOperation::BeginPartial { bounds, scroll });
    }

    #[doc(hidden)]
    pub fn end_partial_region(&mut self) {
        if let Some(region) = self
            .partial_regions
            .iter_mut()
            .rev()
            .find(|region| region.end.is_none())
        {
            region.end = Some(self.paint_operations.len());
            region.end_order = self.max_order + 1;
        }
        self.partial_order_offset = self.max_order;
        for (_, order) in &mut self.layer_stack {
            *order = self.partial_order_offset + 1;
        }
        self.paint_operations.push(PaintOperation::EndPartial);
    }

    /// 返回局部绘制区域和捕获位置。滚动区域上方有模糊时，额外返回浮层的起始位置。
    #[doc(hidden)]
    pub fn partial_frame(
        &self,
        key: &mut PartialFrame,
    ) -> Option<(Bounds<ScaledPixels>, DrawOrder, Option<DrawOrder>)> {
        let (region, others) = self.partial_regions.split_first()?;
        let end = region.end?;
        if !self.surfaces.is_empty() || others.iter().any(|other| other.start <= end) {
            return None;
        }
        let suffix = &self.paint_operations[end + 1..];
        let first_blur = suffix
            .iter()
            .filter_map(|operation| match operation {
                PaintOperation::BackdropBlur(blur) => Some(blur.order),
                _ => None,
            })
            .min();
        // 正文后的固定栏若完全位于滚动区外，可以和正文留在同一纹理中。
        // 这样第一道模糊就能直接读取已合成的底图。
        let overlay = first_blur.map(|order| {
            let hits_scroll = suffix.iter().any(|operation| {
                matches!(operation, PaintOperation::Primitive(p)
                    if PartialFrame::scroll_paint_order(p).0 < order
                        && p.scroll_bounds(ScaledPixels(0.)).intersects(&region.bounds.dilate(ScaledPixels(1.))))
            });
            if hits_scroll { region.end_order } else { order }
        });
        if (overlay.is_some() && region.scroll.is_none())
            || (!others.is_empty() && overlay.is_none())
        {
            return None;
        }
        // A custom element that bypasses the viewport clip can affect pixels
        // retained outside the scissor, so fall back to a full frame.
        if self.paint_operations[region.start + 1..end]
            .iter()
            .any(|operation| {
                if let PaintOperation::Primitive(primitive) = operation {
                    let clip = primitive.content_mask().bounds;
                    clip.intersect(&region.bounds) != clip
                } else {
                    matches!(operation, PaintOperation::BackdropBlur(_))
                }
            })
        {
            return None;
        }
        key.scroll = region.scroll;
        key.moving.clear();
        key.after.clear();
        if region.scroll.is_some() {
            key.moving.extend(
                self.paint_operations[region.start + 1..end]
                    .iter()
                    .filter_map(|operation| {
                        if let PaintOperation::Primitive(primitive) = operation {
                            Some(primitive.clone())
                        } else {
                            None
                        }
                    }),
            );
        }
        if region.scroll.is_some() && overlay.is_none() {
            key.after.extend(
                self.paint_operations[end + 1..]
                    .iter()
                    .filter_map(|operation| match operation {
                        PaintOperation::Primitive(primitive)
                            if primitive
                                .scroll_bounds(ScaledPixels(0.))
                                .intersects(&region.bounds) =>
                        {
                            Some(primitive.clone())
                        }
                        _ => None,
                    }),
            );
        }
        let opaque = key.opaque_scroll_background(region.bounds);
        key.prefix_len = region.start;
        key.outside.clear();
        key.outside.extend(
            self.paint_operations[..region.start]
                .iter()
                .map(PaintOperation::without_order),
        );
        key.separate_overlay = overlay.is_some();
        if let Some(order) = overlay {
            key.outside.extend(suffix.iter().filter_map(|operation| {
                matches!(operation, PaintOperation::Primitive(p)
                    if PartialFrame::scroll_paint_order(p).0 < order)
                .then(|| operation.without_order())
            }));
        }
        if overlay.is_none() {
            key.outside.extend(self.paint_operations[end + 1..].iter().filter(|operation| {
                // Suffix draws fully inside an opaque scroll region are compared
                // with its moving paints. They cannot invalidate retained pixels outside.
                !opaque || !matches!(operation, PaintOperation::Primitive(p)
                    if p.scroll_bounds(ScaledPixels(0.)).intersect(&region.bounds) == p.scroll_bounds(ScaledPixels(0.)))
            }).map(PaintOperation::without_order));
        }
        Some((region.bounds, region.order, overlay))
    }

    pub fn finish(&mut self) {
        self.shadows.sort_by_key(|shadow| shadow.order);
        self.quads.sort_by_key(|quad| quad.order);
        self.paths.sort_by_key(|path| path.order);
        self.underlines.sort_by_key(|underline| underline.order);
        self.monochrome_sprites
            .sort_by_key(|sprite| (sprite.order, sprite.tile.texture_id.index));
        self.subpixel_sprites
            .sort_by_key(|sprite| (sprite.order, sprite.tile.texture_id.index));
        self.polychrome_sprites
            .sort_by_key(|sprite| (sprite.order, sprite.tile.texture_id.index));
        self.surfaces.sort_by_key(|surface| surface.order);
        self.backdrop_blurs.sort_by_key(|blur| blur.order);
    }

    #[cfg_attr(
        all(
            any(target_os = "linux", target_os = "freebsd"),
            not(any(feature = "x11", feature = "wayland"))
        ),
        allow(dead_code)
    )]
    pub fn batches(&self) -> impl Iterator<Item = PrimitiveBatch> + '_ {
        self.batch_iterator()
    }

    /// Draw batches in paint order, with a boundary at each backdrop sample.
    pub fn batches_with_backdrop(&self) -> impl Iterator<Item = SceneBatch> + '_ {
        self.batches_with_capture(None, None)
    }

    #[doc(hidden)]
    pub fn batches_with_capture(
        &self,
        mut capture: Option<DrawOrder>,
        mut overlay: Option<DrawOrder>,
    ) -> impl Iterator<Item = SceneBatch> + '_ {
        let mut batches = self.batch_iterator();
        let mut blurs = self.backdrop_blurs.iter().peekable();
        std::iter::from_fn(move || {
            let blur_order = blurs.peek().map_or(u32::MAX, |blur| blur.order);
            let limit = blur_order
                .min(capture.unwrap_or(u32::MAX))
                .min(overlay.unwrap_or(u32::MAX));
            if let Some(batch) = batches.next_before(limit) {
                Some(SceneBatch::Draw(batch))
            } else if capture == Some(limit) {
                capture = None;
                Some(SceneBatch::Capture)
            } else if overlay == Some(limit) {
                overlay = None;
                Some(SceneBatch::Overlay)
            } else {
                blurs.next().copied().map(SceneBatch::Backdrop)
            }
        })
    }

    /// Compare the scene pixels a backdrop can sample, independently of
    /// foreground animations and of paint commands outside that region.
    #[doc(hidden)]
    pub fn backdrop_input(
        &self,
        effect: BackdropBlur,
        mut region: Bounds<ScaledPixels>,
        input: &mut BackdropInput,
    ) -> bool {
        input.primitives.clear();
        input.effects.clear();
        // Earlier filters contribute both their output and their input pixels.
        for previous in self
            .backdrop_blurs
            .iter()
            .rev()
            .filter(|blur| blur.order < effect.order)
        {
            let visible = previous.bounds.intersect(&previous.content_mask.bounds);
            if !visible.intersect(&region).is_empty() {
                region =
                    region.union(&visible.dilate(ScaledPixels(previous.sampling_margin() as f32)));
                input.effects.push(*previous);
            }
        }
        let visible = |bounds: Bounds<ScaledPixels>, clip: ContentMask<ScaledPixels>| {
            !region.intersect(&bounds.intersect(&clip.bounds)).is_empty()
        };
        if self
            .surfaces
            .iter()
            .take_while(|surface| surface.order < effect.order)
            .any(|surface| visible(surface.bounds, surface.content_mask))
        {
            return false;
        }
        // An outer shadow can intersect the sample even when its source
        // rectangle lies outside it. Transformed glyph bounds are likewise
        // conservative: include them even when their untransformed box misses.
        input.primitives.extend(
            self.shadows
                .iter()
                .take_while(|item| item.order < effect.order)
                .filter(|item| {
                    visible(
                        item.bounds.dilate(ScaledPixels(3. * item.blur_radius.0)),
                        item.content_mask,
                    )
                })
                .copied()
                .map(BackdropPrimitive::Shadow),
        );
        macro_rules! remember {
            ($field:ident, $variant:ident $(, $include:expr)?) => {
                input.primitives.extend(self.$field.iter()
                    .take_while(|item| item.order < effect.order)
                    .filter(|item| visible(item.bounds, item.content_mask) $(|| ($include)(item))?)
                    .cloned().map(BackdropPrimitive::$variant));
            };
        }
        remember!(quads, Quad);
        remember!(paths, Path);
        remember!(underlines, Underline);
        remember!(
            monochrome_sprites,
            MonochromeSprite,
            |item: &&MonochromeSprite| item.transformation != TransformationMatrix::unit()
        );
        remember!(
            subpixel_sprites,
            SubpixelSprite,
            |item: &&SubpixelSprite| item.transformation != TransformationMatrix::unit()
        );
        remember!(polychrome_sprites, PolychromeSprite);
        true
    }

    fn batch_iterator(&self) -> BatchIterator<'_> {
        BatchIterator {
            shadows_start: 0,
            shadows_iter: self.shadows.iter().peekable(),
            quads_start: 0,
            quads_iter: self.quads.iter().peekable(),
            paths_start: 0,
            paths_iter: self.paths.iter().peekable(),
            underlines_start: 0,
            underlines_iter: self.underlines.iter().peekable(),
            monochrome_sprites_start: 0,
            monochrome_sprites_iter: self.monochrome_sprites.iter().peekable(),
            subpixel_sprites_start: 0,
            subpixel_sprites_iter: self.subpixel_sprites.iter().peekable(),
            polychrome_sprites_start: 0,
            polychrome_sprites_iter: self.polychrome_sprites.iter().peekable(),
            surfaces_start: 0,
            surfaces_iter: self.surfaces.iter().peekable(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Default)]
#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
pub(crate) enum PrimitiveKind {
    Shadow,
    #[default]
    Quad,
    Path,
    Underline,
    MonochromeSprite,
    SubpixelSprite,
    PolychromeSprite,
    Surface,
}

#[derive(Clone, PartialEq)]
pub(crate) enum PaintOperation {
    Primitive(Primitive),
    BackdropBlur(BackdropBlur),
    StartLayer(Bounds<ScaledPixels>),
    EndLayer,
    BeginPartial {
        bounds: Bounds<ScaledPixels>,
        scroll: Option<ScaledPixels>,
    },
    EndPartial,
}

struct PartialRegion {
    bounds: Bounds<ScaledPixels>,
    scroll: Option<ScaledPixels>,
    order: DrawOrder,
    end_order: DrawOrder,
    start: usize,
    end: Option<usize>,
}

#[doc(hidden)]
#[derive(Default)]
pub struct PartialFrame {
    scroll: Option<ScaledPixels>,
    moving: Vec<Primitive>,
    after: Vec<Primitive>,
    prefix_len: usize,
    separate_overlay: bool,
    outside: Vec<PaintOperation>,
}

impl PartialEq for PartialFrame {
    fn eq(&self, other: &Self) -> bool {
        self.prefix_len == other.prefix_len
            && self.separate_overlay == other.separate_overlay
            && self.outside == other.outside
            && self.scroll.is_some() == other.scroll.is_some()
    }
}

impl PartialFrame {
    #[doc(hidden)]
    pub fn scroll_offset(&self) -> Option<ScaledPixels> {
        self.scroll
    }

    #[doc(hidden)]
    pub fn scroll_primitives(&self) -> impl Iterator<Item = &Primitive> {
        self.moving.iter().chain(self.after.iter())
    }

    #[doc(hidden)]
    pub fn scroll_paint_order(primitive: &Primitive) -> (DrawOrder, u8, u32) {
        match primitive {
            Primitive::Shadow(p) => (p.order, 0, 0),
            Primitive::Quad(p) => (p.order, 1, 0),
            Primitive::Path(p) => (p.order, 2, 0),
            Primitive::Underline(p) => (p.order, 3, 0),
            Primitive::MonochromeSprite(p) => (p.order, 4, p.tile.texture_id.index),
            Primitive::SubpixelSprite(p) => (p.order, 5, p.tile.texture_id.index),
            Primitive::PolychromeSprite(p) => (p.order, 6, p.tile.texture_id.index),
            Primitive::Surface(p) => (p.order, 7, 0),
        }
    }

    #[doc(hidden)]
    pub fn opaque_scroll_background(&self, region: Bounds<ScaledPixels>) -> bool {
        matches!(self.moving.first(), Some(Primitive::Quad(quad))
            if quad.bounds.intersect(&region) == region
                && quad.content_mask.bounds.intersect(&region) == region
                && quad.background.as_solid().is_some_and(|color| color.is_opaque())
                && quad.corner_radii == Corners::default()
                && quad.border_widths == Edges::default())
    }

    #[doc(hidden)]
    pub fn replace_scroll(&mut self, other: &mut Self) {
        std::mem::swap(&mut self.scroll, &mut other.scroll);
        std::mem::swap(&mut self.moving, &mut other.moving);
        std::mem::swap(&mut self.after, &mut other.after);
    }

    /// Atlas tiles referenced by the pixels retained outside the region.
    #[doc(hidden)]
    pub fn atlas_tiles(&self) -> impl Iterator<Item = AtlasTile> + '_ {
        self.outside.iter().filter_map(|operation| match operation {
            PaintOperation::Primitive(Primitive::MonochromeSprite(item)) => Some(item.tile),
            PaintOperation::Primitive(Primitive::SubpixelSprite(item)) => Some(item.tile),
            PaintOperation::Primitive(Primitive::PolychromeSprite(item)) => Some(item.tile),
            _ => None,
        })
    }
}

impl PaintOperation {
    fn without_order(&self) -> Self {
        let mut operation = self.clone();
        match &mut operation {
            Self::Primitive(primitive) => match primitive {
                Primitive::Shadow(item) => item.order = 0,
                Primitive::Quad(item) => item.order = 0,
                Primitive::Path(item) => {
                    item.order = 0;
                    item.id = PathId(0);
                }
                Primitive::Underline(item) => item.order = 0,
                Primitive::MonochromeSprite(item) => item.order = 0,
                Primitive::SubpixelSprite(item) => item.order = 0,
                Primitive::PolychromeSprite(item) => item.order = 0,
                Primitive::Surface(_) => unreachable!("surface frames cannot be reused"),
            },
            Self::BackdropBlur(blur) => blur.order = 0,
            _ => {}
        }
        operation
    }
}

#[derive(Clone)]
#[expect(missing_docs)]
pub enum Primitive {
    Shadow(Shadow),
    Quad(Quad),
    Path(Path<ScaledPixels>),
    Underline(Underline),
    MonochromeSprite(MonochromeSprite),
    SubpixelSprite(SubpixelSprite),
    PolychromeSprite(PolychromeSprite),
    Surface(PaintSurface),
}

impl PartialEq for Primitive {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Shadow(a), Self::Shadow(b)) => a == b,
            (Self::Quad(a), Self::Quad(b)) => a == b,
            (Self::Path(a), Self::Path(b)) => a == b,
            (Self::Underline(a), Self::Underline(b)) => a == b,
            (Self::MonochromeSprite(a), Self::MonochromeSprite(b)) => a == b,
            (Self::SubpixelSprite(a), Self::SubpixelSprite(b)) => a == b,
            (Self::PolychromeSprite(a), Self::PolychromeSprite(b)) => a == b,
            _ => false,
        }
    }
}

#[expect(missing_docs)]
impl Primitive {
    pub fn bounds(&self) -> &Bounds<ScaledPixels> {
        match self {
            Primitive::Shadow(shadow) => &shadow.bounds,
            Primitive::Quad(quad) => &quad.bounds,
            Primitive::Path(path) => &path.bounds,
            Primitive::Underline(underline) => &underline.bounds,
            Primitive::MonochromeSprite(sprite) => &sprite.bounds,
            Primitive::SubpixelSprite(sprite) => &sprite.bounds,
            Primitive::PolychromeSprite(sprite) => &sprite.bounds,
            Primitive::Surface(surface) => &surface.bounds,
        }
    }

    pub fn content_mask(&self) -> &ContentMask<ScaledPixels> {
        match self {
            Primitive::Shadow(shadow) => &shadow.content_mask,
            Primitive::Quad(quad) => &quad.content_mask,
            Primitive::Path(path) => &path.content_mask,
            Primitive::Underline(underline) => &underline.content_mask,
            Primitive::MonochromeSprite(sprite) => &sprite.content_mask,
            Primitive::SubpixelSprite(sprite) => &sprite.content_mask,
            Primitive::PolychromeSprite(sprite) => &sprite.content_mask,
            Primitive::Surface(surface) => &surface.content_mask,
        }
    }
}

impl Primitive {
    #[doc(hidden)]
    pub fn scroll_tile(&self) -> Option<AtlasTile> {
        match self {
            Self::MonochromeSprite(item) => Some(item.tile),
            Self::SubpixelSprite(item) => Some(item.tile),
            Self::PolychromeSprite(item) => Some(item.tile),
            _ => None,
        }
    }

    #[doc(hidden)]
    pub fn scroll_bounds(&self, dy: ScaledPixels) -> Bounds<ScaledPixels> {
        let mut bounds = *self.bounds();
        match self {
            Self::Shadow(shadow) => bounds = bounds.dilate(ScaledPixels(shadow.blur_radius.0 * 3.)),
            Self::MonochromeSprite(sprite)
                if sprite.transformation != TransformationMatrix::unit() =>
            {
                bounds = self.content_mask().bounds
            }
            Self::SubpixelSprite(sprite)
                if sprite.transformation != TransformationMatrix::unit() =>
            {
                bounds = self.content_mask().bounds
            }
            _ => {}
        }
        bounds = bounds.intersect(&self.content_mask().bounds);
        bounds.origin.y += dy;
        bounds
    }

    #[doc(hidden)]
    pub fn same_scrolled_paint(
        &self,
        other: &Self,
        dy: ScaledPixels,
        tile: Bounds<ScaledPixels>,
    ) -> bool {
        macro_rules! compare {
            ($a:expr, $b:expr) => {{
                let (mut a, mut b) = (*$a, *$b);
                a.order = 0;
                b.order = 0;
                a.bounds.origin.y += dy;
                a.content_mask.bounds.origin.y += dy;
                a.content_mask.bounds = a.content_mask.bounds.intersect(&tile);
                b.content_mask.bounds = b.content_mask.bounds.intersect(&tile);
                a == b
            }};
        }
        match (self, other) {
            (Self::Quad(a), Self::Quad(b)) => {
                // A solid rectangle may extend beyond the reusable pixels.
                if a.corner_radii == Corners::default()
                    && b.corner_radii == Corners::default()
                    && a.border_widths == Edges::default()
                    && b.border_widths == Edges::default()
                    && a.background.as_solid().is_some()
                    && a.background == b.background
                {
                    self.scroll_bounds(dy).intersect(&tile)
                        == other.scroll_bounds(ScaledPixels(0.)).intersect(&tile)
                } else {
                    compare!(a, b)
                }
            }
            (Self::Path(a), Self::Path(b))
                if a.color.as_solid().is_some() && a.color == b.color =>
            {
                // 矢量路径离屏栅格化。裁剪边界和三角形一起平移时，
                // 整数设备像素位移保留相同的采样位置。
                let mut clip = a.clipped_bounds();
                clip.origin.y += dy;
                clip == b.clipped_bounds()
                    && a.vertices.len() == b.vertices.len()
                    && a.vertices.iter().zip(&b.vertices).all(|(a, b)| {
                        a.xy_position.x == b.xy_position.x
                            && a.xy_position.y + dy == b.xy_position.y
                            && a.st_position == b.st_position
                    })
            }
            (Self::Underline(a), Self::Underline(b))
                if a.wavy == false.into() && b.wavy == false.into() =>
            {
                compare!(a, b)
            }
            (Self::MonochromeSprite(a), Self::MonochromeSprite(b))
                if a.transformation == TransformationMatrix::unit()
                    && b.transformation == TransformationMatrix::unit() =>
            {
                compare!(a, b)
            }
            (Self::SubpixelSprite(a), Self::SubpixelSprite(b))
                if a.transformation == TransformationMatrix::unit()
                    && b.transformation == TransformationMatrix::unit() =>
            {
                compare!(a, b)
            }
            (Self::PolychromeSprite(a), Self::PolychromeSprite(b)) => compare!(a, b),
            _ => false,
        }
    }
}

#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
struct BatchIterator<'a> {
    shadows_start: usize,
    shadows_iter: Peekable<slice::Iter<'a, Shadow>>,
    quads_start: usize,
    quads_iter: Peekable<slice::Iter<'a, Quad>>,
    paths_start: usize,
    paths_iter: Peekable<slice::Iter<'a, Path<ScaledPixels>>>,
    underlines_start: usize,
    underlines_iter: Peekable<slice::Iter<'a, Underline>>,
    monochrome_sprites_start: usize,
    monochrome_sprites_iter: Peekable<slice::Iter<'a, MonochromeSprite>>,
    subpixel_sprites_start: usize,
    subpixel_sprites_iter: Peekable<slice::Iter<'a, SubpixelSprite>>,
    polychrome_sprites_start: usize,
    polychrome_sprites_iter: Peekable<slice::Iter<'a, PolychromeSprite>>,
    surfaces_start: usize,
    surfaces_iter: Peekable<slice::Iter<'a, PaintSurface>>,
}

impl<'a> Iterator for BatchIterator<'a> {
    type Item = PrimitiveBatch;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_before(u32::MAX)
    }
}

impl BatchIterator<'_> {
    fn next_before(&mut self, before: DrawOrder) -> Option<PrimitiveBatch> {
        let mut orders_and_kinds = [
            (
                self.shadows_iter.peek().map(|s| s.order),
                PrimitiveKind::Shadow,
            ),
            (self.quads_iter.peek().map(|q| q.order), PrimitiveKind::Quad),
            (self.paths_iter.peek().map(|q| q.order), PrimitiveKind::Path),
            (
                self.underlines_iter.peek().map(|u| u.order),
                PrimitiveKind::Underline,
            ),
            (
                self.monochrome_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::MonochromeSprite,
            ),
            (
                self.subpixel_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::SubpixelSprite,
            ),
            (
                self.polychrome_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::PolychromeSprite,
            ),
            (
                self.surfaces_iter.peek().map(|s| s.order),
                PrimitiveKind::Surface,
            ),
        ];
        orders_and_kinds.sort_by_key(|(order, kind)| (order.unwrap_or(u32::MAX), *kind));

        let first = orders_and_kinds[0];
        let second = orders_and_kinds[1];
        let first_order = first.0?;
        if first_order >= before {
            return None;
        }
        let batch_kind = first.1;
        let max_order_and_kind =
            (second.0.unwrap_or(u32::MAX), second.1).min((before, PrimitiveKind::Shadow));

        match batch_kind {
            PrimitiveKind::Shadow => {
                let shadows_start = self.shadows_start;
                let mut shadows_end = shadows_start + 1;
                self.shadows_iter.next();
                while self
                    .shadows_iter
                    .next_if(|shadow| (shadow.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    shadows_end += 1;
                }
                self.shadows_start = shadows_end;
                Some(PrimitiveBatch::Shadows(shadows_start..shadows_end))
            }
            PrimitiveKind::Quad => {
                let quads_start = self.quads_start;
                let mut quads_end = quads_start + 1;
                self.quads_iter.next();
                while self
                    .quads_iter
                    .next_if(|quad| (quad.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    quads_end += 1;
                }
                self.quads_start = quads_end;
                Some(PrimitiveBatch::Quads(quads_start..quads_end))
            }
            PrimitiveKind::Path => {
                let paths_start = self.paths_start;
                let mut paths_end = paths_start + 1;
                self.paths_iter.next();
                while self
                    .paths_iter
                    .next_if(|path| (path.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    paths_end += 1;
                }
                self.paths_start = paths_end;
                Some(PrimitiveBatch::Paths(paths_start..paths_end))
            }
            PrimitiveKind::Underline => {
                let underlines_start = self.underlines_start;
                let mut underlines_end = underlines_start + 1;
                self.underlines_iter.next();
                while self
                    .underlines_iter
                    .next_if(|underline| (underline.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    underlines_end += 1;
                }
                self.underlines_start = underlines_end;
                Some(PrimitiveBatch::Underlines(underlines_start..underlines_end))
            }
            PrimitiveKind::MonochromeSprite => {
                let texture_id = self.monochrome_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.monochrome_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.monochrome_sprites_iter.next();
                while self
                    .monochrome_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.monochrome_sprites_start = sprites_end;
                Some(PrimitiveBatch::MonochromeSprites {
                    texture_id,
                    range: sprites_start..sprites_end,
                })
            }
            PrimitiveKind::SubpixelSprite => {
                let texture_id = self.subpixel_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.subpixel_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.subpixel_sprites_iter.next();
                while self
                    .subpixel_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.subpixel_sprites_start = sprites_end;
                Some(PrimitiveBatch::SubpixelSprites {
                    texture_id,
                    range: sprites_start..sprites_end,
                })
            }
            PrimitiveKind::PolychromeSprite => {
                let texture_id = self.polychrome_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.polychrome_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.polychrome_sprites_iter.next();
                while self
                    .polychrome_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.polychrome_sprites_start = sprites_end;
                Some(PrimitiveBatch::PolychromeSprites {
                    texture_id,
                    range: sprites_start..sprites_end,
                })
            }
            PrimitiveKind::Surface => {
                let surfaces_start = self.surfaces_start;
                let mut surfaces_end = surfaces_start + 1;
                self.surfaces_iter.next();
                while self
                    .surfaces_iter
                    .next_if(|surface| (surface.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    surfaces_end += 1;
                }
                self.surfaces_start = surfaces_end;
                Some(PrimitiveBatch::Surfaces(surfaces_start..surfaces_end))
            }
        }
    }
}

#[derive(Debug)]
#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
#[allow(missing_docs)]
#[derive(Clone)]
pub enum PrimitiveBatch {
    Shadows(Range<usize>),
    Quads(Range<usize>),
    Paths(Range<usize>),
    Underlines(Range<usize>),
    MonochromeSprites {
        texture_id: AtlasTextureId,
        range: Range<usize>,
    },
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    SubpixelSprites {
        texture_id: AtlasTextureId,
        range: Range<usize>,
    },
    PolychromeSprites {
        texture_id: AtlasTextureId,
        range: Range<usize>,
    },
    Surfaces(Range<usize>),
}

impl PrimitiveBatch {
    #[expect(missing_docs)]
    pub fn label(&self) -> String {
        match self {
            Self::Shadows(range) => format!("shadows ({})", range.len()),
            Self::Quads(range) => format!("quads ({})", range.len()),
            Self::Paths(range) => format!("paths ({})", range.len()),
            Self::Underlines(range) => format!("underlines ({})", range.len()),
            Self::MonochromeSprites { texture_id, range } => {
                format!(
                    "monochrome sprites ({}) on atlas {}",
                    range.len(),
                    texture_id.index
                )
            }
            Self::SubpixelSprites { texture_id, range } => {
                format!(
                    "subpixel sprites ({}) on atlas {}",
                    range.len(),
                    texture_id.index
                )
            }
            Self::PolychromeSprites { texture_id, range } => {
                format!(
                    "polychrome sprites ({}) on atlas {}",
                    range.len(),
                    texture_id.index
                )
            }
            Self::Surfaces(range) => format!("surfaces ({})", range.len()),
        }
    }
}

#[derive(Default, PartialEq)]
#[doc(hidden)]
pub struct BackdropInput {
    primitives: Vec<BackdropPrimitive>,
    effects: Vec<BackdropBlur>,
}

impl BackdropInput {
    #[doc(hidden)]
    pub fn atlas_tiles(&self) -> impl Iterator<Item = AtlasTile> + '_ {
        self.primitives
            .iter()
            .filter_map(|primitive| match primitive {
                BackdropPrimitive::MonochromeSprite(item) => Some(item.tile),
                BackdropPrimitive::SubpixelSprite(item) => Some(item.tile),
                BackdropPrimitive::PolychromeSprite(item) => Some(item.tile),
                _ => None,
            })
    }
}

#[derive(PartialEq)]
enum BackdropPrimitive {
    Shadow(Shadow),
    Quad(Quad),
    Path(Path<ScaledPixels>),
    Underline(Underline),
    MonochromeSprite(MonochromeSprite),
    SubpixelSprite(SubpixelSprite),
    PolychromeSprite(PolychromeSprite),
}

/// A rounded backdrop filter at a particular point in the scene's paint order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BackdropBlur {
    /// Draw order assigned by the scene.
    pub order: DrawOrder,
    /// Rounded destination rectangle in device pixels.
    pub bounds: Bounds<ScaledPixels>,
    /// Clip applied at the paint call.
    pub content_mask: ContentMask<ScaledPixels>,
    /// Corner radii of the uncut rectangle.
    pub corner_radii: Corners<ScaledPixels>,
    /// Gaussian standard deviation in device pixels.
    pub radius: ScaledPixels,
    /// Saturation multiplier for the filtered pixels.
    pub saturation: f32,
    /// Amount of filtered backdrop to mix with the original.
    pub opacity: f32,
}

impl BackdropBlur {
    /// Downsample broad filters while keeping the working Gaussian between
    /// three and eight pixels wide. Narrow filters retain full resolution.
    #[doc(hidden)]
    pub fn kernel(self) -> BackdropKernel {
        let sigma = self.radius.0.clamp(0.25, 64.);
        let scale = self.kernel_scale();
        let scale_f = scale as f32;
        let variance = (scale_f * scale_f - 1.) / 12.;
        let sigma = ((sigma * sigma - variance).sqrt() / scale_f).max(0.25);
        let pairs = (sigma * 1.5).ceil() as usize;
        let mut taps = [[0.; 4]; 16];
        let gaussian = |x: f32| (-0.5 * (x / sigma).powi(2)).exp();
        let mut total = 1.;
        for (i, tap) in taps.iter_mut().take(pairs).enumerate() {
            let first = (2 * i + 1) as f32;
            let a = gaussian(first);
            let b = gaussian(first + 1.);
            let weight = a + b;
            if weight > 0. {
                *tap = [first + b / weight, weight, 0., 0.];
            }
            total += 2. * weight;
        }
        for tap in &mut taps[..pairs] {
            tap[1] /= total;
        }
        BackdropKernel {
            scale,
            pairs: pairs as u32,
            center: 1. / total,
            taps,
        }
    }

    #[doc(hidden)]
    pub fn kernel_scale(self) -> u32 {
        let sigma = self.radius.0;
        if sigma >= 32. {
            8
        } else if sigma >= 16. {
            4
        } else if sigma >= 8. {
            2
        } else {
            1
        }
    }

    #[doc(hidden)]
    pub fn sampling_margin(self) -> u32 {
        (3. * self.radius.0.clamp(0.25, 64.)).ceil() as u32 + 24
    }
}

#[doc(hidden)]
pub struct BackdropKernel {
    pub scale: u32,
    pub pairs: u32,
    pub center: f32,
    pub taps: [[f32; 4]; 16],
}

/// A run of draw commands, or a backdrop filter sampling the preceding runs.
#[derive(Clone)]
pub enum SceneBatch {
    /// A batch of regular primitives.
    Draw(PrimitiveBatch),
    /// A single read of the preceding scene color.
    Backdrop(BackdropBlur),
    /// Capture the composed color before a partial update region.
    Capture,
    /// 正文滚动区域结束，接下来绘制窗口浮层。
    Overlay,
}

#[derive(Default, Debug, Copy, Clone, PartialEq)]
#[repr(C)]
#[expect(missing_docs)]
pub struct Quad {
    pub order: DrawOrder,
    pub border_style: BorderStyle,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub background: Background,
    pub border_color: Hsla,
    pub corner_radii: Corners<ScaledPixels>,
    pub border_widths: Edges<ScaledPixels>,
}

impl From<Quad> for Primitive {
    fn from(quad: Quad) -> Self {
        Primitive::Quad(quad)
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
#[repr(C)]
#[expect(missing_docs)]
pub struct Underline {
    pub order: DrawOrder,
    pub pad: u32, // align to 8 bytes
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub thickness: ScaledPixels,
    pub wavy: PaddedBool32,
}

impl From<Underline> for Primitive {
    fn from(underline: Underline) -> Self {
        Primitive::Underline(underline)
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
#[repr(C)]
#[expect(missing_docs)]
pub struct Shadow {
    pub order: DrawOrder,
    pub blur_radius: ScaledPixels,
    pub bounds: Bounds<ScaledPixels>,
    pub corner_radii: Corners<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub element_bounds: Bounds<ScaledPixels>,
    pub element_corner_radii: Corners<ScaledPixels>,
    /// 0 = drop shadow (rendered outside the element), 1 = inset shadow (rendered inside).
    pub inset: u32,
    /// Clip the drop shadow to the exterior of its element.
    pub clip_interior: u32,
}

impl From<Shadow> for Primitive {
    fn from(shadow: Shadow) -> Self {
        Primitive::Shadow(shadow)
    }
}

/// The style of a border.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[repr(C)]
pub enum BorderStyle {
    /// A solid border.
    #[default]
    Solid = 0,
    /// A dashed border.
    Dashed = 1,
}

/// A data type representing a 2 dimensional transformation that can be applied to an element.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct TransformationMatrix {
    /// 2x2 matrix containing rotation and scale,
    /// stored row-major
    pub rotation_scale: [[f32; 2]; 2],
    /// translation vector
    pub translation: [f32; 2],
}

impl Eq for TransformationMatrix {}

impl TransformationMatrix {
    /// The unit matrix, has no effect.
    pub fn unit() -> Self {
        Self {
            rotation_scale: [[1.0, 0.0], [0.0, 1.0]],
            translation: [0.0, 0.0],
        }
    }

    /// Move the origin by a given point
    pub fn translate(self, point: Point<ScaledPixels>) -> Self {
        self.compose(Self {
            rotation_scale: [[1.0, 0.0], [0.0, 1.0]],
            translation: [point.x.0, point.y.0],
        })
    }

    /// Clockwise rotation in radians around the origin
    pub fn rotate(self, angle: Radians) -> Self {
        self.compose(Self {
            rotation_scale: [
                [angle.0.cos(), -angle.0.sin()],
                [angle.0.sin(), angle.0.cos()],
            ],
            translation: [0.0, 0.0],
        })
    }

    /// Scale around the origin
    pub fn scale(self, size: Size<f32>) -> Self {
        self.compose(Self {
            rotation_scale: [[size.width, 0.0], [0.0, size.height]],
            translation: [0.0, 0.0],
        })
    }

    /// Perform matrix multiplication with another transformation
    /// to produce a new transformation that is the result of
    /// applying both transformations: first, `other`, then `self`.
    #[inline]
    pub fn compose(self, other: TransformationMatrix) -> TransformationMatrix {
        if other == Self::unit() {
            return self;
        }
        // Perform matrix multiplication
        TransformationMatrix {
            rotation_scale: [
                [
                    self.rotation_scale[0][0] * other.rotation_scale[0][0]
                        + self.rotation_scale[0][1] * other.rotation_scale[1][0],
                    self.rotation_scale[0][0] * other.rotation_scale[0][1]
                        + self.rotation_scale[0][1] * other.rotation_scale[1][1],
                ],
                [
                    self.rotation_scale[1][0] * other.rotation_scale[0][0]
                        + self.rotation_scale[1][1] * other.rotation_scale[1][0],
                    self.rotation_scale[1][0] * other.rotation_scale[0][1]
                        + self.rotation_scale[1][1] * other.rotation_scale[1][1],
                ],
            ],
            translation: [
                self.translation[0]
                    + self.rotation_scale[0][0] * other.translation[0]
                    + self.rotation_scale[0][1] * other.translation[1],
                self.translation[1]
                    + self.rotation_scale[1][0] * other.translation[0]
                    + self.rotation_scale[1][1] * other.translation[1],
            ],
        }
    }

    /// Apply transformation to a point, mainly useful for debugging
    pub fn apply(&self, point: Point<Pixels>) -> Point<Pixels> {
        let input = [point.x.0, point.y.0];
        let mut output = self.translation;
        for (i, output_cell) in output.iter_mut().enumerate() {
            for (k, input_cell) in input.iter().enumerate() {
                *output_cell += self.rotation_scale[i][k] * *input_cell;
            }
        }
        Point::new(output[0].into(), output[1].into())
    }
}

impl Default for TransformationMatrix {
    fn default() -> Self {
        Self::unit()
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(C)]
#[expect(missing_docs)]
pub struct MonochromeSprite {
    pub order: DrawOrder,
    pub pad: u32,
    pub tile: AtlasTile,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub transformation: TransformationMatrix,
}

impl From<MonochromeSprite> for Primitive {
    fn from(sprite: MonochromeSprite) -> Self {
        Primitive::MonochromeSprite(sprite)
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(C)]
#[expect(missing_docs)]
pub struct SubpixelSprite {
    pub order: DrawOrder,
    pub pad: u32, // align to 8 bytes
    pub tile: AtlasTile,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub transformation: TransformationMatrix,
}

impl From<SubpixelSprite> for Primitive {
    fn from(sprite: SubpixelSprite) -> Self {
        Primitive::SubpixelSprite(sprite)
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(C)]
#[expect(missing_docs)]
pub struct PolychromeSprite {
    pub order: DrawOrder,
    pub nearest_neighbor: u32,
    pub tile: AtlasTile,
    pub grayscale: PaddedBool32,
    pub opacity: f32,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub corner_radii: Corners<ScaledPixels>,
}

impl From<PolychromeSprite> for Primitive {
    fn from(sprite: PolychromeSprite) -> Self {
        Primitive::PolychromeSprite(sprite)
    }
}

#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct PaintSurface {
    pub order: DrawOrder,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub image_buffer: objc2_core_foundation::CFRetained<objc2_core_video::CVPixelBuffer>,
}

impl From<PaintSurface> for Primitive {
    fn from(surface: PaintSurface) -> Self {
        Primitive::Surface(surface)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[expect(missing_docs)]
pub struct PathId(pub usize);

/// A line made up of a series of vertices and control points.
#[derive(Clone, Debug, PartialEq)]
#[expect(missing_docs)]
pub struct Path<P: Clone + Debug + Default + PartialEq> {
    pub id: PathId,
    pub order: DrawOrder,
    pub bounds: Bounds<P>,
    pub content_mask: ContentMask<P>,
    pub vertices: Vec<PathVertex<P>>,
    pub color: Background,
    start: Point<P>,
    current: Point<P>,
    contour_count: usize,
}

impl Path<Pixels> {
    /// Create a new path with the given starting point.
    pub fn new(start: Point<Pixels>) -> Self {
        Self {
            id: PathId(0),
            order: DrawOrder::default(),
            vertices: Vec::new(),
            start,
            current: start,
            bounds: Bounds {
                origin: start,
                size: Default::default(),
            },
            content_mask: Default::default(),
            color: Default::default(),
            contour_count: 0,
        }
    }

    /// Scale this path by the given factor.
    pub fn scale(&self, factor: f32) -> Path<ScaledPixels> {
        Path {
            id: self.id,
            order: self.order,
            bounds: self.bounds.scale(factor),
            content_mask: self.content_mask.scale(factor),
            vertices: self
                .vertices
                .iter()
                .map(|vertex| vertex.scale(factor))
                .collect(),
            start: self.start.map(|start| start.scale(factor)),
            current: self.current.scale(factor),
            contour_count: self.contour_count,
            color: self.color,
        }
    }

    /// Move the start, current point to the given point.
    pub fn move_to(&mut self, to: Point<Pixels>) {
        self.contour_count += 1;
        self.start = to;
        self.current = to;
    }

    /// Draw a straight line from the current point to the given point.
    pub fn line_to(&mut self, to: Point<Pixels>) {
        self.contour_count += 1;
        if self.contour_count > 1 {
            self.push_triangle(
                (self.start, self.current, to),
                (point(0., 1.), point(0., 1.), point(0., 1.)),
            );
        }
        self.current = to;
    }

    /// Draw a curve from the current point to the given point, using the given control point.
    pub fn curve_to(&mut self, to: Point<Pixels>, ctrl: Point<Pixels>) {
        self.contour_count += 1;
        if self.contour_count > 1 {
            self.push_triangle(
                (self.start, self.current, to),
                (point(0., 1.), point(0., 1.), point(0., 1.)),
            );
        }

        self.push_triangle(
            (self.current, ctrl, to),
            (point(0., 0.), point(0.5, 0.), point(1., 1.)),
        );
        self.current = to;
    }

    /// Push a triangle to the Path.
    pub fn push_triangle(
        &mut self,
        xy: (Point<Pixels>, Point<Pixels>, Point<Pixels>),
        st: (Point<f32>, Point<f32>, Point<f32>),
    ) {
        self.bounds = self
            .bounds
            .union(&Bounds {
                origin: xy.0,
                size: Default::default(),
            })
            .union(&Bounds {
                origin: xy.1,
                size: Default::default(),
            })
            .union(&Bounds {
                origin: xy.2,
                size: Default::default(),
            });

        self.vertices.push(PathVertex {
            xy_position: xy.0,
            st_position: st.0,
            content_mask: Default::default(),
        });
        self.vertices.push(PathVertex {
            xy_position: xy.1,
            st_position: st.1,
            content_mask: Default::default(),
        });
        self.vertices.push(PathVertex {
            xy_position: xy.2,
            st_position: st.2,
            content_mask: Default::default(),
        });
    }
}

impl<T> Path<T>
where
    T: Clone + Debug + Default + PartialEq + PartialOrd + Add<T, Output = T> + Sub<Output = T>,
{
    #[allow(unused)]
    #[expect(missing_docs)]
    pub fn clipped_bounds(&self) -> Bounds<T> {
        self.bounds.intersect(&self.content_mask.bounds)
    }
}

/// Pixel-aligned bounds of a path batch inside a render target.
#[doc(hidden)]
pub fn path_raster_bounds(
    paths: &[Path<ScaledPixels>],
    target: Size<ScaledPixels>,
) -> Option<Bounds<ScaledPixels>> {
    let bounds = paths
        .iter()
        .filter(|path| !path.vertices.is_empty())
        .map(Path::clipped_bounds)
        .filter(|bounds| !bounds.is_empty())
        .reduce(|left, right| left.union(&right))?;
    let left = bounds.left().0.floor().max(0.);
    let top = bounds.top().0.floor().max(0.);
    let right = bounds.right().0.ceil().min(target.width.0);
    let bottom = bounds.bottom().0.ceil().min(target.height.0);
    (right > left && bottom > top).then(|| {
        Bounds::new(
            point(ScaledPixels(left), ScaledPixels(top)),
            crate::size(ScaledPixels(right - left), ScaledPixels(bottom - top)),
        )
    })
}

impl From<Path<ScaledPixels>> for Primitive {
    fn from(path: Path<ScaledPixels>) -> Self {
        Primitive::Path(path)
    }
}

#[derive(Clone, Debug, PartialEq)]
#[repr(C)]
#[expect(missing_docs)]
pub struct PathVertex<P: Clone + Debug + Default + PartialEq> {
    pub xy_position: Point<P>,
    pub st_position: Point<f32>,
    pub content_mask: ContentMask<P>,
}

#[expect(missing_docs)]
impl PathVertex<Pixels> {
    pub fn scale(&self, factor: f32) -> PathVertex<ScaledPixels> {
        PathVertex {
            xy_position: self.xy_position.scale(factor),
            st_position: self.st_position,
            content_mask: self.content_mask.scale(factor),
        }
    }
}
