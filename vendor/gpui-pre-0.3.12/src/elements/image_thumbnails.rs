use crate::{App, AppContext as _, Bounds, Entity, EntityId, ImageId, Pixels, RenderImage, Window};
use image::{Frame, ImageBuffer, Rgba, imageops};
use std::{collections::HashMap, sync::Arc};

type Dimensions = (u32, u32);

#[derive(Default)]
struct Thumbnail {
    images: HashMap<Dimensions, Arc<RenderImage>>,
    loading: HashMap<Dimensions, Vec<EntityId>>,
}

/// 正文图片的缩略图缓存，每张图最多保留八种绘制尺寸。
#[derive(Default)]
pub struct ImageThumbnails(HashMap<ImageId, Thumbnail>);

impl ImageThumbnails {
    /// 创建随文档视图释放的缓存。
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            cx.on_release(|cache: &mut Self, cx| {
                for (_, thumbnail) in cache.0.drain() {
                    for image in thumbnail.images.into_values() {
                        cx.drop_image(image, None);
                    }
                }
            })
            .detach();
            Self::default()
        })
    }

    pub(super) fn for_bounds(
        cache: &Entity<Self>,
        source: Arc<RenderImage>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Arc<RenderImage>> {
        if bounds.intersect(&window.content_mask().bounds).is_empty() {
            return None;
        }
        let source_size = source.size(0);
        let target = bounds.size.scale(window.scale_factor());
        let scale = (target.width.0 / source_size.width.0 as f32)
            .max(target.height.0 / source_size.height.0 as f32);
        if scale >= 1. {
            return Some(source);
        }
        let dimension = |source: i32| (source as f32 * scale).ceil().max(1.) as u32;
        // 窗口连续缩放时复用相近尺寸，最多比所需宽度多 31 个设备像素。
        let width = dimension(source_size.width.0)
            .next_multiple_of(32)
            .min(source_size.width.0 as u32);
        let target = (
            width,
            (u64::from(width) * source_size.height.0 as u64).div_ceil(source_size.width.0 as u64)
                as u32,
        );
        if target.0 == source_size.width.0 as u32 {
            return Some(source);
        }
        let id = source.id;
        if let Some(image) = cache
            .read(cx)
            .0
            .get(&id)
            .and_then(|entry| entry.images.get(&target))
        {
            return Some(image.clone());
        }

        let view = window.current_view();
        cache.update(cx, |cache, cx| {
            let thumbnail = cache.0.entry(id).or_default();
            if let Some(views) = thumbnail.loading.get_mut(&target) {
                if !views.contains(&view) {
                    views.push(view);
                }
            } else {
                thumbnail.loading.insert(target, vec![view]);
                cx.spawn(async move |cache, cx| {
                    let image = cx
                        .background_spawn(async move { resize(&source, target) })
                        .await;
                    cx.update(|cx| {
                        let Ok((views, evicted)) = cache.update(cx, |cache, _| {
                            let thumbnail = cache.0.get_mut(&id).expect("缩略图请求仍在缓存中");
                            let views = thumbnail.loading.remove(&target).unwrap_or_default();
                            let evicted = if thumbnail.images.len() == 8 {
                                let smallest = *thumbnail.images.keys().min().unwrap();
                                thumbnail.images.remove(&smallest)
                            } else {
                                None
                            };
                            thumbnail.images.insert(target, image);
                            (views, evicted)
                        }) else {
                            return;
                        };
                        if let Some(image) = evicted {
                            cx.drop_image(image, None);
                        }
                        for view in views {
                            cx.notify(view);
                        }
                    });
                })
                .detach();
            }
            thumbnail.images.values().next().cloned()
        })
    }
}

fn resize(source: &RenderImage, (width, height): Dimensions) -> Arc<RenderImage> {
    let size = source.size(0);
    // 逐通道缩放 BGRA 像素，保留图集要求的通道顺序。
    let pixels = ImageBuffer::<Rgba<u8>, _>::from_raw(
        size.width.0 as u32,
        size.height.0 as u32,
        source.as_bytes(0).expect("缩略图有一个原图帧"),
    )
    .expect("原图的尺寸和字节数一致");
    let pixels = imageops::thumbnail(&pixels, width, height);
    Arc::new(RenderImage::new([Frame::new(pixels)]))
}
