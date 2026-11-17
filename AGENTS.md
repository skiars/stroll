# AGENTS.md

- The desktop app lives in `src/`. The SSH helper entry point is `src/bin/stroll-remote.rs`.
- Keep changes within the requested scope and reuse shared components and their existing behavior. Confirm new features, components, public APIs, and interaction defaults before implementing them.
- Use English for docs, comments, and strings outside the translation catalogs. Keep README changes focused on setup, build, test, and deployment instructions.
- Use `cargo add` for dependency changes. Run `cargo fmt` for Rust edits and use `--locked` when building, running, or testing with Cargo. Run `cargo test --release --locked` before delivery.
- For UI changes, run the relevant integration tests and report how you verified the result. Use [headless tests and offscreen rendering](docs/headless-validation.md) by default.
- Set `STROLL_GPUI_DATA` and `STROLL_CONFIG` to temporary paths for scripted launches.
- Update the relevant tests and [patch notes](vendor/README.md) when editing vendored patches.
