use crate::{
    WgpuAtlas,
    scroll_repaint::{ScrollPaint, ScrollTiles, device_rect},
};
use gpui::{Bounds, PartialFrame, ScaledPixels};

#[derive(Default, PartialEq)]
pub(crate) struct RetainedFrameKey {
    pub scene: PartialFrame,
    pub tiles: Vec<Option<u64>>,
    pub clear: wgpu::Color,
    pub premultiplied_alpha: bool,
    pub is_bgr: bool,
}

pub(crate) struct RetainedFrame {
    key: RetainedFrameKey,
    base: wgpu::Texture,
    rect: [u32; 4],
    scroll_atlas: Vec<Option<u64>>,
    scroll_tiles: ScrollTiles,
    scroll_opaque: bool,
    shift_scratch: Option<wgpu::Texture>,
    target: wgpu::Texture,
}

impl RetainedFrame {
    pub fn new(device: &wgpu::Device, target: &wgpu::Texture, rect: [u32; 4]) -> Self {
        let base = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("composed viewport background"),
            size: Self::extent(rect),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: target.format(),
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        Self {
            key: RetainedFrameKey::default(),
            base,
            rect,
            scroll_atlas: Vec::new(),
            scroll_tiles: ScrollTiles::default(),
            scroll_opaque: false,
            shift_scratch: None,
            target: target.clone(),
        }
    }

    pub fn rect(bounds: Bounds<ScaledPixels>, target: &wgpu::Texture) -> Option<[u32; 4]> {
        let x = (bounds.left().0 as u32).min(target.width());
        let y = (bounds.top().0 as u32).min(target.height());
        let right = (bounds.right().0 as u32).min(target.width());
        let bottom = (bounds.bottom().0 as u32).min(target.height());
        (right > x && bottom > y).then_some([x, y, right - x, bottom - y])
    }

    fn extent([_, _, width, height]: [u32; 4]) -> wgpu::Extent3d {
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        }
    }

    fn target_region(&self) -> wgpu::TexelCopyTextureInfo<'_> {
        wgpu::TexelCopyTextureInfo {
            origin: wgpu::Origin3d {
                x: self.rect[0],
                y: self.rect[1],
                z: 0,
            },
            ..self.target.as_image_copy()
        }
    }

    pub fn matches(&self, target: &wgpu::Texture, rect: [u32; 4]) -> bool {
        self.target == *target && self.rect == rect
    }

    pub fn reuse(&self, key: &RetainedFrameKey) -> bool {
        &self.key == key
            && self.scroll_opaque == key.scene.opaque_scroll_background(device_rect(self.rect))
    }

    pub fn update_scroll(
        &mut self,
        key: &mut RetainedFrameKey,
        atlas: &WgpuAtlas,
    ) -> Option<(i32, Vec<[u32; 4]>, Option<[u32; 4]>, u64)> {
        let opaque = key.scene.opaque_scroll_background(device_rect(self.rect));
        let current = ScrollPaint::new(&key.scene, Some(atlas));
        let offset = key
            .scene
            .scroll_offset()
            .zip(self.key.scene.scroll_offset())
            .map(|(new, old)| (new.0 - old.0).round() as i32);
        let mut result = None;
        if opaque
            && self.scroll_opaque
            && let Some(dy) = offset.filter(|dy| dy.unsigned_abs() < self.rect[3])
        {
            if dy == 0
                && self.scroll_atlas == current.atlas
                && self
                    .key
                    .scene
                    .scroll_primitives()
                    .eq(key.scene.scroll_primitives())
            {
                result = Some((
                    dy,
                    Vec::new(),
                    None,
                    u64::from(self.rect[2]) * u64::from(self.rect[3]),
                ));
            } else {
                // 图集世代在上一帧已保存，无需对旧图元再查一次图集。
                let previous = ScrollPaint::new(&self.key.scene, None);
                let (damage, copy, reused_pixels) = self.scroll_tiles.damage(
                    &current,
                    &previous,
                    &self.scroll_atlas,
                    self.rect,
                    dy,
                );
                if reused_pixels > 0 && damage.len() <= 12 {
                    result = Some((dy, damage, copy, reused_pixels));
                }
            }
        }
        self.scroll_opaque = opaque;
        self.scroll_atlas = current.atlas;
        self.key.scene.replace_scroll(&mut key.scene);
        result
    }

    pub fn has_opaque_scroll(&self) -> bool {
        self.scroll_opaque
    }

    pub fn reset_scroll(&mut self, atlas: &WgpuAtlas) {
        self.scroll_opaque = self
            .key
            .scene
            .opaque_scroll_background(device_rect(self.rect));
        self.scroll_atlas = ScrollPaint::new(&self.key.scene, Some(atlas)).atlas;
    }

    pub fn shift(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        dy: i32,
        rect: [u32; 4],
    ) {
        if dy == 0 {
            return;
        }
        let [x, y, width, height] = rect;
        let extent = Self::extent(rect);
        if self
            .shift_scratch
            .as_ref()
            .is_none_or(|scratch| scratch.width() < width || scratch.height() < height)
        {
            self.shift_scratch = Some(device.create_texture(&wgpu::TextureDescriptor {
                label: Some("overlapping scroll pixels"),
                size: Self::extent([
                    0,
                    0,
                    width.next_multiple_of(128),
                    height.next_multiple_of(128),
                ]),
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.target.format(),
                usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
        }
        let scratch = self.shift_scratch.as_ref().unwrap();
        let from = wgpu::Origin3d {
            x,
            y: (y as i32 - dy) as u32,
            z: 0,
        };
        let to = wgpu::Origin3d { x, y, z: 0 };
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                origin: from,
                ..self.target.as_image_copy()
            },
            scratch.as_image_copy(),
            extent,
        );
        encoder.copy_texture_to_texture(
            scratch.as_image_copy(),
            wgpu::TexelCopyTextureInfo {
                origin: to,
                ..self.target.as_image_copy()
            },
            extent,
        );
    }

    pub fn capture(&mut self, encoder: &mut wgpu::CommandEncoder, key: &mut RetainedFrameKey) {
        encoder.copy_texture_to_texture(
            self.target_region(),
            self.base.as_image_copy(),
            Self::extent(self.rect),
        );
        std::mem::swap(&mut self.key, key);
    }

    pub fn restore(&self, encoder: &mut wgpu::CommandEncoder) {
        encoder.copy_texture_to_texture(
            self.base.as_image_copy(),
            self.target_region(),
            Self::extent(self.rect),
        );
    }
}
