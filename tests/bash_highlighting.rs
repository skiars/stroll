#![cfg(target_os = "linux")]

use std::sync::Arc;

use gpui::{
    AppContext as _, Context, Entity, ParentElement as _, Render, Styled as _, TestAppContext,
    UnderlineStyle, Window, div, px,
};
use gpui_component::{
    Root, Theme,
    diff::{Diff, DiffFile, DiffState},
    highlighter::HighlightTheme,
    input::{Editor, EditorState, Rope},
    text::{TextView, TextViewState, TextViewStyle},
};

struct BashSurfaces {
    editor: Entity<EditorState>,
    diff: Entity<DiffState>,
    markdown: Entity<TextViewState>,
    theme: Arc<HighlightTheme>,
}

impl Render for BashSurfaces {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(div().h(px(100.)).child(Editor::new(&self.editor)))
            .child(div().h(px(140.)).child(Diff::new(&self.diff)))
            .child(TextView::new(&self.markdown).style(TextViewStyle {
                highlight_theme: self.theme.clone(),
                ..Default::default()
            }))
    }
}

#[gpui::test]
fn paints_bash_keywords_in_editor_diff_and_markdown(cx: &mut TestAppContext) {
    let (_, cx) = cx.add_window_view(|window, cx| {
        gpui_component::init(cx);
        let mut theme = (*HighlightTheme::default_dark()).clone();
        theme.style.syntax.keyword.as_mut().unwrap().underline = Some(UnderlineStyle {
            thickness: px(1.),
            ..Default::default()
        });
        let theme = Arc::new(theme);
        Theme::global_mut(cx).highlight_theme = theme.clone();
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("bash")
                .default_file_text(Rope::from("#!/bin/bash\nif true; then echo editor; fi\n"));
            state.ensure_highlighter_factory(gpui_component::highlighter::input_highlighter_factory());
            state.prepare_language(window, cx);
            state
        });
        let patch = "--- a/run\n+++ b/run\n@@ -1,2 +1,2 @@\n #!/bin/bash\n-if true; then echo old; fi\n+if true; then echo new; fi\n";
        let diff = cx.new(|cx| DiffState::new(DiffFile::parse(patch).unwrap(), cx));
        let markdown = cx.new(|cx| {
            TextViewState::markdown("```sh\nif true; then echo markdown; fi\n```\n", cx)
        });
        let scene = cx.new(|_| BashSurfaces { editor, diff, markdown, theme });
        Root::new(scene, window, cx)
    });
    for _ in 0..5 {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }
    let rows = cx.update(|window, _| {
        window
            .painted_underlines()
            .into_iter()
            .map(|line| line.bounds.origin.y.0 / window.scale_factor())
            .collect::<Vec<_>>()
    });
    for (start, end) in [(0., 100.), (100., 240.), (240., 500.)] {
        assert!(
            rows.iter().any(|&y| (start..end).contains(&y)),
            "{start}..{end}: {rows:?}"
        );
    }
}
