use super::*;

const CACHE_SLOTS: usize = 4;

pub(super) struct BackdropRenderer {
    vertex: ID3D11VertexShader,
    fragment: ID3D11PixelShader,
    params: ID3D11Buffer,
    working: Option<Working>,
    entries: Vec<Entry>,
    input: BackdropInput,
    frame: u64,
}

#[derive(Clone, Copy, PartialEq)]
struct Key {
    region: [u32; 4],
    target: [u32; 2],
    radius: ScaledPixels,
    atlas_revision: u64,
    clear: [f32; 4],
}

struct Entry {
    key: Key,
    frame: u64,
    valid: bool,
    input: BackdropInput,
    original: ID3D11Texture2D,
    original_view: Option<ID3D11ShaderResourceView>,
    filtered: SampledTarget,
}

struct Working {
    size: [u32; 2],
    reduced: SampledTarget,
    horizontal: SampledTarget,
}

struct SampledTarget {
    view: Option<ID3D11ShaderResourceView>,
    target: Option<ID3D11RenderTargetView>,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
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
    padding: [f32; 2],
    taps: [[f32; 4]; 16],
}

impl BackdropRenderer {
    pub(super) fn new(device: &ID3D11Device) -> Result<Self> {
        let vertex = RawShaderBytes::new(ShaderModule::Backdrop, ShaderTarget::Vertex)?;
        let fragment = RawShaderBytes::new(ShaderModule::Backdrop, ShaderTarget::Fragment)?;
        Ok(Self {
            vertex: create_vertex_shader(device, vertex.as_bytes())?,
            fragment: create_fragment_shader(device, fragment.as_bytes())?,
            params: create_constant_buffer::<Params>(device)?
                .context("creating backdrop buffer")?,
            working: None,
            entries: Vec::with_capacity(CACHE_SLOTS),
            input: BackdropInput::default(),
            frame: 0,
        })
    }

    pub(super) fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    pub(super) fn draw(
        &mut self,
        devices: &DirectXRendererDevices,
        resources: &DirectXResources,
        sampler: &Option<ID3D11SamplerState>,
        scene: &Scene,
        blur: BackdropBlur,
        atlas_revision: u64,
        clear: [f32; 4],
    ) -> Result<()> {
        let target = resources
            .render_target
            .as_ref()
            .context("missing render target")?;
        let width = resources.viewport.Width as u32;
        let height = resources.viewport.Height as u32;
        let visible = blur.bounds.intersect(&blur.content_mask.bounds);
        let left = visible.left().0.floor() as u32;
        let top = visible.top().0.floor() as u32;
        let right = (visible.right().0.ceil() as u32).min(width);
        let bottom = (visible.bottom().0.ceil() as u32).min(height);
        if left >= right || top >= bottom {
            return Ok(());
        }

        let margin = blur.sampling_margin();
        let copy = D3D11_BOX {
            left: left.saturating_sub(margin),
            top: top.saturating_sub(margin),
            front: 0,
            right: (right + margin).min(width),
            bottom: (bottom + margin).min(height),
            back: 1,
        };
        let extent = [copy.right - copy.left, copy.bottom - copy.top];
        let scale = blur.kernel_scale();
        let reduced = extent.map(|length| length.div_ceil(scale));
        let region = Bounds {
            origin: point(
                ScaledPixels(copy.left as f32),
                ScaledPixels(copy.top as f32),
            ),
            size: size(
                ScaledPixels(extent[0] as f32),
                ScaledPixels(extent[1] as f32),
            ),
        };
        let key = Key {
            region: [copy.left, copy.top, extent[0], extent[1]],
            target: [width, height],
            radius: blur.radius,
            atlas_revision,
            clear,
        };
        let valid = scene.backdrop_input(blur, region, &mut self.input);
        let matching = valid
            .then(|| {
                self.entries
                    .iter()
                    .position(|entry| entry.valid && entry.key == key && entry.input == self.input)
            })
            .flatten();
        let slot = if let Some(slot) = matching {
            slot
        } else if self.entries.len() < CACHE_SLOTS {
            self.entries.push(Entry::new(&devices.device, key, scale)?);
            self.entries.len() - 1
        } else {
            self.entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.frame)
                .unwrap()
                .0
        };
        if matching.is_none() {
            let entry = &mut self.entries[slot];
            if entry.key.region[2..] != key.region[2..] || entry.key.radius != key.radius {
                *entry = Entry::new(&devices.device, key, scale)?;
            }
            if self
                .working
                .as_ref()
                .is_none_or(|w| w.size[0] < reduced[0] || w.size[1] < reduced[1])
            {
                let extent = self.working.as_ref().map_or(reduced, |w| {
                    [w.size[0].max(reduced[0]), w.size[1].max(reduced[1])]
                });
                self.working = Some(Working {
                    size: extent,
                    reduced: SampledTarget::new(&devices.device, extent)?,
                    horizontal: SampledTarget::new(&devices.device, extent)?,
                });
            }
            entry.key = key;
            entry.valid = false;
        }

        let context = &devices.device_context;
        let params = Params {
            bounds: [
                blur.bounds.left().0,
                blur.bounds.top().0,
                blur.bounds.size.width.0,
                blur.bounds.size.height.0,
            ],
            corners: [
                blur.corner_radii.top_left.0,
                blur.corner_radii.top_right.0,
                blur.corner_radii.bottom_right.0,
                blur.corner_radii.bottom_left.0,
            ],
            origin: [copy.left as f32, copy.top as f32],
            extent: extent.map(|value| value as f32),
            filtered_extent: reduced.map(|value| value as f32),
            scale,
            opacity: blur.opacity,
            saturation: blur.saturation,
            ..Params::default()
        };
        let entry = &mut self.entries[slot];
        unsafe {
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex, None);
            context.PSSetShader(&self.fragment, None);
            context.PSSetSamplers(0, Some(slice::from_ref(sampler)));
            context.PSSetConstantBuffers(2, Some(&[Some(self.params.clone())]));
            context.OMSetBlendState(None::<&ID3D11BlendState>, None, u32::MAX);
        }
        let pass = |output: &Option<ID3D11RenderTargetView>,
                    sources: &[Option<ID3D11ShaderResourceView>; 2],
                    area: D3D11_VIEWPORT,
                    params: Params|
         -> Result<()> {
            update_buffer(context, &self.params, &[params])?;
            unsafe {
                context.PSSetShaderResources(2, Some(&[None, None]));
                context.OMSetRenderTargets(Some(slice::from_ref(output)), None);
                context.RSSetViewports(Some(&[area]));
                context.PSSetShaderResources(2, Some(sources));
                context.Draw(3, 0);
            }
            Ok(())
        };
        let result = (|| {
            if matching.is_none() {
                unsafe {
                    context.OMSetRenderTargets(None, None);
                    context.CopySubresourceRegion(
                        &entry.original,
                        0,
                        0,
                        0,
                        0,
                        target,
                        0,
                        Some(&copy),
                    );
                }
                let working = self.working.as_ref().unwrap();
                let area = viewport(0, 0, reduced[0], reduced[1]);
                let kernel = blur.kernel();
                let taps = Params {
                    pairs: kernel.pairs,
                    center: kernel.center,
                    taps: kernel.taps,
                    ..params
                };
                let source = if scale > 1 {
                    pass(
                        &working.reduced.target,
                        &[entry.original_view.clone(), None],
                        area,
                        taps,
                    )?;
                    &working.reduced.view
                } else {
                    &entry.original_view
                };
                pass(
                    &working.horizontal.target,
                    &[source.clone(), None],
                    area,
                    Params {
                        mode: 1,
                        direction: [1., 0.],
                        ..taps
                    },
                )?;
                pass(
                    &entry.filtered.target,
                    &[working.horizontal.view.clone(), None],
                    area,
                    Params {
                        mode: 1,
                        direction: [0., 1.],
                        ..taps
                    },
                )?;
                entry.valid = valid;
                std::mem::swap(&mut entry.input, &mut self.input);
            }
            entry.frame = self.frame;
            pass(
                &resources.render_target_view,
                &[entry.filtered.view.clone(), entry.original_view.clone()],
                viewport(left, top, right - left, bottom - top),
                Params { mode: 2, ..params },
            )
        })();
        unsafe {
            context.PSSetShaderResources(2, Some(&[None, None]));
            context.OMSetRenderTargets(Some(slice::from_ref(&resources.render_target_view)), None);
            context.RSSetViewports(Some(slice::from_ref(&resources.viewport)));
        }
        result
    }
}

impl Entry {
    fn new(device: &ID3D11Device, key: Key, scale: u32) -> Result<Self> {
        let extent = [key.region[2], key.region[3]];
        let (original, original_view) =
            create_sampled_render_texture(device, extent[0], extent[1])?;
        Ok(Self {
            key,
            original,
            original_view,
            frame: 0,
            valid: false,
            input: BackdropInput::default(),
            filtered: SampledTarget::new(device, extent.map(|value| value.div_ceil(scale)))?,
        })
    }
}

impl SampledTarget {
    fn new(device: &ID3D11Device, size: [u32; 2]) -> Result<Self> {
        let (texture, view) = create_sampled_render_texture(device, size[0], size[1])?;
        let mut target = None;
        unsafe { device.CreateRenderTargetView(&texture, None, Some(&mut target))? };
        Ok(Self { view, target })
    }
}

fn viewport(x: u32, y: u32, width: u32, height: u32) -> D3D11_VIEWPORT {
    D3D11_VIEWPORT {
        TopLeftX: x as f32,
        TopLeftY: y as f32,
        Width: width as f32,
        Height: height as f32,
        MinDepth: 0.,
        MaxDepth: 1.,
    }
}
