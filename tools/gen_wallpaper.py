#!/usr/bin/env python3
"""Converts an image into the raw pixel format wm.rs's wallpaper loader
expects: WIDTHxHEIGHT pixels, each a little-endian u32 0x00RRGGBB, row
major, no header. Cropped/scaled to exactly fill the target size (like
CSS `background-size: cover`) so the kernel can blit it directly with no
scaling logic of its own.
"""
import sys

from PIL import Image, ImageOps


def main():
    if len(sys.argv) != 5:
        print(f"usage: {sys.argv[0]} <input image> <output .raw> <width> <height>", file=sys.stderr)
        sys.exit(1)

    src_path, dst_path, width, height = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])

    img = Image.open(src_path).convert("RGB")
    img = ImageOps.fit(img, (width, height), method=Image.LANCZOS)

    with open(dst_path, "wb") as f:
        for pixel in img.getdata():
            r, g, b = pixel
            color = (r << 16) | (g << 8) | b
            f.write(color.to_bytes(4, byteorder="little"))

    print(f"wrote {dst_path}: {width}x{height} ({width * height * 4} bytes)")


if __name__ == "__main__":
    main()
