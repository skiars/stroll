# File tree desktop integration

The workspace, document, and Git change trees can open local files in the
default application and locate local paths in a file manager. Git history and
staged entries explicitly refer to the current worktree. A missing Git path can
reveal its nearest existing directory.

The workspace tree accepts single-entry moves, file and directory copies, and
cut/paste. Single-click a node to preview it and focus the tree for shortcuts.
Double-click to pin a file and focus the editor. Audio rows keep the existing
preview focus behavior and provide file operations in their context menu.
Drop on a folder or the tree header to target that folder or the worktree
root. External drop copies accessible local paths into the local tree.
File operations reject a conflicting destination. Directory copies use a
temporary sibling and publish each top-level entry after its copy succeeds.

| Environment | System open and reveal | External drop into local tree | Native file clipboard |
| --- | --- | --- | --- |
| Linux Wayland | Default launcher and named Dolphin, Nautilus, or Thunar when detected. Generic file manager fallback. | Files and folders via file URIs | Bidirectional URI lists, KDE cut flag, and GNOME file clipboard format |
| Linux X11 | Same launcher and file manager detection | Existing file-path drag events | File clipboard actions disabled |
| macOS | Default application and Finder | Existing GPUI path drag events | Finder file URLs. Use Option-Command-V for Finder-style move on paste. |
| Windows | Default application and Explorer | Existing GPUI file drop via CF_HDROP | CF_HDROP and Preferred DropEffect |
| SSH worktree | Local system actions are omitted | Local import entries are disabled | Copy, cut and paste operate within the worktree with an upgraded helper |

The SSH helper negotiates workspace transfer separately from the existing
file-operation capability. Reconnect after installing the updated helper.
Earlier helpers continue to offer their existing file operations.

## Remaining work

- Run native drag, clipboard round-trip, and system launch tests with Explorer
  on Windows and Finder on macOS. The current development environment uses
  Linux. On both systems, include multiple paths, Unicode, cancellation, and
  replacement of the clipboard during a pending operation.
- Verify Nautilus and Thunar launch behavior and Nautilus clipboard
  interoperability in native Linux desktop sessions. Thunar currently opens
  the containing directory when revealing a file. Linux shows a generic
  manager name for desktop handlers without a known launcher.
- Add X11 file clipboard support if needed.
- Add local-to-SSH uploads before enabling external drop or paste on SSH
  trees. Cross-worktree and cross-host transfer would need their own transport
  and destination identity.
- Extend compatibility to virtual Shell objects and portal-only file
  transfers. A source must currently provide a readable filesystem path.
- Add cross-filesystem moves, copy support for symbolic links and reparse
  points, and more complete directory metadata preservation. A move across
  filesystems currently returns the filesystem error and preserves its
  source. Moves within a filesystem preserve the original entry. Recursive
  copy currently uses default directory permissions and new timestamps.
  Copies containing nested Git metadata directories are rejected.

## Verification

Run the test suite as described in [headless validation](headless-validation.md).
The workspace transfer test synthesizes both internal and external drag events.
It checks draft and tab migration, directory import, and preservation of the
external sources. The repository transfer tests cover conflicts, stale
entries, recursive copy, and staging cleanup.

An isolated Linux Wayland session verified that the Stroll menu named Dolphin
and sent the selected path to its launcher. A native Dolphin drag copied a
directory into the selected Stroll folder and preserved the source.
Clipboard tests used Dolphin in both directions for copy and cut. A Wayland
client also exercised the GNOME cut format for paste into Stroll. The SSH
workspace test exercised the actual helper through a local transport shim.

For native validation, launch Stroll with temporary paths for
`STROLL_GPUI_DATA` and `STROLL_CONFIG` and a disposable repository.
In a Wayland session, check that a local file context menu names the configured
manager and selects the file. Drop a directory and a file with spaces from the
manager onto a folder row. Copy and cut in each direction between Stroll and
the manager. Confirm that copy leaves the source intact and that a successful
cut moves it once. With an SSH worktree and the current helper, repeat the
tree-internal move and paste and confirm that drafts follow their paths.
