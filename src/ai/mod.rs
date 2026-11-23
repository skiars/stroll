pub(crate) mod context;

#[cfg(feature = "desktop")]
mod commit_message;
#[cfg(feature = "desktop")]
mod hover_translation;
#[cfg(feature = "desktop")]
mod provider;
#[cfg(feature = "desktop")]
mod translation_cache;

#[cfg(feature = "desktop")]
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[cfg(feature = "desktop")]
pub(crate) use provider::{Settings, fetch_models, remove_credential, save_provider};

#[cfg(all(test, feature = "desktop"))]
pub(crate) use provider::tests::{install_test_keyring, mock_endpoint};

#[cfg(feature = "desktop")]
#[derive(Clone)]
pub(crate) struct Service {
    config: Arc<Mutex<(u64, Settings)>>,
    cache: translation_cache::TranslationCache,
    credential_scope: PathBuf,
}

#[cfg(feature = "desktop")]
impl Service {
    pub(crate) fn new(settings: Settings, cache: PathBuf, config_file: Option<&Path>) -> Self {
        Self {
            config: Arc::new(Mutex::new((0, settings))),
            cache: translation_cache::TranslationCache::new(cache),
            credential_scope: config_file.unwrap_or(Path::new("")).to_owned(),
        }
    }

    pub(crate) fn configure(&self, settings: Settings) {
        let mut config = self.config.lock().unwrap();
        if config.1 != settings {
            *config = (config.0.wrapping_add(1), settings);
        }
    }

    pub(crate) fn snapshot(&self, translate: bool) -> Option<Snapshot> {
        let config = self.config.lock().unwrap();
        if !config.1.ready()
            || (translate && !config.1.auto_translate)
            || std::env::var("STROLL_AI_DISABLED").as_deref() == Ok("1")
        {
            return None;
        }
        Some(Snapshot {
            service: self.clone(),
            generation: config.0,
            settings: config.1.clone(),
        })
    }
}

#[cfg(feature = "desktop")]
#[derive(Clone)]
pub(crate) struct Snapshot {
    service: Service,
    generation: u64,
    settings: Settings,
}

#[cfg(feature = "desktop")]
impl Snapshot {
    pub(crate) fn is_current(&self) -> bool {
        self.service.config.lock().unwrap().0 == self.generation
    }
}

#[cfg(feature = "desktop")]
#[derive(Default)]
struct Cancellation(Arc<AtomicBool>);

#[cfg(feature = "desktop")]
impl Cancellation {
    fn flag(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }
}

#[cfg(feature = "desktop")]
impl Drop for Cancellation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
