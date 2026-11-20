use std::sync::Arc;

use gpui::PlatformTextSystem;

pub(crate) fn text_system(_headless: bool) -> Arc<dyn PlatformTextSystem> {
    // Core Text can shape text on a test worker without creating AppKit's main-thread platform.
    #[cfg(target_os = "macos")]
    {
        Arc::new(gpui_apple::AppleTextSystem::new())
    }

    #[cfg(not(target_os = "macos"))]
    {
        gpui_platform::current_platform(_headless).text_system()
    }
}
