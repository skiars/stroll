use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) const FEATURES: wgpu::Features =
    wgpu::Features::TIMESTAMP_QUERY.union(wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS);

pub(crate) fn enabled() -> bool {
    !cfg!(target_family = "wasm")
        && std::env::var_os("STROLL_FRAME_TIMINGS").is_some_and(|value| value == "1")
}

const QUERY_COUNT: u32 = 8;

/// 可选的模糊分段追踪。每帧最多记录第一道模糊。
pub(crate) struct BackdropTiming(wgpu::QuerySet);

impl BackdropTiming {
    pub(crate) fn mark(&self, encoder: &mut wgpu::CommandEncoder, stage: u32) {
        encoder.write_timestamp(&self.0, stage + 2);
    }
}

struct Sample {
    readback: Arc<wgpu::Buffer>,
    pending: Arc<AtomicBool>,
}

/// A bounded asynchronous readback queue. A busy GPU can skip measurements
/// without delaying rendering to wait for a free buffer.
pub(crate) struct FrameTiming {
    queries: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    samples: [Sample; 4],
    period_ns: f64,
    frame: u64,
    trace_backdrop: bool,
}

impl FrameTiming {
    pub(crate) fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Option<Self> {
        if !enabled() || !device.features().contains(FEATURES) {
            return None;
        }
        let buffer = |usage| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("frame timestamps"),
                size: u64::from(QUERY_COUNT) * 8,
                usage,
                mapped_at_creation: false,
            })
        };
        Some(Self {
            queries: device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("frame timestamps"),
                ty: wgpu::QueryType::Timestamp,
                count: QUERY_COUNT,
            }),
            resolve: buffer(wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC),
            samples: std::array::from_fn(|_| Sample {
                readback: Arc::new(buffer(
                    wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                )),
                pending: Arc::default(),
            }),
            period_ns: f64::from(queue.get_timestamp_period()),
            frame: 0,
            trace_backdrop: std::env::var_os("STROLL_BACKDROP_TIMINGS")
                .is_some_and(|value| value == "1"),
        })
    }

    pub(crate) fn begin(&mut self, encoder: &mut wgpu::CommandEncoder) -> Option<usize> {
        self.frame += 1;
        let index = self
            .samples
            .iter()
            .position(|sample| !sample.pending.load(Ordering::Acquire))?;
        encoder.write_timestamp(&self.queries, 0);
        Some(index)
    }

    pub(crate) fn backdrop(&self) -> Option<BackdropTiming> {
        self.trace_backdrop
            .then(|| BackdropTiming(self.queries.clone()))
    }

    pub(crate) fn end(&self, encoder: &mut wgpu::CommandEncoder, index: usize, backdrop: bool) {
        let count = if backdrop { QUERY_COUNT } else { 2 };
        encoder.write_timestamp(&self.queries, 1);
        encoder.resolve_query_set(&self.queries, 0..count, &self.resolve, 0);
        encoder.copy_buffer_to_buffer(
            &self.resolve,
            0,
            self.samples[index].readback.as_ref(),
            0,
            u64::from(count) * 8,
        );
    }

    pub(crate) fn read(&self, index: usize, width: u32, height: u32, backdrop: bool) {
        let sample = &self.samples[index];
        sample.pending.store(true, Ordering::Release);
        let readback = sample.readback.clone();
        let pending = sample.pending.clone();
        let period = self.period_ns;
        let frame = self.frame;
        sample.readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            if result.is_ok() {
                if let Ok(bytes) = readback.slice(..).get_mapped_range() {
                    let stamp = |i: usize| u64::from_ne_bytes(bytes[i * 8..(i + 1) * 8].try_into().unwrap());
                    let elapsed = |a, b| stamp(b).saturating_sub(stamp(a)) as f64 * period / 1_000.;
                    let frame_us = elapsed(0, 1);
                    eprintln!("frame_timing_gpu frame={frame} width={width} height={height} gpu_us={frame_us:.0}");
                    if backdrop {
                        eprintln!(
                            "frame_timing_backdrop_gpu frame={frame} copy_us={:.0} downsample_us={:.0} horizontal_us={:.0} vertical_us={:.0} composite_us={:.0}",
                            elapsed(2,3), elapsed(3,4), elapsed(4,5), elapsed(5,6), elapsed(6,7),
                        );
                    }
                }
                readback.unmap();
            }
            pending.store(false, Ordering::Release);
        });
    }
}
