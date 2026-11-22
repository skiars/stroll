use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io::Cursor,
    net::IpAddr,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, ensure};
use futures_channel::oneshot;
use gpui::{
    App, AppContext as _, Entity, EntityId, Global, Image, ImageAssetLoader, ImageCacheError,
    ImageSource, RenderImage, SharedString, SvgRenderer, WeakEntity, Window,
};
use gpui_component::text::data_url_image;
use image::{
    DynamicImage, Frame, ImageDecoder, ImageFormat, ImageReader, Limits, imageops::FilterType,
};
use ureq::{
    Agent, Body, Error,
    config::Config,
    http::{Response, StatusCode, Uri, header},
    unversioned::{
        resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver},
        transport::{DefaultConnector, NextTimeout},
    },
};
use url::Url;

use crate::{
    image_viewer,
    repository::{
        canonical_local_file,
        resources::{RemoteResources, read_local_resource},
    },
    resource_cache::{
        ResourceCache, SourceKey,
        http::{self, HttpVersion},
    },
};

use super::{Destination, destination};

const MAX_DOWNLOAD_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PIXELS: u64 = 24_000_000;
const MAX_RENDER_DIMENSION: u32 = 1720;
const MAX_CONCURRENT_DOWNLOADS: usize = 4;
const MAX_CONCURRENT_FILE_LOADS: usize = 4;
const MAX_CONCURRENT_SSH_LOADS: usize = 2;
const MAX_DOCUMENT_SSH_IMAGES: usize = 128;
const MAX_DOCUMENT_SSH_BYTES: usize = 32 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const RETRY_DELAY: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;
const IMAGE_ACCEPT: &str = "image/*";

type LoadedImage = std::result::Result<Arc<RenderImage>, ImageCacheError>;

#[derive(Clone, PartialEq, Eq, Hash)]
enum ImageRequest {
    Online { url: Url, original: bool },
    Local(PathBuf),
    Remote { connection: usize, path: PathBuf },
    Embedded(SharedString),
}

#[derive(Clone)]
pub(super) struct DocumentImages {
    pub root: PathBuf,
    pub document: PathBuf,
    pub remote: Option<RemoteResources>,
    pub allow_embedded: bool,
    pub display: Entity<DisplayedImages>,
}

impl DocumentImages {
    pub(super) fn source(&self, uri: &SharedString, block: Option<usize>) -> Option<ImageSource> {
        let request = self.request(uri, false)?;
        Some(request_source(
            request,
            self.remote.clone(),
            self.display.clone(),
            block,
            false,
        ))
    }

    pub(super) fn can_preview(&self, uri: &SharedString) -> bool {
        self.request(uri, true).is_some()
    }

    pub(super) fn preview_source(&self, uri: &SharedString, cx: &mut App) -> Option<ImageSource> {
        let request = self.request(uri, true)?;
        let display = if matches!(request, ImageRequest::Online { .. }) {
            DisplayedImages::new(cx)
        } else {
            if let Some((Ok(image), _)) = self.display.read(cx).results.get(&request) {
                return Some(ImageSource::Render(image.clone()));
            }
            self.display.clone()
        };
        Some(request_source(
            request,
            self.remote.clone(),
            display,
            None,
            true,
        ))
    }

    fn request(&self, uri: &SharedString, original: bool) -> Option<ImageRequest> {
        Some(if let Some(url) = image_url(uri) {
            ImageRequest::Online { url, original }
        } else if is_data_url(uri) {
            if !self.allow_embedded {
                return None;
            }
            ImageRequest::Embedded(uri.clone())
        } else {
            let Destination::Local { path, .. } = destination(&self.document, uri)? else {
                return None;
            };
            if let Some(remote) = &self.remote {
                image_viewer::format(&path)?;
                ImageRequest::Remote {
                    connection: remote.ssh.connection_id(),
                    path,
                }
            } else {
                ImageRequest::Local(self.root.join(path))
            }
        })
    }
}

pub(super) fn is_data_url(uri: &str) -> bool {
    uri.get(..5)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
}

fn image_url(uri: &str) -> Option<Url> {
    let mut url = Url::parse(uri).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_fragment(None);
    Some(url)
}

fn request_source(
    request: ImageRequest,
    remote: Option<RemoteResources>,
    display: Entity<DisplayedImages>,
    block: Option<usize>,
    preview: bool,
) -> ImageSource {
    if preview {
        ImageSource::Custom(Arc::new(move |window, cx| {
            use_image(&request, remote.as_ref(), &display, None, true, window, cx)
        }))
    } else {
        ImageSource::deferred(move |visible, window, cx| {
            use_image(
                &request,
                remote.as_ref(),
                &display,
                block,
                visible,
                window,
                cx,
            )
        })
    }
}

// Decoded pixels are owned by the open reader for layout and painting. The
// shared cache stores the original image files, including across app launches.
#[derive(Default)]
pub(super) struct DisplayedImages {
    results: HashMap<ImageRequest, (LoadedImage, Instant)>,
    blocks: RefCell<HashMap<ImageRequest, HashSet<usize>>>,
    changed: HashSet<usize>,
    ssh_images: usize,
    ssh_bytes: usize,
    ssh_paused: Arc<AtomicBool>,
}

impl DisplayedImages {
    pub(super) fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            cx.on_release(|images: &mut Self, cx| {
                images.pause_ssh(true);
                let images = std::mem::take(&mut images.results);
                cx.defer(move |cx| {
                    for (result, _) in images.into_values() {
                        if let Ok(image) = result
                            && Arc::strong_count(&image) == 1
                        {
                            cx.drop_image(image, None);
                        }
                    }
                });
            })
            .detach();
            Self::default()
        })
    }
    pub(super) fn pause_ssh(&self, paused: bool) {
        self.ssh_paused.store(paused, Ordering::Release);
    }
    fn can_load_ssh_image(&self) -> bool {
        self.ssh_images < MAX_DOCUMENT_SSH_IMAGES && self.ssh_bytes < MAX_DOCUMENT_SSH_BYTES
    }

    fn record_ssh_image(&mut self, bytes: usize) -> Result<(), ImageCacheError> {
        if !self.can_load_ssh_image() || bytes > MAX_DOCUMENT_SSH_BYTES - self.ssh_bytes {
            self.ssh_bytes = MAX_DOCUMENT_SSH_BYTES;
            return Err(ssh_budget_error());
        }
        self.ssh_images += 1;
        self.ssh_bytes += bytes;
        Ok(())
    }

    pub(super) fn take_changed_blocks(&mut self) -> HashSet<usize> {
        std::mem::take(&mut self.changed)
    }
}

fn ssh_budget_error() -> ImageCacheError {
    ImageCacheError::Asset("Document SSH image budget exceeded".into())
}

struct OnlineImages {
    pending: Rc<RefCell<PendingImages>>,
    cache: Option<ResourceCache>,
    agent: Agent,
}

impl Global for OnlineImages {}

impl Default for OnlineImages {
    fn default() -> Self {
        // A proxy resolving the host itself would bypass the address filter.
        let config = Agent::config_builder().proxy(None).max_redirects(0).build();
        Self {
            pending: Rc::default(),
            cache: ResourceCache::for_desktop(),
            agent: Agent::with_parts(config, DefaultConnector::new(), PublicResolver::default()),
        }
    }
}

#[derive(Default)]
struct PendingImages {
    loads: HashMap<ImageRequest, PendingImage>,
    waiting_views: HashSet<EntityId>,
    ssh_checks: HashMap<usize, Vec<(PathBuf, oneshot::Sender<CachedImageCheck>)>>,
}

type ImageInterest = Arc<Mutex<HashMap<EntityId, Weak<AtomicBool>>>>;

#[derive(Default)]
struct PendingImage {
    views: HashMap<EntityId, WeakEntity<DisplayedImages>>,
    interest: ImageInterest,
}

impl PendingImage {
    fn add_view(
        &mut self,
        view: EntityId,
        display: &Entity<DisplayedImages>,
        paused: &Arc<AtomicBool>,
    ) {
        self.views.insert(view, display.downgrade());
        self.interest
            .lock()
            .unwrap()
            .insert(view, Arc::downgrade(paused));
    }
}

type CachedImageCheck = Option<Result<Vec<u8>>>;

fn queue_ssh_check(
    pending: &Rc<RefCell<PendingImages>>,
    path: PathBuf,
    remote: RemoteResources,
    cache: ResourceCache,
    cx: &mut App,
) -> oneshot::Receiver<CachedImageCheck> {
    let connection = remote.ssh.connection_id();
    let (send, result) = oneshot::channel();
    let schedule = {
        let mut pending = pending.borrow_mut();
        let checks = pending.ssh_checks.entry(connection).or_default();
        checks.push((path, send));
        checks.len() == 1
    };
    if schedule {
        let pending = Rc::clone(pending);
        // Collect visible image requests from this draw without waiting for a timer.
        cx.defer(move |cx| {
            let (paths, replies): (Vec<_>, Vec<_>) = pending
                .borrow_mut()
                .ssh_checks
                .remove(&connection)
                .unwrap_or_default()
                .into_iter()
                .unzip();
            cx.spawn(async move |_| {
                let results =
                    blocking::unblock(move || remote.check_cached_assets(&paths, &cache)).await;
                for (reply, result) in replies.into_iter().zip(results) {
                    let _ = reply.send(result);
                }
            })
            .detach();
        });
    }
    result
}

fn use_image(
    request: &ImageRequest,
    remote: Option<&RemoteResources>,
    display: &Entity<DisplayedImages>,
    block: Option<usize>,
    visible: bool,
    window: &mut Window,
    cx: &mut App,
) -> Option<LoadedImage> {
    let displayed = display.read(cx);
    if let Some((result, completed)) = displayed.results.get(request)
        && (!visible || result.is_ok() || completed.elapsed() < RETRY_DELAY)
    {
        return Some(result.clone());
    }

    if matches!(request, ImageRequest::Remote { .. }) {
        if displayed.ssh_paused.load(Ordering::Acquire) {
            return None;
        }
        if !displayed.can_load_ssh_image() {
            return Some(Err(ssh_budget_error()));
        }
    }
    if let Some(block) = block {
        displayed
            .blocks
            .borrow_mut()
            .entry(request.clone())
            .or_default()
            .insert(block);
    }

    if !visible {
        return None;
    }
    let paused = displayed.ssh_paused.clone();
    if !cx.has_global::<OnlineImages>() {
        cx.set_global(OnlineImages::default());
    }
    let online = cx.global::<OnlineImages>();
    let pending = Rc::clone(&online.pending);
    let view = window.current_view();
    let mut state = pending.borrow_mut();
    if let Some(load) = state.loads.get_mut(request) {
        load.add_view(view, display, &paused);
        return None;
    }

    // Wake visible readers when a slot opens, without queuing closed documents.
    if !has_capacity(request, state.loads.keys()) {
        state.waiting_views.insert(view);
        return None;
    }
    let mut load = PendingImage::default();
    load.add_view(view, display, &paused);
    let interest = load.interest.clone();
    state.loads.insert(request.clone(), load);
    drop(state);

    let cache = online.cache.clone();
    let agent = online.agent.clone();
    let svg = cx.svg_renderer();
    let remote = remote.cloned();
    let ssh_check =
        match (request, &remote, &cache) {
            (ImageRequest::Remote { path, .. }, Some(remote), Some(cache)) => Some(
                queue_ssh_check(&pending, path.clone(), remote.clone(), cache.clone(), cx),
            ),
            _ => None,
        };
    let request = request.clone();
    let load = request.clone();
    cx.spawn(async move |cx| {
        let fetched = if let ImageRequest::Embedded(uri) = load {
            Some(
                cx.background_executor()
                    .spawn(async move {
                        let image = data_url_image(&uri)
                            .context("Invalid embedded image")?
                            .to_image_data(svg)?;
                        Ok((image, None))
                    })
                    .await,
            )
        } else {
            let checked = match ssh_check {
                Some(check) => check.await.ok().flatten(),
                None => None,
            };
            blocking::unblock(move || -> Result<Option<_>, ImageCacheError> {
                let mut ssh_bytes = None;
                let image = match load {
                    ImageRequest::Online { url, original } => {
                        load_image(&url, cache.as_ref(), &agent, &svg, original)?
                    }
                    ImageRequest::Local(path) => {
                        let path = canonical_local_file(&path)?;
                        let bytes = read_local_resource(&path, true, cache.as_ref())?;
                        ImageAssetLoader::decode(&bytes, &svg)?
                    }
                    ImageRequest::Remote { path, .. } => {
                        let format =
                            image_viewer::format(&path).context("Unsupported image format")?;
                        let remote = remote.as_ref().context("Missing SSH image session")?;
                        let wanted = || {
                            interest.lock().unwrap().values().any(|flag| {
                                flag.upgrade()
                                    .is_some_and(|paused| !paused.load(Ordering::Acquire))
                            })
                        };
                        if !wanted() {
                            return Ok(None);
                        }
                        let bytes = match checked {
                            Some(bytes) => Some(bytes?),
                            None => remote.read_while(&path, true, cache.as_ref(), &wanted)?,
                        };
                        let Some(bytes) = bytes else { return Ok(None) };
                        ssh_bytes = Some(bytes.len());
                        Image::from_bytes(format, bytes).to_image_data(svg)?
                    }
                    ImageRequest::Embedded(_) => unreachable!(),
                };
                Ok(Some((image, ssh_bytes)))
            })
            .await
            .transpose()
        };
        cx.update(|cx| {
            let mut pending = pending.borrow_mut();
            let views = pending
                .loads
                .remove(&request)
                .expect("Expected an in-flight image request")
                .views;
            let now = Instant::now();
            let mut displayed = false;
            for (view, display) in views {
                if let Some(display) = display.upgrade()
                    && let Some(fetched) = &fetched
                {
                    displayed = true;
                    display.update(cx, |images, cx| {
                        if images
                            .results
                            .get(&request)
                            .is_some_and(|(image, _)| image.is_ok())
                        {
                            return;
                        }
                        let result = fetched.clone().and_then(|(image, ssh_bytes)| {
                            if let Some(bytes) = ssh_bytes {
                                images.record_ssh_image(bytes)?;
                            }
                            Ok(image)
                        });
                        images.results.insert(request.clone(), (result, now));
                        if let Some(blocks) = images.blocks.get_mut().get(&request) {
                            images.changed.extend(blocks);
                            cx.notify();
                        }
                    });
                    cx.notify(view);
                }
            }
            if !displayed && let Some(Ok((image, _))) = fetched {
                cx.drop_image(image, None);
            }
            for view in pending.waiting_views.drain() {
                cx.notify(view);
            }
        });
    })
    .detach();
    None
}

fn has_capacity<'a>(
    request: &ImageRequest,
    active: impl Iterator<Item = &'a ImageRequest>,
) -> bool {
    match request {
        ImageRequest::Online { .. } => {
            active.filter(|key| matches!(key, ImageRequest::Online { .. })).count()
                < MAX_CONCURRENT_DOWNLOADS
        }
        ImageRequest::Remote { connection, .. } => {
            active.filter(|key| matches!(key, ImageRequest::Remote { connection: other, .. } if other == connection)).count()
                < MAX_CONCURRENT_SSH_LOADS
        }
        ImageRequest::Local(_) | ImageRequest::Embedded(_) => {
            active.filter(|key| matches!(key, ImageRequest::Local(_) | ImageRequest::Embedded(_))).count()
                < MAX_CONCURRENT_FILE_LOADS
        }
    }
}

fn load_image(
    url: &Url,
    cache: Option<&ResourceCache>,
    agent: &Agent,
    svg: &SvgRenderer,
    original: bool,
) -> Result<Arc<RenderImage>> {
    let downloaded = download(url.clone(), agent, MAX_DOWNLOAD_BYTES, cache)?;
    let image = decode(&downloaded.bytes, svg, original)?;
    if let (Some(cache), Some((key, version))) = (cache, downloaded.index) {
        let _ = cache.store(key, &version, &downloaded.bytes);
    }
    Ok(image)
}

struct DownloadedImage {
    bytes: Vec<u8>,
    index: Option<(SourceKey, HttpVersion)>,
}

fn image_key(url: &Url) -> SourceKey {
    SourceKey::new(
        "http",
        &[
            url.as_str().as_bytes(),
            IMAGE_ACCEPT.as_bytes(),
            http::ACCEPT_ENCODING.as_bytes(),
        ],
    )
}

fn download(
    mut url: Url,
    agent: &Agent,
    max_bytes: u64,
    mut cache: Option<&ResourceCache>,
) -> Result<DownloadedImage> {
    let started = Instant::now();
    for hop in 0..=MAX_REDIRECTS {
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.has_host()
                && url.username().is_empty()
                && url.password().is_none(),
            "invalid online image URL"
        );
        let key = image_key(&url);
        let mut cached = cache.and_then(|cache| cache.source::<HttpVersion>(key));
        if let (Some(cache), Some(entry)) = (cache, &cached)
            && entry.version.fresh()
        {
            if let Some(bytes) = cache.read_content(&entry.hash, max_bytes) {
                return Ok(DownloadedImage { bytes, index: None });
            }
            cached = None;
        }

        let response = loop {
            let remaining = REQUEST_TIMEOUT
                .checked_sub(started.elapsed())
                .context("online image request timed out")?;
            let mut request = agent
                .get(url.as_str())
                .header(header::ACCEPT, IMAGE_ACCEPT)
                .header(header::ACCEPT_ENCODING, http::ACCEPT_ENCODING);
            if let Some(entry) = &cached {
                let version = &entry.version;
                if let Some(etag) = &version.etag {
                    request = request.header(header::IF_NONE_MATCH, etag.as_str());
                } else if let Some(modified) = &version.last_modified {
                    request = request.header(header::IF_MODIFIED_SINCE, modified.as_str());
                }
            }
            let request_started = Instant::now();
            let response = request
                .config()
                .timeout_global(Some(remaining))
                .build()
                .call()?;
            if response.status() != StatusCode::NOT_MODIFIED {
                break (response, request_started.elapsed());
            }

            let (Some(cache), Some(entry)) = (cache, cached.take()) else {
                anyhow::bail!("Unexpected HTTP not-modified response");
            };
            ensure!(
                entry.version.can_validate(),
                "Unexpected HTTP not-modified response"
            );
            let version = HttpVersion::from_headers(
                response.headers(),
                Some(&entry.version),
                request_started.elapsed(),
            );
            if version.is_none() {
                cache.forget(key);
            }
            if let Some(bytes) = cache.read_content(&entry.hash, max_bytes) {
                return Ok(DownloadedImage {
                    bytes,
                    index: version.map(|version| (key, version)),
                });
            }
            // The content may have been evicted while the request was in flight.
        };
        let (response, request_time) = response;

        if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            ensure!(hop < MAX_REDIRECTS, "too many online image redirects");
            if let Some(previous_cache) = cache {
                previous_cache.forget(key);
                if http::no_store(response.headers()) {
                    cache = None;
                }
            }
            let location = response
                .headers()
                .get(header::LOCATION)
                .context("missing redirect location")?
                .to_str()?;
            let next = url.join(location)?;
            ensure!(
                url.scheme() != "https" || next.scheme() == "https",
                "insecure image redirect"
            );
            url = next;
            url.set_fragment(None);
            continue;
        }

        let index = cache.and_then(|cache| {
            let version = (response.status() == StatusCode::OK)
                .then(|| HttpVersion::from_headers(response.headers(), None, request_time))
                .flatten();
            if version.is_none() {
                cache.forget(key);
            }
            version.map(|version| (key, version))
        });
        return Ok(DownloadedImage {
            bytes: read_body(response, max_bytes)?,
            index,
        });
    }
    unreachable!("the redirect limit is checked before following a redirect");
}

fn read_body(mut response: Response<Body>, max_bytes: u64) -> Result<Vec<u8>> {
    ensure!(
        response.status().is_success(),
        "image HTTP status {}",
        response.status()
    );
    ensure!(
        response
            .body()
            .content_length()
            .is_none_or(|length| length <= max_bytes),
        "online image is too large"
    );
    Ok(response
        .body_mut()
        .with_config()
        .limit(max_bytes)
        .read_to_vec()?)
}

#[derive(Debug, Default)]
struct PublicResolver(DefaultResolver);

impl Resolver for PublicResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, Error> {
        let mut addresses = self.empty();
        for address in &self.0.resolve(uri, config, timeout)? {
            if is_public(address.ip()) {
                addresses.push(*address);
            }
        }
        if addresses.is_empty() {
            return Err(Error::HostNotFound);
        }
        Ok(addresses)
    }
}

fn is_public(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_documentation()
                && !matches!(
                    (a, b, c),
                    (0 | 224..=255, _, _)
                        | (100, 64..=127, _)
                        | (192, 0 | 88, _)
                        | (198, 18 | 19, _)
                )
        }
        IpAddr::V6(ip) => {
            let [a, b, ..] = ip.segments();
            // Global unicast, excluding protocol assignments, tunnels and documentation.
            matches!(a, 0x2000..=0x3fff)
                && !matches!(
                    (a, b),
                    (0x2001, 0..=0x01ff | 0x0db8) | (0x2002, _) | (0x3fff, _)
                )
        }
    }
}

fn decode(bytes: &[u8], svg: &SvgRenderer, original: bool) -> Result<Arc<RenderImage>> {
    let Ok(format) = image::guess_format(bytes) else {
        return Ok(svg.render_standalone_svg(
            str::from_utf8(bytes)?,
            if original { 4096 } else { MAX_RENDER_DIMENSION },
        )?);
    };
    ensure!(
        matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif
        ),
        "unsupported online image format"
    );

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);

    let mut decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    ensure!(
        u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "online image has too many pixels"
    );
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    let mut image = if !original && width.max(height) > MAX_RENDER_DIMENSION {
        image
            .resize(
                MAX_RENDER_DIMENSION,
                MAX_RENDER_DIMENSION,
                FilterType::Triangle,
            )
            .into_rgba8()
    } else {
        image.into_rgba8()
    };
    for pixel in image.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new([Frame::new(image)])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Render, TestApp, img};
    use image::{Rgba, RgbaImage};
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use ureq::{SendBody, http::Request, middleware::MiddlewareNext};

    #[test]
    fn decodes_online_formats_from_bytes() -> Result<()> {
        let svg = SvgRenderer::new(Arc::new(()));
        let raster =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(8, 4, Rgba([230, 20, 60, 255])));
        for format in [
            ImageFormat::Png,
            ImageFormat::Jpeg,
            ImageFormat::WebP,
            ImageFormat::Gif,
        ] {
            let mut bytes = Cursor::new(Vec::new());
            raster.write_to(&mut bytes, format)?;
            let image = decode(bytes.get_ref(), &svg, false)?;
            assert_eq!(image.as_bytes(0).unwrap().len(), 8 * 4 * 4);
            assert!(
                image.as_bytes(0).unwrap()[2] > 200,
                "expected BGRA for {format:?}"
            );
        }

        let badge = br#"<svg xmlns="http://www.w3.org/2000/svg" width="3440" height="4">
            <rect width="3440" height="4" fill="red"/>
        </svg>"#;
        let image = decode(badge, &svg, false)?;
        assert_eq!(image.as_bytes(0).unwrap().len(), 1720 * 2 * 4);
        let original = decode(badge, &svg, true)?;
        assert_eq!(original.as_bytes(0).unwrap().len(), 3440 * 4 * 4);
        let raster = DynamicImage::ImageRgba8(RgbaImage::new(2048, 32));
        let mut bytes = Cursor::new(Vec::new());
        raster.write_to(&mut bytes, ImageFormat::Png)?;
        assert_eq!(decode(bytes.get_ref(), &svg, false)?.size(0).width.0, 1720);
        assert_eq!(decode(bytes.get_ref(), &svg, true)?.size(0).width.0, 2048);
        assert!(decode(b"<html>not an image</html>", &svg, false).is_err());
        Ok(())
    }

    #[test]
    fn svg_skips_image_references() -> Result<()> {
        let file = tempfile::Builder::new().suffix(".png").tempfile()?;
        RgbaImage::from_pixel(2, 2, Rgba([255, 0, 0, 255])).save(file.path())?;
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2">
                <image width="2" height="2" href="{}"/>
            </svg>"#,
            file.path().display()
        );
        let image = decode(svg.as_bytes(), &SvgRenderer::new(Arc::new(())), false)?;
        assert_eq!(image.as_bytes(0).unwrap(), &[0; 16]);
        Ok(())
    }

    #[test]
    fn download_resolves_redirects_and_bounds_response() -> Result<()> {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let agent: Agent = Agent::config_builder()
            .middleware(move |req: Request<SendBody>, _: MiddlewareNext| {
                request_count.fetch_add(1, Ordering::Relaxed);
                assert_eq!(req.headers()[header::ACCEPT], "image/*");
                let response = match req.uri().path() {
                    "/redirect" => Response::builder()
                        .status(302)
                        .header(header::LOCATION, "https://example.com/image?variant=1")
                        .body(Body::builder().data(Vec::new())),
                    "/downgrade" => Response::builder()
                        .status(302)
                        .header(header::LOCATION, "http://example.com/image")
                        .body(Body::builder().data(Vec::new())),
                    "/image" => {
                        assert_eq!(req.uri().query(), Some("variant=1"));
                        Response::builder().body(Body::builder().data(vec![1, 2, 3, 4]))
                    }
                    _ => panic!("unexpected request"),
                };
                Ok(response.unwrap())
            })
            .build()
            .into();
        let redirect = Url::parse("http://example.com/redirect")?;
        assert_eq!(
            download(redirect.clone(), &agent, 4, None)?.bytes,
            vec![1, 2, 3, 4]
        );
        assert!(download(redirect, &agent, 3, None).is_err());
        assert!(
            download(
                Url::parse("https://example.com/downgrade")?,
                &agent,
                4,
                None
            )
            .is_err()
        );
        assert!(
            download(
                Url::parse("https://user:pass@example.com/image")?,
                &agent,
                4,
                None,
            )
            .is_err()
        );
        assert_eq!(requests.load(Ordering::Relaxed), 5);
        Ok(())
    }

    #[test]
    fn rejects_non_public_connection_addresses() {
        for address in [
            "0.0.0.0",
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "169.254.169.254",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "fe80::1",
            "2002:7f00:1::",
        ] {
            assert!(!is_public(address.parse().unwrap()), "{address}");
        }
        for address in ["1.1.1.1", "2606:4700::1111"] {
            assert!(is_public(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn https_cache_revalidates_and_shares_contents_with_local_files() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let cache = ResourceCache::new(directory.path().to_owned());
        let calls = Arc::new(AtomicUsize::new(0));
        let request_count = calls.clone();
        let agent: Agent = Agent::config_builder()
            .middleware(move |req: Request<SendBody>, _: MiddlewareNext| {
                request_count.fetch_add(1, Ordering::Relaxed);
                let mut response =
                    Response::builder().header(header::VARY, "Accept, Accept-Encoding");
                let bytes = match req.uri().path() {
                    "/private" => {
                        response = response.header(header::CACHE_CONTROL, "private, no-store");
                        test_svg()
                    }
                    "/vary" => {
                        response = response
                            .header(header::VARY, "*")
                            .header(header::CACHE_CONTROL, "max-age=600");
                        test_svg()
                    }
                    "/image" if req.headers().contains_key(header::IF_NONE_MATCH) => {
                        assert_eq!(req.headers()[header::IF_NONE_MATCH], "\"svg\"");
                        response = response
                            .status(304)
                            .header(header::CACHE_CONTROL, "max-age=600");
                        Vec::new()
                    }
                    "/image" => {
                        response = response
                            .header(header::ETAG, "\"svg\"")
                            .header(header::CACHE_CONTROL, "max-age=600")
                            .header(header::AGE, "600");
                        test_svg()
                    }
                    _ => panic!("unexpected request"),
                };
                Ok(response.body(Body::builder().data(bytes)).unwrap())
            })
            .build()
            .into();
        let url = Url::parse("https://example.com/image")?;
        let svg = SvgRenderer::new(Arc::new(()));
        let local = directory.path().join("diagram.svg");
        fs::write(&local, test_svg())?;
        assert_eq!(read_local_resource(&local, true, Some(&cache))?, test_svg());

        let first = load_image(&url, Some(&cache), &agent, &svg, false)?;
        let reopened = ResourceCache::new(directory.path().to_owned());
        let second = load_image(&url, Some(&reopened), &agent, &svg, false)?;
        assert_ne!(first.id, second.id);
        assert_eq!(second.as_bytes(0).unwrap().len(), 8 * 4 * 4);
        load_image(&url, Some(&reopened), &agent, &svg, false)?;
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(cache.count_contents(), 1);

        let cached = reopened.source::<HttpVersion>(image_key(&url)).unwrap();
        reopened.corrupt_content(&cached.hash)?;
        load_image(&url, Some(&reopened), &agent, &svg, false)?;
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        assert_eq!(
            reopened
                .read_content(&cached.hash, MAX_DOWNLOAD_BYTES)
                .unwrap(),
            test_svg()
        );

        for path in ["private", "vary"] {
            let alternate = url.join(path)?;
            for _ in 0..2 {
                load_image(&alternate, Some(&reopened), &agent, &svg, false)?;
            }
            assert!(
                reopened
                    .source::<HttpVersion>(image_key(&alternate))
                    .is_none()
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 7);
        Ok(())
    }

    fn test_svg() -> Vec<u8> {
        br#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="4">
            <rect width="8" height="4" fill="red"/>
        </svg>"#
            .to_vec()
    }

    #[test]
    fn image_source_shares_in_flight_requests_between_readers() -> Result<()> {
        struct Preview(ImageSource);
        impl Render for Preview {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
                use gpui::Styled as _;
                img(self.0.clone()).w(gpui::px(8.)).h(gpui::px(4.))
            }
        }

        let mut app = TestApp::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let request_count = calls.clone();
        let (release_response, response_gate) = async_channel::bounded(1);
        let agent: Agent = Agent::config_builder()
            .middleware(move |_: Request<SendBody>, _: MiddlewareNext| {
                if request_count.fetch_add(1, Ordering::Relaxed) == 0 {
                    response_gate.recv_blocking().unwrap();
                }
                Ok(Response::new(Body::builder().data(test_svg())))
            })
            .build()
            .into();
        let directory = tempfile::tempdir()?;
        let (first_display, second_display) = app.update(|cx| {
            cx.set_global(OnlineImages {
                pending: Rc::default(),
                cache: Some(ResourceCache::new(directory.path().to_owned())),
                agent,
            });
            (DisplayedImages::new(cx), DisplayedImages::new(cx))
        });

        let url = Url::parse("https://example.com/image")?;
        let source = |fragment: &str, display: &Entity<DisplayedImages>| {
            let url = image_url(&format!("{url}#{fragment}")).unwrap();
            request_source(
                ImageRequest::Online {
                    url,
                    original: false,
                },
                None,
                display.clone(),
                None,
                false,
            )
        };
        let mut first = app.open_window(|_, _| Preview(source("first", &first_display)));
        let mut second = app.open_window(|_, _| Preview(source("second", &second_display)));
        first.draw();
        second.draw();
        release_response.send_blocking(())?;

        let request = ImageRequest::Online {
            url,
            original: false,
        };
        let started = Instant::now();
        let loaded = loop {
            app.run_until_parked();
            if let Some((result, _)) =
                app.read(|cx| first_display.read(cx).results.get(&request).cloned())
            {
                break result?;
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "image load timed out"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        let second_image = app.read(|cx| second_display.read(cx).results[&request].0.clone())?;
        assert!(Arc::ptr_eq(&loaded, &second_image));
        first.draw();
        second.draw();
        assert_eq!(loaded.as_bytes(0).unwrap().len(), 8 * 4 * 4);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        Ok(())
    }
}
