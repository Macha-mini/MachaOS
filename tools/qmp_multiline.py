#!/usr/bin/env python3
"""QMP driver: multi-line selection -> copy -> cut -> paste -> save.

Document is "ab\\ncd\\nzz" (3 lines). Selects it all (Shift+Right x2,
Shift+Down x2), copies, moves Left (shrinking the selection by one char),
cuts the shrunken selection, pastes the multi-line text back over the
cursor, saves, quits. The disk file is inspected afterwards.
"""
import json
import socket
import time


def read_until(sock, token, timeout=5.0):
    sock.settimeout(timeout)
    buf = b""
    while token not in buf:
        try:
            chunk = sock.recv(4096)
            if not chunk:
                break
            buf += chunk
        except socket.timeout:
            break
    return buf


def cmd(sock, obj):
    sock.sendall(json.dumps(obj).encode() + b"\n")
    read_until(sock, b"return")


def key(sock, down, name):
    cmd(sock, {
        "execute": "input-send-event",
        "arguments": {"events": [{"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": name}}}]},
    })
    time.sleep(0.06)


def tap(sock, name):
    key(sock, True, name)
    time.sleep(0.08)
    key(sock, False, name)


def chord(sock, mod, name):
    key(sock, True, mod)
    tap(sock, name)
    key(sock, False, mod)


def main():
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect("/tmp/qmp.sock")
    read_until(sock, b"QMP")
    cmd(sock, {"execute": "qmp_capabilities"})

    time.sleep(15)
    tap(sock, "down")      # Documents
    time.sleep(0.4)
    tap(sock, "ret")       # enter Documents
    time.sleep(1.0)
    tap(sock, "ret")       # open aa-sel.txt (first entry)
    time.sleep(1.2)

    # Select the whole 3-line document from (0,0).
    key(sock, True, "shift")
    time.sleep(0.1)
    for _ in range(2):
        tap(sock, "right")
        time.sleep(0.15)
    for _ in range(2):
        tap(sock, "down")
        time.sleep(0.15)
    key(sock, False, "shift")
    time.sleep(0.2)

    chord(sock, "ctrl", "c")   # copy "ab\ncd\nzz"
    time.sleep(0.3)
    # Shrink the selection to "ab\ncd\nz" with Shift+Left (a plain
    # Left would collapse the selection instead).
    key(sock, True, "shift")
    time.sleep(0.1)
    tap(sock, "left")
    time.sleep(0.15)
    key(sock, False, "shift")
    time.sleep(0.2)
    chord(sock, "ctrl", "x")   # cut "ab\ncd\nz" -> doc "z"
    time.sleep(0.3)
    chord(sock, "ctrl", "v")   # paste multi-line text back -> "ab\ncd\nzz"
    time.sleep(0.4)
    chord(sock, "ctrl", "s")
    time.sleep(1.0)

    cmd(sock, {"execute": "quit"})
    sock.close()
    print("multiline driver done", flush=True)


if __name__ == "__main__":
    main()
