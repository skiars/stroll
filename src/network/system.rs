use std::time::Duration;
use ureq::{Error, http::Uri};

#[cfg(all(target_os = "linux", feature = "desktop"))]
mod linux;
#[cfg(all(target_os = "linux", feature = "desktop"))]
pub(super) fn routes(uri: &Uri, timeout: Duration) -> Result<Vec<String>, Error> {
    linux::routes(uri, timeout)
}

#[cfg(all(any(windows, target_os = "macos"), feature = "desktop"))]
pub(super) fn routes(uri: &Uri, timeout: Duration) -> Result<Vec<String>, Error> {
    use proxyres::resolver::Resolver;
    use ureq::Timeout;

    let mut resolver = Resolver::new();
    resolver
        .get_proxies_for_url(&uri.to_string())
        .map_err(|_| Error::ConnectionFailed)?;
    if !resolver.wait(Some(timeout)) {
        return Err(Error::Timeout(Timeout::Global));
    }
    if resolver.error().is_some() {
        return Err(Error::ConnectionFailed);
    }
    let proxies: Vec<_> = resolver.proxies().into_iter().map(str::to_owned).collect();
    #[cfg(target_os = "macos")]
    {
        use proxyres::config;
        if proxies == ["direct://"]
            && !config::auto_discover()
            && config::auto_config_url().is_none()
            && config::proxy(uri.scheme_str().unwrap_or("http")).is_none()
            && let Some(socks) = config::proxy("socks")
        {
            // macOS exposes SOCKS independently of its per-scheme web proxies.
            let bypass = config::bypass_list().unwrap_or_default();
            let local = bypass.split(',').any(|rule| rule.trim() == "<local>")
                && uri.host().is_some_and(|host| !host.contains(['.', ':']));
            if !local && !super::matches_bypass(uri, &bypass)? {
                return Ok(vec![format!("socks5://{socks}")]);
            }
        }
    }
    Ok(proxies)
}

#[cfg(not(all(
    any(target_os = "linux", target_os = "macos", windows),
    feature = "desktop"
)))]
pub(super) fn routes(_: &Uri, _: Duration) -> Result<Vec<String>, Error> {
    Ok(vec!["direct://".into()])
}
