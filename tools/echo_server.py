#!/usr/bin/env python3
"""Test servers for the MachaOS network selftest:
  - UDP echo on 127.0.0.1:9999
  - TCP echo on 127.0.0.1:9998
  - static HTTP on 127.0.0.1:8000, serving the directory given as argv[1]

`make test` starts this before QEMU; slirp forwards the guest's
packets to 10.0.2.2:<port> to the host's 127.0.0.1:<port> and NATs the
replies back, so guest->host round trips exercise the kernel's e1000 +
IPv4 + UDP/TCP + HTTP paths end to end.
"""
import functools
import http.server
import socket
import sys
import threading

UDP_PORT = 9999
TCP_PORT = 9998
HTTP_PORT = 8000


def udp_loop():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind(("127.0.0.1", UDP_PORT))
    print(f"udp echo on 127.0.0.1:{UDP_PORT}", flush=True)
    while True:
        data, addr = s.recvfrom(65535)
        s.sendto(data, addr)


def tcp_loop():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", TCP_PORT))
    s.listen(4)
    print(f"tcp echo on 127.0.0.1:{TCP_PORT}", flush=True)
    while True:
        conn, _ = s.accept()
        try:
            while True:
                data = conn.recv(4096)
                if not data:
                    break
                conn.sendall(data)
        except OSError:
            pass
        finally:
            conn.close()


def http_loop(directory: str):
    handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=directory)
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", HTTP_PORT), handler)
    print(f"http on 127.0.0.1:{HTTP_PORT} (dir {directory})", flush=True)
    srv.serve_forever()


def main():
    directory = sys.argv[1] if len(sys.argv) > 1 else "."
    threading.Thread(target=udp_loop, daemon=True).start()
    threading.Thread(target=http_loop, args=(directory,), daemon=True).start()
    tcp_loop()


if __name__ == "__main__":
    sys.exit(main())

