#!/usr/bin/env python3
"""Headless GUI/WM regression test for MachaOS (Phase D.10).

The Makefile (`make test-gui`) boots QEMU with `-display none` and a QMP
unix socket; this script connects, waits for the desktop's first-composite
serial marker, screendumps the framebuffer via QMP, and pixel-verifies:

  A. the boot state: the wallpaper is composited 1:1 and the taskbar is
     drawn (its opaque 0x2E2E2E top line, and the translucent body clearly
     darkening the wallpaper behind it);
  B. the start-menu launcher opens when the cursor is moved onto the Start
     button and left-clicked (rel PS/2 deltas + QMP btn events) — verified
     by the launcher's opaque 0x3F3F3F border and 0x3A3A3A search box;
  C. clicking the first launcher tile spawns a Terminal window — verified
     by its console background (0x202020) and a darkened title bar.

The cursor starts at the screen center on a fresh boot; the script moves it
with relative deltas computed from there. QEMU's PS/2 emulation drops the
*first* input event after boot (the kernel's own SKIP_FIRST_PACKET), so the
script sends a net-zero probe pair first.

Screendumps come out as PPM on this QEMU; a PNG path is included for
portability. No third-party libraries — zlib is only used for the PNG
fallback.

Usage: gui_wm_test.py <serial log> <wallpaper.raw> <qmp socket> <shot prefix>
"""

import json
import socket
import struct
import sys
import time
import zlib

MARKER = "desktop: first frame composited"
BOOT_TIMEOUT = 90.0

# Expected colors (see src/wm.rs).
TASKBAR_TOP_LINE = (46, 46, 46)  # 0x2E2E2E
LAUNCHER_BORDER = (63, 63, 63)   # 0x3F3F3F
LAUNCHER_SEARCH = (58, 58, 58)   # 0x3A3A3A
CONSOLE_BG = (32, 32, 32)        # 0x202020

START_BTN = (30, 1060)           # inside the Start button (12..52, 1038..1074)
TILE0 = (78, 466)                # launcher tile 0 ("Terminal") center, computed
                                 # from the border scan at runtime instead

CENTER = (960, 540)
WALLPAPER_POINTS = [(400, 300), (1500, 400), (600, 800), (100, 100), (1800, 1000)]


def fail(msg):
    print(f"[FAIL] {msg}", flush=True)
    sys.exit(1)


class Ppm:
    """Minimal P6 PPM decoder (QEMU's screendump output here)."""

    def __init__(self, data):
        if data[:2] != b"P6":
            raise ValueError("not a P6 PPM")
        idx = 2
        toks = []
        while len(toks) < 3:
            while data[idx:idx + 1] in b" \n\t\r":
                idx += 1
            if data[idx:idx + 1] == b"#":
                while data[idx:idx + 1] != b"\n":
                    idx += 1
                continue
            start = idx
            while data[idx:idx + 1] not in b" \n\t\r":
                idx += 1
            toks.append(data[start:idx])
        self.w, self.h, self.maxval = (int(t) for t in toks)
        idx += 1  # single whitespace before the raster
        self.pix = data[idx:]
        if len(self.pix) < self.w * self.h * 3:
            raise ValueError("truncated PPM raster")

    def px(self, x, y):
        o = (y * self.w + x) * 3
        return (self.pix[o], self.pix[o + 1], self.pix[o + 2])


class Png:
    """Minimal PNG decoder (color 2 RGB / 6 RGBA, 8-bit, non-interlaced),
    used only if a future QEMU writes PNG screendumps instead of PPM."""

    def __init__(self, data):
        if data[:8] != b"\x89PNG\r\n\x1a\n":
            raise ValueError("not a PNG")
        pos = 8
        idat = b""
        w = h = None
        while pos < len(data):
            ln = struct.unpack(">I", data[pos:pos + 4])[0]
            typ = data[pos + 4:pos + 8]
            if typ == b"IHDR":
                w, h, bd, ct, comp, filt, inter = struct.unpack(">IIBBBBB", data[pos + 8:pos + 29])
                if bd != 8 or comp != 0 or inter != 0 or ct not in (2, 6):
                    raise ValueError(f"unsupported PNG format bd={bd} ct={ct}")
                self.bpp = 3 if ct == 2 else 4
            elif typ == b"IDAT":
                idat += data[pos + 8:pos + 8 + ln]
            pos += 12 + ln
        raw = zlib.decompress(idat)
        self.w, self.h = w, h
        stride = w * self.bpp
        self.rgb = bytearray(w * h * 3)
        prev = bytearray(stride)
        out = 0
        q = 0
        for y in range(h):
            f = raw[q]
            q += 1
            line = bytearray(raw[q:q + stride])
            q += stride
            if f == 1:
                for i in range(self.bpp, stride):
                    line[i] = (line[i] + line[i - self.bpp]) & 0xFF
            elif f == 2:
                for i in range(stride):
                    line[i] = (line[i] + prev[i]) & 0xFF
            elif f == 3:
                for i in range(stride):
                    a = line[i - self.bpp] if i >= self.bpp else 0
                    line[i] = (line[i] + ((a + prev[i]) >> 1)) & 0xFF
            elif f == 4:
                for i in range(stride):
                    a = line[i - self.bpp] if i >= self.bpp else 0
                    b = prev[i]
                    c = prev[i - self.bpp] if i >= self.bpp else 0
                    p = a + b - c
                    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                    pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                    line[i] = (line[i] + pr) & 0xFF
            for i in range(stride):
                ch = i % self.bpp
                if ch == 3:
                    continue  # alpha (RGBA)
                self.rgb[out] = line[i]
                out += 1
            prev = line

    def px(self, x, y):
        o = (y * self.w + x) * 3
        return (self.rgb[o], self.rgb[o + 1], self.rgb[o + 2])


def load_image(path):
    data = open(path, "rb").read()
    if data[:2] == b"P6":
        return Ppm(data)
    return Png(data)


class Qmp:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(path)
        buf = b""
        while b"QMP" not in buf:
            chunk = self.s.recv(4096)
            if not chunk:
                raise RuntimeError("QMP socket closed during greeting")
            buf += chunk
        self.cmd({"execute": "qmp_capabilities"})

    def cmd(self, obj, timeout=10.0):
        self.s.sendall(json.dumps(obj).encode() + b"\n")
        self.s.settimeout(timeout)
        out = b""
        try:
            while b"return" not in out and b"error" not in out:
                chunk = self.s.recv(4096)
                if not chunk:
                    break
                out += chunk
        except socket.timeout:
            pass
        return json.loads(out) if out.strip() else {}

    def rel(self, dx, dy):
        r = self.cmd({"execute": "input-send-event", "arguments": {"events": [
            {"type": "rel", "data": {"axis": "x", "value": dx}},
            {"type": "rel", "data": {"axis": "y", "value": dy}},
        ]}})
        if "error" in r:
            raise RuntimeError(f"QMP rel event failed: {r}")
        time.sleep(0.25)

    def btn(self, name, down):
        r = self.cmd({"execute": "input-send-event", "arguments": {"events": [
            {"type": "btn", "data": {"button": name, "down": down}},
        ]}})
        if "error" in r:
            raise RuntimeError(f"QMP btn event failed: {r}")
        time.sleep(0.15)

    def click(self, name="left"):
        self.btn(name, True)
        self.btn(name, False)

    def screendump(self, path):
        r = self.cmd({"execute": "screendump", "arguments": {"filename": path}})
        if "error" in r:
            raise RuntimeError(f"QMP screendump failed: {r}")
        time.sleep(0.3)

    def quit(self):
        try:
            self.cmd({"execute": "quit"})
        except Exception:
            pass


def move_to(qmp, dx, dy):
    """Moves the cursor by (dx, dy) from its current position, chunking
    deltas into PS/2-friendly pieces (per-packet range is ±255)."""
    step = 255
    while dx != 0 or dy != 0:
        sx = max(-step, min(step, dx))
        sy = max(-step, min(step, dy))
        qmp.rel(sx, sy)
        dx -= sx
        dy -= sy


def read_log(log_path):
    try:
        with open(log_path, "rb") as f:
            return f.read().decode(errors="replace")
    except OSError:
        return ""


def wait_for_marker(log_path):
    deadline = time.time() + BOOT_TIMEOUT
    tail = ""
    while time.time() < deadline:
        tail = read_log(log_path)
        if MARKER in tail:
            return
        time.sleep(0.5)
    print(f"[FAIL] desktop marker '{MARKER}' not seen in {BOOT_TIMEOUT:.0f}s; log tail:")
    print("\n".join(tail.splitlines()[-40:]))
    sys.exit(1)


def main():
    log_path, wallpaper_path, sock_path, shot_prefix = sys.argv[1:5]

    wait_for_marker(log_path)
    # The marker prints "(WxH)"; those must match the wallpaper and the
    # screendump, otherwise every pixel comparison would be meaningless.
    # The serial file can be caught mid-line, so keep polling until the
    # "(WxH)" token is actually flushed.
    w = h = None
    deadline = time.time() + 10.0
    while time.time() < deadline:
        dims_tok = [t for t in read_log(log_path).split()
                    if t.startswith("(") and "x" in t and t.strip("()").replace("x", "").isdigit()]
        if dims_tok:
            try:
                w, h = (int(v) for v in dims_tok[-1].strip("()").split("x"))
                break
            except ValueError:
                pass
        time.sleep(0.3)
    if not w or not h:
        fail("could not parse resolution from the desktop marker")

    wp_bytes = open(wallpaper_path, "rb").read()
    if len(wp_bytes) != w * h * 4:
        fail(f"wallpaper.raw is {len(wp_bytes)} bytes, expected {w * h * 4} ({w}x{h})")

    def wp_px(x, y):
        o = (y * w + x) * 4
        v = struct.unpack("<I", wp_bytes[o:o + 4])[0]
        return ((v >> 16) & 0xFF, (v >> 8) & 0xFF, v & 0xFF)

    def lum(c):
        return 0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]

    qmp = Qmp(sock_path)
    time.sleep(2.0)  # let the clock render and any late composite settle

    # ---- Group A: boot state (wallpaper + taskbar) --------------------
    shot1 = shot_prefix + "-1-boot.ppm"
    qmp.screendump(shot1)
    img = load_image(shot1)
    if (img.w, img.h) != (w, h):
        fail(f"screendump is {img.w}x{img.h}, expected {w}x{h}")

    for (x, y) in WALLPAPER_POINTS:
        s = img.px(x, y)
        e = wp_px(x, y)
        if not all(abs(s[i] - e[i]) <= 2 for i in range(3)):
            fail(f"wallpaper mismatch at ({x},{y}): screendump {s} vs wallpaper {e}")
    print("[OK] wallpaper composited 1:1 at 5 sample points")

    if img.px(500, 1032) != TASKBAR_TOP_LINE:
        fail(f"taskbar top line at (500,1032) is {img.px(500,1032)}, "
             f"expected {TASKBAR_TOP_LINE}")
    print("[OK] taskbar present (opaque 0x2E2E2E top line)")

    # Sampled where the wallpaper is bright (center-bottom): the acrylic
    # blur + 190/255 blend toward 0x1F1F1F must visibly darken it.
    body = img.px(960, 1060)
    wp_body = wp_px(960, 1060)
    if lum(body) > lum(wp_body) - 15:
        fail(f"taskbar body at (960,1060) is {body} (wallpaper {wp_body}); "
             "expected the translucent bar to darken it")
    print("[OK] taskbar translucent body darkens the wallpaper behind it")

    # ---- Group B: open the launcher by clicking the Start button -------
    # The first input event after boot is dropped (kernel SKIP_FIRST_PACKET
    # consumes the first real packet under QEMU), so send a net-zero probe
    # pair first. The cursor is at the center on a fresh boot.
    qmp.rel(-1, -1)
    qmp.rel(1, 1)
    move_to(qmp, START_BTN[0] - CENTER[0], START_BTN[1] - CENTER[1])
    qmp.click("left")
    time.sleep(1.0)

    shot2 = shot_prefix + "-2-launcher.ppm"
    qmp.screendump(shot2)
    img = load_image(shot2)

    # Launcher is left-aligned (popup_x = 12) with an opaque 0x3F3F3F
    # border; its vertical left edge sits at x=11. Require a contiguous
    # run of border pixels (a real panel edge, not coincidental wallpaper
    # matches) and take the run's top as the panel's y.
    border_ys = [y for y in range(150, 1050) if img.px(11, y) == LAUNCHER_BORDER]
    if not border_ys:
        fail("launcher border not found at x=11; the Start-button click did "
             "not open the launcher")
    best = (0, 0)
    run_start = border_ys[0]
    run_len = 1
    for y in border_ys[1:] + [9999]:
        if y == run_start + run_len:
            run_len += 1
        else:
            if run_len > best[0]:
                best = (run_len, run_start)
            run_start = y
            run_len = 1
    if best[0] < 40:
        fail(f"launcher border at x=11 is not a contiguous edge "
             f"(longest run {best[0]} px)")
    py = best[1]
    print("[OK] launcher opened (0x3F3F3F border found)")

    for sx in (100, 350):
        if not all(abs(img.px(sx, py + 26)[i] - LAUNCHER_SEARCH[i]) <= 2 for i in range(3)):
            fail(f"launcher search box mismatch at ({sx},{py + 26}): "
                 f"{img.px(sx, py + 26)} expected {LAUNCHER_SEARCH}")
    print("[OK] launcher search box rendered (0x3A3A3A)")

    # ---- Group C: click launcher tile 0 -> Terminal window opens ------
    # Tile grid starts at (px+20, py+64); tile 0 spans (32..124, py+64..py+148).
    t0 = (32 + 46, py + 64 + 42)
    move_to(qmp, t0[0] - START_BTN[0], t0[1] - START_BTN[1])
    qmp.click("left")
    time.sleep(1.0)

    shot3 = shot_prefix + "-3-terminal.ppm"
    qmp.screendump(shot3)
    img = load_image(shot3)

    if len([y for y in range(150, 1050) if img.px(11, y) == LAUNCHER_BORDER]) >= 40:
        fail("launcher is still open after clicking a tile")

    # First spawned window cascades to (168, 128) (spawn_counter=1 -> offset 28).
    wx, wy = 168, 128
    if not all(abs(img.px(900, 600)[i] - CONSOLE_BG[i]) <= 2 for i in range(3)):
        fail(f"terminal console background at (900,600) is {img.px(900, 600)}, "
             f"expected {CONSOLE_BG}")
    print("[OK] Terminal window opened from the launcher (console rendered)")

    # (wx+180, wy+16) is inside the title bar but clear of the white
    # "Terminal" label (left) and the min/max/close buttons (right).
    title = img.px(wx + 180, wy + 16)
    wp_title = wp_px(wx + 180, wy + 16)
    if lum(title) > lum(wp_title) - 20:
        fail(f"terminal title bar at ({wx + 180},{wy + 16}) is {title} "
             f"(wallpaper {wp_title}); expected a dark bar")
    print("[OK] Terminal title bar drawn over the wallpaper")

    qmp.quit()
    print(f"[PASS] GUI/WM screendump test: wallpaper, taskbar, launcher, "
          f"and app launch all correct at {w}x{h}", flush=True)


if __name__ == "__main__":
    main()
