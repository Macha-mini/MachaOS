#!/usr/bin/env python3
"""Host-side probe for MachaOS's Phase 10 TCP *server* selftest.

The guest (prog_linux_tcp_server) binds port 8000, listens, prints
`[TCP-SRV-READY]` on the serial log, and blocks in accept(). QEMU's
user-net hostfwd maps host 127.0.0.1:<fwd_port> onto guest port 8000.
This script:

  1. watches the serial log for the ready marker (fails fast on the
     guest's own FAIL markers or a QEMU death),
  2. connects to 127.0.0.1:<fwd_port>,
  3. sends "ping-from-host\\n", reads the echo back,
  4. exits 0 iff the echo is byte-identical.

`make test` runs this in the background alongside QEMU; the selftest
still gates PASS/FAIL on the guest's exit code, so this probe is the
traffic generator rather than the verdict.
"""

import socket
import sys
import time

LOG = sys.argv[1] if len(sys.argv) > 1 else "/tmp/machaos-selftest.log"
FWD_PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 18000
PING = b"ping-from-host\n"
DEADLINE_S = 75  # selftest wait is 30 s; give margin


def main() -> int:
    deadline = time.time() + DEADLINE_S
    seen_ready = False
    # Poll the log for the marker (the log file may not exist yet).
    while time.time() < deadline:
        try:
            with open(LOG, "rb") as f:
                data = f.read()
        except OSError:
            data = b""
        if b"[SELFTEST FAIL]" in data or b"[TCP-SRV-FAIL]" in data:
            print("tcp_probe: guest reported failure before ready", flush=True)
            return 1
        if b"[TCP-SRV-READY]" in data:
            seen_ready = True
            break
        if b"SELFTEST OK" in data:
            print("tcp_probe: selftest finished before the server was ready", flush=True)
            return 1
        time.sleep(0.2)
    if not seen_ready:
        print("tcp_probe: timeout waiting for [TCP-SRV-READY] marker", flush=True)
        return 1

    # The handshake + echo happens in the guest within a second or two
    # of the marker; a modest connect budget is plenty.
    try:
        s = socket.create_connection(("127.0.0.1", FWD_PORT), timeout=10)
    except OSError as e:
        print(f"tcp_probe: connect failed: {e}", flush=True)
        return 1

    try:
        s.sendall(PING)
        got = b""
        while len(got) < len(PING):
            chunk = s.recv(64)
            if not chunk:
                break
            got += chunk
    finally:
        s.close()

    if got == PING:
        print("tcp_probe: echo matched", flush=True)
        return 0
    print(f"tcp_probe: echo mismatch: {got!r}", flush=True)
    return 1


if __name__ == "__main__":
    sys.exit(main())
