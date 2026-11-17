# WSL development

Build and run Stroll on WSL2 with WSLg. See the [README](../README.md#prerequisites) for the toolchain and Linux dependencies.

## Set up WSLg

Run `wsl --update` on Windows. If the WSL shell has no display variables, add the following to its startup file.

```sh
if [ -S /mnt/wslg/runtime-dir/wayland-0 ]; then
  export WAYLAND_DISPLAY=/mnt/wslg/runtime-dir/wayland-0
  export DISPLAY=:0
fi
```

Keep the session's `XDG_RUNTIME_DIR` and use an absolute path for the Wayland socket. GPUI prefers Wayland. For Xwayland, unset `WAYLAND_DISPLAY` and keep `DISPLAY`.

Mesa Dozen lets Vulkan use the Windows GPU through D3D12. On Arch Linux, install it after the prerequisites from the README.

```sh
sudo pacman -S --needed vulkan-dzn
```

Install `vulkan-tools` and run `vulkaninfo --summary` to check available adapters. If Dozen is unavailable, install `vulkan-swrast` to use software Vulkan.

## Build and run

Run the following commands from the repository root. They set an isolated state directory and configuration file for scripted runs and screenshots.

```sh
dev_dir=$(mktemp -d)
export STROLL_GPUI_DATA="$dev_dir/state"
export STROLL_CONFIG="$dev_dir/config.toml"

cargo build --release --locked
./target/release/stroll --git .
```

If no window opens, check the socket in `WAYLAND_DISPLAY` or the Xwayland display in `DISPLAY`. If no GPU adapter is found, check `vulkaninfo --summary`. With Dozen, also check that `/dev/dxg` exists.

## Headless tests

See [Headless validation](headless-validation.md) for test commands, GPU selection, and offscreen screenshots. Tests in WSL use fonts installed inside WSL.

## Window screenshots

### Xwayland

Use the same temporary state and configuration paths. Run Stroll and wait for the window to draw. The [screenshot script](../scripts/wsl-screenshot.py) requires Python 3 and `libX11`:

```sh
env -u WAYLAND_DISPLAY DISPLAY=:0 ./target/release/stroll --git . &
python3 scripts/wsl-screenshot.py --list
python3 scripts/wsl-screenshot.py -o /tmp/stroll-wsl.png
```

Use `--window` to match a window title or class when several windows are open.

### Wayland

Start Stroll in Wayland mode and use `Win+Shift+S` from an unlocked Windows desktop. Use PowerShell to capture the entire Windows desktop from WSL.

```sh
powershell.exe -NoProfile -Command '
  Add-Type -AssemblyName System.Windows.Forms,System.Drawing
  $path = Join-Path $env:TEMP "stroll-wsl.png"
  $screen = [System.Windows.Forms.SystemInformation]::VirtualScreen
  $bitmap = New-Object System.Drawing.Bitmap $screen.Width $screen.Height
  $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
  try {
    $graphics.CopyFromScreen($screen.Left, $screen.Top, 0, 0, $bitmap.Size)
    $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    Write-Output $path
  } finally {
    $graphics.Dispose()
    $bitmap.Dispose()
  }
'
```

Use `wslpath -u` to convert the output to a path accessible from WSL.
