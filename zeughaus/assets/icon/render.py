#!/usr/bin/env python3
"""Source of truth for the Zeughaus app icon.

The mark is a stencilled Z, the way a depot crate is marked: one geometric
letter, two bridges cut through the bars. Everything here is polygons, so the
same numbers produce the SVG and the PNG set without an external rasterizer.

    python3 zeughaus/assets/icon/render.py

writes zeughaus.svg and zeughaus-<size>.png next to this file.
"""

from __future__ import annotations

import math
import pathlib
import struct
import zlib

GRID = 512.0
BG = (0x1E, 0x1E, 0x2E)
FG = (0xCD, 0xD6, 0xF4)
CORNER = 114.0

# The Z: top bar, diagonal, bottom bar. Counter-clockwise, closed.
Z = [
    (116, 108), (396, 108), (396, 176), (230, 336),
    (396, 336), (396, 404), (116, 404), (116, 336),
    (282, 176), (116, 176),
]
# Stencil bridges: one slot through each bar, both on the same axis so the
# letter still reads as one stroke.
BRIDGES = [(248, 98, 22, 88), (248, 326, 22, 88)]
# Below this the bridges are thinner than a pixel and only smear the bars.
BRIDGE_MIN_SIZE = 32


def rounded_rect(x, y, w, h, r, steps=16):
    pts = []
    for cx, cy, start in (
        (x + w - r, y + h - r, 0.0),
        (x + r, y + h - r, 90.0),
        (x + r, y + r, 180.0),
        (x + w - r, y + r, 270.0),
    ):
        for i in range(steps + 1):
            a = math.radians(start + 90.0 * i / steps)
            pts.append((cx + r * math.cos(a), cy + r * math.sin(a)))
    return pts


def rect(x, y, w, h):
    return [(x, y), (x + w, y), (x + w, y + h), (x, y + h)]


def coverage(poly, size, scale, ss=4):
    """Even-odd scanline fill with `ss` subsamples per axis, as coverage 0..1."""
    edges = []
    for (x0, y0), (x1, y1) in zip(poly, poly[1:] + poly[:1]):
        x0, y0, x1, y1 = x0 * scale, y0 * scale, x1 * scale, y1 * scale
        if y0 != y1:
            edges.append((x0, y0, x1, y1))
    cov = [0.0] * (size * size)
    weight = 1.0 / ss
    for sub in range(size * ss):
        yc = (sub + 0.5) / ss
        xs = sorted(
            x0 + (yc - y0) * (x1 - x0) / (y1 - y0)
            for x0, y0, x1, y1 in edges
            if min(y0, y1) <= yc < max(y0, y1)
        )
        row = (sub // ss) * size
        for a, b in zip(xs[0::2], xs[1::2]):
            a, b = max(a, 0.0), min(b, float(size))
            if b <= a:
                continue
            first, last = int(a), min(int(b), size - 1)
            if first == last:
                cov[row + first] += (b - a) * weight
                continue
            cov[row + first] += (first + 1 - a) * weight
            for px in range(first + 1, last):
                cov[row + px] += weight
            cov[row + last] += (b - last) * weight
    return cov


def over(buf, cov, color, size):
    r, g, b = color
    for i, c in enumerate(cov):
        if c <= 0.0:
            continue
        c = min(c, 1.0)
        o = i * 4
        inv = 1.0 - c
        buf[o] = round(buf[o] * inv + r * c)
        buf[o + 1] = round(buf[o + 1] * inv + g * c)
        buf[o + 2] = round(buf[o + 2] * inv + b * c)
        buf[o + 3] = round(buf[o + 3] * inv + 255 * c)


def render(size):
    scale = size / GRID
    buf = bytearray(size * size * 4)
    over(buf, coverage(rounded_rect(0, 0, GRID, GRID, CORNER), size, scale), BG, size)
    over(buf, coverage(Z, size, scale), FG, size)
    if size >= BRIDGE_MIN_SIZE:
        for x, y, w, h in BRIDGES:
            over(buf, coverage(rect(x, y, w, h), size, scale), BG, size)
    return bytes(buf)


def png(path, rgba, size):
    raw = b"".join(
        b"\x00" + rgba[y * size * 4:(y + 1) * size * 4] for y in range(size)
    )

    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def svg():
    hexed = lambda c: "#%02x%02x%02x" % c
    z = " ".join(
        f"{'M' if i == 0 else 'L'} {x} {y}" for i, (x, y) in enumerate(Z)
    ) + " Z"
    cuts = "".join(
        f'\n  <rect x="{x}" y="{y}" width="{w}" height="{h}" fill="{hexed(BG)}"/>'
        for x, y, w, h in BRIDGES
    )
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512" '
        'width="512" height="512">\n'
        f'  <rect width="512" height="512" rx="{CORNER:.0f}" fill="{hexed(BG)}"/>\n'
        f'  <path d="{z}" fill="{hexed(FG)}"/>{cuts}\n'
        "</svg>\n"
    )


def main():
    here = pathlib.Path(__file__).parent
    (here / "zeughaus.svg").write_text(svg())
    # 256 is the window icon compiled into the editor, 64 the browser tab's
    # favicon. Nothing else is used, so nothing else is written.
    for size in (64, 256):
        png(here / f"zeughaus-{size}.png", render(size), size)
        print(f"zeughaus-{size}.png")


if __name__ == "__main__":
    main()
