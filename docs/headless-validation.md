# Headless validation

Run these commands from the repository root. GPUI's `TestAppContext` and `VisualTestContext` simulate windows, input, and layout. GPU tests use `WgpuHeadlessRenderer` to render offscreen and read back pixels. Both approaches work on Linux without a desktop session.

## Run tests

On Linux, including WSL, unset the display variables for the test process.

```sh
env -u DISPLAY -u WAYLAND_DISPLAY cargo test --release --locked
env -u DISPLAY -u WAYLAND_DISPLAY cargo test --release --locked --test smooth_scroll
env -u DISPLAY -u WAYLAND_DISPLAY cargo test --release --locked --test popups
```

The [scroll test](../tests/smooth_scroll.rs) sends input, advances frames, and checks layout. The Linux GPU cases in the [popup test](../tests/popups.rs) use pixel assertions to verify blur, clipping, and partial repaint.

The [editor interaction audit](editor-issues.md#automated-audit) runs Stroll and Zed on a private virtual display and compares keyboard, mouse, and workspace behavior.

On macOS and Windows, run `cargo test --release --locked --test smooth_scroll`. The wgpu screenshot example below uses a Linux-only test dependency. On Windows, `directx_blurs_the_scene_under_the_popup` opens a hidden Win32 window and reads back the DirectX render. Use the following command in sessions where no system windows are allowed.

```powershell
cargo test --release --locked -- --skip directx_blurs_the_scene_under_the_popup
```

## Select a GPU

Run `vulkaninfo --summary` in the environment where the test will run. Pixel tests can use a software adapter such as llvmpipe. For GPU performance tests, check that the test uses the intended hardware.

Set `ZED_DEVICE_ID` to the four-digit hexadecimal PCI `deviceID` listed by `vulkaninfo`. Use your device ID in place of the example value.

```sh
env -u DISPLAY -u WAYLAND_DISPLAY ZED_DEVICE_ID=2704 \
  cargo test --release --locked --test popups
```

The renderer tries other adapters if the requested device is unavailable or fails to initialize. Check the `Selected GPU adapter` log to verify which device was used. See [wgpu adapter initialization](../vendor/gpui-pre-wgpu-0.3.12/src/wgpu_context.rs) and the [WSL Vulkan setup](wsl-development.md).

## Capture screenshots offscreen

In a Linux integration test, pass `WgpuHeadlessRenderer` to `HeadlessAppContext::with_platform`. Load the view, wait for background work, draw a frame, and save the image returned by `capture_screenshot`. Use temporary repository and state directories when testing a workspace.

Save this example temporarily as `tests/headless_screenshot.rs`. Replace `ScreenshotProbe` with the view under test.

```rust
use std::sync::Arc;

use gpui::{
    AppContext as _, Context, HeadlessAppContext, PlatformHeadlessRenderer, Render, Styled, Window,
    div, px, rgb, size,
};
use gpui_wgpu::WgpuHeadlessRenderer;

struct ScreenshotProbe;

impl Render for ScreenshotProbe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        div().size_full().bg(rgb(0x365c85))
    }
}

#[test]
fn save_headless_screenshot() -> anyhow::Result<()> {
    let text_system = gpui_platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(text_system, Arc::new(()), || {
        Ok(Some(Box::new(WgpuHeadlessRenderer::new()?) as Box<dyn PlatformHeadlessRenderer>))
    });
    let window = cx.open_window(size(px(640.), px(360.)), |_, cx| cx.new(|_| ScreenshotProbe))?;
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))?;
    let output = std::env::var_os("STROLL_SCREENSHOT_PATH")
        .ok_or_else(|| anyhow::anyhow!("Set STROLL_SCREENSHOT_PATH to a PNG output path"))?;
    cx.capture_screenshot(window.into())?.save(output)?;
    Ok(())
}
```

```sh
screenshot_dir=$(mktemp -d)
env -u DISPLAY -u WAYLAND_DISPLAY \
  STROLL_SCREENSHOT_PATH="$screenshot_dir/probe.png" \
  cargo test --release --locked --test headless_screenshot
ls -l "$screenshot_dir/probe.png"
```

Window dimensions are in logical pixels. The PNG uses device pixels and its dimensions reflect the window scale. Remove the temporary test when finished.

System input methods, desktop drag and drop, file picker portals, and compositor presentation depend on the desktop session. Record the environment used to verify them.
