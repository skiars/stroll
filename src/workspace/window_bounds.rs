//! Remember the window size of each top-level window mode.
//!
//! Repository, document, and editor windows each keep their own placement so a
//! single-file window does not take over the repository window's size. Saving is
//! delayed while a resize or move is in flight and repeated on close.

use std::time::Duration;

use gpui::*;

use super::{Workspace, WorkspaceView};
use crate::repository::{Store, WindowGeometry, WindowSlot};

/// Delay before a resized or moved window writes its geometry.
pub(super) const SAVE_DELAY: Duration = Duration::from_millis(500);

/// Title bar region that must stay on screen for a restored window.
const MINIMUM_VISIBLE_WIDTH: f32 = 64.;
const MINIMUM_VISIBLE_HEIGHT: f32 = 32.;

pub(super) fn slot(standalone: bool, view: Option<WorkspaceView>) -> WindowSlot {
    match (standalone, view) {
        (true, Some(WorkspaceView::Docs)) => WindowSlot::Document,
        (true, _) => WindowSlot::Editor,
        (false, _) => WindowSlot::Repository,
    }
}

/// Initial bounds for a repository window, falling back to a centered default size.
pub fn repository_window_bounds(store: &Store, minimum: Size<Pixels>, cx: &App) -> WindowBounds {
    initial_window_bounds(store, WindowSlot::Repository, minimum, cx)
}

/// Initial bounds for a single-file window, remembering document and editor mode separately.
pub fn standalone_window_bounds(
    store: &Store,
    view: WorkspaceView,
    minimum: Size<Pixels>,
    cx: &App,
) -> WindowBounds {
    initial_window_bounds(store, slot(true, Some(view)), minimum, cx)
}

fn initial_window_bounds(
    store: &Store,
    slot: WindowSlot,
    minimum: Size<Pixels>,
    cx: &App,
) -> WindowBounds {
    let saved = match store.window_geometry(slot) {
        Ok(saved) => saved,
        Err(error) => {
            eprintln!("window size not restored: {error:#}");
            None
        }
    };
    let displays: Vec<Bounds<Pixels>> = cx
        .displays()
        .iter()
        .map(|display| display.visible_bounds())
        .collect();
    saved
        .and_then(|geometry| fit(geometry, &displays, minimum))
        .unwrap_or_else(|| {
            WindowBounds::Windowed(Bounds::centered(None, size(px(1280.), px(850.)), cx))
        })
}

fn record(bounds: WindowBounds) -> WindowGeometry {
    let maximized = matches!(bounds, WindowBounds::Maximized(_));
    let bounds = bounds.get_bounds();
    WindowGeometry {
        x: bounds.origin.x.as_f32(),
        y: bounds.origin.y.as_f32(),
        width: bounds.size.width.as_f32(),
        height: bounds.size.height.as_f32(),
        maximized,
    }
}

/// Place a saved window on a display that still exists, keeping its title bar
/// reachable and its size within the display and the window's minimum size.
fn fit(
    geometry: WindowGeometry,
    displays: &[Bounds<Pixels>],
    minimum: Size<Pixels>,
) -> Option<WindowBounds> {
    if ![geometry.x, geometry.y, geometry.width, geometry.height]
        .into_iter()
        .all(f32::is_finite)
        || geometry.width <= 0.
        || geometry.height <= 0.
    {
        return None;
    }
    let saved = Bounds::new(
        point(px(geometry.x), px(geometry.y)),
        size(px(geometry.width), px(geometry.height)),
    );
    let display = displays
        .iter()
        .map(|display| (display, visible_area(&saved.intersect(display))))
        .filter(|(_, area)| *area >= MINIMUM_VISIBLE_WIDTH * MINIMUM_VISIBLE_HEIGHT)
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(display, _)| display)?;
    let window = Size {
        width: saved
            .size
            .width
            .clamp(minimum.width, display.size.width.max(minimum.width)),
        height: saved
            .size
            .height
            .clamp(minimum.height, display.size.height.max(minimum.height)),
    };
    let min_x = display.origin.x - window.width + px(MINIMUM_VISIBLE_WIDTH);
    let max_x = (display.origin.x + display.size.width - px(MINIMUM_VISIBLE_WIDTH)).max(min_x);
    let min_y = display.origin.y;
    let max_y = (display.origin.y + display.size.height - px(MINIMUM_VISIBLE_HEIGHT)).max(min_y);
    let window = Bounds::new(
        point(
            saved.origin.x.clamp(min_x, max_x),
            saved.origin.y.clamp(min_y, max_y),
        ),
        window,
    );
    Some(if geometry.maximized {
        WindowBounds::Maximized(window)
    } else {
        WindowBounds::Windowed(window)
    })
}

fn visible_area(bounds: &Bounds<Pixels>) -> f32 {
    bounds.size.width.as_f32() * bounds.size.height.as_f32()
}

impl Workspace {
    pub(super) fn window_slot(&self) -> WindowSlot {
        slot(
            self.standalone.is_some() && self.repository.is_none(),
            Some(self.view),
        )
    }

    pub(super) fn observe_window_geometry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.observe_window_bounds(window, |this, window, cx| {
            let slot = this.window_slot();
            let geometry = record(window.window_bounds());
            if this.saved_window == Some((slot, geometry)) {
                // The window is back at the remembered geometry, so a pending
                // write would only overwrite it with an older placement.
                this.window_write = Task::ready(());
                this.pending_window = None;
                return;
            }
            this.pending_window = Some((slot, geometry));
            this.window_write = cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SAVE_DELAY).await;
                _ = this.update(cx, |this, _| this.flush_window_geometry());
            });
        })
        .detach();
    }

    fn flush_window_geometry(&mut self) {
        let Some((slot, geometry)) = self.pending_window.take() else {
            return;
        };
        if self.saved_window == Some((slot, geometry)) {
            return;
        }
        match self.store.save_window_geometry(slot, geometry) {
            Ok(()) => self.saved_window = Some((slot, geometry)),
            Err(error) => eprintln!("window size not saved: {error:#}"),
        }
    }

    pub(super) fn save_window_geometry(&mut self, window: &Window) {
        self.window_write = Task::ready(());
        self.pending_window = None;
        let slot = self.window_slot();
        let geometry = record(window.window_bounds());
        if self.saved_window == Some((slot, geometry)) {
            return;
        }
        match self.store.save_window_geometry(slot, geometry) {
            Ok(()) => self.saved_window = Some((slot, geometry)),
            Err(error) => eprintln!("window size not saved: {error:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn display(origin: (f32, f32), size: (f32, f32)) -> Bounds<Pixels> {
        Bounds::new(
            point(px(origin.0), px(origin.1)),
            gpui::size(px(size.0), px(size.1)),
        )
    }

    fn geometry(x: f32, y: f32, width: f32, height: f32) -> WindowGeometry {
        WindowGeometry {
            x,
            y,
            width,
            height,
            maximized: false,
        }
    }

    fn minimum() -> Size<Pixels> {
        gpui::size(px(400.), px(360.))
    }

    fn windowed(bounds: Bounds<Pixels>) -> WindowBounds {
        WindowBounds::Windowed(bounds)
    }

    #[::core::prelude::v1::test]
    fn slots_follow_the_window_mode() {
        assert_eq!(slot(false, None), WindowSlot::Repository);
        assert_eq!(
            slot(false, Some(WorkspaceView::Docs)),
            WindowSlot::Repository
        );
        assert_eq!(slot(true, Some(WorkspaceView::Docs)), WindowSlot::Document);
        assert_eq!(slot(true, Some(WorkspaceView::Editor)), WindowSlot::Editor);
    }

    #[::core::prelude::v1::test]
    fn saved_geometry_is_restored() {
        let displays = [display((0., 0.), (1920., 1080.))];
        let restored = fit(geometry(100., 80., 1200., 800.), &displays, minimum());

        assert_eq!(
            restored,
            Some(windowed(Bounds::new(
                point(px(100.), px(80.)),
                gpui::size(px(1200.), px(800.)),
            )))
        );
    }

    #[::core::prelude::v1::test]
    fn saved_geometry_returns_to_its_second_display() {
        let displays = [
            display((0., 0.), (1920., 1080.)),
            display((1920., 0.), (1920., 1080.)),
        ];
        let restored = fit(geometry(2100., 200., 1000., 700.), &displays, minimum());

        assert_eq!(
            restored,
            Some(windowed(Bounds::new(
                point(px(2100.), px(200.)),
                gpui::size(px(1000.), px(700.)),
            )))
        );
    }

    #[::core::prelude::v1::test]
    fn missing_display_falls_back_to_the_default_size() {
        let displays = [display((0., 0.), (1920., 1080.))];

        assert_eq!(
            fit(geometry(5000., 200., 1200., 800.), &displays, minimum()),
            None
        );
        assert_eq!(
            fit(geometry(1200., 1080., 1200., 800.), &displays, minimum()),
            None
        );
    }

    #[::core::prelude::v1::test]
    fn invalid_geometry_falls_back_to_the_default_size() {
        let displays = [display((0., 0.), (1920., 1080.))];

        assert_eq!(fit(geometry(0., 0., 0., 800.), &displays, minimum()), None);
        assert_eq!(
            fit(geometry(0., 0., 1200., -1.), &displays, minimum()),
            None
        );
        assert_eq!(
            fit(geometry(f32::NAN, 0., 1200., 800.), &displays, minimum()),
            None
        );
        assert_eq!(
            fit(geometry(0., 0., f32::INFINITY, 800.), &displays, minimum()),
            None
        );
    }

    #[::core::prelude::v1::test]
    fn size_is_clamped_to_the_minimum_and_the_display() {
        let displays = [display((0., 0.), (1920., 1080.))];

        let small = fit(geometry(100., 80., 200., 200.), &displays, minimum()).unwrap();
        assert_eq!(small.get_bounds().size, gpui::size(px(400.), px(360.)));

        let large = fit(geometry(100., 80., 3000., 2000.), &displays, minimum()).unwrap();
        assert_eq!(large.get_bounds().size, gpui::size(px(1920.), px(1080.)));
    }

    #[::core::prelude::v1::test]
    fn title_bar_stays_on_screen() {
        let displays = [display((0., 0.), (1920., 1080.))];

        let above = fit(geometry(100., -500., 1200., 800.), &displays, minimum())
            .unwrap()
            .get_bounds();
        assert_eq!(above.origin, point(px(100.), px(0.)));

        let right = fit(geometry(1900., 80., 1200., 800.), &displays, minimum())
            .unwrap()
            .get_bounds();
        assert_eq!(right.origin, point(px(1856.), px(80.)));
    }

    #[::core::prelude::v1::test]
    fn maximized_windows_are_restored_maximized() {
        let displays = [display((0., 0.), (1920., 1080.))];
        let mut saved = geometry(100., 80., 1200., 800.);
        saved.maximized = true;

        let restored = fit(saved, &displays, minimum()).unwrap();

        assert!(matches!(restored, WindowBounds::Maximized(_)));
        assert_eq!(
            restored.get_bounds(),
            Bounds::new(point(px(100.), px(80.)), gpui::size(px(1200.), px(800.)),)
        );
    }

    #[::core::prelude::v1::test]
    fn record_keeps_the_window_state() {
        let bounds = Bounds::new(point(px(12.), px(34.)), gpui::size(px(1200.), px(800.)));

        assert_eq!(
            record(WindowBounds::Maximized(bounds)),
            WindowGeometry {
                x: 12.,
                y: 34.,
                width: 1200.,
                height: 800.,
                maximized: true,
            }
        );
        assert!(!record(WindowBounds::Fullscreen(bounds)).maximized);
    }

    #[::core::prelude::v1::test]
    fn store_keeps_one_geometry_per_slot() -> anyhow::Result<()> {
        let fixture = tempfile::tempdir()?;
        let store = Store::new(fixture.path().join("state"))?;
        let repository = geometry(10., 20., 1200., 800.);
        let document = geometry(30., 40., 900., 700.);

        store.save_window_geometry(WindowSlot::Repository, repository)?;
        store.save_window_geometry(WindowSlot::Document, document)?;

        assert_eq!(
            store.window_geometry(WindowSlot::Repository)?,
            Some(repository)
        );
        assert_eq!(store.window_geometry(WindowSlot::Document)?, Some(document));
        assert_eq!(store.window_geometry(WindowSlot::Editor)?, None);
        Ok(())
    }

    #[::core::prelude::v1::test]
    fn malformed_records_are_replaced_instead_of_blocking_saves() -> anyhow::Result<()> {
        let fixture = tempfile::tempdir()?;
        let state = fixture.path().join("state");
        let store = Store::new(state.clone())?;
        fs::write(state.join("window-sizes.json"), b"{ not json")?;
        let geometry = geometry(10., 20., 1200., 800.);

        store.save_window_geometry(WindowSlot::Editor, geometry)?;

        assert_eq!(store.window_geometry(WindowSlot::Editor)?, Some(geometry));
        Ok(())
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod window_tests {
    use super::*;
    use crate::config::ConfigFile;
    use gpui::{TestAppContext, TestDispatcher};
    use gpui_component::Root;
    use std::sync::Arc;

    #[::core::prelude::v1::test]
    fn resized_window_geometry_is_saved_and_restored() -> anyhow::Result<()> {
        let fixture = tempfile::tempdir()?;
        let store = Arc::new(Store::new(fixture.path().join("state"))?);
        let config = ConfigFile::load(Some(fixture.path().join("config.toml")));
        let mut app = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            crate::test_support::text_system(true),
        );
        app.update(gpui_component::init);
        let mut workspace = None;
        let (_, visual) = app.add_window_view(|window, cx| {
            let view = cx.new(|cx| {
                Workspace::with_config(store.clone(), config, WorkspaceView::Editor, window, cx)
            });
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        assert!(workspace.is_some());

        visual.simulate_resize(gpui::size(px(1100.), px(700.)));
        visual
            .cx
            .executor()
            .advance_clock(SAVE_DELAY + Duration::from_millis(100));
        visual.run_until_parked();

        assert_eq!(
            store
                .window_geometry(WindowSlot::Repository)?
                .map(|geometry| (geometry.width, geometry.height)),
            Some((1100., 700.))
        );
        let restored = visual
            .update(|_, cx| repository_window_bounds(&store, gpui::size(px(740.), px(480.)), cx));
        assert_eq!(restored.get_bounds().size, gpui::size(px(1100.), px(700.)));
        Ok(())
    }
}
