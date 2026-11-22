use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs::{self, File, FileTimes, OpenOptions},
    io::{Cursor, Read as _, Write as _},
    net::IpAddr,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context as _, Result, ensure};
use gpui::{
    App, AppContext as _, Entity, EntityId, Global, Image, ImageAssetLoader, ImageCacheError,
    ImageSource, RenderImage, SharedString, SvgRenderer, WeakEntity, Window,
};
use gpui_component::text::data_url_image;
use image::{
    DynamicImage, Frame, ImageDecoder, ImageFormat, ImageReader, Limits, imageops::FilterType,
};
use sha2::{Digest, Sha256};
use ureq::{
    Agent, Body, Error,
    config::Config,
    http::{Response, Uri, header},
    unversioned::{
        resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver},
        transport::{DefaultConnector, NextTimeout},
    },
};
use url::Url;

use crate::{
    image_viewer,
    remote::SshSession,
    repository::{MAX_ASSET_BYTES, read_remote_bytes},
};

use super::{Destination, destination};

const MAX_DOWNLOAD_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PIXELS: u64 = 24_000_000;
const MAX_RENDER_DIMENSION: u32 = 1720;
const MAX_CACHE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 256;
const MAX_CONCURRENT_DOWNLOADS: usize = 4;
const MAX_CONCURRENT_FILE_LOADS: usize = 4;
const MAX_CONCURRENT_SSH_LOADS: usize = 2;
const MAX_DOCUMENT_SSH_IMAGES: usize = 128;
const MAX_DOCUMENT_SSH_BYTES: usize = 32 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const RETRY_DELAY: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;

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
    pub ssh: Option<SshSession>,
    pub allow_embedded: bool,
    pub display: Entity<DisplayedImages>,
}

impl DocumentImages {
    pub(super) fn source(&self, uri: &SharedString, block: Option<usize>) -> Option<ImageSource> {
        let request = self.request(uri, false)?;
        Some(request_source(
            request,
            self.ssh.clone(),
            self.display.clone(),
            block,
            false,
        ))
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
            self.ssh.clone(),
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
            if let Some(ssh) = &self.ssh {
                image_viewer::format(&path)?;
                ImageRequest::Remote {
                    connection: ssh.connection_id(),
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
    ssh: Option<SshSession>,
    display: Entity<DisplayedImages>,
    block: Option<usize>,
    preview: bool,
) -> ImageSource {
    if preview {
        ImageSource::Custom(Arc::new(move |window, cx| {
            use_image(&request, ssh.as_ref(), &display, None, true, window, cx)
        }))
    } else {
        ImageSource::deferred(move |visible, window, cx| {
            use_image(&request, ssh.as_ref(), &display, block, visible, window, cx)
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
}

impl DisplayedImages {
    pub(super) fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            cx.on_release(|images: &mut Self, cx| {
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
    directory: Option<PathBuf>,
    agent: Agent,
}

impl Global for OnlineImages {}

impl Default for OnlineImages {
    fn default() -> Self {
        // A proxy resolving the host itself would bypass the address filter.
        let config = Agent::config_builder().proxy(None).max_redirects(0).build();
        Self {
            pending: Rc::default(),
            directory: dirs::cache_dir().map(|path| path.join("stroll").join("online-images")),
            agent: Agent::with_parts(config, DefaultConnector::new(), PublicResolver::default()),
        }
    }
}

#[derive(Default)]
struct PendingImages {
    loads: HashMap<ImageRequest, HashMap<EntityId, WeakEntity<DisplayedImages>>>,
    waiting_views: HashSet<EntityId>,
}

fn use_image(
    request: &ImageRequest,
    ssh: Option<&SshSession>,
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

    if matches!(request, ImageRequest::Remote { .. }) && !displayed.can_load_ssh_image() {
        return Some(Err(ssh_budget_error()));
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
    if !cx.has_global::<OnlineImages>() {
        cx.set_global(OnlineImages::default());
    }
    let online = cx.global::<OnlineImages>();
    let pending = Rc::clone(&online.pending);
    let view = window.current_view();
    let mut state = pending.borrow_mut();
    if let Some(views) = state.loads.get_mut(request) {
        views.insert(view, display.downgrade());
        return None;
    }

    // Wake visible readers when a slot opens, without queuing closed documents.
    if !has_capacity(request, state.loads.keys()) {
        state.waiting_views.insert(view);
        return None;
    }
    state.loads.insert(
        request.clone(),
        HashMap::from([(view, display.downgrade())]),
    );
    drop(state);

    let directory = online.directory.clone();
    let agent = online.agent.clone();
    let svg = cx.svg_renderer();
    let ssh = ssh.cloned();
    let request = request.clone();
    let load = request.clone();
    cx.spawn(async move |cx| {
        let fetched = if let ImageRequest::Embedded(uri) = load {
            cx.background_executor()
                .spawn(async move {
                    let image = data_url_image(&uri)
                        .context("Invalid embedded image")?
                        .to_image_data(svg)?;
                    Ok((image, None))
                })
                .await
        } else {
            blocking::unblock(move || -> Result<_, ImageCacheError> {
                let mut ssh_bytes = None;
                let image = match load {
                    ImageRequest::Online { url, original } => {
                        load_image(&url, directory.as_deref(), &agent, &svg, original)?
                    }
                    ImageRequest::Local(path) => {
                        let bytes = read_local_image(&path)?;
                        ImageAssetLoader::decode(&bytes, &svg)?
                    }
                    ImageRequest::Remote { path, .. } => {
                        let format =
                            image_viewer::format(&path).context("Unsupported image format")?;
                        let ssh = ssh.as_ref().context("Missing SSH image session")?;
                        let bytes = read_remote_bytes(ssh, &path, true)?;
                        ssh_bytes = Some(bytes.len());
                        Image::from_bytes(format, bytes).to_image_data(svg)?
                    }
                    ImageRequest::Embedded(_) => unreachable!(),
                };
                Ok((image, ssh_bytes))
            })
            .await
        };
        cx.update(|cx| {
            let mut pending = pending.borrow_mut();
            let views = pending
                .loads
                .remove(&request)
                .expect("Expected an in-flight image request");
            let now = Instant::now();
            let mut displayed = false;
            for (view, display) in views {
                if let Some(display) = display.upgrade() {
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
            if !displayed && let Ok((image, _)) = fetched {
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

fn read_local_image(path: &Path) -> Result<Vec<u8>> {
    let path = path.canonicalize()?;
    ensure!(
        path.is_file() && path.components().all(|part| part.as_os_str() != ".git"),
        "Invalid local image path",
    );
    let file = File::open(path)?;
    ensure!(
        file.metadata()?.len() <= MAX_ASSET_BYTES,
        "Local image is too large"
    );
    let mut bytes = Vec::new();
    file.take(MAX_ASSET_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_ASSET_BYTES,
        "Local image is too large"
    );
    Ok(bytes)
}

fn load_image(
    url: &Url,
    directory: Option<&Path>,
    agent: &Agent,
    svg: &SvgRenderer,
    original: bool,
) -> Result<Arc<RenderImage>> {
    let name = format!(
        "{}.image",
        hex::encode(Sha256::digest(url.as_str().as_bytes()))
    );
    if let Some(directory) = directory
        && let Ok(image) = read_cached_image(directory, &name, svg, original)
    {
        return Ok(image);
    }
    let (bytes, cacheable) = download(url.clone(), agent, MAX_DOWNLOAD_BYTES)?;
    let image = decode(&bytes, svg, original)?;
    if cacheable && let Some(directory) = directory {
        let _ = store_cached_image(directory, &name, &bytes);
    }
    Ok(image)
}

fn read_cached_image(
    directory: &Path,
    name: &str,
    svg: &SvgRenderer,
    original: bool,
) -> Result<Arc<RenderImage>> {
    let path = directory.join(name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .or_else(|_| File::open(path))?;
    let mut bytes = Vec::new();
    (&file)
        .take(MAX_DOWNLOAD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_DOWNLOAD_BYTES,
        "cached image is too large"
    );
    let image = decode(&bytes, svg, original)?;
    let _ = file.set_times(FileTimes::new().set_modified(SystemTime::now()));
    Ok(image)
}

fn store_cached_image(directory: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    fs::create_dir_all(directory)?;
    // Serialize publication and eviction across application instances.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("cache.lock"))?;
    lock.lock()?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(bytes)?;
    file.persist(directory.join(name))?;
    prune_cache(directory)
}

fn prune_cache(directory: &Path) -> Result<()> {
    let mut files = Vec::new();
    let mut bytes = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "image")
            && path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| {
                    stem.len() == 64 && stem.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            && entry.file_type()?.is_file()
        {
            let metadata = entry.metadata()?;
            bytes += metadata.len();
            files.push((metadata.modified()?, metadata.len(), path));
        }
    }
    files.sort_unstable();
    let mut count = files.len();
    for (_, size, path) in files {
        if count <= MAX_CACHE_ENTRIES && bytes <= MAX_CACHE_BYTES {
            break;
        }
        fs::remove_file(path)?;
        count -= 1;
        bytes -= size;
    }
    Ok(())
}

fn download(mut url: Url, agent: &Agent, max_bytes: u64) -> Result<(Vec<u8>, bool)> {
    let mut cacheable = true;
    let started = Instant::now();
    for hop in 0..=MAX_REDIRECTS {
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.has_host()
                && url.username().is_empty()
                && url.password().is_none(),
            "invalid online image URL"
        );
        let remaining = REQUEST_TIMEOUT
            .checked_sub(started.elapsed())
            .context("online image request timed out")?;
        let response = agent
            .get(url.as_str())
            .header(header::ACCEPT, "image/*")
            .config()
            .timeout_global(Some(remaining))
            .build()
            .call()?;

        cacheable &= !response
            .headers()
            .get_all(header::CACHE_CONTROL)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|directive| {
                matches!(
                    directive.trim().to_ascii_lowercase().as_str(),
                    "no-store" | "no-cache"
                )
            });
        if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            ensure!(hop < MAX_REDIRECTS, "too many online image redirects");
            let location = response
                .headers()
                .get(header::LOCATION)
                .context("missing redirect location")?;
            let next = url.join(location.to_str()?)?;
            ensure!(
                url.scheme() != "https" || next.scheme() == "https",
                "insecure image redirect"
            );
            url = next;
            url.set_fragment(None);
            continue;
        }

        return Ok((read_body(response, max_bytes)?, cacheable));
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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
            download(redirect.clone(), &agent, 4)?,
            (vec![1, 2, 3, 4], true)
        );
        assert!(download(redirect, &agent, 3).is_err());
        assert!(download(Url::parse("https://example.com/downgrade")?, &agent, 4).is_err());
        assert!(
            download(
                Url::parse("https://user:pass@example.com/image")?,
                &agent,
                4
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
    fn disk_cache_evicts_oldest_files() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let old = directory.path().join(format!("{:064x}.image", 0));
        fs::write(&old, b"old image")?;
        OpenOptions::new()
            .write(true)
            .open(&old)?
            .set_times(FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))?;
        for index in 1..MAX_CACHE_ENTRIES {
            fs::write(
                directory.path().join(format!("{index:064x}.image")),
                b"image",
            )?;
        }
        store_cached_image(
            directory.path(),
            &format!("{MAX_CACHE_ENTRIES:064x}.image"),
            b"new image",
        )?;
        assert!(!old.exists());
        assert_eq!(
            fs::read_dir(directory.path())?.count(),
            MAX_CACHE_ENTRIES + 1
        );
        Ok(())
    }

    #[test]
    fn disk_cache_reads_after_reopening_and_replaces_invalid_files() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let calls = Arc::new(AtomicUsize::new(0));
        let request_count = calls.clone();
        let agent: Agent = Agent::config_builder()
            .middleware(move |req: Request<SendBody>, _: MiddlewareNext| {
                request_count.fetch_add(1, Ordering::Relaxed);
                let mut response = Response::new(Body::builder().data(test_svg()));
                if req.uri().path() == "/private" {
                    response
                        .headers_mut()
                        .insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
                }
                Ok(response)
            })
            .build()
            .into();
        let url = Url::parse("https://example.com/image")?;
        let svg = SvgRenderer::new(Arc::new(()));
        let first = load_image(&url, Some(directory.path()), &agent, &svg, false)?;
        let image_id = first.id;
        drop(first);
        let second = load_image(&url, Some(directory.path()), &agent, &svg, false)?;
        assert_ne!(second.id, image_id);
        assert_eq!(second.as_bytes(0).unwrap().len(), 8 * 4 * 4);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let cached = directory.path().join(format!(
            "{}.image",
            hex::encode(Sha256::digest(url.as_str()))
        ));
        fs::write(&cached, b"corrupt contents")?;
        load_image(&url, Some(directory.path()), &agent, &svg, false)?;
        assert_eq!(fs::read(&cached)?, test_svg());
        let private = Url::parse("https://example.com/private")?;
        for _ in 0..2 {
            load_image(&private, Some(directory.path()), &agent, &svg, false)?;
        }
        assert_eq!(calls.load(Ordering::Relaxed), 4);
        assert_eq!(fs::read_dir(directory.path())?.count(), 2);
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
                directory: Some(directory.path().to_owned()),
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
