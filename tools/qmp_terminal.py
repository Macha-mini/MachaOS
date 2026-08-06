#!/usr/bin/env python3
"""QMP input driver for the Phase B.4 native-shell E2E test.

Boots are driven by the parent shell; this connects to the QMP socket,
waits for the desktop, clicks the restored Terminal window to focus it,
types `echo hello | cat`, presses Enter, waits a moment, then quits QEMU.
The serial log is inspected afterwards for the pipeline output.
"""
import json
import socket
import time

SOCK = "/tmp/qmp.sock"


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


def click(sock, x, y):
    cmd(sock, {
        "execute": "input-send-event",
        "arguments": {"events": [
            {"type": "abs", "data": {"axis": "x", "value": x}},
            {"type": "abs", "data": {"axis": "y", "value": y}},
            {"type": "button", "data": {"button": 1, "down": True}},
            {"type": "button", "data": {"button": 1, "down": False}},
        ]},
    })
    time.sleep(0.3)


# `|` and the like need Shift; plain letters/digits map to their qcode.
def type_text(sock, text):
    shift_map = {"|": "backslash"}
    for ch in text:
        if ch == " ":
            tap(sock, "spc")
        elif ch in shift_map:
            chord(sock, "shift", shift_map[ch])
        elif ch.isalpha():
            q = ch.lower()
            if ch.isupper():
                chord(sock, "shift", q)
            else:
                tap(sock, q)
        else:
            tap(sock, ch)


def main():
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(SOCK)
    read_until(sock, b"QMP")
    cmd(sock, {"execute": "qmp_capabilities"})

    time.sleep(15)  # desktop restores the terminal window
    # The session places Terminal at (100, 80); click inside its body.
    click(sock, 240, 240)
    time.sleep(0.5)

    type_text(sock, "echo hello | cat")
    time.sleep(0.3)
    tap(sock, "ret")
    time.sleep(2.0)

    cmd(sock, {"execute": "quit"})
    sock.close()
    print("QMP terminal driver done", flush=True)


if __name__ == "__main__":
    main()
