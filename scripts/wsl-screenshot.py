#!/usr/bin/env python3
"""Capture an X11 window under WSLg to a PNG file.

The GPUI app only becomes an X11 client when WAYLAND_DISPLAY is unset, so run it
like this before capturing:

    env -u WAYLAND_DISPLAY DISPLAY=:0 cargo run --release --locked

Then:

    python3 scripts/wsl-screenshot.py                 # stroll window -> /tmp/wsl-screenshot.png
    python3 scripts/wsl-screenshot.py --list          # list windows
    python3 scripts/wsl-screenshot.py --window gpui   # match class or title
    python3 scripts/wsl-screenshot.py -o /tmp/a.png

Only libX11 is required; no ImageMagick and no root window capture (Xwayland
does not implement it, XGetImage(root) fails with BadMatch).
"""

import argparse
import ctypes
import ctypes.util
import struct
import sys
import zlib

LIBC = ctypes.CDLL(ctypes.util.find_library("X11") or "libX11.so.6")


class XImage(ctypes.Structure):
    _fields_ = [
        ("width", ctypes.c_int),
        ("height", ctypes.c_int),
        ("xoffset", ctypes.c_int),
        ("format", ctypes.c_int),
        ("data", ctypes.c_void_p),
        ("byte_order", ctypes.c_int),
        ("bitmap_unit", ctypes.c_int),
        ("bitmap_bit_order", ctypes.c_int),
        ("bitmap_pad", ctypes.c_int),
        ("depth", ctypes.c_int),
        ("bytes_per_line", ctypes.c_int),
        ("bits_per_pixel", ctypes.c_int),
        ("red_mask", ctypes.c_ulong),
        ("green_mask", ctypes.c_ulong),
        ("blue_mask", ctypes.c_ulong),
        ("obdata", ctypes.c_void_p),
    ]


class XClassHint(ctypes.Structure):
    _fields_ = [("res_name", ctypes.c_char_p), ("res_class", ctypes.c_char_p)]


class XErrorEvent(ctypes.Structure):
    _fields_ = [
        ("type", ctypes.c_int),
        ("display", ctypes.c_void_p),
        ("resourceid", ctypes.c_ulong),
        ("serial", ctypes.c_ulong),
        ("error_code", ctypes.c_ubyte),
        ("request_code", ctypes.c_ubyte),
        ("minor_code", ctypes.c_ubyte),
    ]


X_ERROR_HANDLER = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.POINTER(XErrorEvent))
last_error = None


@X_ERROR_HANDLER
def on_x_error(_display, event):
    global last_error
    last_error = (event.contents.error_code, event.contents.request_code)
    return 0


X = LIBC
X.XOpenDisplay.restype = ctypes.c_void_p
X.XOpenDisplay.argtypes = [ctypes.c_char_p]
X.XDefaultRootWindow.restype = ctypes.c_ulong
X.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
X.XSetErrorHandler.restype = ctypes.c_void_p
X.XSetErrorHandler.argtypes = [X_ERROR_HANDLER]
X.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
X.XQueryTree.argtypes = [
    ctypes.c_void_p,
    ctypes.c_ulong,
    ctypes.POINTER(ctypes.c_ulong),
    ctypes.POINTER(ctypes.c_ulong),
    ctypes.POINTER(ctypes.POINTER(ctypes.c_ulong)),
    ctypes.POINTER(ctypes.c_uint),
]
X.XFetchName.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_char_p)]
X.XGetClassHint.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(XClassHint)]
X.XGetGeometry.argtypes = [
    ctypes.c_void_p,
    ctypes.c_ulong,
    ctypes.POINTER(ctypes.c_ulong),
    ctypes.POINTER(ctypes.c_int),
    ctypes.POINTER(ctypes.c_int),
    ctypes.POINTER(ctypes.c_uint),
    ctypes.POINTER(ctypes.c_uint),
    ctypes.POINTER(ctypes.c_uint),
    ctypes.POINTER(ctypes.c_uint),
]
X.XGetImage.restype = ctypes.POINTER(XImage)
X.XGetImage.argtypes = [
    ctypes.c_void_p,
    ctypes.c_ulong,
    ctypes.c_int,
    ctypes.c_int,
    ctypes.c_uint,
    ctypes.c_uint,
    ctypes.c_ulong,
    ctypes.c_int,
]
X.XGetPixel.restype = ctypes.c_ulong
X.XGetPixel.argtypes = [ctypes.POINTER(XImage), ctypes.c_int, ctypes.c_int]
X.XFree.argtypes = [ctypes.c_void_p]


def walk(display, window, depth=0):
    name = ctypes.c_char_p()
    title = ""
    if X.XFetchName(display, window, ctypes.byref(name)) and name.value:
        title = name.value.decode(errors="replace")
    hint = XClassHint()
    label = ""
    if X.XGetClassHint(display, window, ctypes.byref(hint)):
        label = (hint.res_class or b"").decode(errors="replace") or (
            hint.res_name or b""
        ).decode(errors="replace")
    yield window, depth, label, title

    root, parent = ctypes.c_ulong(), ctypes.c_ulong()
    children = ctypes.POINTER(ctypes.c_ulong)()
    count = ctypes.c_uint()
    if X.XQueryTree(
        display, window, ctypes.byref(root), ctypes.byref(parent),
        ctypes.byref(children), ctypes.byref(count),
    ):
        for index in range(count.value):
            yield from walk(display, children[index], depth + 1)


def list_windows(display, root):
    windows = []
    for window, _depth, label, title in walk(display, root):
        geometry = window_geometry(display, window)
        if geometry and geometry[2] > 1 and geometry[3] > 1:
            windows.append((window, label, title, geometry))
    return windows


def window_geometry(display, window):
    root, x, y = ctypes.c_ulong(), ctypes.c_int(), ctypes.c_int()
    width, height = ctypes.c_uint(), ctypes.c_uint()
    border, depth = ctypes.c_uint(), ctypes.c_uint()
    if not X.XGetGeometry(
        display, window, ctypes.byref(root), ctypes.byref(x), ctypes.byref(y),
        ctypes.byref(width), ctypes.byref(height), ctypes.byref(border),
        ctypes.byref(depth),
    ):
        return None
    return (x.value, y.value, width.value, height.value)


def image_rows(image, wrapper):
    """Return RGB rows from an XImage, using the raw buffer when possible."""
    width, height = image.width, image.height
    stride = image.bytes_per_line
    fast = (
        image.bits_per_pixel == 32
        and image.byte_order == 0  # LSBFirst
        and (image.red_mask, image.green_mask, image.blue_mask)
        == (0xFF0000, 0xFF00, 0xFF)
    )
    if fast:
        raw = ctypes.string_at(image.data, stride * height)
        rows = []
        for row in range(height):
            start = row * stride
            line = bytearray(width * 3)
            for column in range(width):
                offset = start + column * 4
                line[column * 3] = raw[offset + 2]
                line[column * 3 + 1] = raw[offset + 1]
                line[column * 3 + 2] = raw[offset]
            rows.append(bytes(line))
        return rows

    def channel(pixel, mask):
        if not mask:
            return 0
        shift = (mask & -mask).bit_length() - 1
        value = (pixel & mask) >> shift
        return value * 255 // (mask >> shift)

    rows = []
    for row in range(height):
        line = bytearray()
        for column in range(width):
            pixel = X.XGetPixel(wrapper, column, row)
            line += bytes(
                (
                    channel(pixel, image.red_mask),
                    channel(pixel, image.green_mask),
                    channel(pixel, image.blue_mask),
                )
            )
        rows.append(bytes(line))
    return rows


def write_png(path, width, height, rows):
    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    raw = b"".join(b"\x00" + row for row in rows)
    header = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    with open(path, "wb") as file:
        file.write(b"\x89PNG\r\n\x1a\n")
        file.write(chunk(b"IHDR", header))
        file.write(chunk(b"IDAT", zlib.compress(raw, 6)))
        file.write(chunk(b"IEND", b""))


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--window", default="stroll", help="window class or title substring")
    parser.add_argument("-o", "--output", default="/tmp/wsl-screenshot.png")
    parser.add_argument("--list", action="store_true", help="list capturable windows and exit")
    args = parser.parse_args()

    X.XSetErrorHandler(on_x_error)
    display = X.XOpenDisplay(None)
    if not display:
        sys.exit("cannot open the X display; is DISPLAY set and is Xwayland running?")

    candidates = list_windows(display, X.XDefaultRootWindow(display))
    if args.list:
        for window, label, title, (x, y, width, height) in candidates:
            print(f"{hex(window):>10}  {width}x{height}+{x}+{y}  {label!r} {title!r}")
        return

    matches = [
        candidate
        for candidate in candidates
        if args.window.lower() in candidate[1].lower()
        or args.window.lower() in candidate[2].lower()
    ]
    if not matches:
        print("no window matched; available windows:", file=sys.stderr)
        for window, label, title, geometry in candidates:
            print(f"  {hex(window)} {geometry[2]}x{geometry[3]} {label!r} {title!r}", file=sys.stderr)
        sys.exit(1)

    window, label, title, (x, y, width, height) = matches[0]
    wrapper = X.XGetImage(display, window, 0, 0, width, height, ctypes.c_ulong(-1).value, 2)
    X.XSync(display, 0)
    if not wrapper or last_error:
        sys.exit(f"XGetImage failed for {hex(window)} {label!r} {title!r}: error {last_error}")

    image = wrapper.contents
    width, height = image.width, image.height
    rows = image_rows(image, wrapper)
    X.XFree(wrapper)
    write_png(args.output, width, height, rows)
    print(f"{args.output}: {width}x{height} {label!r} {title!r}")


main()
