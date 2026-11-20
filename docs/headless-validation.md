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

On macOS, run `cargo test --release --locked --test smooth_scroll --test scrollbars --test tooltips` for shared UI integration tests. Workspace tests in the full suite use Core Text with GPUI's test platform. The wgpu screenshot example below uses a Linux-only test dependency.

On Windows, run `cargo test --release --locked --test smooth_scroll`. The popup test `directx_blurs_the_scene_under_the_popup` opens a hidden Win32 window and reads back the DirectX render. Use the following command in sessions where no system windows are allowed.

```powershell
cargo test --release --locked -- --skip directx_blurs_the_scene_under_the_popup
```

## Git image diff scrolling

The release benchmark generates a temporary Git repository containing synthetic images and a dense text patch. With the history and file panes visible, it replays the same image range with cold and warm thumbnails, reverses direction, and repeats in split view. The offscreen run also measures the text patch.

```sh
bench_dir=$(mktemp -d)
env -u DISPLAY -u WAYLAND_DISPLAY \
  STROLL_GPUI_DATA="$bench_dir/data" STROLL_CONFIG="$bench_dir/config.toml" \
  STROLL_DIFF_BENCH_GPU=1 cargo test --release --locked --lib \
  git_image_diff_scroll_budget -- --ignored --nocapture
```

On Linux, `STROLL_DIFF_BENCH_GPU=1` includes GPU submission in the reported CPU-path P95, with an 8 ms target. Omitting it measures input and draw only.

For presentation timing, use a 60 Hz Wayland output that supports `wp_presentation`. The benchmark opens a focused 1180×780 logical pixel window and sends 660 scroll events per case. It also scrolls past the largest JPEG, reverses, and checks the viewport after the remaining thumbnails settle. Run it three times with fresh state, then summarize the presentation intervals.

```sh
for run in 1 2 3; do
  STROLL_GPUI_DATA="$bench_dir/data-$run" STROLL_CONFIG="$bench_dir/config-$run.toml" \
    STROLL_FRAME_TIMINGS=1 cargo test --release --locked --lib \
    git_image_diff_wayland_presentation -- --ignored --nocapture \
    >"$bench_dir/wayland-$run.log" 2>&1 || break
done
python3 scripts/git-diff-presentation.py "$bench_dir"/wayland-*.log
```

Each case needs at least 10 seconds of feedback, at least 99% of refreshes presented, and at most one consecutive missed refresh. A private KWin virtual output at 2360×1560 with scale 2 can exercise the same Wayland path on a physical GPU.

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
