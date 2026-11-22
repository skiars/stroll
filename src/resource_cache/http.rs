use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use ureq::http::{HeaderMap, HeaderName, header};

// Keep negotiated request fields stable and include their values in the source key.
pub(crate) const ACCEPT_ENCODING: &str = "gzip";

#[derive(Clone, Default, Deserialize, Serialize)]
struct CacheControl {
    revalidate: bool,
    max_age: Option<u64>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct HttpVersion {
    control: CacheControl,
    expires: Option<SystemTime>,
    fresh_until: SystemTime,
    pub(crate) etag: Option<String>,
    pub(crate) last_modified: Option<String>,
}

impl HttpVersion {
    pub(crate) fn fresh(&self) -> bool {
        SystemTime::now() < self.fresh_until
    }

    pub(crate) fn can_validate(&self) -> bool {
        self.etag.is_some() || self.last_modified.is_some()
    }

    pub(crate) fn from_headers(
        headers: &HeaderMap,
        previous: Option<&Self>,
        request_time: Duration,
    ) -> Option<Self> {
        if no_store(headers)
            || headers
                .get_all(header::VARY)
                .iter()
                .any(|value| value.to_str().is_err())
            || directives(headers, header::VARY).any(|field| {
                !field.eq_ignore_ascii_case("accept")
                    && !field.eq_ignore_ascii_case("accept-encoding")
            })
        {
            return None;
        }
        let mut control = previous
            .map(|version| version.control.clone())
            .unwrap_or_default();
        if headers.contains_key(header::CACHE_CONTROL) {
            let mut ages = directives(headers, header::CACHE_CONTROL)
                .filter(|directive| directive_name(directive).eq_ignore_ascii_case("max-age"));
            control.max_age = ages.next().map(|directive| {
                directive
                    .split_once('=')
                    .and_then(|(_, value)| value.trim().trim_matches('"').parse().ok())
                    .filter(|_| ages.next().is_none())
                    .unwrap_or(0)
            });
            control.revalidate = directives(headers, header::CACHE_CONTROL)
                .any(|directive| directive_name(directive).eq_ignore_ascii_case("no-cache"));
        }
        let expires = if headers.contains_key(header::EXPIRES) {
            http_date(headers, header::EXPIRES).or(Some(SystemTime::UNIX_EPOCH))
        } else {
            previous.and_then(|version| version.expires)
        };
        let now = SystemTime::now();
        let date = http_date(headers, header::DATE).unwrap_or(now);
        let age = header_text(headers, header::AGE)
            .and_then(|value| value.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or_default();
        let age = now
            .duration_since(date)
            .unwrap_or_default()
            .max(age.saturating_add(request_time));
        let lifetime = if control.revalidate {
            Duration::ZERO
        } else if let Some(seconds) = control.max_age {
            Duration::from_secs(seconds)
        } else {
            expires
                .and_then(|expires| expires.duration_since(date).ok())
                .unwrap_or_default()
        };
        Some(Self {
            control,
            expires,
            fresh_until: now.checked_add(lifetime.saturating_sub(age)).unwrap_or(now),
            etag: header_text(headers, header::ETAG)
                .map(str::to_owned)
                .or_else(|| previous.and_then(|version| version.etag.clone())),
            last_modified: header_text(headers, header::LAST_MODIFIED)
                .map(str::to_owned)
                .or_else(|| previous.and_then(|version| version.last_modified.clone())),
        })
    }
}

pub(crate) fn no_store(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::CACHE_CONTROL)
        .iter()
        .any(|value| value.to_str().is_err())
        || directives(headers, header::CACHE_CONTROL)
            .any(|directive| directive_name(directive).eq_ignore_ascii_case("no-store"))
}

fn directive_name(directive: &str) -> &str {
    directive.split('=').next().unwrap_or_default().trim()
}

fn directives(headers: &HeaderMap, header: HeaderName) -> impl Iterator<Item = &str> {
    headers
        .get_all(header)
        .iter()
        .flat_map(|value| {
            // A quoted Cache-Control value can itself contain commas.
            let mut quoted = false;
            let mut escaped = false;
            value.to_str().unwrap_or_default().split(move |ch| {
                let separator = ch == ',' && !quoted;
                if escaped {
                    escaped = false;
                } else if ch == '\\' && quoted {
                    escaped = true;
                } else if ch == '"' {
                    quoted = !quoted;
                }
                separator
            })
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn header_text(headers: &HeaderMap, name: HeaderName) -> Option<&str> {
    headers.get(name)?.to_str().ok()
}

fn http_date(headers: &HeaderMap, name: HeaderName) -> Option<SystemTime> {
    let date = chrono::DateTime::parse_from_rfc2822(header_text(headers, name)?).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(date.timestamp().try_into().ok()?))
}
