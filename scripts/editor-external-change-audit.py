#!/usr/bin/env python3
"""Exercise filesystem changes through the real editor, keyboard, and save prompts."""

import argparse
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "editor_interaction_audit", Path(__file__).with_name("editor-interaction-audit.py")
)
audit = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(audit)

FILE = "fixture.txt"
BASE = "alpha line\nbeta line\ngamma line\n"
LOCAL = BASE.replace("alpha line", "alpha line LOCAL")
EXTERNAL = BASE.replace("beta line", "external beta line")
NEWER = BASE.replace("beta line", "newer beta line")
SETTLE = 0.85


class Editor(audit.Editor):
    def __init__(self, name, root, evidence, env, standalone):
        super().__init__(name, root, evidence, env)
        self.path = root / FILE
        self.standalone = standalone

    def find_window(self):
        title = ("fixture\\.txt" if self.standalone else "Stroll$") if self.name == "stroll" else (
            "fixture\\.txt|standalone-audit" if self.standalone else "editor-audit"
        )
        result = subprocess.run(
            ["xdotool", "search", "--onlyvisible", "--name", title], env=self.env,
            capture_output=True, text=True, timeout=3,
        )
        return next(iter(result.stdout.splitlines()), None)

    def text(self):
        value = self.buffer()
        self.key("ctrl+Home", settle=0.04)
        return value

    def clipboard(self):
        for attempt in range(3):
            try:
                return super().clipboard()
            except subprocess.CalledProcessError:
                if attempt == 2:
                    raise
                time.sleep(0.05)

    def disk(self, path=None):
        path = path or self.path
        return path.read_bytes().decode("utf-8", errors="backslashreplace") if path.is_file() else None

    def change(self, action, text="", settle=SETTLE):
        # The driver is also a separate process from the editor, but use a
        # one-shot writer so its completion boundary is explicit for races.
        self.run(sys.executable, "-c", '''
import os
import sys
from pathlib import Path
path = Path(sys.argv[1])
action = sys.argv[2]
data = sys.stdin.buffer.read()
if action == "atomic":
    temp = path.with_suffix(".tmp")
    temp.write_bytes(data)
    temp.replace(path)
elif action == "same-mtime":
    old = path.stat()
    path.write_bytes(data)
    os.utime(path, ns=(old.st_atime_ns, old.st_mtime_ns))
elif action == "delete":
    path.unlink()
elif action == "rename":
    path.rename(path.with_name("renamed.txt"))
elif action == "rename-parent":
    path.parent.rename(path.parent.with_name("moved-audit"))
elif action == "directory":
    path.unlink()
    path.mkdir()
elif action == "chmod":
    path.chmod(int(data, 8))
elif action == "hardlink":
    os.link(path, path.with_name("alias.txt"))
else:
    path.write_bytes(data)
''', str(self.path), action, input=text.encode())
        time.sleep(settle)

    def append_local(self):
        self.key("ctrl+Home", "End")
        self.type(" LOCAL")
        self.key("ctrl+Home")

    def observe(self, label, value, expected=audit.UNSPECIFIED):
        self.record(label, value, expected)

    def ui(self, label):
        rows = self.words(label)
        self.observe(label, " ".join(row["text"] for row in rows))
        return rows

    def press_button(self, label, word):
        self.shot(f"{label}-window")
        geometry = self.run("xdotool", "getwindowgeometry", "--shell", self.window,
                            capture_output=True, text=True).stdout
        bounds = dict(line.split("=", 1) for line in geometry.splitlines() if "=" in line)
        left = int(bounds["WIDTH"]) // 2 - 255
        top = int(bounds["HEIGHT"]) // 2 - 140
        rows = self.words(label, f"510x280+{left}+{top}")
        self.observe(label, " ".join(row["text"] for row in rows))
        matches = [row for row in rows if row["text"].strip("|:., ") == word]
        if not matches:
            return False
        row = max(matches, key=lambda row: int(row["top"]))
        x, y, width, height = (int(row[key]) for key in ("left", "top", "width", "height"))
        self.click(left + (x + width // 2) // audit.OCR_SCALE,
                   top + (y + height // 2) // audit.OCR_SCALE)
        time.sleep(0.35)
        return True


def clean(editor):
    editor.change("write", EXTERNAL)
    editor.observe("in-place reload", editor.text(), EXTERNAL)
    editor.change("atomic", NEWER)
    editor.observe("atomic reload", editor.text(), NEWER)
    equal_length = NEWER.replace("newer", "equal")
    editor.change("same-mtime", equal_length)
    editor.observe("same size and mtime reload", editor.text(), equal_length)


def caret(editor):
    editor.key("ctrl+Home", "Down", "End")
    editor.change("atomic", "inserted above\n" + BASE)
    editor.type(" MARK")
    editor.observe("caret after insertion above", editor.text(),
                   "inserted above\n" + BASE.replace("beta line", "beta line MARK"))


def history(editor):
    editor.append_local()
    editor.key("ctrl+s", settle=0.65)
    editor.observe("first save", editor.disk(), LOCAL)
    external = LOCAL.replace("beta line", "external beta line")
    editor.change("atomic", external)
    editor.observe("reload", editor.text(), external)
    editor.key("ctrl+z")
    editor.observe("first undo", editor.text(), LOCAL)
    editor.key("ctrl+z")
    editor.observe("second undo", editor.text(), BASE)
    editor.key("ctrl+shift+z")
    editor.observe("first redo", editor.text(), LOCAL)
    editor.key("ctrl+shift+z")
    editor.observe("second redo", editor.text(), external)


def selection(editor):
    editor.key("ctrl+Home", "Down", "Home", "shift+End")
    editor.change("atomic", "inserted above\n" + BASE)
    editor.observe("selection survives shift", editor.copy(), "beta line")
    editor.key("ctrl+Home", "shift+Down", "shift+Down", "shift+Down")
    editor.change("atomic", "inserted above\nalpha line\nbeta line extended\ngamma line\n")
    editor.observe("selection across an edited line", editor.copy(),
                   "inserted above\nalpha line\nbeta line extended\n")


def dirty_undo(editor):
    editor.append_local()
    editor.change("atomic", EXTERNAL)
    editor.observe("draft after external change", editor.text(), LOCAL)
    editor.key("End")
    editor.type(" NEXT")
    editor.observe("typing still works", editor.text(), LOCAL.replace(" LOCAL", " LOCAL NEXT"))
    editor.key("ctrl+z", settle=0.36)
    editor.observe("undo additional typing", editor.text(), LOCAL)
    editor.key("ctrl+z", settle=SETTLE)
    editor.observe("undo to initial baseline reloads disk", editor.text(), EXTERNAL)
    editor.observe("disk", editor.disk(), EXTERNAL)


def convergence(editor, match):
    editor.append_local()
    if match == "reverted":
        editor.change("write", EXTERNAL)
        editor.change("write", BASE)
        expected = LOCAL
    elif match == "external":
        editor.change("write", LOCAL)
        expected = LOCAL
    else:
        editor.change("write", EXTERNAL)
        editor.key("ctrl+a")
        editor.paste(EXTERNAL)
        time.sleep(SETTLE)
        expected = EXTERNAL
    editor.key("ctrl+s", settle=0.45)
    editor.observe("buffer accessible after ordinary save", editor.text(), expected)
    editor.observe("disk after ordinary save", editor.disk(), expected)
    editor.ui("convergence-save")


def conflict(editor, action):
    editor.append_local()
    editor.change("atomic", EXTERNAL)
    if action == "close":
        editor.key("ctrl+w", settle=0.6)
        editor.ui("close-confirmation")
        button = "Save" if editor.name == "stroll" else "Overwrite"
        editor.observe("save action visible on close", editor.press_button("close-save", button))
        editor.observe("disk after attempting save and close", editor.disk(), LOCAL)
        return
    editor.key("ctrl+s", settle=0.55)
    editor.observe("disk before decision", editor.disk(), EXTERNAL)
    if action == "cancel":
        editor.ui("cancel-prompt")
        editor.key("Escape", settle=0.5)
        editor.observe("draft after cancel", editor.text(), LOCAL)
        editor.observe("disk after cancel", editor.disk(), EXTERNAL)
    elif action == "reload":
        first = "Reload" if editor.name == "stroll" else "Discard"
        editor.observe("reload offered", editor.press_button("reload-prompt", first), True)
        editor.observe("draft after reload", editor.text(), EXTERNAL)
        editor.key("ctrl+z", settle=0.5)
        editor.observe("undo reload", editor.text(), LOCAL)
        editor.observe("disk after undoing reload", editor.disk(), EXTERNAL)
    elif action == "overwrite":
        editor.observe("overwrite offered", editor.press_button("overwrite-prompt", "Overwrite"), True)
        editor.observe("disk after overwrite", editor.disk(), LOCAL)
        editor.observe("buffer after overwrite", editor.text(), LOCAL)
    elif action == "newer":
        editor.change("write", NEWER)
        editor.observe("overwrite offered", editor.press_button("first-overwrite-prompt", "Overwrite"), True)
        editor.observe("disk after stale confirmation", editor.disk(), NEWER)
        editor.observe("fresh confirmation offered",
                       editor.press_button("second-overwrite-prompt", "Overwrite"), True)
        editor.observe("disk after fresh confirmation", editor.disk(), LOCAL)
    elif action == "disappears":
        editor.change("delete")
        editor.observe("overwrite offered before delete",
                       editor.press_button("stale-overwrite-prompt", "Overwrite"), True)
        editor.observe("disk after stale confirmation", editor.disk(), None)
        editor.ui("after-stale-overwrite")


def removed(editor, state):
    dirty = state in ("dirty", "reappear-dirty")
    if dirty:
        editor.append_local()
    editor.change("delete")
    editor.ui("after-delete")
    editor.key("ctrl+Home", "End")
    editor.type(" NEXT")
    before = LOCAL if dirty else BASE
    expected = before.replace("\n", " NEXT\n", 1)
    editor.observe("editing after delete", editor.text(), expected)
    if state.startswith("reappear"):
        editor.change("write", EXTERNAL)
        editor.observe("buffer after external recreation", editor.text(), expected)
        editor.key("ctrl+s", settle=0.45)
        editor.ui("recreated-file-save")
        editor.observe("disk remains intact", editor.disk(), EXTERNAL)
    else:
        editor.key("ctrl+s", settle=0.55)
        editor.ui("save-deleted-file")
        editor.observe("disk after save shortcut", editor.disk())
        if editor.name == "zed":
            editor.press_button("recreate-option", "Save")
            editor.observe("disk after choosing save if prompted", editor.disk(), expected)


def renamed(editor, dirty=False):
    if dirty:
        editor.append_local()
    editor.change("rename")
    editor.ui("after-rename")
    editor.key("ctrl+Home", "End")
    editor.type(" NEXT")
    expected = (LOCAL if dirty else BASE).replace("\n", " NEXT\n", 1)
    editor.observe("editing after rename", editor.text(), expected)
    editor.key("ctrl+s", settle=0.6)
    editor.observe("renamed file receives save", editor.disk(editor.root / "renamed.txt"), expected)
    editor.observe("old file after save", editor.disk(), None)
    editor.ui("after-rename-save")


def parent_move(editor):
    editor.append_local()
    editor.change("rename-parent")
    editor.ui("moved-parent")
    editor.key("ctrl+Home", "End")
    editor.type(" NEXT")
    editor.observe("editing after parent move", editor.text(), LOCAL.replace(" LOCAL", " LOCAL NEXT"))
    editor.key("ctrl+s", settle=0.6)
    editor.observe("save follows parent move", editor.disk(editor.root.with_name("moved-audit") / FILE),
                   LOCAL.replace(" LOCAL", " LOCAL NEXT"))
    editor.observe("old file after parent moved", editor.disk(), None)
    editor.ui("moved-parent-save")


def mixed(editor, dirty=False):
    if dirty:
        editor.append_local()
    external = EXTERNAL.replace("\n", "\r\n", 1)
    editor.change("atomic", external)
    editor.ui("mixed-line-endings")
    editor.key("ctrl+Home", "End")
    editor.type(" NEXT")
    expected = (LOCAL if dirty else EXTERNAL).replace("\n", " NEXT\n", 1)
    editor.observe("typing with mixed disk newlines", editor.text(), expected)
    editor.key("ctrl+s", settle=0.6)
    editor.observe("disk after save", editor.disk())
    if dirty:
        editor.observe("overwrite offered", editor.press_button("mixed-overwrite", "Overwrite"), True)
        editor.ui("after-mixed-overwrite")
        editor.observe("disk after confirming overwrite", editor.disk())


def metadata(editor, dirty=False):
    if dirty:
        editor.append_local()
    editor.change("atomic", "\ufeff" + BASE.replace("\n", "\r\n"))
    editor.observe("buffer after BOM and line-ending change", editor.text(), LOCAL if dirty else BASE)
    if not dirty:
        editor.append_local()
    editor.key("ctrl+s", settle=0.55)
    editor.observe("disk after save", editor.disk())
    editor.ui("metadata-save")


def unreadable(editor, kind):
    editor.append_local()
    if kind == "binary":
        editor.change("atomic", "binary\0data")
    elif kind == "oversized":
        editor.change("write", "123456789\n" * 850_000)
    else:
        editor.change("directory")
    editor.ui("unreadable-target")
    editor.observe("draft after target became unreadable", editor.text(), LOCAL)
    editor.key("ctrl+s", settle=0.55)
    editor.ui("unreadable-save")
    if kind == "oversized":
        editor.observe("disk size after save", editor.path.stat().st_size, 8_500_000)
    else:
        editor.observe("disk after save", editor.disk())


def rapid_save(editor):
    editor.append_local()
    editor.change("atomic", EXTERNAL, settle=0)
    editor.key("ctrl+s", settle=0.65)
    editor.observe("disk after immediate save", editor.disk(), EXTERNAL)
    editor.ui("immediate-save")


def rapid_type(editor):
    editor.key("ctrl+Home", "End")
    editor.change("atomic", EXTERNAL, settle=0)
    editor.type(" LOCAL")
    time.sleep(SETTLE)
    editor.observe("typing around reload", editor.text())
    editor.observe("disk after typing", editor.disk(), EXTERNAL)


def save_race(editor):
    editor.append_local()
    editor.key("ctrl+s", settle=0.65)
    editor.observe("disk after competing write and rename", editor.disk(), EXTERNAL)
    audit.wait_for(lambda: "Audit writer finished before rename" in
                   (editor.evidence / "app.log").read_text(), "competing write before rename")
    editor.ui("save-race")


def helper_save_error(editor):
    editor.append_local()
    editor.key("ctrl+s", settle=1.4)
    marker = Path(editor.env["STROLL_AUDIT_SAVE_REPLY_ERROR"])
    audit.wait_for(marker.is_file, "helper save acknowledgement injection")
    editor.observe("helper wrote despite returning an error", editor.disk(), LOCAL)
    editor.shot("helper-save-error")
    editor.change("write", NEWER)
    editor.observe("next external change reloads a clean buffer", editor.text(), NEWER)


def permissions(editor):
    editor.change("chmod", "444")
    editor.append_local()
    editor.key("ctrl+s", settle=0.55)
    editor.observe("disk with write permission removed", editor.disk(), BASE)
    editor.observe("mode after save", oct(editor.path.stat().st_mode & 0o777), "0o444")
    editor.ui("permission-save")


def hardlink(editor):
    editor.change("hardlink")
    alias = editor.path.with_name("alias.txt")
    alias.write_text(EXTERNAL)
    time.sleep(SETTLE)
    editor.observe("reload after writing through hard link", editor.text(), EXTERNAL)


def hardlink_save(editor):
    editor.change("hardlink")
    alias = editor.path.with_name("alias.txt")
    editor.append_local()
    editor.key("ctrl+s", settle=0.55)
    editor.observe("edited path was saved", editor.disk(), LOCAL)
    editor.observe("hard-link identity after a normal save", editor.path.samefile(alias), True)
    editor.observe("bytes seen through hard link", editor.disk(alias), LOCAL)


def background(editor):
    editor.open("other.txt")
    editor.observe("other tab opened", editor.text(), "other tab\n")
    editor.change("atomic", EXTERNAL)
    editor.open(FILE)
    editor.observe("background tab caught up", editor.text(), EXTERNAL)


def checkout(editor):
    editor.run("git", "-C", str(editor.root), "switch", "-qc", "external")
    editor.change("write", EXTERNAL)
    editor.run("git", "-C", str(editor.root), "add", FILE)
    editor.run("git", "-C", str(editor.root), "-c", "user.name=Editor Audit",
               "-c", "user.email=audit@example.invalid", "-c", "commit.gpgSign=false",
               "-c", "core.hooksPath=/dev/null", "commit", "-qm", "External edit")
    editor.run("git", "-C", str(editor.root), "switch", "-q", "-")
    time.sleep(SETTLE)
    editor.observe("checkout reloads clean buffer", editor.text(), BASE)
    editor.append_local()
    editor.run("git", "-C", str(editor.root), "switch", "-q", "external")
    time.sleep(SETTLE)
    editor.observe("checkout preserves draft", editor.text(), LOCAL)
    editor.key("ctrl+s", settle=0.55)
    editor.observe("save preserves checked-out bytes until confirmed", editor.disk(), EXTERNAL)
    editor.ui("checkout-conflict")


def prepare_recovery(editor):
    editor.append_local()
    editor.change("atomic", EXTERNAL)
    editor.observe("draft before restart", editor.text(), LOCAL)


def recover(editor):
    editor.observe("draft recovered with external changes", editor.text(), LOCAL)
    editor.key("ctrl+z", settle=SETTLE)
    editor.observe("undo recovered draft reloads the disk", editor.text(), EXTERNAL)
    editor.observe("disk after recovery", editor.disk(), EXTERNAL)


def shared_draft(editor, args, env):
    other = Editor("stroll", editor.root, editor.evidence, env, standalone=True)
    with (editor.evidence / "second-window.log").open("w") as log, audit.process(
        [args.stroll, str(editor.path)], env, stdout=log, stderr=log,
    ):
        other.window = audit.wait_for(other.prepare_window, "parallel standalone window")
        audit.wait_for(lambda: other.text() == BASE, "the same initial file in the second window")
        editor.run("xdotool", "windowfocus", editor.window)
        editor.append_local()
        time.sleep(SETTLE)
        editor.observe("workspace draft", editor.text(), LOCAL)
        other.run("xdotool", "windowfocus", other.window)
        other.key("ctrl+Home", "Down", "End")
        other.type(" SECOND")
        time.sleep(SETTLE)
        editor.observe("standalone draft", other.text(), BASE.replace("beta line", "beta line SECOND"))
        editor.observe("neither draft was saved to disk", editor.disk(), BASE)


CASES = {
    "clean": clean, "caret": caret, "history": history, "selection": selection, "dirty-undo": dirty_undo,
    "disk-reverts": lambda editor: convergence(editor, "reverted"),
    "disk-matches-draft": lambda editor: convergence(editor, "external"),
    "draft-matches-disk": lambda editor: convergence(editor, "local"),
    "rapid-save": rapid_save, "rapid-type": rapid_type, "save-race": save_race,
    "helper-save-error": helper_save_error,
    "cancel": lambda editor: conflict(editor, "cancel"),
    "reload": lambda editor: conflict(editor, "reload"),
    "overwrite": lambda editor: conflict(editor, "overwrite"),
    "newer": lambda editor: conflict(editor, "newer"),
    "disappears": lambda editor: conflict(editor, "disappears"),
    "close": lambda editor: conflict(editor, "close"),
    "delete-clean": lambda editor: removed(editor, "clean"),
    "delete-dirty": lambda editor: removed(editor, "dirty"),
    "reappear": lambda editor: removed(editor, "reappear"),
    "reappear-dirty": lambda editor: removed(editor, "reappear-dirty"),
    "rename-clean": renamed, "rename-dirty": lambda editor: renamed(editor, True),
    "parent-move": parent_move, "mixed-clean": mixed, "mixed-dirty": lambda editor: mixed(editor, True),
    "metadata-clean": metadata, "metadata-dirty": lambda editor: metadata(editor, True),
    "binary": lambda editor: unreadable(editor, "binary"),
    "oversized": lambda editor: unreadable(editor, "oversized"),
    "directory": lambda editor: unreadable(editor, "directory"),
    "permissions": permissions, "hardlink": hardlink, "hardlink-save": hardlink_save,
    "background": background, "checkout": checkout, "recover": prepare_recovery, "shared-draft": shared_draft,
}


def setup(state, env, name, entry, args, case):
    audit.FILE, audit.SOURCE = FILE, BASE
    root, env, launch = audit.workspace(state, env, name, args)
    if entry == "standalone":
        isolated = root.parent / "standalone-audit"
        isolated.mkdir()
        (root / FILE).rename(isolated / FILE)
        root = isolated
        launch = [args.stroll, str(root / FILE)] if name == "stroll" else [*launch[:-2], str(root / FILE)]
    else:
        (root / "other.txt").write_text("other tab\n")
    if entry == "helper":
        # Run the real remote helper over local pipes with an isolated ssh
        # shim. No SSH server, keys, or user configuration are involved.
        shim = state / "bin"
        shim.mkdir()
        ssh = shim / "ssh"
        ssh.write_text("""#!/usr/bin/env python3
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import threading

assert sys.argv[-2] == "editor-audit.invalid"
helper = os.environ["STROLL_AUDIT_HELPER"]
arguments = sys.argv[-1].split("exec stroll-remote ", 1)[1].split("; else", 1)[0]
command = [helper, *shlex.split(arguments)]
if "STROLL_AUDIT_SAVE_REPLY_ERROR" not in os.environ:
    os.execv(helper, command)

def read(source):
    header = source.read(4)
    return json.loads(source.read(int.from_bytes(header, "big"))) if header else None

def send(destination, frame):
    data = json.dumps(frame).encode()
    destination.write(len(data).to_bytes(4, "big") + data)
    destination.flush()

with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE) as process:
    saves = set()

    def forward_requests():
        while (call := read(sys.stdin.buffer)) is not None:
            request = call["request"]
            if isinstance(request, dict) and "Save" in request:
                saves.add(call["id"])
            send(process.stdin, call)
        process.stdin.close()

    threading.Thread(target=forward_requests, daemon=True).start()
    while (reply := read(process.stdout)) is not None:
        if reply["id"] in saves and reply["result"] == {"Ok": True}:
            reply["result"] = {"Err": "Audit: save completed, acknowledgement failed."}
            Path(os.environ["STROLL_AUDIT_SAVE_REPLY_ERROR"]).touch()
        send(sys.stdout.buffer, reply)
""")
        ssh.chmod(0o700)
        env = {**env, "PATH": str(shim) + os.pathsep + env["PATH"],
               "STROLL_AUDIT_HELPER": str(args.helper.resolve(strict=True))}
        if case == "helper-save-error":
            env["STROLL_AUDIT_SAVE_REPLY_ERROR"] = str(state / "save-reply-replaced")
        launch = [args.stroll, "ssh://editor-audit.invalid" + str(root), FILE]
    return root, env, launch


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stroll", default=str(audit.ROOT / "target/release/stroll"))
    parser.add_argument("--zed", default=shutil.which("zeditor") or "zed")
    parser.add_argument("--helper", type=Path, default=audit.ROOT / "target/release/stroll-remote")
    parser.add_argument("--app", choices=("stroll-workspace", "stroll-standalone", "stroll-helper",
                                         "zed-workspace", "zed-standalone"), action="append")
    parser.add_argument("--case", choices=CASES, action="append")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    failures = 0
    for app in args.app or ("stroll-workspace", "stroll-standalone", "zed-workspace", "zed-standalone"):
        name, entry = app.split("-")
        standalone = entry == "standalone"
        output = args.output / app
        output.mkdir()
        with tempfile.TemporaryDirectory(prefix="stroll-external-audit-") as temporary:
            state = Path(temporary)
            with audit.virtual_display(state, output) as env:
                env.setdefault("OMP_THREAD_LIMIT", "1")
                env.setdefault("MAGICK_THREAD_LIMIT", "2")
                for case in args.case or [key for key in CASES if key != "save-race"]:
                    if case == "save-race" and app not in ("stroll-workspace", "stroll-standalone"):
                        continue
                    if case == "helper-save-error" and entry != "helper":
                        continue
                    if case == "shared-draft" and app != "stroll-workspace":
                        continue
                    if (standalone or entry == "helper") and case == "checkout":
                        continue
                    if standalone and case == "background":
                        continue
                    print(f"{app}: {case}", flush=True)
                    evidence = output / case
                    evidence.mkdir()
                    root, app_env, launch = setup(state / case, env, name, entry, args, case)
                    editor = Editor(name, root, evidence, app_env, standalone)
                    if case == "save-race":
                        library = state / "save-race.so"
                        fixture = audit.ROOT / "tests/fixtures/editor-save-race.c"
                        audit.command(["cc", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC",
                                       "-o", str(library), str(fixture), "-ldl"], env)
                        app_env = {**app_env, "LD_PRELOAD": str(library),
                                   "STROLL_AUDIT_SAVE_TARGET": str(editor.path)}
                    audit.wait_for(lambda: editor.find_window() is None, f"previous {app} window to close")
                    with (evidence / "app.log").open("w") as log, audit.process(
                        launch, app_env, stdout=log, stderr=log,
                    ):
                        try:
                            editor.window = audit.wait_for(editor.prepare_window, f"{app} window", seconds=15)
                            audit.wait_for(lambda: editor.text() == BASE, "initial file contents", seconds=8)
                            time.sleep(0.35)
                            if case == "shared-draft":
                                shared_draft(editor, args, app_env)
                            else:
                                CASES[case](editor)
                        except (audit.ProbeError, subprocess.SubprocessError, OSError) as error:
                            editor.observe("probe error", str(error))
                            failures += 1
                    if case in ("recover", "shared-draft") and "probe error" not in editor.results:
                        audit.wait_for(lambda: editor.find_window() is None, f"{app} window to close")
                        with (evidence / "restart.log").open("w") as log, audit.process(
                            launch, app_env, stdout=log, stderr=log,
                        ):
                            try:
                                editor.window = audit.wait_for(editor.prepare_window, f"{app} restart", seconds=15)
                                time.sleep(SETTLE)
                                if case == "shared-draft":
                                    editor.observe("workspace draft after restart", editor.text(), LOCAL)
                                else:
                                    recover(editor)
                            except (audit.ProbeError, subprocess.SubprocessError, OSError) as error:
                                editor.observe("probe error", str(error))
                                failures += 1
                    (evidence / "results.json").write_text(json.dumps(editor.results, indent=2) + "\n")
                    for label, row in editor.results.items():
                        if label == "probe error" or ("expected" in row and row["actual"] != row["expected"]):
                            print(f"  {label}: {row['actual']!r}", flush=True)
    return int(bool(failures))


if __name__ == "__main__":
    sys.exit(main())
