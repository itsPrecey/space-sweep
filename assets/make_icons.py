"""Build the app icons from the traced logo.

logo.svg is the mark traced with LineMorph from the design clip
(`linemorph trace clean.png --smoothing 3 --tolerance 0.4 --min-region 4 --min-area 1`).
This script wraps it on a dark rounded tile (icon.svg), renders every Windows icon size
with `linemorph render`, packs them into icon.ico, and writes icon-128.rgba for the window.

Run: python assets/make_icons.py <path to linemorph.exe>
"""

import os
import re
import struct
import subprocess
import sys
import tempfile
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]

# Mark bounds inside logo.svg's 200x200 box (measured from the clip).
MARK = (10, 28, 191, 171)


def icon_svg() -> str:
    logo = open(os.path.join(HERE, "logo.svg"), encoding="utf-8").read()
    inner = re.search(r"<svg[^>]*>(.*)</svg>", logo, re.S).group(1)
    x0, y0, x1, y1 = MARK
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    scale = 0.80 * 256 / (x1 - x0)
    return f"""<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 256 256">
<defs>
<linearGradient id="tile" x1="0" y1="0" x2="0" y2="1">
<stop offset="0" stop-color="#1a2040"/><stop offset="1" stop-color="#090b14"/>
</linearGradient>
</defs>
<rect x="4" y="4" width="248" height="248" rx="56" fill="url(#tile)"/>
<rect x="4.75" y="4.75" width="246.5" height="246.5" rx="55.25" fill="none" stroke="#ffffff" stroke-opacity="0.08" stroke-width="1.5"/>
<g transform="translate(128 {128 + 4}) scale({scale:.4f}) translate({-cx} {-cy})">
{inner}
</g>
</svg>
"""


def read_png_rgba(path: str):
    """Minimal PNG decoder for 8-bit RGBA, non-interlaced (what linemorph writes)."""
    data = open(path, "rb").read()
    assert data[:8] == b"\x89PNG\r\n\x1a\n"
    pos, idat, w = 8, b"", 0
    while pos < len(data):
        length, kind = struct.unpack(">I4s", data[pos:pos + 8])
        body = data[pos + 8:pos + 8 + length]
        if kind == b"IHDR":
            w, h, depth, ctype, _, _, interlace = struct.unpack(">IIBBBBB", body)
            assert depth == 8 and ctype == 6 and interlace == 0, (depth, ctype, interlace)
        elif kind == b"IDAT":
            idat += body
        pos += 12 + length
    raw = zlib.decompress(idat)
    stride = w * 4
    out = bytearray()
    prev = bytearray(stride)
    i = 0
    while i < len(raw):
        f = raw[i]
        line = bytearray(raw[i + 1:i + 1 + stride])
        for x in range(stride):
            a = line[x - 4] if x >= 4 else 0
            b = prev[x]
            c = prev[x - 4] if x >= 4 else 0
            if f == 1:
                line[x] = (line[x] + a) & 255
            elif f == 2:
                line[x] = (line[x] + b) & 255
            elif f == 3:
                line[x] = (line[x] + (a + b) // 2) & 255
            elif f == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                line[x] = (line[x] + pr) & 255
        out += line
        prev = line
        i += 1 + stride
    return w, bytes(out)


def main():
    linemorph = sys.argv[1] if len(sys.argv) > 1 else "linemorph"
    svg_path = os.path.join(HERE, "icon.svg")
    open(svg_path, "w", encoding="utf-8").write(icon_svg())
    tmp = tempfile.mkdtemp(prefix="space-sweep-icons-")
    pngs = []
    for s in SIZES:
        p = os.path.join(tmp, f"icon-{s}.png")
        subprocess.run([linemorph, "render", svg_path, "-o", p, "--width", str(s), "--height", str(s)], check=True)
        pngs.append((s, open(p, "rb").read()))
        if s == 256:
            open(os.path.join(HERE, "icon-256.png"), "wb").write(pngs[-1][1])
        if s == 128:
            w, rgba = read_png_rgba(p)
            assert w == 128
            open(os.path.join(HERE, "icon-128.rgba"), "wb").write(rgba)

    # ICO with PNG-compressed entries (supported since Windows Vista).
    header = struct.pack("<HHH", 0, 1, len(pngs))
    offset = 6 + 16 * len(pngs)
    entries, blobs = b"", b""
    for s, png in pngs:
        dim = 0 if s >= 256 else s
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(png), offset + len(blobs))
        blobs += png
    open(os.path.join(HERE, "icon.ico"), "wb").write(header + entries + blobs)
    print(f"icon.svg, icon.ico ({len(pngs)} sizes), icon-256.png, icon-128.rgba written to {HERE}")


if __name__ == "__main__":
    main()
