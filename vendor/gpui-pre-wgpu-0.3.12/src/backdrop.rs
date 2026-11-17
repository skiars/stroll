//! Shared popup filtering. Keep the unfiltered sample and the reduced-resolution
//! Gaussian result until the scene beneath the filter changes.

use gpui::{BackdropBlur, BackdropInput, Bounds, ScaledPixels, Scene, point, size};
use wgpu::util::DeviceExt;

use crate::{WgpuAtlas, frame_timing::BackdropTiming};

const CACHE_SLOTS: usize = 4;

pub(crate) struct BackdropRenderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    working: Option<Working>,
    entries: Vec<Entry>,
    input: BackdropInput,
    tiles: Vec<Option<u64>>,
    frame: u64,
}

#[derive(Clone, Copy, PartialEq)]
struct Key {
    region: [u32; 4],
    target: [u32; 2],
    radius: ScaledPixels,
    clear: wgpu::Color,
    premultiplied_alpha: bool,
    is_bgr: bool,
    retained_revision: Option<u64>,
}

struct Entry {
    key: Key,
    frame: u64,
    valid: bool,
    input: BackdropInput,
    tiles: Vec<Option<u64>>,
    original: Option<wgpu::TextureView>,
    filtered: wgpu::TextureView,
    composite: Option<(Params, wgpu::TextureView, wgpu::BindGroup)>,
}

struct Working {
    reduced: wgpu::TextureView,
    horizontal: wgpu::TextureView,
}

#[derive(Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct Params {
    bounds: [f32; 4],
    corners: [f32; 4],
    origin: [f32; 2],
    extent: [f32; 2],
    filtered_extent: [f32; 2],
    direction: [f32; 2],
    mode: u32,
    scale: u32,
    pairs: u32,
    center: f32,
    opacity: f32,
    saturation: f32,
    sample_origin: [f32; 2],
    taps: [[f32; 4]; 16],
}

impl BackdropRenderer {
    pub(crate) fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("backdrop blur"),
            source: wgpu::ShaderSource::Wgsl(include_str!("backdrop.wgsl").into()),
        });
        let pipeline = fullscreen_pipeline(device, &shader, format);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("backdrop blur"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            layout: pipeline.get_bind_group_layout(0),
            pipeline,
            sampler,
            working: None,
            entries: Vec::with_capacity(CACHE_SLOTS),
            input: BackdropInput::default(),
            tiles: Vec::new(),
            frame: 0,
        }
    }

    pub(crate) fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    pub(crate) fn draw(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        retained_source: Option<(&wgpu::TextureView, u64)>,
        scene: &Scene,
        blur: BackdropBlur,
        atlas: &WgpuAtlas,
        clear: wgpu::Color,
        premultiplied_alpha: bool,
        is_bgr: bool,
        timing: Option<BackdropTiming>,
    ) -> bool {
        let texture = target.texture();
        let visible = blur.bounds.intersect(&blur.content_mask.bounds);
        let x = visible.left().0.floor().max(0.) as u32;
        let y = visible.top().0.floor().max(0.) as u32;
        let right = (visible.right().0.ceil() as u32).min(texture.width());
        let bottom = (visible.bottom().0.ceil() as u32).min(texture.height());
        let mark = |encoder: &mut wgpu::CommandEncoder, stage| {
            if let Some(timing) = &timing {
                timing.mark(encoder, stage);
            }
        };
        mark(encoder, 0);
        if x >= right || y >= bottom {
            for stage in 1..6 {
                mark(encoder, stage);
            }
            return false;
        }

        // 窄核直接在原始分辨率上卷积，沿用裁剪纹理的边界。
        let retained_source = retained_source.filter(|_| blur.kernel_scale() > 1);
        let margin = blur.sampling_margin();
        let left = x.saturating_sub(margin);
        let top = y.saturating_sub(margin);
        let extent = [
            (right + margin).min(texture.width()) - left,
            (bottom + margin).min(texture.height()) - top,
        ];
        let key = Key {
            region: [left, top, extent[0], extent[1]],
            target: [texture.width(), texture.height()],
            radius: blur.radius,
            clear,
            premultiplied_alpha,
            is_bgr,
            retained_revision: retained_source.map(|(_, revision)| revision),
        };
        let region = Bounds {
            origin: point(ScaledPixels(left as f32), ScaledPixels(top as f32)),
            size: size(
                ScaledPixels(extent[0] as f32),
                ScaledPixels(extent[1] as f32),
            ),
        };
        let valid =
            retained_source.is_some() || scene.backdrop_input(blur, region, &mut self.input);
        if retained_source.is_none() {
            atlas.tile_generations(self.input.atlas_tiles(), &mut self.tiles);
        }
        let matching = valid
            .then(|| {
                self.entries.iter().position(|entry| {
                    entry.valid
                        && entry.key == key
                        && (retained_source.is_some()
                            || (entry.input == self.input && entry.tiles == self.tiles))
                })
            })
            .flatten();
        let retained_source = retained_source.map(|(view, _)| view);
        let slot = matching.unwrap_or_else(|| {
            if self.entries.len() < CACHE_SLOTS {
                self.entries.push(Entry::new(
                    device,
                    texture.format(),
                    key,
                    blur.kernel_scale(),
                    retained_source.is_none(),
                ));
                return self.entries.len() - 1;
            }
            self.entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.frame)
                .unwrap()
                .0
        });

        if matching.is_none() {
            let kernel = blur.kernel();
            let reduced = extent.map(|length| length.div_ceil(kernel.scale));
            let entry = &mut self.entries[slot];
            if entry.original.is_some() != retained_source.is_none()
                || entry.key.region[2..] != extent
                || entry.filtered.texture().width() != reduced[0]
                || entry.filtered.texture().height() != reduced[1]
            {
                *entry = Entry::new(
                    device,
                    texture.format(),
                    key,
                    kernel.scale,
                    retained_source.is_none(),
                );
            }
            entry.key = key;
            entry.valid = valid;
            if retained_source.is_none() {
                std::mem::swap(&mut entry.input, &mut self.input);
                std::mem::swap(&mut entry.tiles, &mut self.tiles);
            }
            let working = self
                .working
                .get_or_insert_with(|| Working::new(device, texture.format(), reduced));
            let scratch = working.horizontal.texture();
            if scratch.width() < reduced[0] || scratch.height() < reduced[1] {
                *working = Working::new(
                    device,
                    texture.format(),
                    [
                        scratch.width().max(reduced[0]),
                        scratch.height().max(reduced[1]),
                    ],
                );
            }
            if let Some(original) = &entry.original {
                encoder.copy_texture_to_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d {
                            x: left,
                            y: top,
                            z: 0,
                        },
                        aspect: wgpu::TextureAspect::All,
                    },
                    original.texture().as_image_copy(),
                    texture_extent(extent),
                );
            }
            mark(encoder, 1);
            let original = retained_source.or(entry.original.as_ref()).unwrap();
            let params = Params {
                extent: extent.map(|v| v as f32),
                filtered_extent: reduced.map(|v| v as f32),
                scale: kernel.scale,
                pairs: kernel.pairs,
                center: kernel.center,
                taps: kernel.taps,
                sample_origin: if retained_source.is_some() {
                    [left as f32, top as f32]
                } else {
                    [0.; 2]
                },
                ..bytemuck::Zeroable::zeroed()
            };
            let scissor = [0, 0, reduced[0], reduced[1]];
            let source = if kernel.scale > 1 {
                let group = bind(
                    device,
                    &self.layout,
                    &self.sampler,
                    original,
                    original,
                    params,
                );
                draw(
                    &self.pipeline,
                    encoder,
                    &group,
                    &working.reduced,
                    scissor,
                    false,
                );
                &working.reduced
            } else {
                original
            };
            mark(encoder, 2);
            let group = bind(
                device,
                &self.layout,
                &self.sampler,
                source,
                original,
                Params {
                    mode: 1,
                    direction: [1., 0.],
                    ..params
                },
            );
            draw(
                &self.pipeline,
                encoder,
                &group,
                &working.horizontal,
                scissor,
                false,
            );
            mark(encoder, 3);
            let group = bind(
                device,
                &self.layout,
                &self.sampler,
                &working.horizontal,
                original,
                Params {
                    mode: 1,
                    direction: [0., 1.],
                    ..params
                },
            );
            draw(
                &self.pipeline,
                encoder,
                &group,
                &entry.filtered,
                scissor,
                false,
            );
            mark(encoder, 4);
        } else {
            for stage in 1..5 {
                mark(encoder, stage);
            }
        }

        let entry = &mut self.entries[slot];
        entry.frame = self.frame;
        let scale = blur.kernel_scale();
        let original = retained_source.or(entry.original.as_ref()).unwrap();
        let params = Params {
            bounds: [
                blur.bounds.left().0,
                blur.bounds.top().0,
                blur.bounds.right().0,
                blur.bounds.bottom().0,
            ],
            corners: [
                blur.corner_radii.top_left.0,
                blur.corner_radii.top_right.0,
                blur.corner_radii.bottom_right.0,
                blur.corner_radii.bottom_left.0,
            ],
            origin: [left as f32, top as f32],
            filtered_extent: extent.map(|v| v.div_ceil(scale) as f32),
            mode: if retained_source.is_some() { 3 } else { 2 },
            extent: [x as f32, y as f32],
            direction: [right as f32, bottom as f32],
            scale,
            opacity: blur.opacity,
            saturation: blur.saturation,
            ..bytemuck::Zeroable::zeroed()
        };
        if entry
            .composite
            .as_ref()
            .is_none_or(|(previous, view, _)| *previous != params || view != original)
        {
            entry.composite = Some((
                params,
                original.clone(),
                bind(
                    device,
                    &self.layout,
                    &self.sampler,
                    &entry.filtered,
                    original,
                    params,
                ),
            ));
        }
        // 同一次绘制把正文送到目标纹理，并合成面板范围内的模糊。
        let direct = retained_source.is_some();
        let scissor = if direct {
            [0, 0, texture.width(), texture.height()]
        } else {
            [x, y, right - x, bottom - y]
        };
        draw(
            &self.pipeline,
            encoder,
            &entry.composite.as_ref().unwrap().2,
            target,
            scissor,
            !direct,
        );
        mark(encoder, 5);
        direct
    }
}

fn bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    source: &wgpu::TextureView,
    original: &wgpu::TextureView,
    params: Params,
) -> wgpu::BindGroup {
    let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("backdrop parameters"),
        contents: bytemuck::bytes_of(&params),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("backdrop inputs"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(source),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(original),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: uniform.as_entire_binding(),
            },
        ],
    })
}

fn draw(
    pipeline: &wgpu::RenderPipeline,
    encoder: &mut wgpu::CommandEncoder,
    group: &wgpu::BindGroup,
    target: &wgpu::TextureView,
    scissor: [u32; 4],
    composite: bool,
) {
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("backdrop blur"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: target,
            resolve_target: None,
            depth_slice: None,
            ops: wgpu::Operations {
                load: if composite {
                    wgpu::LoadOp::Load
                } else {
                    wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
                },
                store: wgpu::StoreOp::Store,
            },
        })],
        ..Default::default()
    });
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, group, &[]);
    pass.set_scissor_rect(scissor[0], scissor[1], scissor[2], scissor[3]);
    pass.draw(0..3, 0..1);
}

fn texture_extent([width, height]: [u32; 2]) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    }
}

fn texture(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    size: [u32; 2],
    usage: wgpu::TextureUsages,
    label: &'static str,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: texture_extent(size),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: usage | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

impl Entry {
    fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        key: Key,
        scale: u32,
        copy_original: bool,
    ) -> Self {
        let extent = [key.region[2], key.region[3]];
        let original = copy_original.then(|| {
            texture(
                device,
                format,
                extent,
                wgpu::TextureUsages::COPY_DST,
                "backdrop original",
            )
            .create_view(&Default::default())
        });
        let filtered = texture(
            device,
            format,
            extent.map(|v| v.div_ceil(scale)),
            wgpu::TextureUsages::RENDER_ATTACHMENT,
            "backdrop cached blur",
        );
        Self {
            key,
            frame: 0,
            valid: false,
            input: BackdropInput::default(),
            tiles: Vec::new(),
            original,
            filtered: filtered.create_view(&Default::default()),
            composite: None,
        }
    }
}

impl Working {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat, size: [u32; 2]) -> Self {
        let create = |label| {
            texture(
                device,
                format,
                size,
                wgpu::TextureUsages::RENDER_ATTACHMENT,
                label,
            )
            .create_view(&Default::default())
        };
        Self {
            reduced: create("backdrop downsample"),
            horizontal: create("backdrop horizontal blur"),
        }
    }
}

/// Fallback when the swapchain image cannot be copied. Copy the resulting
/// color to the real surface in a single final draw.
pub(crate) struct BackdropTarget {
    pub view: wgpu::TextureView,
    pipeline: wgpu::RenderPipeline,
    group: wgpu::BindGroup,
}

impl BackdropTarget {
    pub fn new(device: &wgpu::Device, surface: &wgpu::TextureView) -> Self {
        let texture = surface.texture();
        let color = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("copyable scene color"),
            size: texture.size(),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: texture.format(),
            view_formats: &[],
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::TEXTURE_BINDING,
        });
        let view = color.create_view(&Default::default());
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("present filtered scene"),
            source: wgpu::ShaderSource::Wgsl(include_str!("backdrop_present.wgsl").into()),
        });
        let pipeline = fullscreen_pipeline(device, &shader, texture.format());
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("filtered scene color"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            }],
        });
        Self {
            view,
            pipeline,
            group,
        }
    }

    pub fn matches(&self, surface: &wgpu::TextureView) -> bool {
        self.view.texture().size() == surface.texture().size()
            && self.view.texture().format() == surface.texture().format()
    }

    pub fn present(&self, encoder: &mut wgpu::CommandEncoder, surface: &wgpu::TextureView) {
        if surface
            .texture()
            .usage()
            .contains(wgpu::TextureUsages::COPY_DST)
        {
            encoder.copy_texture_to_texture(
                self.view.texture().as_image_copy(),
                surface.texture().as_image_copy(),
                surface.texture().size(),
            );
            return;
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("present filtered scene"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: surface,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.group, &[]);
        pass.draw(0..3, 0..1);
    }
}

fn fullscreen_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("backdrop blur"),
        layout: None,
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vertex"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fragment"),
            compilation_options: Default::default(),
            targets: &[Some(format.into())],
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    })
}
