#!/usr/bin/env python3
"""Builds packaging/windows/merpati.ico (16-256 px, PNG entries) from the app icon.

    python3 scripts/make-windows-icon.py

Uses macOS `sips` to scale packaging/macos/icon-1024.png; the .ico is committed,
so only rerun it when the icon changes.
"""

import os
import struct
import subprocess
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SOURCE = os.path.join(ROOT, "packaging", "macos", "icon-1024.png")
OUT = os.path.join(ROOT, "packaging", "windows", "merpati.ico")
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]


def main():
    images = []
    with tempfile.TemporaryDirectory() as tmp:
        for size in SIZES:
            path = os.path.join(tmp, f"{size}.png")
            subprocess.run(["sips", "-z", str(size), str(size), SOURCE, "--out", path],
                           check=True, stdout=subprocess.DEVNULL)
            with open(path, "rb") as f:
                images.append((size, f.read()))
    # ICONDIR, then one ICONDIRENTRY per image, then the PNG data.
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = len(header) + 16 * len(images)
    entries, data = b"", b""
    for size, png in images:
        dim = 0 if size >= 256 else size  # 0 means 256
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(png), offset + len(data))
        data += png
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "wb") as f:
        f.write(header + entries + data)
    print(f"{OUT}: {len(images)} sizes, {os.path.getsize(OUT)} bytes")


if __name__ == "__main__":
    main()
