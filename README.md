# Stroll

A Rust and GPUI desktop app for exploring Git repositories, reviewing changes, and reading or editing source files and Markdown. Open local worktrees or connect to a remote worktree over SSH.

## Prerequisites

Install Rust, Git, and a C toolchain. The project was tested with Rust 1.99.1.

- Linux needs `pkg-config`, Wayland, libxkbcommon with X11 support, libxcb, and Fontconfig. The desktop app also needs a Vulkan loader and driver.
- For WSL2 with WSLg, see [WSL development](docs/wsl-development.md).
- On macOS, install the Xcode Command Line Tools with `xcode-select --install`.
- On Windows, install Visual Studio C++ Build Tools, the Windows SDK, and the Rust MSVC toolchain.
- For audio previews, install `mpv` on the desktop and make it available on `PATH`.

The following command installs the common prerequisites on Arch Linux.

```sh
sudo pacman -S --needed base-devel pkgconf git wayland libxkbcommon libxkbcommon-x11 libxcb fontconfig vulkan-icd-loader
```

## Build, run, and test

Run these commands from the repository root.

```sh
cargo build --release --locked
cargo run --release --locked
cargo test --release --locked
```

Language server integration tests require Python 3. For Linux steps to run tests without a display server or render screenshots offscreen, see [Headless validation](docs/headless-validation.md).

The interface follows supported system languages and falls back to English. Choose English or Simplified Chinese in Settings. See [Language packs](docs/language-packs.md) to add syntax grammars or configure a language server.

## Install on Arch Linux

```sh
scripts/package-arch.sh              # Build and install
scripts/package-arch.sh --build-only # Build into target/arch
scripts/package-arch.sh --clean      # Build from source in an isolated directory
```

The default build reuses `target/release` for incremental builds. Use `--clean` when distributing a package to strip temporary build paths from the binary.

Use `xdg-mime` to set Stroll as the default Markdown reader.

```sh
xdg-mime default io.stroll.Stroll.desktop text/markdown
```

## Install on Windows

Install the x64 Rust MSVC toolchain, Visual Studio C++ Build Tools with the
Visual C++ Redistributable files, and [Inno Setup 6.3 or later](https://jrsoftware.org/isdl.php).
From the repository root, run:

```powershell
.\scripts\package-windows.ps1
```

The script builds the release binaries and writes `Stroll-Setup-<version>-x64.exe`
to `target\windows` by default. For a custom Inno Setup installation, pass
`-InnoSetupCompiler 'C:\path\to\ISCC.exe'`.

Setup installs Stroll and its app-local Visual C++ runtime for the current user.
Its optional Explorer integrations add
**Open with Stroll** to folder context menus and make Stroll available under
**Open with** for `.md` files. On Windows 11, find the folder action under
**Show more options**. To make Stroll the default Markdown app, use the
Windows **Open with** dialog.

## SSH helper (optional)

Stroll uses system OpenSSH and connects by key to hosts in `known_hosts`. On the Linux server, install Git and build the helper from the same version of Stroll as the desktop app.

```sh
scripts/install-remote.sh              # Build and install to ~/.local/bin
scripts/install-remote.sh --build-only # Build into target/release
scripts/install-remote.sh --bin-dir /usr/local/bin
```

The script builds `stroll-remote` with `--no-default-features`. The desktop app looks for it on the server's `PATH` and in `~/.local/bin`.
