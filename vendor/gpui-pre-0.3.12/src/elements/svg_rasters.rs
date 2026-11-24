use crate::{
    App, Bounds, DevicePixels, EntityId, Global, GlobalElementId, ParsedSvg, Pixels, RenderImage,
    Size, SvgSize, Task, Window,
};
use scheduler::Instant;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    sync::{Arc, Weak},
    time::Duration,
};

const RESIZE_IDLE: Duration = Duration::from_millis(80);
const MAX_JOBS: usize = 2;
const MAX_DIMENSION: f64 = 8192.;
const MAX_IMAGE_PIXELS: f64 = 8_000_000.;
const CACHE_BYTES: usize = 64 * 1024 * 1024;
const CACHE_ENTRIES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RasterKey {
    svg: usize,
    dimensions: Size<DevicePixels>,
}

#[derive(Eq, Hash, PartialEq)]
struct Consumer {
    svg: usize,
    view: EntityId,
    element: Option<GlobalElementId>,
}

struct Demand {
    key: RasterKey,
    svg: Weak<ParsedSvg>,
    ready_at: Instant,
}

struct Entry {
    svg: Weak<ParsedSvg>,
    raster: Option<Arc<RenderImage>>,
    views: HashSet<EntityId>,
    used: u64,
}

impl Entry {
    fn bytes(&self) -> usize {
        self.raster
            .as_ref()
            .and_then(|image| image.as_bytes(0))
            .map_or(0, <[u8]>::len)
    }
}

#[derive(Default)]
struct Cache {
    rasters: HashMap<RasterKey, Entry>,
    pending: HashMap<Consumer, Demand>,
    active: HashMap<RasterKey, HashSet<EntityId>>,
    wake: Option<(Instant, Task<()>)>,
    used: u64,
    bytes: usize,
    #[cfg(any(test, feature = "test-support"))]
    started: usize,
}

#[derive(Default)]
pub(super) struct SvgRasterCache(RefCell<Cache>);
impl Global for SvgRasterCache {}

impl SvgRasterCache {
    pub(super) fn for_bounds(
        svg: Arc<ParsedSvg>,
        image: Bounds<Pixels>,
        clip: Bounds<Pixels>,
        element: Option<&GlobalElementId>,
        window: &Window,
        cx: &mut App,
    ) -> Option<Arc<RenderImage>> {
        if image
            .intersect(&clip)
            .intersect(&window.content_mask().bounds)
            .is_empty()
        {
            return None;
        }

        let dimensions = raster_dimensions(image.size, window.scale_factor())?;
        let key = RasterKey {
            svg: svg.id,
            dimensions,
        };
        let view = window.current_view();
        let consumer = Consumer {
            svg: svg.id,
            view,
            element: element.cloned(),
        };
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
        let mut cache = cx.global::<Self>().0.borrow_mut();
        cache.used += 1;
        let used = cache.used;
        if let Some(entry) = cache.rasters.get_mut(&key) {
            entry.used = used;
            entry.views.insert(view);
            let image = entry.raster.clone();
            cache.pending.remove(&consumer);
            return image;
        }

        // Keep a nearby existing raster on screen while a resize settles.
        let fallback = cache
            .rasters
            .iter_mut()
            .filter(|(candidate, entry)| candidate.svg == key.svg && entry.raster.is_some())
            .min_by_key(|(candidate, _)| {
                candidate.dimensions.width.0.abs_diff(dimensions.width.0)
                    + candidate.dimensions.height.0.abs_diff(dimensions.height.0)
            })
            .and_then(|(_, entry)| {
                entry.used = used;
                entry.views.insert(view);
                entry.raster.clone()
            });

        if let Some(views) = cache.active.get_mut(&key) {
            views.insert(view);
            cache.pending.remove(&consumer);
        } else if !cache
            .pending
            .get(&consumer)
            .is_some_and(|demand| demand.key == key)
        {
            let now = cx.background_executor().now();
            let ready_at = if fallback.is_some() {
                now + RESIZE_IDLE
            } else {
                now
            };
            cache.pending.insert(
                consumer,
                Demand {
                    key,
                    svg: Arc::downgrade(&svg),
                    ready_at,
                },
            );
            drop(cache);
            Self::schedule(cx);
        }
        fallback
    }

    fn schedule(cx: &mut App) {
        let mut cache = cx.global::<Self>().0.borrow_mut();
        if cache.active.len() >= MAX_JOBS {
            return;
        }
        let Some(ready_at) = cache.pending.values().map(|demand| demand.ready_at).min() else {
            cache.wake = None;
            return;
        };
        if cache
            .wake
            .as_ref()
            .is_some_and(|(deadline, _)| *deadline <= ready_at)
        {
            return;
        }
        let delay = ready_at.saturating_duration_since(cx.background_executor().now());
        drop(cache);
        let wake = cx.spawn(async move |cx| {
            cx.background_executor().timer(delay).await;
            cx.update(Self::start_ready);
        });
        cx.global::<Self>().0.borrow_mut().wake = Some((ready_at, wake));
    }

    fn start_ready(cx: &mut App) {
        let now = cx.background_executor().now();
        let mut cache = cx.global::<Self>().0.borrow_mut();
        cache.wake = None;
        cache
            .pending
            .retain(|_, demand| demand.svg.strong_count() > 0);
        let mut jobs = Vec::new();
        while cache.active.len() < MAX_JOBS {
            let Some(demand) = cache
                .pending
                .values()
                .filter(|demand| demand.ready_at <= now)
                .min_by_key(|demand| demand.ready_at)
            else {
                break;
            };
            let key = demand.key;
            let svg = demand.svg.upgrade();
            let mut views = HashSet::new();
            cache.pending.retain(|consumer, demand| {
                if demand.key == key {
                    views.insert(consumer.view);
                    false
                } else {
                    true
                }
            });
            if let Some(svg) = svg {
                cache.active.insert(key, views);
                jobs.push((key, svg));
            }
        }
        #[cfg(any(test, feature = "test-support"))]
        {
            cache.started += jobs.len();
        }
        drop(cache);

        for (key, svg) in jobs {
            let renderer = cx.svg_renderer();
            cx.spawn(async move |cx| {
                let weak_svg = Arc::downgrade(&svg);
                let raster = cx
                    .background_executor()
                    .spawn(async move {
                        renderer
                            .render_parsed(&svg, SvgSize::ExactSize(key.dimensions))
                            .ok()
                    })
                    .await;
                cx.update(|cx| Self::complete(key, weak_svg, raster, cx));
            })
            .detach();
        }
        Self::schedule(cx);
    }

    fn complete(
        key: RasterKey,
        svg: Weak<ParsedSvg>,
        raster: Option<Arc<RenderImage>>,
        cx: &mut App,
    ) {
        let mut cache = cx.global::<Self>().0.borrow_mut();
        let mut views = cache.active.remove(&key).unwrap_or_default();
        let mut evicted = Vec::new();
        if svg.strong_count() > 0 {
            cache.used += 1;
            let entry = Entry {
                svg,
                raster,
                views: HashSet::new(),
                used: cache.used,
            };
            cache.bytes += entry.bytes();
            cache.rasters.insert(key, entry);
            while cache.bytes > CACHE_BYTES || cache.rasters.len() > CACHE_ENTRIES {
                evicted.extend(cache.remove_oldest());
            }
        }
        drop(cache);
        for entry in evicted {
            if let Some(image) = entry.raster {
                cx.drop_image(image, None);
            }
            views.extend(entry.views);
        }
        for view in views {
            cx.notify(view);
        }
        Self::schedule(cx);
    }
}

impl Cache {
    fn remove_oldest(&mut self) -> Option<Entry> {
        let key = *self
            .rasters
            .iter()
            .min_by_key(|(_, entry)| (entry.svg.strong_count() > 0, entry.used))?
            .0;
        let entry = self.rasters.remove(&key)?;
        self.bytes -= entry.bytes();
        Some(entry)
    }
}

fn raster_dimensions(logical: Size<Pixels>, scale: f32) -> Option<Size<DevicePixels>> {
    let width = f64::from(logical.width.0 * scale).round();
    let height = f64::from(logical.height.0 * scale).round();
    if !width.is_finite() || !height.is_finite() || width < 1. || height < 1. {
        return None;
    }
    let limit =
        (MAX_DIMENSION / width.max(height)).min((MAX_IMAGE_PIXELS / (width * height)).sqrt());
    // Floor after limiting so the rounded dimensions cannot exceed the pixel budget.
    Some(Size::new(
        DevicePixels((width * limit.min(1.)).floor().max(1.) as i32),
        DevicePixels((height * limit.min(1.)).floor().max(1.) as i32),
    ))
}

/// Snapshot of vector image work for headless integration tests.
#[cfg(any(test, feature = "test-support"))]
pub fn svg_raster_stats(cx: &App) -> (usize, usize, Vec<Size<DevicePixels>>) {
    cx.try_global::<SvgRasterCache>()
        .map_or((0, 0, Vec::new()), |cache| {
            let cache = cache.0.borrow();
            (
                cache.started,
                cache.bytes,
                cache.rasters.keys().map(|key| key.dimensions).collect(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{px, size};

    #[test]
    fn raster_dimensions_fit_device_pixels_and_bound_memory() {
        for (dpi, expected) in [(1., (360, 120)), (1.25, (450, 150)), (2., (720, 240))] {
            let result = raster_dimensions(size(px(360.), px(120.)), dpi).unwrap();
            assert_eq!((result.width.0, result.height.0), expected);
        }
        let huge = raster_dimensions(size(px(7800.), px(4000.)), 2.).unwrap();
        assert!(huge.width.0 <= MAX_DIMENSION as i32);
        assert!(huge.width.0 * huge.height.0 <= MAX_IMAGE_PIXELS as i32);
        assert!((huge.width.0 as f32 / huge.height.0 as f32 - 1.95).abs() < 0.002);
        assert!(raster_dimensions(size(px(0.), px(1.)), 1.).is_none());
    }
}
