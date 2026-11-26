//! Proxy selection for HTTP requests made by Stroll.

mod connector;
mod system;

use std::time::{Duration, Instant};

use ureq::{
    Agent, AsSendBody, Body, Error, Proxy, ProxyProtocol, RequestExt, Timeout,
    http::{HeaderValue, Request, Response, Uri, header, uri::Scheme},
    unversioned::resolver::DefaultResolver,
};

use self::connector::{RouteConnector, connection_error};

#[cfg(all(test, feature = "desktop"))]
pub(crate) use self::connector::is_public;

pub(crate) struct Client {
    agent: Agent,
    public_destination: bool,
    #[cfg(test)]
    mock: bool,
}

impl Client {
    pub(crate) fn new() -> Self {
        Self::with_public_destination(false)
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn public() -> Self {
        Self::with_public_destination(true)
    }

    fn with_public_destination(public_destination: bool) -> Self {
        let config = Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .build();
        Self {
            agent: Agent::with_parts(
                config,
                RouteConnector::new(public_destination),
                DefaultResolver::default(),
            ),
            public_destination,
            #[cfg(test)]
            mock: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn mock(agent: Agent) -> Self {
        Self {
            agent,
            public_destination: false,
            mock: true,
        }
    }

    // Callers handle redirects so that every destination gets its own proxy decision.
    pub(crate) fn send<T: AsSendBody + Clone>(
        &self,
        request: Request<T>,
        timeout: Duration,
    ) -> Result<Response<Body>, Error> {
        let started = Instant::now();
        let routes = self.routes(request.uri(), timeout)?;
        self.send_routes(request, routes, started, timeout)
    }

    fn send_routes<T: AsSendBody + Clone>(
        &self,
        request: Request<T>,
        routes: Vec<Route>,
        started: Instant,
        timeout: Duration,
    ) -> Result<Response<Body>, Error> {
        let mut last_error = Error::InvalidProxyUrl;
        for route in routes {
            let proxy = match route {
                Route::Direct => None,
                Route::Proxy(proxy) => Some(proxy),
            };
            let remaining = timeout
                .checked_sub(started.elapsed())
                .ok_or(Error::Timeout(Timeout::Global))?;
            let result = request
                .clone()
                .with_agent(&self.agent)
                .configure()
                .timeout_global(Some(remaining))
                .proxy(proxy)
                .run();
            match result {
                Ok(response) => return Ok(response),
                Err(error) => {
                    // Only try another route if no HTTP request has been sent.
                    last_error = connection_error(error)?;
                }
            }
        }
        Err(last_error)
    }

    fn routes(&self, uri: &Uri, timeout: Duration) -> Result<Vec<Route>, Error> {
        if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.host().is_none() {
            return Err(Error::BadUri("expected an absolute HTTP(S) URL".into()));
        }
        #[cfg(test)]
        if self.mock {
            return Ok(vec![Route::Direct]);
        }

        if let Some(route) = environment_route(uri, |name| std::env::var(name).ok())? {
            return Ok(vec![
                route.with_public_destination(self.public_destination)?,
            ]);
        }

        system::routes(uri, timeout)?
            .into_iter()
            // A system list can contain proxy protocols this HTTP client does not support.
            .filter_map(|entry| Route::parse(&entry).ok())
            .map(|route| route.with_public_destination(self.public_destination))
            .collect()
    }
}

enum Route {
    Direct,
    Proxy(Proxy),
}

impl Route {
    fn parse(value: &str) -> Result<Self, Error> {
        if value.eq_ignore_ascii_case("direct://") {
            return Ok(Self::Direct);
        }
        Proxy::new(value).map(Self::Proxy)
    }

    fn with_public_destination(self, public_destination: bool) -> Result<Self, Error> {
        if !public_destination {
            return Ok(self);
        }
        let Self::Proxy(proxy) = self else {
            return Ok(self);
        };
        match proxy.protocol() {
            ProxyProtocol::Socks4
            | ProxyProtocol::Socks4A
            | ProxyProtocol::Socks5
            | ProxyProtocol::Socks5h => {
                let mut builder = Proxy::builder(proxy.protocol())
                    .host(proxy.host())
                    .port(proxy.port())
                    .resolve_target(true);
                if let Some(username) = proxy.username() {
                    builder = builder.username(username);
                }
                if let Some(password) = proxy.password() {
                    builder = builder.password(password);
                }
                builder.build().map(Self::Proxy)
            }
            _ => Ok(Self::Proxy(proxy)),
        }
    }
}

fn environment_route(
    uri: &Uri,
    get: impl Fn(&str) -> Option<String>,
) -> Result<Option<Route>, Error> {
    if let Some(exclusions) = get("NO_PROXY").or_else(|| get("no_proxy"))
        && matches_bypass(uri, &exclusions)?
    {
        return Ok(Some(Route::Direct));
    }

    let names: &[&str] = if uri.scheme() == Some(&Scheme::HTTPS) {
        &[
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ]
    } else {
        &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
    };
    names
        .iter()
        .copied()
        .filter_map(get)
        .find(|value| !value.trim().is_empty())
        .map(|value| Route::parse(value.trim()))
        .transpose()
}

fn matches_bypass(uri: &Uri, exclusions: &str) -> Result<bool, Error> {
    let bypass = exclusions
        .split(',')
        .fold(Proxy::builder(ProxyProtocol::Http), |builder, exclusion| {
            builder.no_proxy(exclusion.trim())
        });
    Ok(bypass.build()?.is_no_proxy(uri))
}

pub(crate) fn redirect(response: &Response<Body>) -> Option<&HeaderValue> {
    matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308)
        .then(|| response.headers().get(header::LOCATION))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_proxy_matches_scheme_and_respects_bypass() -> anyhow::Result<()> {
        let lookup = |name: &str| {
            match name {
                "HTTPS_PROXY" => Some("http://secure.example:8080"),
                "http_proxy" => Some("http://plain.example:8080"),
                "all_proxy" => Some("socks5://fallback.example:1080"),
                "no_proxy" => Some("localhost, .local.example"),
                _ => None,
            }
            .map(str::to_owned)
        };
        for (url, host) in [
            ("https://example.com/", "secure.example"),
            ("http://example.com/", "plain.example"),
        ] {
            let uri = url.parse()?;
            let Some(Route::Proxy(proxy)) = environment_route(&uri, lookup)? else {
                anyhow::bail!("expected the explicit proxy for {url}");
            };
            assert_eq!(proxy.host(), host);
        }
        for url in ["http://localhost/", "https://api.local.example/"] {
            assert!(matches!(
                environment_route(&url.parse()?, lookup)?,
                Some(Route::Direct)
            ));
        }
        assert!(environment_route(&"https://example.com/".parse()?, |_| None)?.is_none());
        Ok(())
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn public_requests_tunnel_to_checked_ip_and_reject_private_destinations() -> anyhow::Result<()>
    {
        use std::{
            io::{BufRead, BufReader, Write},
            net::{TcpListener, TcpStream},
            thread,
        };

        fn read_headers(stream: &mut BufReader<TcpStream>) -> anyhow::Result<String> {
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                if stream.read_line(&mut line)? == 0 || line == "\r\n" {
                    return Ok(headers);
                }
                headers.push_str(&line);
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let proxy_route = format!("http://{}", listener.local_addr()?);
        let client = Client::public();
        let timeout = Duration::from_secs(2);
        let private = Request::get("http://127.0.0.1:8080/image").body(())?;
        assert!(matches!(
            client.send_routes(
                private,
                vec![Route::parse(&proxy_route)?],
                Instant::now(),
                timeout
            ),
            Err(Error::HostNotFound)
        ));

        listener.set_nonblocking(true)?;
        let server = thread::spawn(move || -> anyhow::Result<(String, String)> {
            let deadline = Instant::now() + timeout;
            let (stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            stream.set_read_timeout(Some(timeout))?;
            let mut stream = BufReader::new(stream);
            let connect = read_headers(&mut stream)?;
            stream
                .get_mut()
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
            let request = read_headers(&mut stream)?;
            stream
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")?;
            Ok((connect, request))
        });
        let public = Request::get("http://93.184.216.34:8080/image").body(())?;
        let blocked = TcpListener::bind("127.0.0.1:0")?;
        let failed_proxy = format!("http://{}", blocked.local_addr()?);
        drop(blocked);
        let routes = vec![Route::parse(&failed_proxy)?, Route::parse(&proxy_route)?];
        let result = client.send_routes(public, routes, Instant::now(), timeout);
        let (connect, request) = server.join().expect("test proxy panicked")?;
        assert_eq!(result?.status(), 200);
        assert!(
            connect.starts_with("CONNECT 93.184.216.34:8080 HTTP/1.1\r\n"),
            "{connect}"
        );
        assert!(request.starts_with("GET /image HTTP/1.1\r\n"), "{request}");
        Ok(())
    }
}
