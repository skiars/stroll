use std::{
    borrow::Cow,
    ops::Range,
    time::{Duration, Instant},
};

use fancy_regex::{Captures, Regex};
use gpui_base::input::{CompiledSearch, SearchOptions};
use rust_i18n::t;

use crate::locator::TextQuery;

const MAX_MATCHES: usize = 100_000;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const SEARCH_BUDGET: Duration = Duration::from_millis(250);

pub(super) fn compile(
    query: &str,
    case_insensitive: bool,
    options: SearchOptions,
) -> Result<CompiledSearch, String> {
    let pattern = TextQuery {
        query: query.replace("\r\n", "\n"),
        case_sensitive: !case_insensitive,
        whole_word: options.whole_word,
        regex: options.regex,
        ..TextQuery::default()
    }
    .pattern()
    .map_err(|error| format!("{error:#}"))?;
    let replacement_pattern = pattern.clone();
    Ok(CompiledSearch::new(
        move |text, scope| scan(&pattern, text, scope),
        move |text, ranges, replacement| {
            let replacement = if replacement.contains("\r\n") {
                Cow::Owned(replacement.replace("\r\n", "\n"))
            } else {
                Cow::Borrowed(replacement)
            };
            if !options.regex {
                if replacement.len().saturating_mul(ranges.len()) > MAX_BYTES {
                    return Err(t!("editor.find_limit").to_string());
                }
                return Ok(vec![replacement.into_owned(); ranges.len()]);
            }
            let template = Replacement::parse(&replacement_pattern, &replacement)?;
            let mut expanded = Vec::with_capacity(ranges.len());
            let mut bytes = 0;
            let started = Instant::now();
            for range in ranges {
                if started.elapsed() > SEARCH_BUDGET {
                    return Err(t!("locator.search_timeout").to_string());
                }
                let captures = replacement_pattern
                    .captures_from_pos(text, range.start)
                    .map_err(|_| t!("locator.search_timeout").to_string())?
                    .filter(|captures| captures.get(0).is_some_and(|hit| hit.range() == *range))
                    .ok_or_else(|| t!("locator.stale").to_string())?;
                let replacement = template.expand(&captures, MAX_BYTES - bytes)?;
                bytes += replacement.len();
                expanded.push(replacement);
            }
            Ok(expanded)
        },
    ))
}

fn scan(
    pattern: &Regex,
    text: &str,
    scope: Option<&Range<usize>>,
) -> Result<Vec<Range<usize>>, String> {
    if text.len() > MAX_BYTES {
        return Err(t!("editor.find_limit").to_string());
    }
    let mut ranges = Vec::new();
    let mut empty = false;
    let mut next = scope.map_or(0, |range| range.start);
    if !text.is_char_boundary(next) {
        return Err(t!("locator.stale").to_string());
    }
    let started = Instant::now();
    while let Some(hit) = pattern
        .find_from_pos(text, next)
        .map_err(|_| t!("locator.search_timeout").to_string())?
    {
        if started.elapsed() > SEARCH_BUDGET {
            return Err(t!("locator.search_timeout").to_string());
        }
        let range = hit.range();
        if scope.is_some_and(|scope| range.start >= scope.end) {
            break;
        }
        if range.is_empty() {
            empty = true;
            let Some(character) = text[range.end..].chars().next() else {
                break;
            };
            next = range.end + character.len_utf8();
            continue;
        }
        next = range.end;
        if scope.is_some_and(|scope| range.end > scope.end) {
            continue;
        }
        if ranges.len() == MAX_MATCHES {
            return Err(t!("editor.find_limit").to_string());
        }
        ranges.push(range);
    }
    if ranges.is_empty() && empty {
        return Err(t!("editor.find_empty_matches").to_string());
    }
    Ok(ranges)
}

enum ReplacementPart<'a> {
    Text(&'a str),
    Group(usize),
}

struct Replacement<'a>(Vec<ReplacementPart<'a>>);

impl<'a> Replacement<'a> {
    fn parse(pattern: &Regex, mut text: &'a str) -> Result<Self, String> {
        let names = pattern.capture_names().collect::<Vec<_>>();
        let mut parts = Vec::new();
        while let Some((prefix, rest)) = text.split_once('$') {
            parts.push(ReplacementPart::Text(prefix));
            if let Some(rest) = rest.strip_prefix('$') {
                parts.push(ReplacementPart::Text("$"));
                text = rest;
                continue;
            }
            let (name, remaining) = if let Some(braced) = rest.strip_prefix('{') {
                braced
                    .split_once('}')
                    .ok_or_else(|| t!("editor.find_capture", capture = rest).to_string())?
            } else {
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                if digits == 0 {
                    parts.push(ReplacementPart::Text("$"));
                    text = rest;
                    continue;
                }
                rest.split_at(digits)
            };
            let index = name
                .parse::<usize>()
                .ok()
                .filter(|index| *index < names.len())
                .or_else(|| names.iter().position(|candidate| *candidate == Some(name)))
                .ok_or_else(|| t!("editor.find_capture", capture = name).to_string())?;
            parts.push(ReplacementPart::Group(index));
            text = remaining;
        }
        parts.push(ReplacementPart::Text(text));
        Ok(Self(parts))
    }

    fn expand(&self, captures: &Captures<'_>, remaining: usize) -> Result<String, String> {
        let mut result = String::new();
        for part in &self.0 {
            let text = match part {
                ReplacementPart::Text(text) => text,
                ReplacementPart::Group(index) => captures.get(*index).map_or("", |m| m.as_str()),
            };
            if text.len() > remaining - result.len() {
                return Err(t!("editor.find_limit").to_string());
            }
            result.push_str(text);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, process::Command, sync::Arc};

    use anyhow::Result;
    use gpui::{
        AppContext as _, Entity, TestAppContext, TestDispatcher, VisualTestContext, px, size,
    };
    use gpui_base::input::EditorState;
    use gpui_component::{ActiveTheme as _, Colorize as _, Root, ThemeMode, input::Editor};

    use super::*;
    use crate::{
        config::ConfigFile,
        repository::{Location, Store},
        workspace::{Workspace, WorkspaceView},
    };

    fn frame(visual: &mut VisualTestContext) {
        visual.executor().advance_clock(Duration::from_millis(20));
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.simulate_next_frame(cx);
            window.draw(cx).clear(cx);
        });
    }

    fn wait(visual: &mut VisualTestContext, mut ready: impl FnMut(&mut VisualTestContext) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            frame(visual);
            if ready(visual) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for editor update"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn with_file(
        text: &str,
        test: impl FnOnce(&mut VisualTestContext, &Entity<Workspace>, &Entity<EditorState>),
    ) -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("repo");
        fs::create_dir(&root)?;
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&root)
                .status()?
                .success()
        );
        fs::write(root.join("plain.txt"), text)?;
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
            let view = cx
                .new(|cx| Workspace::with_config(store, config, WorkspaceView::Editor, window, cx));
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let workspace = workspace.unwrap();
        visual.simulate_resize(size(px(980.), px(630.)));
        visual.update(|window, cx| {
            window.activate_window();
            workspace.update(cx, |this, cx| {
                this.open_repository(
                    Location {
                        root,
                        file: Some(PathBuf::from("plain.txt")),
                    },
                    window,
                    cx,
                )
            });
        });
        wait(visual, |visual| {
            visual.update(|_, cx| workspace.read(cx).active_file().is_some())
        });
        let editor = visual.update(|window, cx| {
            let editor = workspace.read(cx).active_file().unwrap().editor.clone();
            editor.update(cx, |editor, cx| editor.focus(window, cx));
            editor
        });
        frame(visual);
        test(visual, &workspace, &editor);
        Ok(())
    }

    #[test]
    fn advanced_find_multiline_fields_keep_navigation_and_replace_focus() -> Result<()> {
        let source = "foo\r\nbar\r\nfoo\r\nbar\r\n";
        with_file(source, |visual, workspace, editor| {
            visual.simulate_keystrokes(if cfg!(target_os = "macos") {
                "cmd-shift-f"
            } else {
                "ctrl-h"
            });
            frame(visual);
            visual.update(|window, cx| assert!(Editor::search_has_focus(editor, window, cx)));
            visual.simulate_input("foo");
            visual.simulate_keystrokes(if cfg!(target_os = "macos") {
                "cmd-enter"
            } else {
                "ctrl-enter"
            });
            visual.simulate_input("bar");
            frame(visual);
            visual.update(|_, cx| {
                let session = editor.read(cx).search_session();
                assert_eq!(session.query, "foo\nbar");
                assert_eq!(session.matcher.len(), 2);
            });
            visual.simulate_keystrokes("enter shift-enter tab");
            frame(visual);
            visual.update(|window, cx| assert!(Editor::search_has_focus(editor, window, cx)));
            visual.simulate_input("first\r\nsecond");
            visual.update(|_, cx| assert_eq!(editor.read(cx).search_session().query, "foo\nbar"));
            visual.simulate_keystrokes("enter");
            frame(visual);
            visual.update(|_, cx| {
                let actual = editor.read(cx).text().to_string();
                assert_eq!(actual, "first\nsecond\nfoo\nbar\n");
                let file = workspace.read(cx).active_file().unwrap();
                assert_eq!(
                    file.document.encode_text(&actual),
                    b"first\r\nsecond\r\nfoo\r\nbar\r\n"
                );
            });
        })
    }

    #[test]
    fn find_bar_keeps_default_field_geometry_and_folds_options_when_narrow() -> Result<()> {
        with_file("foo\nbar\n", |visual, _, editor| {
            let (find, replace) = if cfg!(target_os = "macos") {
                ("cmd-f", "cmd-shift-f")
            } else {
                ("ctrl-f", "ctrl-h")
            };
            visual.simulate_keystrokes(find);
            frame(visual);
            let collapsed = visual.debug_bounds("find-query-field").unwrap();
            let default_height = visual.update(|window, _| window.rem_size() * 2.);
            assert_eq!(collapsed.size.height, default_height);
            assert!(visual.debug_bounds("find-replacement-field").is_none());

            visual.simulate_keystrokes(replace);
            frame(visual);
            let search = visual.debug_bounds("find-query-field").unwrap();
            let replacement = visual.debug_bounds("find-replacement-field").unwrap();
            assert_eq!(search, collapsed);
            assert_eq!(search.size, replacement.size);
            assert_eq!(search.left(), replacement.left());
            assert_eq!(replacement.top() - search.bottom(), px(4.));

            visual.simulate_input("one\ntwo\nthree\nfour");
            frame(visual);
            assert_eq!(visual.debug_bounds("find-query-field"), Some(search));
            assert_eq!(
                visual.debug_bounds("find-replacement-field"),
                Some(replacement)
            );

            let window_width = visual.update(|window, _| window.viewport_size().width);
            let panel = visual.debug_bounds("find-bar").unwrap();
            visual.simulate_resize(size(window_width - panel.size.width + px(260.), px(630.)));
            frame(visual);
            frame(visual);
            let search = visual.debug_bounds("find-query-field").unwrap();
            let replacement = visual.debug_bounds("find-replacement-field").unwrap();
            assert_eq!(search.size, replacement.size);
            assert_eq!(search.size.height, default_height);
            let options = visual
                .debug_bounds("find-options-trigger")
                .expect("narrow match options");
            for mode in [ThemeMode::Light, ThemeMode::Dark] {
                for case_insensitive in [true, false] {
                    visual.update(|_, cx| {
                        crate::workspace::apply_theme(mode, cx);
                        editor.update(cx, |editor, cx| {
                            editor.set_find_query(
                                "foo",
                                case_insensitive,
                                SearchOptions::default(),
                                cx,
                            );
                        });
                    });
                    frame(visual);
                    visual.update(|window, cx| {
                        let theme = cx.theme();
                        let center = options.center().scale(window.scale_factor());
                        let fill =
                            gpui::Background::from(theme.primary.mix_oklab(theme.popover, 0.33));
                        let selected = window
                            .painted_quads()
                            .iter()
                            .any(|quad| quad.bounds.contains(&center) && quad.background == fill);
                        assert_eq!(selected, !case_insensitive, "{mode:?}");
                    });
                }
            }
            visual.simulate_click(options.center(), gpui::Modifiers::none());
            frame(visual);
            assert!(visual.debug_bounds("find-options-actions").is_some());
        })
    }

    #[test]
    fn advanced_find_retains_regex_and_maps_the_selection_scope() -> Result<()> {
        let source = "foo1 xfoo2\nfoo3 foo4\nfoo5\n";
        with_file(source, |visual, _, editor| {
            visual.update(|window, cx| {
                editor.update(cx, |state, cx| {
                    let start = source.find("foo3").unwrap();
                    let end = source.find("\nfoo5").unwrap();
                    state.set_selected_range_with_direction(start..end, true, cx);
                    state.open_search(false, cx);
                    state.set_find_in_selection(true, cx);
                    let options = SearchOptions {
                        regex: true,
                        whole_word: true,
                    };
                    state.set_find_query(r"foo(?<n>\d)", false, options, cx);
                    assert_eq!(state.search_session().matcher.len(), 2);
                    state.next_search_match(cx);
                    assert!(state.explicit_search_selection().is_none());
                    state.edit_ranges(&[(0..0, "😀 ".to_owned())], window, cx);
                    assert_eq!(state.search_session().matcher.len(), 2);
                    assert_eq!(
                        state.replace_all_search_matches("${unknown}", window, cx),
                        0
                    );
                    assert_eq!(state.replace_all_search_matches("bar${n}$$", window, cx), 2);
                    assert_eq!(
                        state.text().to_string(),
                        format!("😀 {}", source.replace("foo3 foo4", "bar3$ bar4$"))
                    );
                    state.set_find_query("(", false, options, cx);
                    assert!(state.search_session().matcher.error().is_some());
                    assert_eq!(state.replace_all_search_matches("", window, cx), 0);
                });
            });
            frame(visual);
            visual.update(|window, cx| {
                editor.update(cx, |state, cx| {
                    state.close_search(cx);
                    state.focus(window, cx);
                })
            });
            visual.simulate_keystrokes(if cfg!(target_os = "macos") {
                "cmd-z"
            } else {
                "ctrl-z"
            });
            visual.update(|_, cx| {
                assert_eq!(editor.read(cx).text().to_string(), format!("😀 {source}"))
            });
        })
    }

    #[test]
    fn selection_highlight_is_literal_and_find_selection_does_not_trigger_it() -> Result<()> {
        let source = "foo Foo xfoo foo\n";
        with_file(source, |visual, workspace, editor| {
            let ranges = |visual: &mut VisualTestContext| {
                visual.update(|_, cx| {
                    workspace
                        .read(cx)
                        .active_file()
                        .unwrap()
                        .selection_highlight
                        .test_ranges(cx)
                })
            };
            visual.simulate_keystrokes("home shift-right shift-right shift-right");
            wait(visual, |visual| ranges(visual) == [9..12, 13..16]);
            visual.update(|_, cx| assert_eq!(editor.read(cx).selected_range(), 0..3));
            visual.simulate_keystrokes(if cfg!(target_os = "macos") {
                "cmd-f"
            } else {
                "ctrl-f"
            });
            frame(visual);
            assert!(ranges(visual).is_empty());
            visual.simulate_keystrokes("enter escape");
            frame(visual);
            assert!(ranges(visual).is_empty());
            visual.update(|_, cx| assert!(editor.read(cx).explicit_search_selection().is_none()));
            visual.simulate_keystrokes("home shift-right shift-right shift-right");
            wait(visual, |visual| ranges(visual) == [9..12, 13..16]);
            visual.deactivate_window();
            frame(visual);
            assert!(ranges(visual).is_empty());
        })
    }
}
