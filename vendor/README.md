# GPUI patches

The patched crates are the crates.io releases selected in Cargo.lock.
The published Apache-2.0 licenses are included. The local manifests omit
example and benchmark targets. The application tests live in `tests/`.

| Crate | Release | Local changes |
| --- | --- | --- |
| gpui-pre | 0.3.12 | Backdrop draw ordering, cached scene replay, paint-only exit frames, smooth wheel scrolling, touchpad momentum estimated from input timestamps with per-frame scroll delta coalescing, device-pixel alignment for scrolled document content, paint boundaries for overlays and scroll regions, detection of solid vector paths reusable as document content moves, per-frame atlas and bounds lookup reuse for repeated glyphs, document thumbnail caching by draw size, per-image linear or nearest-neighbor sampling, skipped texture atlas allocation for images outside the clip region, element bounds in release tests with `test-support`, client decorations in test windows, Linux and Windows text rendering presets with cached fractional glyph positions, dashed and dotted text underlines with device-pixel clipping, and typed file clipboard helpers |
| gpui-pre-wgpu | 0.3.12 | Nearest-neighbor image sampling with exact atlas texel reads, region-limited two-pass blur, a copyable intermediate target, retained document content beneath overlays and GPU instances for stationary overlays, direct downsampling from document content with the first live-blur pass combined, blur texture reuse by sample tile, GPU timings for each blur stage, culling of outer-shadow geometry inside overlays, tile index buffer reuse, merging of adjacent repaint regions and skipping batches outside them, and Swash glyph rasterization with device-pixel offsets and configurable hinting |
| gpui-pre-windows | 0.3.12 | Nearest-neighbor image sampling with exact atlas texel reads, region-limited two-pass backdrop blur in the DirectX renderer, support for overlay paint boundaries, configurable DirectWrite grid fitting and fractional glyph positioning, and native file clipboard formats |
| gpui-pre-apple | 0.3.12 | Nearest-neighbor image sampling in the Metal shader, with shader bindings generated from the adjacent patched GPUI crate |
| gpui-pre-linux | 0.3.12 | Forwarding of Wayland touchpad gesture phases, scroll deltas, and `AxisStop` timestamps, with timing for submission return, frame callbacks, and compositor presentation feedback, and Wayland file clipboard formats |
| gpui-base | 0.9.1 | A gutter strip flush with the editor's left edge, shared popup fade and popover and tooltip exit lifecycle, managed tooltip requests with optional positioning and paint-only presentation, shared tooltip origin motion for entrances and adjacent triggers, progressive editor soft wrapping, source-anchored inline labels with wrapping and hit testing, and a logical viewport observer, silent range updates and bounded peer scrolling for projected editors, smooth wheel scrolling for text inputs and scroll wrappers, fade-only scrollbar exits that hold their position, persistent code-editor and document scrollbars, Markdown image click handling, a shared data URL image decoder, Markdown extensions installed before the first parse, diagnostic underline precedence over semantic and link styles, shared hover state for overlapping diagnostics and asynchronous documentation, code action request lifecycles and line probes, and tagged editor undo transactions for multi-file edits and external Git changes, and text-only snapshots of completed undo and redo groups, workspace code fence aliases for hover providers, and an optional per-editor Enter edit using the shared editor history, and completion provider input hooks and atomic candidate edits |
| gpui-component | 0.9.1 | Shared popup material and seamless Popover arrows, managed tooltip triggers with immediate exit after hover handoff, accessible checkable callback menu items, split buttons with standard button heights and matching corner radii, in-window menu host, editor hover lifecycle, version-checked completion menu selection, a single hover surface for documentation and Web-style diagnostic rows, flat documentation code blocks in editor popovers, embedded code action previews and diagnostic shortcuts in the shared hover surface, inline code action indicators, re-exported editor inlay labels, background syntax highlighting, code delimiter captures and Markdown injections in diff highlighting, shared Bash filename and shebang detection for editors and diffs, scoped syntax tokenizers and fence aliases shared by editor, diff, and Markdown rendering, and parser factories reused for editing syntax context, incremental diff loading and navigation, path-based file reconciliation, layer-aware source anchors, and pixel offsets for projected diff viewports, Markdown image click forwarding and data URL image decoder re-export, per-field reset buttons in settings, per-button tooltip placement and delay restart options, per-instance checkbox indicator colors including temporary input blocking, a notification icon that shares the first line's top edge, thin non-content scrollbars with an 800 ms idle hold and 300 ms exit, a scrollbar-motion accessor for custom-painted indicators, inset 22 px circular window-control highlights with full-size hit areas, darker neutral hover colors, and a solid red close state, and extended code theme captures, text decoration, and code-surface colors |

To upgrade, start from the corresponding published crates and reapply the
local changes. Run `cargo test --release --locked` on Linux with a Vulkan or
other supported WGPU adapter. A software Vulkan adapter also runs the
pixel regression. Check editor and tab menus, hover, the overflow list and
dialogs in a Wayland window after changes to paint order or focus handling.

- The application uses the UI font size as its window rem size and as the shared
  control font size for inputs, searchable select rows, dropdown buttons, and
  popup menu options. The Git comparison integration test covers live changes.
- In-window context menus use a leading icon column two logical pixels wider
  than the default menu icon size. The popup integration test checks the
  leading column and label alignment.
- Default inputs and dropdowns are twice the UI font size. Custom
  heights are respected, and fixed-height input groups fit the inner field inside
  the group's border. The popup and Git integration tests check control geometry.
- Resize handles support configurable hit padding and hover delay, scroll
  passthrough, and drag callbacks anchored at the initial press. These options
  let consumers customize a splitter through the existing appearance hook.
  The resize handle tests cover default behavior and opt-in geometry and input.
- The shared editor search and replace fields use the default input height.
- The editor, Git diff, and Markdown reader mount the same Find panel.
  All three targets use Unicode case-insensitive literal matching. The Markdown
  reader counts and navigates every match in rendered text, and the text view
  highlights the first 5,000 results and the active result wherever it occurs.
  The reader tests cover wrapping past the highlight limit and Find with Escape.
- Explicit Find navigation selects the reached match before returning focus to
  the editor. Single Replace resumes after the inserted text so a replacement
  containing the query does not consume the next Replace click. The editor
  editing tests cover both workflows and Unicode matching.
- Plain Enter in the replacement field runs Single Replace. The shared Markdown
  Find panel accepts a seed query from the caller. Stroll's reader handles
  selection and refocus policy. The reader and editor editing tests cover these
  actions.
- Source Find shortcuts navigate relative to the editor selection and reuse a
  retained query without moving focus into the Find panel.
- Escape in the editor dismisses its built-in Find session after transient
  editor overlays. An explicit Find action requests focus once. Remounting a
  retained Find session restores its query without moving focus from the editor.
  The editor editing and workspace source history tests cover both routes.
- Interactive caret motion and deletion use extended Unicode graphemes across
  rope chunks. Code-editor word commands recognize dots before member names
  while keeping decimal segments intact. The editor editing tests cover both.
- Backward word deletion on a whitespace-only code-editor line removes that
  row up to the previous line's end and keeps the preceding text. It shares the
  line-ending and undo handling with the editor's other deletions.
- CMake uses the shared Tree-sitter highlighter with a query that uses Rust
  Tree-sitter predicates. Run `cargo test --release --locked --test cmake_highlighting`
  to check highlighting in the source editor, Git diff, and Markdown code fences.
- Select dropdowns in dialogs use the owning dialog's paint priority so their
  search field and rows stay visible over the dialog. The Git version comparison
  integration test checks the paint order for both selectors.
- Button dropdown menus use the same owning-dialog priority as Select. The
  Git interactive rebase test clicks an action where the menu overlaps its
  dialog and checks that Reword opens the commit message editor.
- Select dropdowns open below when there is room for their search field and one
  row. The candidate viewport is limited by the window edge and scrolls under
  the fixed search field. Search results do not affect the side used for
  placement. The shared select and narrow Git comparison integration tests
  cover placement, filtering, and scrolling.
- After input changes, run `cargo test --release --locked --test touchpad_scroll --test smooth_scroll`
  to check logical document offsets and device-pixel paint offsets. Check continuous scrolling
  in documents, editors, and lists in a native Wayland session.
- Wayland IME edits retain the last submitted candidate rectangle until GPUI
  lays out the updated text. The platform tests cover deferred updates and
  protocol rectangle deduplication. For native verification, compose across
  a wrap boundary near the end of a long file and check that the rectangle
  does not alternate between rows in a `WAYLAND_DEBUG=client` trace.
- Fold candidates are remapped through edits and replaced when the background
  highlighter publishes its completed parse. The editor editing integration test
  checks candidate continuity, line remapping, and removal.
- Pointer-triggered diagnostics and documentation share a 300 ms reveal delay.
  The editor requests documentation halfway through the delay and adds late
  results to the same popup, including while its Quick Fix menu is open.
  Moving to another symbol resets the timer, including within a shared
  diagnostic range. Clicking Quick Fix or the inline lightbulb, or using the
  Code Actions shortcut, opens the menu immediately.
  Explicit diagnostic presentation replaces a pending pointer hover immediately.
  The popup integration tests cover timing, cancellation, and scrolling.
- Interactive code editor edits reveal the caret with minimal scrolling before shaping the
  viewport. Programmatic replacements and history replay use the existing caret-follow margin.
  Mapped edit transactions support both endpoint and range callbacks. Range
  callbacks can preserve the original selection and its direction at insertion boundaries.
  Removing inline Git deleted rows keeps the visible source anchored. The workspace
  opts its source editor into whole-line clipboard actions for collapsed selections.
  Clipboard metadata distinguishes whole lines from ordinary text, and multi-line
  cuts and pastes use the shared undo transaction. Run
  `cargo test --release --locked --test editor_editing` and
  `cargo test --release --locked --lib editing_below_an_inline_hunk` for coverage.
- Caret-anchored editor popups use intrinsic layout so short signatures size to
  their content and viewport caps apply to long signatures. The workspace
  signature-help and popup integration tests verify the rendered bounds.
- Source editor Up and Down reach the document edges when no visual row remains
  in that direction. The editor editing integration tests verify the boundary
  behavior and column preservation when moving between rows.
- Source editors opt into deleting, duplicating, moving and commenting logical lines.
  Commands merge overlapping or adjacent line selections and use a single undo transaction.
  The language provider supplies built-in comment markers. Language extensions can supply
  their own through `comments.lineComment`. The editor editing integration tests exercise the
  shortcuts, selection tracking, line endings and the interactive scroll policy.
- The Linux text backend converts high-precision vertical glyph phases to Swash's
  upward outline axis. Run `cargo test --release --locked --test text_rendering`
  to verify quarter-pixel glyph motion across whole pixels at 100% and 125% scale
  with grayscale and LCD masks.
- The code editor can append items to its built-in context menu. The workspace uses this hook
  for Find References. Run `cargo test --release --locked --lib find_references` to check that
  the built-in items and the appended action share the same menu and editor focus.
- The workspace editor opts into a navigable overview ruler that mirrors gutter decorations
  and diagnostics in 3 px lanes. The vertical track resolves clicks by height and uses the
  track halves to disambiguate overlaps. A press over a mark can still start a thumb drag.
  Run `cargo test --release --locked --test scrollbars` for paint and mouse interaction coverage.
- The workspace editor opts into a 16 px Git gutter, including its gap before the line numbers.
  Actionable marks use the entire strip for hit testing and animate their painted width from
  5 px to 12 px with a fixed 1 px stroke for outlines. Line numbers and text keep their positions.
  Stroll selects outlined marks for staged hunks, including expanded hunks.
  Run `cargo test --release --locked --test scrollbars interactive_editor_gutter` for coverage.
- The Git review generates draft worktree patches in `src/git/changes.rs` and
  passes them to the shared diff parser. Run
  `cargo test --release --locked --lib custom_workspace_diff_edits` to verify
  drafts, undo, and save against a selected comparison base.
- To verify presentation feedback in a native Wayland session, set
  `STROLL_FRAME_TIMINGS=1` and check `wayland_submit`,
  `wayland_present_return`, `wayland_callback`, and the compositor's
  `wayland_presentation` entries.
- After image upload changes, run `cargo test --release --locked --test documents`
  to check atlas dimensions for inline thumbnails and full-resolution previews.
- Run `cargo test --release --locked --test images` on Linux for a GPU pixel comparison
  of the two image sampling modes. Check a magnified pixel-art image on DirectX and
  Metal to verify that only nearest-neighbor rendering preserves hard pixel edges.
- The shared Switch uses a 34 × 20 px track and a 16 px thumb with a 2 px inset at its
  default size. The default light and dark themes use a white, softly shadowed thumb. Run
  `cargo test --release --locked --test switch` to check the track and thumb geometry
  in both themes. Set `STROLL_SWITCH_SCREENSHOTS` to capture both states offscreen.
- Run `cargo test --release --locked --test popups editor_hover_appends_documentation_to_web_diagnostics`
  to check that diagnostics, code actions, previews, and documentation share
  the same editor surface. Set `STROLL_ACTION_SCREENSHOTS` to capture
  both themes with 14 px pencil and preview icons. The workspace language action
  tests check the 22 × 22 px hit areas for the inline bulb and preview button,
  equal top, bottom, and right insets for the preview button in a single-line row,
  draft edits, and grouped undo and redo.
  Current-line probes use the same request trigger as the explicitly opened menu.
- Atomic editor batches accept a selection offset mapper and record the results
  alongside the original selections for undo and redo.
  Run `cargo test --release --locked --lib language_actions` to verify code
  actions, grouped rename, and a formatting edit distant from the selection.
- The editor exposes current-frame line-end space and the shaped text baseline of
  the cursor's final wrapped row for paint-only line decorations. Interactive tooltips can restart their delay on pointer
  motion, hold while the pointer is over the popup, and position within an editor viewport.
  Run `cargo test --release --locked editor_blame` for first-frame scrolling and baseline alignment,
  pending edits, hover timing, popup handoff, and clipboard actions.
- The editor can reveal a caret with context once the target row has been wrapped and
  laid out. Inline hunk jumps can anchor to the virtual deletion block. The workspace
  definition jump test covers a newly opened buffer, soft wrapping, and EOF.
- Mouse-triggered tooltips and editor hovers validate their original target after layout.
  A target moved under a stationary pointer waits for a pointer move. The release tests
  `editor_hover_waits_for_pointer_motion_after_scrolling` and
  `commit_preview_waits_for_pointer_motion_after_scrolling` cover this behavior.
- Client-decorated roots paint their background under the frame border and use
  concentric content corners to prevent a transparent seam. Check the frame
  and its content against a contrasting background in a native Wayland window.
- The default small and extra-small text tiers use 12 logical pixels.
  Control size presets retain their dimensions and explicit text sizes remain
  available for compact badges. The text rendering tests cover both tiers
  and an explicit smaller font size.
- After overlay compositing changes, run `cargo test --release --locked --test popups`.
  The pixel tests compare partial updates with full-window repaints and cover rounded
  table corners drawn as vector paths, disjoint repaint regions, overlay fade-out, and
  translucent popup arrow joins at multiple scales. Commit preview tests also verify the
  shared Popover material at the viewport edge, animated Tooltip entry and exit, and pointer pass-through.
  Also check scrolling inside and outside overlays in a native Wayland session.
- On Windows, run `cargo test --release --locked --test popups --test smooth_scroll`
  in a desktop session to exercise DirectX readback and mouse wheel interpolation.
- Run `cargo test --release --locked --lib staging_keeps_the_diff_viewport_and_checkbox_colors`
  to compare the staging frames and verify that source lines and the file tree stay in place.
  Set `STROLL_GIT_SCREENSHOTS` to save these frames as PNG files.

- Run `cargo test --release --locked --lib partial_stage` to verify staged and
  unstaged layer transitions and restoration after window activation. The
  split-view case saves PNGs when `STROLL_GIT_SCREENSHOTS` is set.
- Run `cargo test --release --locked --lib cold_bottom_probe -- --ignored --nocapture`
  to exercise the projected editor at the end of a large diff.

- Run `cargo test --release --locked --test checkbox` and
  `cargo test --release --locked --lib document_list_switches_and_restores_by_worktree`
  to verify that release tests can read element bounds.

- The settings component takes an instance-level text size for headings,
  labels, and controls. Its default is 13.5 px, with 12 px descriptions.
  Accordion chevrons follow the component's size preset.
  Search and field controls keep their size presets, and the number input
  passes typography overrides to its editable text. The settings typography
  test checks the instance font size and label and description line heights.

- Run `cargo test --release --locked --lib editor_renders_web_theme_decorations_in_light_and_dark`
  on Linux to verify semantic underline patterns, strikethrough, and diagnostic
  precedence in the editor's rendered scene. Set `STROLL_THEME_SCREENSHOTS`
  to a directory to save both appearances.

- Run `cargo test --release --locked --test popups editor_hover_appends_documentation_to_web_diagnostics`
  to verify overlapping diagnostic order, gradient tokens, and asynchronous documentation in
  the same hover surface. Set `STROLL_DIAGNOSTIC_SCREENSHOTS` to save light and dark images.

- Run `cargo test --release --locked --lib persistent_edit_history` to verify edit and redo snapshots across app restarts, including shared multi-file transactions.

- The shared hover provider can publish additional documentation after the
  original response. The existing popover keeps its anchor, scroll position and
  motion when the update arrives. Paired Markdown paragraphs use the shared text
  renderer and wrap independently. Run
  `cargo test --release --locked --test popups editor_hover_enrichment` to
  verify the delayed update, layout and cancellation.
- Paired hover paragraphs preserve the editor's code font. Applications can
  set `Theme::hover_inline_code_size` to override the default inline code
  scaling. Run `cargo test --release --locked --test hover_font` to verify the
  rendered font families, sizes, and prose weight.
- Markdown file links in hover documentation use the editor's existing
  show-document hook. Web links keep their default URL behavior. Run
  `cargo test --release --locked --lib hover_source_links_use_workspace_navigation`
  to verify link clicks, source positions, and navigation history.
- The editor that owns a documentation popup defers hover retargeting during
  pointer handoff from the symbol. The documentation uses the shared clipboard
  button to copy the original LSP text, preserving Markdown and literal
  plaintext. The square button has equal top and right insets and an icon sized
  to the documentation text. Run
  `cargo test --release --locked --test popups lsp_hover_stays_open`
  to verify pointer handoff across editors, selection, button layout, clipboard
  content, and dismissal.

- Tab accepts the selected item in the shared editor completion menu through
  the same edit path as Enter. With no list candidate it retains inline
  completion and indentation. Candidate rows use completion kind icons and
  separately truncate the label and detail. Run
  `cargo test --release --locked workspace_completion` for the workspace
  fixture and `cargo test --release --locked --test popups completion_popup`
  for offscreen geometry and selection coverage. Set
  `STROLL_COMPLETION_SCREENSHOTS` to save the popup screenshots.
  Candidate text and kind icons use the current editor font size minus 0.5 px.
  The list stays visible when typing extends its query and keeps the selected
  candidate when it survives the refresh. The list alone determines its
  initial side and size, keeps its side for the session, and resizes from
  the edge nearest the caret. Documentation aligns to that edge when
  alongside the list and uses remaining space beyond the list when stacked.
  The session closes if the caret leaves the editor viewport or the retained
  side can no longer fit a row. A close followed by a reopen before the
  next render also chooses a fresh side. The menu
  validates the document and menu revisions before applying a candidate's edits.

- C++ overload names use function captures, and primitive type captures
  recover after declaration macros. Boolean literals `true` and `false` use
  the existing `boolean` capture. The editor maps unaffected semantic token
  ranges and inlay rows through edits while refreshes are pending. Run
  `cargo test --release --locked cpp_highlights_survive_delayed_semantic_responses`
  and `cargo test --release --locked inlay_hints` to verify paint, wrapping,
  projected diff hints, and stale language-service responses.
  Inlay hints use the light weight of the current editor font. The light and
  dark editor theme captures include a rendered hint.

- Interactive edits remap surviving diagnostics using the editor's tracked range
  logic, including partial deletions. An open diagnostic hover follows its
  remapped target and reconciles the current diagnostic set after layout. Run
  `cargo test --release --locked --test editor_editing edits_track_surviving_diagnostics_until_the_next_reply`
  and `cargo test --release --locked --test popups open_diagnostic_updates_in_place_after_an_edit`
  to verify range and popup updates. The workspace
  `editor_annotations_handle_delayed_out_of_order_and_empty_results` test controls
  LSP replies. With clangd installed, run
  `cargo test --release --locked --lib local_cpp_diagnostics_and_highlights_stay_visible_during_edits -- --ignored`
  for frame checks during timed backspace and restore sequences in workspace and
  standalone windows.

- The shared caret-anchored popup can prefer the space above the caret. The
  workspace uses this placement for LSP signature help and hides the hint while
  the completion or code-action menu is open. The workspace caret-hint test
  exercises the shared popup in a headless window.

- The file clipboard item uses the existing paths and text entries and carries
  the cut action alongside them. Wayland translates those entries to and from
  URI lists and the KDE and GNOME file clipboard formats. The Windows backend
  translates them to and from CF_HDROP and Preferred DropEffect. The format
  tests cover URI round trips and the Windows wide path list. See
  [file tree desktop integration](../docs/workspace-file-operations.md) for
  the native round-trip checks.
