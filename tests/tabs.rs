use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::Result;
use stroll::{
    repository::{Repository, Store},
    tabs::TabSession,
};

#[test]
fn preview_is_reused_in_place_and_kept_tabs_stay_open() {
    let mut tabs = TabSession::default();
    tabs.open("a.rs".into(), true);
    tabs.open("b.rs".into(), false);
    assert_eq!(tabs.open("c.rs".into(), true), Some("a.rs".into()));
    assert_eq!(tabs.paths(), [PathBuf::from("c.rs"), "b.rs".into()]);
    assert_eq!(tabs.active(), Some(Path::new("c.rs")));
    assert!(tabs.is_preview(Path::new("c.rs")));

    tabs.open("c.rs".into(), false);
    assert!(!tabs.is_preview(Path::new("c.rs")));
    tabs.open("d.rs".into(), true);
    assert_eq!(
        tabs.paths(),
        [PathBuf::from("c.rs"), "b.rs".into(), "d.rs".into()]
    );
    tabs.open("c.rs".into(), true);
    assert_eq!(tabs.active(), Some(Path::new("c.rs")));
    assert!(tabs.is_preview(Path::new("d.rs")));

    tabs.close(Path::new("c.rs"));
    assert_eq!(tabs.active(), Some(Path::new("b.rs")));
    tabs.close(Path::new("d.rs"));
    assert_eq!(tabs.active(), Some(Path::new("b.rs")));
    assert!(!tabs.is_preview(Path::new("d.rs")));
}

#[test]
fn restores_many_tabs_in_order_with_active_preview_and_repository_isolation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let first = repository(&dir.path().join("first"))?;
    let second = repository(&dir.path().join("second"))?;
    let state = dir.path().join("state");
    let store = Store::new(state.clone())?;
    let mut tabs = TabSession::default();
    for index in 0..90 {
        let path = PathBuf::from(format!("tab-{index:03}.rs"));
        fs::write(first.root.join(&path), "")?;
        tabs.open(path, index == 89);
    }
    store.save_tabs(&first, &tabs)?;
    store.remember_repository(&second)?;
    drop(store);

    let store = Store::new(state)?;
    assert_eq!(store.location()?.unwrap().root, second.root);
    assert_eq!(store.tabs(&first)?, tabs);
    assert_eq!(store.tabs(&second)?, TabSession::default());

    let mut expected = tabs.clone();
    let missing = Path::new("tab-089.rs");
    expected.close(missing);
    fs::remove_file(first.root.join(missing))?;
    assert_eq!(store.tabs(&first)?, expected);
    Ok(())
}

#[test]
fn migrates_the_legacy_active_file_and_honors_an_empty_session() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let repo = repository(&dir.path().join("repo"))?;
    let state = dir.path().join("state");
    let store = Store::new(state.clone())?;
    fs::write(repo.root.join("legacy.rs"), "")?;
    fs::write(
        state.join("workspace.json"),
        serde_json::to_vec(&serde_json::json!({
            "root": repo.root, "file": "legacy.rs"
        }))?,
    )?;

    let tabs = store.tabs(&repo)?;
    assert_eq!(tabs.paths(), [PathBuf::from("legacy.rs")]);
    assert_eq!(tabs.active(), Some(Path::new("legacy.rs")));
    assert!(!tabs.is_preview(Path::new("legacy.rs")));

    store.save_tabs(&repo, &TabSession::default())?;
    assert_eq!(store.tabs(&repo)?, TabSession::default());
    Ok(())
}

#[test]
fn imports_legacy_worktree_tab_json_and_prefers_the_new_snapshot() -> Result<()> {
    use sha2::{Digest, Sha256};

    let dir = tempfile::tempdir()?;
    let repo = repository(&dir.path().join("repo"))?;
    let state = dir.path().join("state");
    let store = Store::new(state.clone())?;
    let path = PathBuf::from("legacy.rs");
    fs::write(repo.root.join(&path), "")?;
    let mut legacy = TabSession::default();
    legacy.open(path, true);
    let key = hex::encode(Sha256::digest(repo.root.as_os_str().as_encoded_bytes()));
    fs::create_dir_all(state.join("tabs"))?;
    fs::write(
        state.join("tabs").join(format!("{key}.json")),
        serde_json::to_vec(&legacy)?,
    )?;

    assert_eq!(store.tabs(&repo)?, legacy);
    store.save_tabs(&repo, &TabSession::default())?;
    assert!(state.join("sessions").join(format!("{key}.json")).is_file());
    drop(store);

    let store = Store::new(state)?;
    assert_eq!(store.tabs(&repo)?, TabSession::default());
    Ok(())
}

fn repository(root: &Path) -> Result<Repository> {
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(root)
            .status()?
            .success()
    );
    Repository::open(root)
}

#[cfg(target_os = "linux")]
#[test]
fn progressive_wrap_discards_stale_width_and_text_snapshots() {
    use gpui::{AppContext as _, Context, Entity, Render, Styled as _, Window, div, point, px};
    use gpui_component::input::{Editor, EditorState, Position, RopeExt as _};

    struct View {
        progressive: Entity<EditorState>,
        reference: Entity<EditorState>,
        width: gpui::Pixels,
    }

    impl Render for View {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
            use gpui::ParentElement as _;
            div()
                .size_full()
                .child(
                    div()
                        .w(self.width)
                        .h(px(210.))
                        .child(Editor::new(&self.progressive)),
                )
                .child(
                    div()
                        .w(self.width)
                        .h(px(210.))
                        .child(Editor::new(&self.reference)),
                )
        }
    }

    let original = (0..160)
        .map(|row| format!("{row}  中文混排 👩‍💻 office ==\t {row}\n").repeat(2))
        .collect::<String>();
    let edited = format!("新增一行\n{original}尾行");
    let platform = gpui_platform::current_platform(true);
    let mut cx = gpui::TestAppContext::build_with_text_system(
        gpui::TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    let (view, visual) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        let reference = cx.new(|cx| EditorState::new(window, cx));
        let progressive = cx.new(|cx| EditorState::new(window, cx).progressive_wrap());
        View {
            progressive,
            reference,
            width: px(160.),
        }
    });
    let (progressive, reference) = visual.update(|window, cx| {
        let state = view.read(cx);
        let progressive = state.progressive.clone();
        let reference = state.reference.clone();
        progressive.update(cx, |editor, cx| {
            editor.set_value(original.as_str(), window, cx)
        });
        window.draw(cx).clear(cx);
        assert!(progressive.read(cx).is_wrapping());
        for editor in [&progressive, &reference] {
            editor.update(cx, |editor, cx| {
                editor.set_value(edited.as_str(), window, cx);
                editor.set_cursor_position(Position::new(23, 3), window, cx);
                editor.set_scroll_offset(point(px(0.), px(-410.)), cx);
            });
        }
        view.update(cx, |view, cx| {
            view.width = px(235.);
            cx.notify();
        });
        window.draw(cx).clear(cx);
        (progressive, reference)
    });
    for _ in 0..20 {
        visual.run_until_parked();
        if visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            !progressive.read(cx).is_wrapping()
        }) {
            break;
        }
    }
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
        let progressive = progressive.read(cx);
        let reference = reference.read(cx);
        assert!(!progressive.is_wrapping());
        assert_eq!(progressive.text(), reference.text());
        assert_eq!(progressive.cursor_position(), reference.cursor_position());
        assert_eq!(progressive.selected_range(), reference.selected_range());
        for line in 0..progressive.text().lines_len() {
            assert_eq!(
                progressive
                    .display_map()
                    .buffer_line_to_display_row_range(line),
                reference
                    .display_map()
                    .buffer_line_to_display_row_range(line),
                "buffer line {line}"
            );
        }
    });
}
