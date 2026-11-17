use std::{fmt, io::Read, path::Path, time::Duration};

use anyhow::{Result, ensure};
use rust_i18n::t;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use url::Url;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct Settings {
    pub base_url: String,
    pub model: String,
    pub auto_translate: bool,
    pub credential: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: String::new(),
            credential: String::new(),
            auto_translate: true,
        }
    }
}

impl Settings {
    pub(super) fn ready(&self) -> bool {
        !self.model.trim().is_empty()
            && !self.credential.is_empty()
            && endpoint(&self.base_url).is_some()
    }

    pub(crate) fn has_key_for(&self, base: &str) -> bool {
        !self.credential.is_empty()
            && endpoint(base)
                .is_some_and(|base| endpoint(&self.base_url).as_deref() == Some(base.as_str()))
    }
}

fn endpoint(text: &str) -> Option<String> {
    let url = Url::parse(text.trim()).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none())
    .then(|| {
        url.as_str()
            .strip_suffix('/')
            .unwrap_or(url.as_str())
            .to_owned()
    })
}

fn credential_entry(scope: &Path, base: &str, reference: &str) -> Result<keyring::Entry> {
    let mut hash = Sha256::new();
    hash.update(scope.as_os_str().as_encoded_bytes());
    hash.update([0]);
    hash.update(base);
    let account = format!("{}-{reference}", hex::encode(hash.finalize()));
    keyring::Entry::new("stroll-ai", &account)
        .map_err(|_| anyhow::anyhow!(t!("ai.credential_error")))
}

pub(crate) fn save_provider(
    scope: &Path,
    current: &Settings,
    base: &str,
    model: &str,
    key: &str,
) -> Result<Settings> {
    let model = model.trim();
    let key = key.trim();
    if base.trim().is_empty() && model.is_empty() && key.is_empty() {
        return Ok(Settings {
            auto_translate: current.auto_translate,
            ..Settings::default()
        });
    }
    let base_url = endpoint(base).ok_or_else(|| anyhow::anyhow!(t!("ai.invalid_url")))?;
    ensure!(!model.is_empty(), t!("ai.model_required"));
    let credential = if key.is_empty() {
        ensure!(current.has_key_for(&base_url), t!("ai.key_required"));
        current.credential.clone()
    } else {
        ensure!(!key.chars().any(char::is_control), t!("ai.invalid_key"));
        let nonce = tempfile::Builder::new().prefix("rr-ai-").tempfile()?;
        let reference = nonce
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        credential_entry(scope, &base_url, &reference)?
            .set_password(key)
            .map_err(|_| anyhow::anyhow!(t!("ai.credential_error")))?;
        reference
    };
    Ok(Settings {
        base_url,
        model: model.to_owned(),
        credential,
        auto_translate: current.auto_translate,
    })
}

pub(crate) fn fetch_models(
    scope: &Path,
    current: &Settings,
    base: &str,
    key: &str,
) -> Result<Vec<String>> {
    let base = endpoint(base).ok_or_else(|| anyhow::anyhow!(t!("ai.invalid_url")))?;
    let key = key.trim();
    let provider = if key.is_empty() {
        ensure!(current.has_key_for(&base), t!("ai.key_required"));
        Provider::open(current, scope).map_err(|error| anyhow::anyhow!(error.localized()))?
    } else {
        ensure!(!key.chars().any(char::is_control), t!("ai.invalid_key"));
        Provider {
            base,
            model: String::new(),
            key: key.to_owned(),
        }
    };
    let models = provider
        .list_models()
        .map_err(|error| anyhow::anyhow!(error.localized()))?;
    ensure!(!models.is_empty(), t!("ai.no_models"));
    Ok(models)
}

pub(crate) fn remove_credential(scope: &Path, settings: &Settings) {
    if let Some(base) = endpoint(&settings.base_url)
        && !settings.credential.is_empty()
        && let Ok(entry) = credential_entry(scope, &base, &settings.credential)
    {
        let _ = entry.delete_credential();
    }
}

pub(super) struct Provider {
    pub base: String,
    pub model: String,
    key: String,
}

pub(super) enum Thinking {
    Default,
    /// Disable thinking when the provider supports an override.
    Disable,
}

impl Provider {
    pub(super) fn open(settings: &Settings, scope: &Path) -> Result<Self, Failure> {
        let base = endpoint(&settings.base_url).ok_or(Failure::Credential)?;
        let key = credential_entry(scope, &base, &settings.credential)
            .and_then(|entry| Ok(entry.get_password()?))
            .map_err(|_| Failure::Credential)?;
        Ok(Self {
            base,
            model: settings.model.trim().to_owned(),
            key,
        })
    }

    fn list_models(&self) -> Result<Vec<String>, Failure> {
        #[derive(Deserialize)]
        struct ModelList {
            data: Vec<Model>,
        }

        #[derive(Deserialize)]
        struct Model {
            id: String,
        }

        let response = agent(Duration::from_secs(12))
            .get(format!("{}/models", self.base))
            .header("authorization", self.authorization()?)
            .call()
            .map_err(network_failure)?;
        let body = response_bytes(response, 2 * 1024 * 1024)?;
        let list: ModelList = serde_json::from_slice(&body).map_err(|_| Failure::ResponseJson)?;
        let mut ids: Vec<_> = list
            .data
            .into_iter()
            .map(|model| model.id)
            .filter(|id| !id.trim().is_empty() && !id.chars().any(char::is_control))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    fn authorization(&self) -> Result<ureq::http::HeaderValue, Failure> {
        let mut value = ureq::http::HeaderValue::try_from(format!("Bearer {}", self.key))
            .map_err(|_| Failure::Credential)?;
        value.set_sensitive(true);
        Ok(value)
    }

    fn completion_body(
        &self,
        prompt: &str,
        input: &str,
        tokens: u32,
        thinking: Thinking,
    ) -> serde_json::Value {
        let host = Url::parse(&self.base).ok();
        let host = host.as_ref().and_then(Url::host_str);
        let limit_param = if host == Some("api.openai.com") {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        let mut body = json!({
            "model": &self.model,
            "messages": [
                {"role": "system", "content": prompt.trim_end()},
                {"role": "user", "content": input},
            ],
            (limit_param): tokens,
            "stream": false,
        });
        if host == Some("api.deepseek.com") {
            body["temperature"] = json!(0);
            if matches!(thinking, Thinking::Disable) {
                body["thinking"] = json!({"type": "disabled"});
            }
        }
        body
    }

    pub(super) fn complete(
        &self,
        prompt: &str,
        input: &str,
        tokens: u32,
        timeout: Duration,
        max_response: usize,
        thinking: Thinking,
    ) -> Result<String, Failure> {
        let body = self.completion_body(prompt, input, tokens, thinking);
        let response = agent(timeout)
            .post(format!("{}/chat/completions", self.base))
            .header("authorization", self.authorization()?)
            .header("content-type", "application/json")
            .send(body.to_string())
            .map_err(network_failure)?;
        let bytes = response_bytes(response, max_response)?;
        let response: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| Failure::ResponseJson)?;
        let choice = &response["choices"][0];
        if choice["finish_reason"] != "stop" {
            return Err(Failure::ResponseIncomplete);
        }
        choice["message"]["content"]
            .as_str()
            .map(str::to_owned)
            .ok_or(Failure::ResponseIncomplete)
    }
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .new_agent()
}

fn response_bytes(
    response: ureq::http::Response<ureq::Body>,
    max_response: usize,
) -> Result<Vec<u8>, Failure> {
    let status = response.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            429 => Failure::RateLimited,
            code if status.is_server_error() => Failure::Unavailable(code),
            code => Failure::Rejected(code),
        });
    }
    let mut bytes = Vec::new();
    response
        .into_body()
        .into_reader()
        .take((max_response + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            let cause = error
                .get_ref()
                .and_then(|err| err.downcast_ref::<ureq::Error>());
            if error.kind() == std::io::ErrorKind::TimedOut
                || matches!(cause, Some(ureq::Error::Timeout(_)))
            {
                Failure::Timeout
            } else {
                Failure::Network
            }
        })?;
    if bytes.len() > max_response {
        return Err(Failure::ResponseTooLarge);
    }
    Ok(bytes)
}

fn network_failure(error: ureq::Error) -> Failure {
    match error {
        ureq::Error::Timeout(_) => Failure::Timeout,
        _ => Failure::Network,
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum Failure {
    Credential,
    Rejected(u16),
    Unavailable(u16),
    RateLimited,
    Timeout,
    Network,
    ResponseJson,
    ResponseIncomplete,
    ResponseShape,
    ResponseTooLarge,
    Busy,
    Cancelled,
    CacheWrite,
}

impl Failure {
    pub(super) fn response_error(self) -> bool {
        matches!(
            self,
            Self::ResponseJson
                | Self::ResponseIncomplete
                | Self::ResponseShape
                | Self::ResponseTooLarge
        )
    }

    pub(super) fn retryable(self) -> bool {
        matches!(
            self,
            Self::Unavailable(_)
                | Self::Timeout
                | Self::Network
                | Self::ResponseJson
                | Self::ResponseIncomplete
                | Self::ResponseShape
        )
    }

    pub(super) fn localized(self) -> String {
        match self {
            Self::Credential => t!("ai.credential_error"),
            Self::Rejected(_) => t!("ai.provider_rejected"),
            Self::RateLimited | Self::Busy => t!("ai.rate_limited"),
            Self::Timeout => t!("ai.timeout"),
            error if error.response_error() => t!("ai.response_invalid"),
            _ => t!("ai.provider_unavailable"),
        }
        .into_owned()
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        io::{BufRead as _, BufReader, Write as _},
        net::TcpListener,
        sync::{Arc, Mutex, Once},
        thread::{self, JoinHandle},
        time::Instant,
    };

    use super::*;

    pub(crate) type RequestLog = Vec<serde_json::Value>;

    pub(in crate::ai) fn mock_provider(
        model: &str,
        replies: Vec<(u16, &'static str)>,
    ) -> (Provider, JoinHandle<RequestLog>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in replies {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut connection = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(10))
                        }
                        Err(error) => panic!("mock AI request did not arrive: {error}"),
                    }
                };
                connection
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(&mut connection);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert!(line.starts_with("POST /v1/chat/completions "));
                let mut content_length = None;
                let mut authorized = false;
                loop {
                    line.clear();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(length) = lower.strip_prefix("content-length:") {
                        content_length = Some(length.trim().parse().unwrap());
                    }
                    if lower == "authorization: bearer mock-key\r\n" {
                        authorized = true;
                    }
                }
                assert!(authorized);
                let mut input = vec![0; content_length.unwrap()];
                reader.read_exact(&mut input).unwrap();
                requests.push(serde_json::from_slice(&input).unwrap());
                write!(connection,
                    "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                ).unwrap();
            }
            requests
        });
        (
            Provider {
                base,
                model: model.into(),
                key: "mock-key".into(),
            },
            server,
        )
    }

    pub(crate) fn install_test_keyring() {
        use keyring::credential::{Credential, CredentialApi, CredentialBuilderApi};
        use std::{any::Any, collections::HashMap};

        #[derive(Default)]
        struct TestStore(Mutex<HashMap<String, Arc<TestEntry>>>);

        #[derive(Default)]
        struct TestEntry {
            secret: Mutex<Option<Vec<u8>>>,
        }
        struct Handle(Arc<TestEntry>);

        impl CredentialBuilderApi for TestStore {
            fn build(
                &self,
                target: Option<&str>,
                service: &str,
                user: &str,
            ) -> keyring::Result<Box<Credential>> {
                let id = format!("{target:?}/{service}/{user}");
                let entry = self.0.lock().unwrap().entry(id).or_default().clone();
                Ok(Box::new(Handle(entry)))
            }

            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        impl CredentialApi for Handle {
            fn set_secret(&self, secret: &[u8]) -> keyring::Result<()> {
                *self.0.secret.lock().unwrap() = Some(secret.to_vec());
                Ok(())
            }

            fn get_secret(&self) -> keyring::Result<Vec<u8>> {
                self.0
                    .secret
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or(keyring::Error::NoEntry)
            }

            fn delete_credential(&self) -> keyring::Result<()> {
                self.0
                    .secret
                    .lock()
                    .unwrap()
                    .take()
                    .map(|_| ())
                    .ok_or(keyring::Error::NoEntry)
            }

            fn as_any(&self) -> &dyn Any {
                self
            }
        }

        static INSTALL: Once = Once::new();
        INSTALL
            .call_once(|| keyring::set_default_credential_builder(Box::new(TestStore::default())));
    }

    pub(crate) fn mock_endpoint(
        replies: Vec<(u16, &'static str)>,
    ) -> (String, JoinHandle<RequestLog>) {
        let (provider, worker) = mock_provider("model", replies);
        (provider.base, worker)
    }

    #[test]
    fn thinking_overrides_are_provider_specific() {
        let mut provider = Provider {
            base: "https://api.deepseek.com/v1".into(),
            model: "deepseek".into(),
            key: "mock-key".into(),
        };
        let body = provider.completion_body("prompt", "input", 1024, Thinking::Default);
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());

        let body = provider.completion_body("prompt", "input", 1024, Thinking::Disable);
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert_eq!(body["temperature"], json!(0));

        for base in ["https://api.openai.com/v1", "https://example.com/v1"] {
            provider.base = base.into();
            let body = provider.completion_body("prompt", "input", 1024, Thinking::Disable);
            assert!(body.get("thinking").is_none());
            assert!(body.get("reasoning_effort").is_none());
        }
    }

    #[test]
    fn normalizes_base_urls_and_binds_credentials_to_address() {
        assert_eq!(
            endpoint(" https://EXAMPLE.com/v1/ ").as_deref(),
            Some("https://example.com/v1")
        );
        for base in [
            "file:///tmp/a",
            "https://u:s@example.com",
            "https://example.com?q=k",
            "https://example.com/#frag",
        ] {
            assert!(endpoint(base).is_none(), "{base}");
        }
        let config = Path::new("/test/config.toml");
        let first = Settings {
            base_url: "https://example.com/v1".into(),
            model: "a".into(),
            credential: "reference".into(),
            ..Settings::default()
        };
        let updated = save_provider(config, &first, "https://example.com/v1/", "b", "").unwrap();
        assert_eq!(updated.credential, first.credential);
        assert!(save_provider(config, &first, "https://elsewhere.example/v1", "b", "").is_err());
        assert!(!save_provider(config, &first, "", "", "").unwrap().ready());
    }
}
