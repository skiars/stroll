use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    ptr::NonNull,
    sync::{OnceLock, mpsc},
    thread,
    time::{Duration, Instant},
};

use ureq::{Error, Timeout, http::Uri};

#[link(name = "proxy")]
unsafe extern "C" {
    fn px_proxy_factory_new() -> *mut c_void;
    fn px_proxy_factory_get_proxies(factory: *mut c_void, url: *const c_char) -> *mut *mut c_char;
    fn px_proxy_factory_free_proxies(proxies: *mut *mut c_char);
    fn px_proxy_factory_free(factory: *mut c_void);
}

#[link(name = "glib-2.0")]
unsafe extern "C" {
    fn g_main_context_new() -> *mut c_void;
    fn g_main_context_push_thread_default(context: *mut c_void);
    fn g_main_context_pop_thread_default(context: *mut c_void);
    fn g_main_context_iteration(context: *mut c_void, may_block: c_int) -> c_int;
    fn g_main_context_unref(context: *mut c_void);
}

struct Lookup {
    uri: Uri,
    deadline: Instant,
    response: mpsc::Sender<Result<Vec<String>, Error>>,
}

pub(super) fn routes(uri: &Uri, timeout: Duration) -> Result<Vec<String>, Error> {
    static WORKER: OnceLock<Option<mpsc::Sender<Lookup>>> = OnceLock::new();
    let deadline = Instant::now() + timeout;
    let worker = WORKER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::channel();
            thread::Builder::new()
                .name("system-proxy".into())
                .spawn(move || resolve(receiver))
                .ok()?;
            Some(sender)
        })
        .as_ref()
        .ok_or(Error::ConnectionFailed)?;
    let (response, result) = mpsc::channel();
    worker
        .send(Lookup {
            uri: uri.clone(),
            deadline,
            response,
        })
        .map_err(|_| Error::ConnectionFailed)?;
    match result.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(routes) => routes,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(Error::Timeout(Timeout::Global)),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::ConnectionFailed),
    }
}

fn resolve(requests: mpsc::Receiver<Lookup>) {
    // libproxy registers its configuration monitors in the thread-default GLib
    // context. GPUI does not run that context, so dispatch it here before lookup.
    let context = unsafe { g_main_context_new() };
    unsafe { g_main_context_push_thread_default(context) };
    let factory = NonNull::new(unsafe { px_proxy_factory_new() });
    for request in requests {
        if Instant::now() >= request.deadline {
            continue;
        }
        while unsafe { g_main_context_iteration(context, 0) } != 0 {}
        let result = factory
            .ok_or(Error::ConnectionFailed)
            .and_then(|factory| linux_routes(factory, &request.uri));
        let _ = request.response.send(result);
    }
    unsafe {
        if let Some(factory) = factory {
            px_proxy_factory_free(factory.as_ptr());
        }
        g_main_context_pop_thread_default(context);
        g_main_context_unref(context);
    }
}

fn linux_routes(factory: NonNull<c_void>, uri: &Uri) -> Result<Vec<String>, Error> {
    let routes = lookup(factory, uri)?;
    if routes == ["direct://"] && kde_manual_proxy() {
        // libproxy's KDE backend selects the manual SOCKS entry for non-web
        // schemes only. Ask for that entry with the same host and port.
        let mut parts = uri.clone().into_parts();
        parts.scheme = Some("socks".parse().map_err(|_| Error::InvalidProxyUrl)?);
        let port = uri
            .port_u16()
            .unwrap_or(if uri.scheme_str() == Some("https") {
                443
            } else {
                80
            });
        let host = uri.host().ok_or(Error::InvalidProxyUrl)?;
        parts.authority = Some(
            format!("{host}:{port}")
                .parse()
                .map_err(|_| Error::InvalidProxyUrl)?,
        );
        let socks = Uri::from_parts(parts).map_err(|_| Error::InvalidProxyUrl)?;
        return lookup(factory, &socks);
    }
    Ok(routes)
}

fn kde_manual_proxy() -> bool {
    if !std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|desktops| {
        desktops
            .split(':')
            .any(|desktop| desktop.eq_ignore_ascii_case("KDE"))
    }) {
        return false;
    }
    let Some(config) =
        dirs::config_dir().and_then(|dir| std::fs::read_to_string(dir.join("kioslaverc")).ok())
    else {
        return false;
    };
    config
        .lines()
        .map(str::trim)
        .skip_while(|line| *line != "[Proxy Settings]")
        .skip(1)
        .take_while(|line| !line.starts_with('['))
        .filter_map(|line| line.split_once('='))
        .any(|(key, value)| key.trim() == "ProxyType" && value.trim() == "1")
}

fn lookup(factory: NonNull<c_void>, uri: &Uri) -> Result<Vec<String>, Error> {
    let url = CString::new(uri.to_string()).map_err(|_| Error::InvalidProxyUrl)?;
    let proxies = unsafe { px_proxy_factory_get_proxies(factory.as_ptr(), url.as_ptr()) };
    if proxies.is_null() {
        return Err(Error::ConnectionFailed);
    }

    let mut result = Vec::new();
    let mut index = 0;
    // libproxy owns the null-terminated array and strings until we free them together.
    unsafe {
        while !(*proxies.add(index)).is_null() {
            if let Ok(proxy) = CStr::from_ptr(*proxies.add(index)).to_str() {
                result.push(proxy.to_owned());
            }
            index += 1;
        }
        px_proxy_factory_free_proxies(proxies);
    }
    Ok(result)
}
