#!/usr/bin/env python3
"""QMP input driver for the editor selection E2E test.

Boots are driven by the parent shell; this connects to the QMP socket,
waits for the desktop to come up, navigates the File Explorer into
Documents, opens aa-sel.txt in Notepad, selects "abc" with Shift+Right
x3, cuts it (Ctrl+X), saves (Ctrl+S), pastes it back (Ctrl+V) and saves
again — then quits QEMU. The disk is inspected afterwards.
"""
import json
import socket
import sys
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


def main():
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(SOCK)
    read_until(sock, b"QMP")
    cmd(sock, {"execute": "qmp_capabilities"})

    # Give the desktop time to restore the session before typing.
    time.sleep(15)
    tap(sock, "down")     # select Documents (index 1 in the home listing)
    time.sleep(0.4)
    tap(sock, "ret")      # enter Documents
    time.sleep(1.0)
    tap(sock, "ret")      # open aa-sel.txt (first entry) in Notepad
    time.sleep(1.2)

    # Shift+Right x3 selects "abc" (cursor starts at 0,0).
    key(sock, True, "shift")
    time.sleep(0.1)
    for _ in range(3):
        tap(sock, "right")
        time.sleep(0.2)
    key(sock, False, "shift")
    time.sleep(0.2)

    chord(sock, "ctrl", "x")   # cut the selection
    time.sleep(0.4)
    chord(sock, "ctrl", "s")   # save (file should now be "defghij")
    time.sleep(1.0)
    chord(sock, "ctrl", "v")   # paste "abc" back at the cursor
    time.sleep(0.4)
    chord(sock, "ctrl", "s")   # save (file should be "abcdefghij" again)
    time.sleep(1.0)

    cmd(sock, {"execute": "quit"})
    sock.close()
    print("QMP driver done", flush=True)


if __name__ == "__main__":
    main()
