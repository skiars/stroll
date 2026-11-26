use std::{fmt, net::IpAddr, sync::Arc};

use ureq::{
    Error, ProxyProtocol,
    http::Uri,
    unversioned::{
        resolver::{ResolvedSocketAddrs, Resolver},
        transport::{
            ConnectProxyConnector, ConnectionDetails, Connector, DefaultConnector, RustlsConnector,
            Transport,
        },
    },
};

#[derive(Debug)]
pub(super) struct RouteConnector {
    default: Arc<DefaultConnector>,
    tunnel: ConnectProxyConnector,
    tls: RustlsConnector,
    public_destination: bool,
}

impl RouteConnector {
    pub(super) fn new(public_destination: bool) -> Self {
        Self {
            default: Arc::new(DefaultConnector::new()),
            tunnel: ConnectProxyConnector::default(),
            tls: RustlsConnector::default(),
            public_destination,
        }
    }

    fn connect_route(&self, details: &ConnectionDetails<'_>) -> Result<Box<dyn Transport>, Error> {
        if !self.public_destination {
            let routed = ConnectionDetails {
                run_connector: self.direct_connector(),
                ..copy_details(details)
            };
            return self
                .default
                .connect(&routed, None)?
                .ok_or(Error::ConnectionFailed);
        }

        if details.config.proxy().is_some_and(|proxy| {
            matches!(proxy.protocol(), ProxyProtocol::Http | ProxyProtocol::Https)
        }) {
            return self.connect_public_tunnel(details);
        }

        // For a direct connection or locally resolved SOCKS, ureq will dial the
        // addresses here. Proxy server addresses are resolved separately.
        let checked = ConnectionDetails {
            addrs: public_addresses(&details.addrs, details.resolver)?,
            ..copy_details(details)
        };
        self.default
            .connect(&checked, None)?
            .ok_or(Error::ConnectionFailed)
    }

    fn direct_connector(
        &self,
    ) -> Arc<impl Fn(&ConnectionDetails<'_>) -> Result<Box<dyn Transport>, Error> + use<>> {
        let connector = self.default.clone();
        Arc::new(move |details: &ConnectionDetails<'_>| {
            connector
                .connect(details, None)?
                .ok_or(Error::ConnectionFailed)
        })
    }

    fn connect_public_tunnel(
        &self,
        details: &ConnectionDetails<'_>,
    ) -> Result<Box<dyn Transport>, Error> {
        let addresses = details
            .resolver
            .resolve(details.uri, details.config, details.timeout)?;
        let addresses = public_addresses(&addresses, details.resolver)?;
        let destination = addresses.first().ok_or(Error::HostNotFound)?;
        // Pin CONNECT to the checked address. Keep the original URI for TLS and
        // HTTP. ureq 3.5.2's HTTP CONNECT resolve_target option repeats the port.
        let pinned = Uri::builder()
            .scheme(details.uri.scheme().cloned().ok_or(Error::HostNotFound)?)
            .authority(destination.to_string())
            .path_and_query("/")
            .build()
            .map_err(|_| Error::ConnectionFailed)?;
        let tunnel_details = ConnectionDetails {
            uri: &pinned,
            // This nested connection addresses the configured proxy itself, which
            // may be local. It must not be treated as another document URL.
            run_connector: self.direct_connector(),
            ..copy_details(details)
        };
        let tunnel = self.tunnel.connect(&tunnel_details, None::<()>)?;
        self.tls
            .connect(details, tunnel)?
            .map(Transport::boxed)
            .ok_or(Error::ConnectionFailed)
    }
}

impl Connector for RouteConnector {
    type Out = Box<dyn Transport>;

    fn connect(
        &self,
        details: &ConnectionDetails<'_>,
        _: Option<()>,
    ) -> Result<Option<Self::Out>, Error> {
        self.connect_route(details)
            .map(Some)
            .map_err(|error| Error::Other(Box::new(ConnectError(error))))
    }
}

fn copy_details<'a>(details: &ConnectionDetails<'a>) -> ConnectionDetails<'a> {
    ConnectionDetails {
        uri: details.uri,
        addrs: details.addrs.clone(),
        resolver: details.resolver,
        config: details.config,
        request_level: details.request_level,
        now: details.now,
        timeout: details.timeout,
        current_time: details.current_time.clone(),
        run_connector: details.run_connector.clone(),
        register_shutdown: details.register_shutdown.clone(),
    }
}

#[derive(Debug)]
struct ConnectError(Error);

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for ConnectError {}

pub(super) fn connection_error(error: Error) -> Result<Error, Error> {
    match error {
        Error::Other(error) => match error.downcast::<ConnectError>() {
            Ok(error) => Ok(error.0),
            Err(error) => Err(Error::Other(error)),
        },
        // Resolution can also fail before ureq calls the connector.
        Error::HostNotFound | Error::Timeout(ureq::Timeout::Resolve) => Ok(error),
        error => Err(error),
    }
}

fn public_addresses(
    addresses: &ResolvedSocketAddrs,
    resolver: &dyn Resolver,
) -> Result<ResolvedSocketAddrs, Error> {
    let mut public = resolver.empty();
    for address in addresses {
        if is_public(address.ip()) {
            public.push(*address);
        }
    }
    if public.is_empty() {
        return Err(Error::HostNotFound);
    }
    Ok(public)
}

pub(crate) fn is_public(ip: IpAddr) -> bool {
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
