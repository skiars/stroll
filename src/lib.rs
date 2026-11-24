rust_i18n::i18n!("locales", fallback = "en");

mod ai;
#[cfg(feature = "desktop")]
pub mod config;
#[cfg(feature = "desktop")]
mod doc_graphics;
#[cfg(feature = "desktop")]
pub mod docs;
mod git;
#[cfg(feature = "desktop")]
mod image_viewer;
mod language;
#[cfg(feature = "desktop")]
pub mod locale;
mod locator;
#[cfg(feature = "desktop")]
pub mod lsp;
mod paths;
mod process;
#[cfg(any(feature = "desktop", target_os = "linux"))]
mod process_memory;
#[path = "terminal/pty.rs"]
mod pty;
pub mod remote;
pub mod repository;
#[cfg(feature = "desktop")]
mod resource_cache;
mod session;
#[cfg(feature = "desktop")]
mod symbols;
pub mod tabs;
#[cfg(feature = "desktop")]
mod terminal;
#[cfg(all(test, feature = "desktop"))]
mod test_support;
#[cfg(feature = "desktop")]
pub mod workspace;
