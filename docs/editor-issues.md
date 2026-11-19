# Editor issues

This register records differences found by running the same task in Stroll and another editor. Record the build, platform, fixture, initial focus or selection, input sequence, and observable result on each side. Use saved bytes or copied selections to verify edits and selection boundaries. Use the counter, highlight, viewport, and focus to verify Find.

- **Confirmed defect** means the observed result violates a functional invariant, such as editing the chosen location or reporting all matches within the stated scope. The reference editor helps demonstrate the failure.
- **Design or usability defect** means the workflow has a concrete cost and review of mature editors gives high confidence that the behavior is defective. Record the comparison and cite the relevant behavior test, upstream source, or documentation.
- **Interaction difference** means there is a product choice or the evidence is insufficient for a defect. Keep capability gaps and deliberate defaults here. Record resolved defects separately with the retest result.

The 2026-11-19 baseline is Stroll `ca6cfbc` and Zed 1.28.1 on Linux X11, with English UI, fresh profiles, default bindings, language servers disabled, and separate disposable Git worktrees. The tables report observed Stroll and Zed behavior. VS Code and IntelliJ IDEA references below are design evidence. Character input uses the keyboard except where a query is pasted explicitly. See the [automated audit](#automated-audit) for the runner and its limits.

## Confirmed defects

### Find reveals one line and edits another

Open `find-sample.ts` from the [comparison workspace](#prepare-the-comparison-workspaces). Press Ctrl+Home, open Find with Ctrl+F, enter `EndAnchor`, and press Enter. Confirm that line 107 is visible. Press Esc, press Right once, type `PROBEMARK`, then save with Ctrl+S. Inspect the saved file.

Stroll saves `/PROBEMARK/ StartAnchor` on line 1. Zed saves `EndAnchorPROBEMARK` at the end of line 107. The selected search result should be the editing location when focus returns to the source.

### Two Replace clicks can modify the same occurrence

Open a fresh copy of [the original TS fixture](../tests/fixtures/editor-interactions.ts) and press Ctrl+Home. Open Replace with Ctrl+H, set Find to `auditNeedle`, and set Replace to `auditNeedleX`. Click the single Replace button twice without moving the active match. Close with Esc, save, and compare the values of `matchFirst` and `matchSecond`.

Stroll saves `auditNeedleXX` and `auditNeedle`. Zed saves `auditNeedleX` in both places. Sequential replacement should advance to the next occurrence even if the new text still contains the query. [Zed replacement logic](https://github.com/zed-industries/zed/blob/v1.28.1/crates/search/src/buffer_search.rs#L1664-L1681)

### Case-insensitive Find misses uppercase accented text in source and Git diff

In the prepared workspace, open `find-sample.ts` in the source editor, `diff-probe.ts` in the **long** worktree's unified Git diff, and `README.md` in rendered Markdown. In each view, focus the body, open Find with Ctrl+F, turn Match Case off, and paste `café` into the query. Record the total. In source and Markdown, turn Match Case on and repeat as a control.

Stroll finds one of the two occurrences in source and Git diff with Match Case off, missing `CAFÉ`. Zed finds both in each view. Stroll Markdown finds both. With Match Case on, both apps find only the lowercase occurrence in source and Markdown. A shared case toggle should apply the same Unicode case semantics across its targets.

### Markdown Find silently omits matches after the 5,000th result

Open the prepared `cap.md` as rendered Markdown. From the top, open Find and enter `hitcount`. Compare the total. Use Shift+Enter from the first result to wrap to the last indexed result. Check whether the active occurrence is the lone `hitcount` beside `LastAnchor`. A separate query for `LastAnchor` locates that final paragraph.

Stroll reports 5,000 matches and omits the final occurrence from navigation. Zed reports 5,001. An exact total implies complete coverage. VS Code's source editor marks its highlight limit with a plus sign and continues to navigate beyond it. [VS Code counter](https://github.com/microsoft/vscode/blob/1.146.0/src/vs/editor/contrib/find/browser/findWidget.ts#L424-L451) · [VS Code navigation](https://github.com/microsoft/vscode/blob/1.146.0/src/vs/editor/contrib/find/browser/findModel.ts#L356-L383)

## Design or usability defects

### Replace input lacks a usable keyboard confirmation

Open a fresh original TS fixture at the top. Press Ctrl+H, enter `auditNeedle` as the query, press Tab to focus the replacement input, and type `FIRST`. Press Enter once. Press Esc, save, and inspect the two values. Repeat from a fresh fixture with Ctrl+Enter instead of Enter. In Stroll, also press Tab and Shift+Tab from the replacement input and check the focus destinations.

Stroll's Enter moves to the next match without applying the replacement. Ctrl+Enter also leaves the file unchanged. Tab and Shift+Tab cycle between the inputs. Zed's Enter replaces the first match and Ctrl+Enter replaces both in their respective runs. VS Code also binds Enter in the replacement input to Replace One. The defect is the missing keyboard path in the panel. The Replace All binding can be chosen independently. [Zed input bindings](https://github.com/zed-industries/zed/blob/v1.28.1/assets/keymaps/default-linux.json#L402-L431) · [VS Code replacement commands](https://github.com/microsoft/vscode/blob/1.146.0/src/vs/editor/contrib/find/browser/findController.ts#L1071-L1099)

### Find navigation is unavailable from the source body

Open `find-sample.ts` at the top and use Ctrl+F to enter `auditNeedle`. Leave Find visible. Single-click within `StartAnchor` on line 1 without selecting text, press F3 three times, then press Shift+F3. Read the counter after each key.

Stroll stays at `1/3`. Zed reports `1/3`, `2/3`, `3/3`, then `2/3`. Zed and VS Code let users navigate matches while the source body has focus. Stroll's query-focused Enter and Shift+Enter work, so the defect is the missing direct operation from the body. The exact keys remain a product choice. [Zed pane bindings](https://github.com/zed-industries/zed/blob/v1.28.1/assets/keymaps/default-linux.json#L508-L516) · [VS Code navigation commands](https://github.com/microsoft/vscode/blob/1.146.0/src/vs/editor/contrib/find/browser/findController.ts#L724-L754)

### Reissuing Find closes the bar in Markdown

Open rendered `README.md`, press Ctrl+F, and enter `auditNeedle`. With focus still in the query, press Ctrl+F again. Repeat from a fresh Find session and click the `ReaderAnchor` heading before the second Ctrl+F. Check whether the bar stays visible and where typing would go.

Stroll closes the bar in both cases. Zed keeps it open and focuses the query in both cases. Stroll's own source editor also opens or refocuses the query with Ctrl+F. Reusing the same command to dismiss the shared panel only in the reader interrupts the normal query-editing sequence. [Zed Find focus binding](https://github.com/zed-industries/zed/blob/v1.28.1/assets/keymaps/default-linux.json#L402-L415) · [Zed Markdown binding](https://github.com/zed-industries/zed/blob/v1.28.1/assets/keymaps/default-linux.json#L1415-L1429) · [VS Code Find command](https://code.visualstudio.com/docs/editing/codebasics#_find-and-replace)

## Interaction differences

Use a fresh copy of the original TS fixture for each row that names an audit case. The [audit script](../scripts/editor-interaction-audit.py) accepts the case name via `--case`.

| Interaction | Reproduce and compare | Assessment |
| --- | --- | --- |
| Double-click continuous Chinese text | Double-click the second character, `文`, in `中文混合编辑尾巴` on line 3 and copy the selection. Stroll selects `文`. Zed selects `中文混合编辑尾巴`. The `double-click` audit also double-clicks `LatinWord`, which both select as an identifier. | Keep as a word-boundary choice. The Zed result covers the entire Han run. VS Code offers locale-based word segmentation as an option. [VS Code word segmentation setting](https://github.com/microsoft/vscode/blob/1.146.0/src/vs/editor/common/config/editorOptions.ts#L5388-L5416) |
| Ctrl+Backspace after continuous Chinese text | In `deletion`, put the caret directly after `中文混合编辑尾巴` on line 3 and press Ctrl+Backspace once. Stroll leaves `中文混合编辑尾`. Zed removes the Han run and preserves the space after `CjkAnchor`. | Keep as a word-deletion choice alongside double-click. Changing the deletion unit changes how much text one press removes. |
| Ctrl+Delete before consecutive punctuation | In `deletion`, move to column 1 of line 4, before `// wordProbe`, and press Ctrl+Delete once. Stroll removes one slash. Zed removes both. | Keep as a punctuation-boundary choice. |
| Parenthesis auto-pairing inside a TypeScript line comment | In `editing-commands`, place the caret after `// randomEdit SeedText` and type `(`. Stroll adds `(`. Zed adds `()`. Repeat at the end of `const pairInput = pairProbe` in a fresh fixture. Both add `()` in code. | Keep as a comment-context choice. VS Code's TypeScript rule also permits `(` pairing in comments, and its language API supports context-specific exclusions. IntelliJ IDEA exposes bracket pairing as a preference. [VS Code TypeScript rule](https://github.com/microsoft/vscode/blob/1.146.0/extensions/typescript-basics/language-configuration.json#L27-L55) · [VS Code language configuration](https://code.visualstudio.com/api/language-extensions/language-configuration-guide#_autoclosing) · [IntelliJ IDEA Smart Keys](https://www.jetbrains.com/help/idea/settings-smart-keys.html) |
| Commenting a selection across a blank line | In `line-commands`, start at column 1 on line 7, press Shift+Down twice and Shift+Right once, then Ctrl+/. Both comment lines 7 and 9. Stroll leaves line 8 empty and Zed inserts `// `. | Keep as a comment command choice. VS Code defaults to skipping empty lines and allows changing it. [VS Code comment options](https://github.com/microsoft/vscode/blob/1.146.0/src/vs/editor/common/config/editorOptions.ts#L1470-L1507) |
| Home on an indented line | In `line-commands`, click line 10, press End, then Home. Stroll moves to column 1. Zed moves to the first non-whitespace character. | Keep as a navigation choice. IntelliJ IDEA exposes Smart Home as a setting. [IntelliJ IDEA Smart Keys](https://www.jetbrains.com/help/idea/settings-smart-keys.html) |
| Ctrl+Backspace on a whitespace-only line | In `blank-line-deletion`, type three spaces on empty line 8, press Ctrl+Backspace once, then type `CARET`. Stroll removes line 8 and appends the marker to line 7. Zed clears the spaces and leaves the marker on line 8. | The current Stroll behavior is the agreed default. The earlier deletion into line 7 is recorded below. |
| Find follows a tab or a pane | Open `find-sample.ts`, find `auditNeedle`, quick-open `nav-alpha.ts` with Ctrl+P, then quick-open the first file again. Stroll hides Find on the second file and restores it on return. Zed keeps Find visible with `0/0` on the second file and restores the hits on return. | Keep as a session-scope choice. [Zed active-item search binding](https://github.com/zed-industries/zed/blob/v1.28.1/crates/search/src/buffer_search.rs#L504-L545) |
| Query seed with a caret and no selection | Find `auditNeedle` in `find-sample.ts`. With Find visible, single-click inside `StartAnchor` on line 1 and press Ctrl+F. Stroll retains `auditNeedle`. Zed seeds `StartAnchor`. As a control, select `StartAnchor` explicitly in the body and press Ctrl+F. Both can seed from an explicit selection. | Keep as a query-seeding choice. VS Code exposes selection and caret seeding as a setting. [VS Code Find options](https://code.visualstudio.com/docs/editing/codebasics#_seed-search-string-from-selection) |
| Reveal the first hit while typing | At the top of `find-sample.ts`, search for `EndAnchor`. In the long unified Git diff, click `DiffAnchor` near the top and search for `FarGitHit`. Wait for `1/1` before pressing Enter. Stroll keeps the viewport at the top until Enter in both views. Zed reveals both distant hits while typing. Both rendered Markdown views reveal a distant unique hit while typing `EndAnchor` in `README.md`. | Keep as a reveal-timing choice. IntelliJ IDEA exposes scrolling during typing as a setting. The separate source caret handoff defect still applies. [IntelliJ IDEA search options](https://www.jetbrains.com/help/idea/finding-and-replacing-text-in-file.html) |
| Git diff search scope | Use the standard workspace's Git diff. In unified view, search for `oldOnly` and `newOnly`. Both find one removed line and two added lines across two expanded files. Search for `filler 034` in omitted unchanged context. Both report zero. Switch to split view. Click the old side of `DiffAnchor`, open Find, and search for `newOnly`. Close Find and repeat from the new side for `oldOnly`. Both return zero for the opposite side. Return to unified view, collapse `diff-probe.ts`, and search for `oldOnly`. Stroll reports zero. Zed reports one and leaves the file folded after Enter. | Keep as a scope choice. The focus-dependent side and treatment of collapsed files are candidates for clearer scope cues. [Zed diff modes](https://zed.dev/docs/git#diff-view-styles) |
| Esc in rendered Markdown content | Open rendered `README.md`, find `auditNeedle`, and press Esc in the query field. Both close the bar. Reopen Find, click the `ReaderAnchor` heading, and press Esc. Stroll closes the bar. Zed leaves it visible in this build. | Stroll meets the shared dismissal requirement. |
| Source Find options | Open `find-sample.ts` at the top, open Find, and query `a.b`. With literal matching, both find `a.b` only. Enable regex in Zed and repeat to match `axb` as well. Inspect the available controls for regex, whole word, and search in selection. Stroll exposes literal text and Match Case. Zed and VS Code also offer the other three controls. | Record as feature gaps. [Zed search controls](https://github.com/zed-industries/zed/blob/v1.28.1/crates/search/src/buffer_search.rs) · [VS Code Find options](https://code.visualstudio.com/docs/editing/codebasics#_advanced-find-and-replace-options) |

## Resolved defects

The initial comparison used Stroll `fdf69c1`. The following regressions passed with the fixes in `ca6cfbc` against the original fixture. The shared Find check also passed from both the query field and body in Stroll's Git diff and Markdown reader.

| Defect | Reproduce and compare |
| --- | --- |
| Esc left source Find open | Run `--case search`, or press Ctrl+F, enter `auditNeedle`, click at the end of the `LatinWord` line, and press Esc. The old Stroll bar and highlights stayed visible. Zed and the fixed Stroll close the bar and leave focus in the source body. |
| History replay stole focus and blocked Forward | Run `--case history`. With `auditNeedle` visible, place the source caret after the second character of line 2 and quick-open `nav-alpha.ts` then `nav-beta.ts`. Go Back twice, then Forward twice. Stroll uses Alt+Left and Alt+Right. Zed uses Ctrl+Alt+- and Ctrl+Alt+Shift+-. The old Stroll focused the Find field at the source stop and stalled Forward. Zed and the fixed Stroll return focus to the source caret and reach both forward stops. |
| Caret motion and deletion split a ZWJ emoji | In `--case deletion`, use line 5, ending in `A👩‍💻B`. From End, press Left twice and verify the caret is before the complete emoji. In separate fresh copies, press End, Left, Backspace, or place the caret after `A` and press Delete. Old Stroll could leave part of the emoji and a stray U+200D. Zed and the fixed Stroll move across and delete the complete grapheme, leaving `AB` on deletion. [Unicode grapheme boundary rules](https://www.unicode.org/reports/tr29/#Grapheme_Cluster_Boundaries) |
| Ctrl+Backspace deleted past a member-access dot | In `--case deletion`, press End and Ctrl+Backspace on line 4, ending in `beta_gamma.delta`. The old Stroll removed the entire dotted expression. Zed and the fixed Stroll remove `delta` and leave the prefix and dot. The retest also appends `.delta` to `const pairInput = pairProbe` and verifies both editors preserve `pairProbe.`. |
| Ctrl+Backspace on a whitespace-only line deleted previous-line text | Run `--case blank-line-deletion`, starting at line 8 with three spaces and pressing Ctrl+Backspace once. The prior Stroll could delete text on line 7. The fixed Stroll removes line 8 and preserves all of line 7, placing the caret at its end. The current comparison with Zed is listed above. |

## Automated audit

The [audit script](../scripts/editor-interaction-audit.py) drives installed Stroll and Zed builds with X11 keyboard and mouse events. Both open an independent Git workspace containing the same [TypeScript fixture](../tests/fixtures/editor-interactions.ts). Each case starts with fresh workspace and editor state.

### Run on Linux

Use Python 3, Git, Zed (`zeditor` or `zed`), Xvfb, xauth, xrdb, xdotool, xclip, ImageMagick, Tesseract with its `eng` data, and a Vulkan renderer supported by both editors. Install a CJK font and an emoji font for the mouse and grapheme cases. The script starts a private Xvfb server and uses temporary Stroll and Zed profiles. The saved evidence directory is printed at startup.

```sh
cargo build --release --locked --bin stroll
python3 scripts/editor-interaction-audit.py
python3 scripts/editor-interaction-audit.py --case editing --seed 19 --seed 42
python3 scripts/editor-interaction-audit.py --case search --case history --case tabs
```

Use `--stroll` or `--zed` to select a binary and `--output` to choose a new evidence directory. If Xvfb fails during EGL initialization on a machine with multiple EGL vendors, select the installed Mesa vendor JSON using `__EGL_VENDOR_LIBRARY_FILENAMES` and set `LIBGL_ALWAYS_SOFTWARE=1`. Software Vulkan rendering also requires an installed driver such as lavapipe.

### What the checks exercise

| Case | Observations |
| --- | --- |
| `editing` | Two default seeds run 24 real editing actions each. Typing, caret motion, selection replacement, Backspace and Delete are checked against a small text model at save checkpoints. |
| `deletion` | Plain deletion, Latin word deletion, dotted and adjacent punctuation boundaries, CJK deletion, and caret motion and deletion around a ZWJ emoji. |
| `blank-line-deletion` | Adds whitespace to the fixture's existing blank line and records both the edited text and the caret after one Ctrl+Backspace. |
| `editing-commands` | TypeScript bracket pairing and indentation, Unicode clipboard paste, undo and redo. |
| `double-click` | Clipboard selection from a Latin identifier and a verified point in CJK text. |
| `search` | Initial and next match counters, Escape from either focus location, focus after dismissal, and refocusing Find with the retained query. |
| `history` | Back and forward across three files, including the exact source caret and editor focus when returning to a file with a search session. Opening a fourth file after rewinding checks the forward branch and Find's visibility across tabs. |
| `tabs` | Quick-open reactivates an existing offscreen tab after opening twenty additional files. The audit verifies the document and the visible tab label separately. |
| `selection-clipboard` | Forward and reversed selections, typing at multiple cursors, the cursor retained by Escape, and copying, pasting, and cutting whole lines. |
| `line-commands` | Moving and deleting lines, toggling comments on single lines and selections that span blank lines, and Home on an indented line. |

The clipboard and saved fixture are the text oracles. Screenshots and OCR track visible search controls and the tab shelf. The JSON report preserves each editor's observation, any explicit expectation, and the comparison. Seed traces preserve the generated edits. A comparison exits with code 0 for passes and matches, 1 for discrepancies, or 2 if a probe is inconclusive. Word boundaries and comment pairing can differ by editor policy and require interpretation.

Stroll's picker completion also has a headless layout test. It simulates returning an existing path from the platform dialog and checks that the whole tab falls within the visible strip and the tab count stays the same.

```sh
env -u DISPLAY -u WAYLAND_DISPLAY cargo test --release --locked choosing_an_open_local_file_reveals_its_overflowed_tab
```

Unicode text insertion uses the clipboard. IME composition, Wayland, and the system's native file dialog UI require desktop verification.

The 2026-11-19 automated run used seeds 19 and 42. After the fixes and the whitespace-only line case were added, 60 of 68 checks met their expectations or matched across editors. The interaction differences above record the remaining eight.

## Prepare the comparison workspaces

Run from the repository root. The original fixture covers editing and replacement. The generated files supply distant hits, Unicode case pairs, a two-file diff, a long diff, and large rendered Markdown. The Git fixtures commit the baseline and leave the edits unstaged.

```sh
comparison=$(mktemp -d)

python3 - "$comparison" <<'PY'
import os
from pathlib import Path
import subprocess
import sys

root = Path(sys.argv[1])
source = Path("tests/fixtures/editor-interactions.ts").read_bytes()
sample = [
    "// StartAnchor",
    'const first = "auditNeedle";',
    'const capital = "AuditNeedle";',
    'const accent = "CAFÉ";',
    'const other = "café";',
    'const meta = "a.b axb";',
    *(f"// source filler {i:03d}" for i in range(100)),
    'const distant = "auditNeedle"; // EndAnchor',
]
reader = (
    "# ReaderAnchor\n\n"
    "Start auditNeedle and **formattedNeedle** with [shownLabel](https://example.com/hiddenTarget).\n\n"
    "Accent CAFÉ and café.\n\n"
    + "".join(f"Paragraph number {i:03d}.\n\n" for i in range(72))
    + "EndAnchor auditNeedle.\n"
)
large_reader = "# ReaderAnchor\n\n" + ("hitcount " * 25 + "\n\n") * 200 + "hitcount LastAnchor.\n"
base = (
    '// DiffAnchor\nconst delta = "oldOnly";\nconst common = "sharedNeedle";\n'
    + "".join(f"// diff filler {i:03d}\n" for i in range(70))
    + 'const tail = "beforeTail";\n'
)
modified = base.replace("oldOnly", "newOnly").replace("beforeTail", "afterTail")
side = '// OtherAnchor\nconst side = "beforeSide";\n'
added = "".join(f"// new filler {i:03d}\n" for i in range(65)) + "// FarGitHit MarkerNearFarGit CAFÉ café\n"
git_env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
git_env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")

for app in ("stroll", "zed"):
    for scenario in ("standard", "long"):
        repo = root / app / scenario
        repo.mkdir(parents=True)
        (repo / "editor-interactions.ts").write_bytes(source)
        for name, content in {
            "find-sample.ts": "\n".join(sample) + "\n",
            "nav-alpha.ts": "// Alpha destination\n",
            "nav-beta.ts": "// Beta destination\n",
            "README.md": reader,
            "cap.md": large_reader,
            "diff-probe.ts": base,
            "other-probe.ts": side,
        }.items():
            (repo / name).write_text(content, encoding="utf-8")
        for args in (
            ["init", "-q"],
            ["add", "--all"],
            ["-c", "user.name=Editor Audit", "-c", "user.email=audit@example.invalid",
             "-c", "commit.gpgSign=false", "-c", "core.hooksPath=/dev/null",
             "commit", "-qm", "Add comparison fixtures"],
        ):
            subprocess.run(["git", *args], cwd=repo, env=git_env, check=True)
        if scenario == "standard":
            (repo / "diff-probe.ts").write_text(modified, encoding="utf-8")
            (repo / "other-probe.ts").write_text(side.replace("beforeSide", "newOnly"), encoding="utf-8")
        else:
            marker = "// diff filler 001\n"
            (repo / "diff-probe.ts").write_text(modified.replace(marker, marker + added), encoding="utf-8")
PY
```

Use the matching editor's worktree for each run. After a case that edits source, close the file and restore it with `git -C "$comparison/stroll/standard" restore -- editor-interactions.ts`, substituting the editor and fixture used. Use the build command in the [Linux audit instructions](#run-on-linux) and these launch helpers for a fresh profile on each run.

```sh
stroll_compare() (
    profile=$(mktemp -d "$comparison/stroll-profile.XXXXXX")
    cat > "$profile/stroll.toml" <<'TOML'
version = 1
[appearance]
theme = "light"
language = "en"
[lsp]
enabled = false
TOML
    STROLL_GPUI_DATA="$profile/data" STROLL_CONFIG="$profile/stroll.toml" \
        target/release/stroll "$@"
)

zed_compare() (
    profile=$(mktemp -d "$comparison/zed-profile.XXXXXX")
    mkdir -p "$profile/config"
    cat > "$profile/config/settings.json" <<JSON
{
    "theme": "One Light",
    "enable_language_server": false,
    "auto_update": false,
    "disable_ai": true,
    "format_on_save": "off",
    "remove_trailing_whitespace_on_save": false,
    "ensure_final_newline_on_save": false,
    "preview_tabs": {"enabled": false},
    "session": {"trust_all_worktrees": true},
    "diff_view_style": "${ZED_DIFF_STYLE:-unified}",
    "minimum_split_diff_width": 0
}
JSON
    zeditor --foreground --user-data-dir "$profile" "$@"
)
```

- **Source.** Run `stroll_compare "$comparison/stroll/standard" find-sample.ts` or `zed_compare "$comparison/zed/standard" "$comparison/zed/standard/find-sample.ts"`. Use `editor-interactions.ts` for the original fixture.
- **Git diff.** Run `stroll_compare --git "$comparison/stroll/standard"` and open **Uncommitted changes**. In Zed, run `zed_compare "$comparison/zed/standard"` and execute `git: diff head` in the command palette. Substitute `long` for the long diff. For side-by-side, select **Side-by-side diff** in Stroll and launch Zed with `ZED_DIFF_STYLE=split zed_compare "$comparison/zed/standard"`.
- **Rendered Markdown.** Run `stroll_compare --docs "$comparison/stroll/standard" README.md`. In Zed, open the matching `README.md` as in the source launch, then execute `markdown: open preview` in the command palette. Use `cap.md` in both apps for the match-limit case. Click the rendered body before invoking Find if focus is elsewhere.
