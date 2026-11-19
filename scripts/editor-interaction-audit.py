#!/usr/bin/env python3
"""Replay editor interactions in a private X11 display and compare observations."""

import argparse
import csv
import io
import json
import os
import random
import secrets
import select
import shutil
import signal
import subprocess
import tempfile
import time
from contextlib import contextmanager
from difflib import SequenceMatcher
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "tests/fixtures/editor-interactions.ts").read_text(encoding="utf-8")
FILE = "editor-interactions.ts"
LINES = SOURCE.splitlines()
SIZE = (1120, 760)
SEARCH = "auditNeedle"
CLIPBOARD_SENTINEL = "stroll-audit-empty-clipboard"
UNSPECIFIED = object()
IMAGE_CONVERTER = shutil.which("magick") or "convert"
OCR_SCALE = 2


class ProbeError(Exception):
    pass


def command(args, env=None, **kwargs):
    return subprocess.run(args, env=env, check=True, timeout=15, **kwargs)


def wait_for(probe, description, seconds=8):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = probe()
        if result:
            return result
        time.sleep(0.12)
    raise ProbeError(f"Timed out waiting for {description}")


@contextmanager
def process(args, env, **kwargs):
    with subprocess.Popen(args, env=env, start_new_session=True, **kwargs) as child:
        try:
            yield child
        finally:
            # The Zed CLI also starts a GUI child in this group.
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)


@contextmanager
def virtual_display(state, evidence):
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    for key in ("WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS"):
        env.pop(key, None)
    runtime = state / "runtime"
    runtime.mkdir(mode=0o700)
    auth = state / "Xauthority"
    auth.touch(mode=0o600)
    env.update(
        XAUTHORITY=str(auth), XDG_RUNTIME_DIR=str(runtime), GPUI_X11_SCALE_FACTOR="1",
        ZED_ALLOW_EMULATED_GPU="1", LANG="C.UTF-8", LC_ALL="C.UTF-8",
        GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
    )
    cookie = secrets.token_hex(16)

    def authorize(display):
        # Read the cookie from stdin so it doesn't appear in the command line.
        command(["xauth", "-q"], env, input=f"add {display} . {cookie}\n", text=True)

    authorize(":0")
    with (evidence / "xvfb.log").open("w") as log, process(
        ["Xvfb", "-displayfd", "1", "-screen", "0", "1280x900x24", "-dpi", "96",
         "-auth", str(auth), "-nolisten", "tcp", "-noreset"],
        env, stdout=subprocess.PIPE, stderr=log, text=True,
    ) as xvfb:
        if not select.select([xvfb.stdout], [], [], 10)[0]:
            raise ProbeError("Xvfb did not announce a display")
        display = xvfb.stdout.readline().strip()
        if not display.isdecimal():
            raise ProbeError(f"Xvfb failed to start. See {evidence / 'xvfb.log'}")
        env["DISPLAY"] = f":{display}"
        authorize(env["DISPLAY"])
        command(["xrdb", "-merge"], env, input="Xft.dpi: 96\n", text=True)
        yield env


class Editor:
    def __init__(self, name, root, evidence, env):
        self.name = name
        self.root = root
        self.evidence = evidence
        self.env = env
        self.window = None
        self.results = {}

    def run(self, *args, **kwargs):
        return command(args, self.env, **kwargs)

    def find_window(self):
        title = "Stroll$" if self.name == "stroll" else "editor-audit"
        result = subprocess.run(
            ["xdotool", "search", "--onlyvisible", "--name", title],
            env=self.env, capture_output=True, text=True, timeout=3,
        )
        return next(iter(result.stdout.splitlines()), None)

    def prepare_window(self):
        window = self.find_window()
        if window is None:
            return None
        result = subprocess.run(
            ["xdotool", "windowmove", window, "0", "0", "windowsize", window,
             str(SIZE[0]), str(SIZE[1]), "windowfocus", window],
            env=self.env, capture_output=True, timeout=3,
        )
        return window if result.returncode == 0 else None

    def key(self, *keys, settle=0.11):
        self.run("xdotool", "key", "--clearmodifiers", "--delay", "28", *keys)
        time.sleep(settle)

    def type(self, text):
        self.run("xdotool", "type", "--clearmodifiers", "--delay", "28", "--", text)

    def click(self, x, y, count=1):
        self.run(
            "xdotool", "mousemove", "--window", self.window, str(x), str(y),
            "click", "--repeat", str(count), "--delay", "80", "1",
            "mousemove", "--window", self.window, "4", "4",
        )
        time.sleep(0.21)

    def clipboard(self):
        return self.run("xclip", "-selection", "clipboard", "-out", capture_output=True).stdout.decode()

    def set_clipboard(self, value):
        self.run(
            "xclip", "-selection", "clipboard", "-in", input=value.encode(),
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        wait_for(lambda: self.clipboard() == value, "X11 clipboard owner", seconds=2)

    def copy(self):
        self.set_clipboard(CLIPBOARD_SENTINEL)
        self.key("ctrl+c")
        result = self.clipboard()
        return None if result == CLIPBOARD_SENTINEL else result

    def paste(self, value):
        self.set_clipboard(value)
        self.key("ctrl+v")

    def shot(self, label, crop=None):
        path = self.evidence / f"{label}.png"
        args = ["import", "-window", self.window]
        if crop:
            args.extend(["-crop", crop])
        self.run(*args, str(path))
        return path

    def words(self, label, crop=None):
        path = self.shot(label, crop)
        pixels = self.run(IMAGE_CONVERTER, str(path), "-resize", f"{100 * OCR_SCALE}%",
                          "png:-", capture_output=True).stdout
        tsv = self.run(
            "tesseract", "stdin", "stdout", "-l", "eng", "--psm", "11",
            "-c", "tessedit_create_tsv=1", input=pixels, capture_output=True,
        ).stdout.decode()
        (self.evidence / f"{label}.tsv").write_text(tsv, encoding="utf-8")
        return [row for row in csv.DictReader(io.StringIO(tsv), delimiter="\t") if row.get("text", "").strip()]

    def at(self, token, label, min_y=0, max_y=SIZE[1]):
        matches = []
        for row in self.words(label):
            bounds = tuple(int(row[key]) // OCR_SCALE for key in ("left", "top", "width", "height"))
            if not min_y <= bounds[1] < max_y:
                continue
            word = row["text"].casefold()
            score = 1.0 if token.casefold() in word else SequenceMatcher(None, token.casefold(), word).ratio()
            matches.append((score, bounds))
        best = max(matches, default=(0, None), key=lambda match: match[0])
        if best[0] >= 0.85:
            return best[1]
        raise ProbeError(f"Could not locate {token!r} in {label}.png")

    def line(self, row):
        self.key("ctrl+Home", *(["Down"] * row), "Home")

    def suffix(self):
        self.key("shift+End")
        value = self.copy()
        if value:
            self.key("Left")
        return value

    def open(self, filename):
        self.key("ctrl+p", settle=0.18)
        self.type(filename)
        # The file list is matched asynchronously.
        time.sleep(0.34)
        self.key("Return", settle=0.32)

    def buffer(self):
        self.key("ctrl+a")
        return self.copy()

    def reset(self):
        if self.buffer() != SOURCE:
            self.paste(SOURCE)
            if self.buffer() != SOURCE:
                raise ProbeError("Could not restore the TypeScript fixture in the editor")
        self.key("ctrl+Home")

    def record(self, check, actual, expected=UNSPECIFIED):
        entry = {"actual": actual}
        if expected is not UNSPECIFIED:
            entry["expected"] = expected
        self.results[check] = entry


def editing(editor, seeds):
    for seed in seeds:
        editor.reset()
        row = len(LINES) - 1
        prefix = "// randomEdit "
        text = LINES[row][len(prefix):]
        caret = len(text)
        rng = random.Random(seed)
        editor.line(row)
        editor.key("End")
        trace = []

        for step in range(24):
            choices = ["type", "move"]
            if caret:
                choices.append("backspace")
            if caret < len(text):
                choices.extend(["delete", "replace selection"])
            action = rng.choice(choices)
            entry = {"step": step + 1, "action": action, "before": text, "caret": caret}
            if action == "move":
                destination = rng.randrange(len(text) + 1)
                key = "Left" if destination < caret else "Right"
                if destination != caret:
                    editor.key(*([key] * abs(destination - caret)))
                caret = destination
            elif action == "backspace":
                editor.key("BackSpace")
                text = text[:caret - 1] + text[caret:]
                caret -= 1
            elif action == "delete":
                editor.key("Delete")
                text = text[:caret] + text[caret + 1:]
            else:
                length = rng.randint(1, min(3, len(text) - caret)) if action == "replace selection" else 0
                if length:
                    editor.key(*(["shift+Right"] * length))
                value = rng.choice(["let", "_id", "42", "x", " "])
                entry.update(text=value, selection_length=length)
                editor.type(value)
                text = text[:caret] + value + text[caret + length:]
                caret += len(value)
            entry.update(after=text, caret_after=caret)
            trace.append(entry)

            if (step + 1) % 6 == 0:
                editor.key("ctrl+s", settle=0.3)
                actual = (editor.root / FILE).read_text(encoding="utf-8")
                expected = "\n".join([*LINES[:-1], prefix + text]) + "\n"
                editor.record(f"seed {seed}, edit {step + 1}", actual, expected)
        (editor.evidence / f"seed-{seed}.json").write_text(json.dumps(trace, indent=2) + "\n")


def changed_line(editor, row):
    contents = editor.buffer()
    if contents is None or len(contents.splitlines()) <= row:
        raise ProbeError(f"Could not read edited fixture line {row + 1}")
    return contents.splitlines()[row]


def deletion(editor, _):
    row = 3
    for label, keys, expected in [
        ("Backspace", ("End", "BackSpace"), "// wordProbe alpha beta_gamma.delt"),
        ("Delete", ("Delete",), "/ wordProbe alpha beta_gamma.delta"),
        ("Ctrl+Backspace after a word", (*(["Right"] * 18), "ctrl+BackSpace"), "// wordProbe  beta_gamma.delta"),
        ("Ctrl+Delete before a word", (*(["Right"] * 13), "ctrl+Delete"), "// wordProbe  beta_gamma.delta"),
        ("Ctrl+Backspace after a dotted expression", ("End", "ctrl+BackSpace"), "// wordProbe alpha beta_gamma."),
        ("Ctrl+Delete before adjacent punctuation", ("ctrl+Delete",), UNSPECIFIED),
    ]:
        editor.reset()
        editor.line(row)
        editor.key(*keys)
        editor.record(label, changed_line(editor, row), expected)

    editor.reset()
    editor.line(2)
    editor.key("End", "ctrl+BackSpace")
    editor.record("Ctrl+Backspace on CJK", changed_line(editor, 2))

    for label, keys in [
        ("Backspace across ZWJ emoji", ("End", "Left", "BackSpace")),
        ("Delete across ZWJ emoji", (*(["Right"] * len("// graphemeProbe A")), "Delete")),
    ]:
        editor.reset()
        editor.line(4)
        editor.key(*keys)
        editor.record(label, changed_line(editor, 4), "// graphemeProbe AB")

    editor.reset()
    editor.line(4)
    editor.key("End", "Left", "Left")
    editor.record("Left arrow crosses a complete ZWJ emoji", editor.suffix(), "👩‍💻B")

    editor.reset()
    code_row = len(LINES) - 2
    editor.line(code_row)
    editor.key("End")
    editor.type(".delta")
    editor.key("ctrl+BackSpace")
    editor.record("Ctrl+Backspace in TypeScript member access", changed_line(editor, code_row),
                  LINES[code_row] + ".")


def blank_line_deletion(editor, _):
    editor.reset()
    editor.line(7)
    editor.type("   ")
    editor.key("ctrl+BackSpace")
    # Insert a marker to identify the caret without changing its position first.
    editor.type("CARET")
    contents = editor.buffer()
    if contents is None:
        raise ProbeError("Could not read the buffer after deleting a whitespace-only line")
    editor.record("Ctrl+Backspace on a whitespace-only line", contents.splitlines()[6:9])


def editing_commands(editor, _):
    editor.reset()
    editor.line(len(LINES) - 2)
    editor.key("End")
    editor.type("(")
    editor.record("TypeScript auto-pair in code", changed_line(editor, len(LINES) - 2), LINES[-2] + "()")

    editor.reset()
    editor.line(len(LINES) - 1)
    editor.key("End")
    editor.type("(")
    editor.record("TypeScript auto-pair in a comment", changed_line(editor, len(LINES) - 1))

    editor.reset()
    editor.line(9)
    editor.key("End", "Return")
    editor.type("return 2;")
    editor.record("TypeScript newline indentation", changed_line(editor, 10), "    return 2;")

    editor.reset()
    editor.line(0)
    editor.key("End")
    editor.paste(" 中文 café 👩‍💻")
    expected = LINES[0] + " 中文 café 👩‍💻"
    editor.record("Unicode clipboard paste", changed_line(editor, 0), expected)
    editor.key("ctrl+z")
    editor.record("Undo paste", changed_line(editor, 0), LINES[0])
    redo = "ctrl+y" if editor.name == "stroll" else "ctrl+shift+z"
    editor.key(redo)
    editor.record("Redo paste", changed_line(editor, 0), expected)


def selection_clipboard(editor, _):
    editor.reset()
    editor.line(1)
    editor.key(*(["Right"] * len("const ")), *(["shift+Right"] * len("LatinWord")))
    editor.record("Keyboard selection copies the identifier", editor.copy(), "LatinWord")
    editor.key("Left")
    editor.type("probe_")
    editor.record("Left collapses a forward selection to its start", changed_line(editor, 1),
                  "const probe_LatinWord = 123;")

    editor.reset()
    editor.line(1)
    editor.key(*(["Right"] * len("const LatinWord")), *(["shift+Left"] * len("LatinWord")), "Right")
    editor.type("_probe")
    editor.record("Right collapses a reversed selection to its end", changed_line(editor, 1),
                  "const LatinWord_probe = 123;")

    editor.reset()
    editor.line(5)
    editor.key("alt+shift+Down")
    editor.type("// ")
    editor.record("Typing inserts at both vertical cursors", editor.buffer(),
                  SOURCE.replace(LINES[5], "// " + LINES[5]).replace(LINES[6], "// " + LINES[6]))

    editor.reset()
    editor.line(5)
    editor.key("alt+shift+Down", "Escape")
    editor.type("probe_")
    contents = editor.buffer()
    if contents is None:
        raise ProbeError("Could not read the buffer after dismissing multiple cursors")
    editor.record("Escape retains one cursor", contents.count("probe_"), 1)
    editor.record("Cursor retained by Escape", [index + 1 for index, line in enumerate(contents.splitlines())
                                               if "probe_" in line])

    editor.reset()
    editor.line(9)
    editor.record("Copy with no selection copies the current line", editor.copy(), LINES[9] + "\n")
    editor.line(10)
    editor.key("ctrl+v")
    editor.record("Whole-line paste at an empty selection", editor.buffer())

    editor.reset()
    editor.line(9)
    editor.set_clipboard(CLIPBOARD_SENTINEL)
    editor.key("ctrl+x")
    editor.record("Cut with no selection copies the current line", editor.clipboard(), LINES[9] + "\n")
    editor.record("Cut with no selection removes the current line", editor.buffer(),
                  SOURCE.replace(LINES[9] + "\n", ""))


def line_commands(editor, _):
    editor.reset()
    editor.line(6)
    editor.key("alt+Up")
    swapped = "\n".join([*LINES[:5], LINES[6], LINES[5], *LINES[7:]]) + "\n"
    editor.record("Alt+Up moves a source line up", editor.buffer(), swapped)
    editor.line(5)
    editor.key("alt+Down")
    editor.record("Alt+Down moves a source line down", editor.buffer(), SOURCE)

    editor.reset()
    editor.line(6)
    editor.key("ctrl+shift+k")
    editor.record("Delete line removes the complete source line", editor.buffer(),
                  SOURCE.replace(LINES[6] + "\n", ""))

    editor.reset()
    editor.line(6)
    editor.key("ctrl+slash")
    editor.record("Toggle line comment uses the TypeScript marker", changed_line(editor, 6), "// " + LINES[6])
    editor.line(6)
    editor.key("ctrl+slash")
    editor.record("Toggle line comment restores the source", editor.buffer(), SOURCE)

    editor.reset()
    editor.line(6)
    editor.key("shift+Down", "shift+Down", "shift+Right", "ctrl+slash")
    contents = editor.buffer()
    if contents is None:
        raise ProbeError("Could not read the commented line range")
    editor.record("Toggle comments on a selection spanning a blank line", contents.splitlines()[6:9])

    editor.reset()
    editor.line(9)
    editor.key("End", "Home")
    editor.record("Home from the end of an indented line", editor.suffix())


def double_click(editor, _):
    editor.reset()
    x, y, width, height = editor.at("LatinWord", "point-LatinWord")
    editor.click(x + width // 2, y + height // 2, count=2)
    editor.shot("double-click-LatinWord")
    editor.record("Double-click a Latin identifier", editor.copy(), "LatinWord")

    anchor = "CjkAnchor"
    x, y, width, height = editor.at(anchor, "point-CjkAnchor")
    # Land inside the first ideograph after the ASCII anchor and one space.
    x += width + round(1.8 * width / len(anchor))
    y += height // 2
    editor.click(x, y)
    suffix = editor.suffix()
    if not suffix or not suffix.startswith(("中", "文")):
        raise ProbeError(f"Click missed the intended CJK text. Copied {suffix!r}")
    time.sleep(0.55)
    editor.click(x, y, count=2)
    editor.shot("double-click-Cjk")
    editor.record("Double-click on CJK text", editor.copy())


def open_search(editor):
    editor.key("ctrl+f")
    editor.key("ctrl+a")
    editor.type(SEARCH)
    time.sleep(0.5)


def search_counter(editor, label):
    words = editor.words(label)
    query = next((word for word in words if word["text"].lower().startswith(SEARCH.lower())
                  and int(word["top"]) < 160 * OCR_SCALE), None)
    if query is None:
        raise ProbeError(f"Could not locate the search input in {label}.png")
    center = int(query["top"]) + int(query["height"]) / 2
    for word in words:
        word_center = int(word["top"]) + int(word["height"]) / 2
        # OCR sometimes drops the slash at this font size.
        counter = word["text"].replace("/", "")
        if (abs(word_center - center) < 6 * OCR_SCALE
                and int(word["left"]) > int(query["left"]) + int(query["width"])
                and len(counter) == 2 and counter.isascii() and counter.isdecimal()):
            return f"{counter[0]}/{counter[1]}"
    raise ProbeError(f"Could not read the search counter in {label}.png")


def search(editor, _):
    editor.reset()
    _, initial_y, _, _ = editor.at("LatinWord", "before-search")
    open_search(editor)
    editor.record("Search finds the first of two matches", search_counter(editor, "search-first-match"), "1/2")
    editor.key("ctrl+a")
    editor.record("Search query focused", editor.copy(), SEARCH)
    editor.key("Return")
    editor.record("Enter advances to the next search match", search_counter(editor, "search-next-match"), "2/2")
    x, open_y, width, height = editor.at("LatinWord", "search-open")
    editor.record("Search UI displaced the editor", open_y > initial_y + 8, True)
    editor.click(x + width + 85, open_y + height // 2)
    editor.key("Escape")
    _, closed_y, _, _ = editor.at("LatinWord", "escape-from-editor", min_y=initial_y - 6)
    editor.record("Escape from editor closes Search", abs(closed_y - initial_y) <= 4, True)
    editor.key("ctrl+Home")
    editor.record("Editor owns focus after Escape", editor.suffix(), LINES[0])
    editor.key("ctrl+f")
    editor.key("ctrl+a")
    editor.record("Reinvoking Find focuses and retains the query", editor.copy(), SEARCH)
    editor.key("Escape")
    _, field_closed_y, _, _ = editor.at("LatinWord", "escape-from-search-input", min_y=initial_y - 6)
    editor.record("Escape from the search input closes Find", abs(field_closed_y - initial_y) <= 4, True)


def history(editor, _):
    editor.reset()
    back, forward = (
        ("alt+Left", "alt+Right") if editor.name == "stroll"
        else ("ctrl+alt+minus", "ctrl+alt+shift+minus")
    )
    # Leave Search visible on the source file with the caret in the editor.
    open_search(editor)
    x, y, width, height = editor.at("LatinWord", "history-with-search")
    editor.click(x + width + 85, y + height // 2)
    editor.key("Home", "Right", "Right")
    source_suffix = LINES[1][2:]
    if editor.suffix() != source_suffix:
        raise ProbeError("Could not place the history source caret with Search open")

    editor.open("nav-alpha.ts")
    alpha_suffix = "// Alpha destination"
    editor.record("History opens first destination", editor.suffix(), alpha_suffix)
    editor.open("nav-beta.ts")
    beta_suffix = "// Beta destination"
    editor.record("History opens second destination", editor.suffix(), beta_suffix)
    for step, key, expected in [
        ("back to alpha", back, alpha_suffix),
        ("back to source with Search open", back, source_suffix),
        ("forward to alpha", forward, alpha_suffix),
        ("forward to beta", forward, beta_suffix),
    ]:
        editor.key(key, settle=0.37)
        editor.shot(f"history-{step.replace(' ', '-')}")
        editor.record(f"History {step} with editor focus", editor.suffix(), expected)

    editor.key(back, settle=0.37)
    if editor.suffix() != alpha_suffix:
        raise ProbeError("Could not rewind to alpha before branching the history")
    editor.open("nav-gamma.ts")
    gamma_suffix = "// Gamma destination"
    editor.record("History opens a new branch after rewinding", editor.suffix(), gamma_suffix)
    branch_words = editor.words("history-new-branch")
    editor.record("Find remains visible on a different file",
                  any(word["text"].casefold().startswith(SEARCH.casefold()) for word in branch_words))
    editor.key(forward, settle=0.37)
    x, y, width, height = editor.at("destination", "history-forward-after-branch", min_y=70)
    editor.key("ctrl+a")
    editor.record("Keyboard target after Forward at the end of a new branch", editor.copy())
    # Check which buffer is active independently of keyboard focus.
    editor.click(x + width + 35, y + height // 2)
    editor.key("ctrl+Home")
    editor.record("A new history branch discards the old forward stop", editor.suffix(), gamma_suffix)


def tabs(editor, _):
    editor.open("RevealProbe.ts")
    editor.key("ctrl+Home")
    editor.record("Overflow target initially active", editor.suffix(), "// Reveal target")
    _, y, _, height = editor.at("RevealProbe", "overflow-start", max_y=67)
    # The initial label calibrates the tab shelf. Exclude the tree and breadcrumbs.
    top = max(y - 5, 0)
    shelf = f"{SIZE[0]}x{height + 10}+0+{top}"
    for index in range(1, 21):
        editor.open(f"buffer-{index:02d}.ts")
        if editor.suffix() != f"// Buffer number {index:02d}":
            raise ProbeError(f"Could not verify that buffer-{index:02d}.ts opened")
    editor.record("Opened twenty additional tabs", 20, 20)
    before = editor.words("overflow-before", shelf)
    editor.record("Overflow target starts offscreen", not any("revealprobe" in w["text"].lower() for w in before), True)

    editor.open("RevealProbe.ts")
    editor.key("ctrl+Home")
    editor.record("Quick open reactivates existing file", editor.suffix(), "// Reveal target")
    after = editor.words("overflow-after", shelf)
    editor.shot("overflow-reactivated")
    editor.record("Reactivated tab label is visible", any("revealprobe" in w["text"].lower() for w in after), True)


CASES = {
    "editing": editing, "deletion": deletion, "editing-commands": editing_commands,
    "double-click": double_click, "search": search, "history": history, "tabs": tabs,
    "selection-clipboard": selection_clipboard, "line-commands": line_commands,
    "blank-line-deletion": blank_line_deletion,
}


def workspace(state, env, name, args):
    profile = state / name
    repo = profile / "editor-audit"
    repo.mkdir(parents=True)
    (repo / FILE).write_text(SOURCE, encoding="utf-8")
    (repo / "nav-alpha.ts").write_text("// Alpha destination\n")
    (repo / "nav-beta.ts").write_text("// Beta destination\n")
    (repo / "nav-gamma.ts").write_text("// Gamma destination\n")
    (repo / "RevealProbe.ts").write_text("// Reveal target\n")
    for index in range(1, 21):
        (repo / f"buffer-{index:02d}.ts").write_text(f"// Buffer number {index:02d}\n")

    env = {**env, **{f"XDG_{label}_HOME": str(profile / label.lower())
                     for label in ("CONFIG", "DATA", "CACHE", "STATE")}}
    command(["git", "init", "-q", str(repo)], env)
    command(["git", "-C", str(repo), "add", "--all"], env)
    command(["git", "-C", str(repo), "-c", "user.name=Editor Audit",
             "-c", "user.email=audit@example.invalid", "-c", "commit.gpgSign=false",
             "-c", "core.hooksPath=/dev/null", "commit", "-qm", "Add interaction fixtures"], env)

    if name == "stroll":
        config = profile / "stroll.toml"
        config.write_text('version = 1\n[appearance]\ntheme = "light"\nlanguage = "en"\n[lsp]\nenabled = false\n')
        env.update(STROLL_GPUI_DATA=str(profile / "stroll-data"), STROLL_CONFIG=str(config))
        launch = [args.stroll, str(repo), FILE]
    else:
        data = profile / "zed-data"
        config = data / "config" / "settings.json"
        config.parent.mkdir(parents=True)
        config.write_text(json.dumps({
            "theme": "One Light", "auto_update": False, "enable_language_server": False,
            "disable_ai": True, "format_on_save": "off", "remove_trailing_whitespace_on_save": False,
            "ensure_final_newline_on_save": False, "preview_tabs": {"enabled": False},
            "telemetry": {"metrics": False, "diagnostics": False},
            "session": {"trust_all_worktrees": True},
        }))
        launch = [args.zed, "--foreground", "--user-data-dir", str(data), str(repo), str(repo / FILE)]
    return repo, env, launch


def run_editor(state, env, name, args, output):
    results = {}
    errors = {}
    for case in dict.fromkeys(args.case or CASES):
        print(f"{name}: {case}", flush=True)
        evidence = output / name / case
        evidence.mkdir(parents=True)
        repo, case_env, launch = workspace(state / case, env, name, args)
        editor = Editor(name, repo, evidence, case_env)
        wait_for(lambda: editor.find_window() is None, f"previous {name} window to close", seconds=5)
        with (evidence / "app.log").open("w") as log, process(launch, case_env, stdout=log, stderr=log):
            try:
                editor.window = wait_for(editor.prepare_window, f"{name} workspace window", seconds=15)
                time.sleep(0.8)
                editor.shot("startup")
                wait_for(lambda: editor.buffer() == SOURCE, "editable TypeScript fixture")
                editor.key("ctrl+Home")
                CASES[case](editor, args.seed)
            except (ProbeError, subprocess.SubprocessError) as error:
                errors[case] = str(error)
                try:
                    if editor.window:
                        editor.shot("probe-error")
                except subprocess.SubprocessError:
                    pass
        results.update(editor.results)
        zed_log = repo.parent / "zed-data" / "logs" / "Zed.log"
        if zed_log.exists():
            shutil.copyfile(zed_log, evidence / "Zed.log")
    return results, errors


def compare(results):
    stroll, zed = results["stroll"], results["zed"]
    rows = []
    for label in dict.fromkeys([*stroll, *zed]):
        left, right = stroll.get(label), zed.get(label)
        if left is None or right is None:
            status = "inconclusive"
        elif "expected" not in left:
            status = "match" if left["actual"] == right["actual"] else "difference"
        else:
            failed = [name for name, entry in [("stroll", left), ("zed", right)]
                      if entry["actual"] != entry["expected"]]
            status = "-".join(failed) + "-fail" if failed else "pass"
        rows.append({"check": label, "status": status, "stroll": left, "zed": right})
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stroll", default=str(ROOT / "target/release/stroll"))
    parser.add_argument("--zed", default=shutil.which("zeditor") or "zed")
    parser.add_argument("--seed", type=int, action="append", help="Repeatable random editing seed")
    parser.add_argument("--case", action="append", choices=CASES, help="Run only the named case")
    parser.add_argument("--output", type=Path, help="New directory for screenshots, logs and report.json")
    args = parser.parse_args()
    args.seed = args.seed or [19, 42]
    for program in (args.stroll, args.zed, "Xvfb", "xauth", "xrdb", "xdotool", "xclip",
                    "import", IMAGE_CONVERTER, "tesseract", "git"):
        if shutil.which(program) is None:
            parser.error(f"Executable not found: {program}")
    languages = command(["tesseract", "--list-langs"], capture_output=True, text=True)
    if "eng" not in languages.stdout.splitlines():
        parser.error("Tesseract's eng language data is required")
    output = args.output.resolve() if args.output else Path(tempfile.mkdtemp(prefix="stroll-editor-audit-"))
    if args.output:
        output.mkdir()
    print(f"Evidence: {output}", flush=True)
    results, errors = {}, {}
    versions = {
        "zed": command([args.zed, "--version"], capture_output=True, text=True).stdout.strip(),
        "stroll_checkout": command(["git", "-C", str(ROOT), "rev-parse", "HEAD"],
                                   capture_output=True, text=True).stdout.strip(),
        "stroll_binary": str(Path(shutil.which(args.stroll)).resolve()),
    }
    try:
        with tempfile.TemporaryDirectory(prefix="stroll-editor-state-") as state:
            with virtual_display(Path(state), output) as env:
                for name in ("stroll", "zed"):
                    try:
                        results[name], errors[name] = run_editor(Path(state), env, name, args, output)
                    except (ProbeError, subprocess.SubprocessError) as error:
                        results[name], errors[name] = {}, {"startup or session": str(error)}
    except (ProbeError, subprocess.SubprocessError) as error:
        errors["display"] = {"startup": str(error)}
    rows = compare(results) if len(results) == 2 else []
    report = {"versions": versions, "seeds": args.seed, "checks": rows, "errors": errors}
    (output / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    for row in rows:
        print(f"{row['status']:16} {row['check']}")
    for name, failures in errors.items():
        for case, message in failures.items():
            print(f"inconclusive     {name}/{case}: {message}")
    if any(errors.values()):
        return 2
    return int(any(row["status"] not in ("pass", "match") for row in rows))


if __name__ == "__main__":
    raise SystemExit(main())
