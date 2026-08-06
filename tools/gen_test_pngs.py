#!/usr/bin/env python3
"""Generates the small PNG fixtures used by the Phase D.9 selftest.

The RGBA fixture is 4x4 with pixel (x, y) = (r, g, b, a) where
r = x * 85, g = y * 85, b = 42, a = 255. The palette fixture is 2x2
using a 3-entry palette plus a tRNS chunk, with one row filtered with
the Sub filter and one with Paeth, to exercise the de-filter paths.
"""
import struct
import zlib

CRC = zlib.crc32  # polynomial matches PNG's


def chunk(ctype: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + ctype
        + data
        + struct.pack(">I", CRC(ctype + data) & 0xFFFFFFFF)
    )


def png(ihdr: bytes, idat_raw: bytes) -> bytes:
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(idat_raw, 9))
        + chunk(b"IEND", b"")
    )


# --- 4x4 RGBA ---------------------------------------------------------
W = H = 4
rows = bytearray()
for y in range(H):
    rows.append(0)  # filter: None
    for x in range(W):
        rows += bytes((x * 85, y * 85, 42, 255))
ihdr = struct.pack(">IIBBBBB", W, H, 8, 6, 0, 0, 0)
open("target/test-png-rgba.png", "wb").write(png(ihdr, bytes(rows)))

# --- 2x2 palette with tRNS -------------------------------------------
# palette: 0 = red, 1 = green, 2 = blue; tRNS alpha: 255, 255, 128
palette = b"\xff\x00\x00\x00\xff\x00\x00\x00\xff"
trns = b"\xff\xff\x80"
# row 0: indices [0,1] filtered with Sub; row 1: indices [1,2] with
# Paeth (raw = cur - paeth(left, up, upleft) = 01 01).
raw = b"\x01\x00\x01\x04\x01\x01"
ihdr = struct.pack(">IIBBBBB", 2, 2, 8, 3, 0, 0, 0)
open("target/test-png-palette.png", "wb").write(
    b"\x89PNG\r\n\x1a\n"
    + chunk(b"IHDR", ihdr)
    + chunk(b"PLTE", palette)
    + chunk(b"tRNS", trns)
    + chunk(b"IDAT", zlib.compress(raw, 9))
    + chunk(b"IEND", b"")
)

print("wrote target/test-png-rgba.png and target/test-png-palette.png")
