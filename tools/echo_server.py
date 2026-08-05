#!/usr/bin/env python3
"""UDP (9999) and TCP (9998) echo servers for the MachaOS network
selftest. `make test` starts this before QEMU; slirp forwards the
guest's packets to 10.0.2.2:<port> to the host's 127.0.0.1:<port> and
NATs the replies back, so a guest->host echo round trip exercises the
kernel's e1000 + IPv4 + UDP/TCP stack end to end.
"""
import socket
import sys
import threading

UDP_PORT = 9999
TCP_PORT = 9998


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


def main():
    threading.Thread(target=udp_loop, daemon=True).start()
    tcp_loop()


if __name__ == "__main__":
    sys.exit(main())
