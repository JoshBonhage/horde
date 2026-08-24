#!/usr/bin/env python3
"""Render horde's wordmark to a transparent PNG for the README.

Artwork and colours are read straight out of `src/client/ui/logo.rs` and
`src/theme.rs`, so the image cannot drift from what the sidebar draws. Re-run it
after changing either.

    python3 scripts/render-logo.py            # SMALL, the everyday wordmark
    python3 scripts/render-logo.py BIG        # the tall ANSI-shadow one

Glyphs are drawn as rectangles rather than set in a font on purpose. Menlo puts
`█` 102px tall and `║` 127px at the same size, so no single row spacing makes
solid blocks and double-rule shadows meet; hand-drawing the handful of block and
box characters makes every join exact at any size.
"""
import re
import sys
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
LOGO = (ROOT / "src/client/ui/logo.rs").read_text()
THEME = (ROOT / "src/theme.rs").read_text()


def banner(name):
    block = re.search(name + r": Banner = Banner \{\s*rows: &\[(.*?)\],\s*\};", LOGO, re.S)
    return re.findall(r'"((?:[^"\\]|\\.)*)"', block.group(1))


def theme_rgb(name):
    m = re.search(name + r": rgb\(0x([0-9a-f]{2}), 0x([0-9a-f]{2}), 0x([0-9a-f]{2})\)", THEME)
    return tuple(int(g, 16) for g in m.groups())


def mix(a, b, t):
    """`theme::mix` — the fade logo.rs applies down the rows."""
    t = max(0.0, min(1.0, t))
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


# Two parallel rules of thickness T separated by GAP, centred in the cell.
T, GAP = 0.20, 0.18
x1, x2 = (1 - GAP) / 2 - T, (1 + GAP) / 2
y1, y2 = (1 - GAP) / 2 - T, (1 + GAP) / 2

# Each glyph as (x, y, w, h) rectangles in cell fractions.
GLYPHS = {
    "█": [(0, 0, 1, 1)],
    "▀": [(0, 0, 1, 0.5)],
    "▄": [(0, 0.5, 1, 0.5)],
    "═": [(0, y1, 1, T), (0, y2, 1, T)],
    "║": [(x1, 0, T, 1), (x2, 0, T, 1)],
    # Corners: the outer rule turns the whole way, the inner one stops short.
    "╗": [(0, y1, x2 + T, T), (0, y2, x1 + T, T), (x2, y1, T, 1 - y1), (x1, y2, T, 1 - y2)],
    "╔": [(x1, y1, 1 - x1, T), (x2, y2, 1 - x2, T), (x1, y1, T, 1 - y1), (x2, y2, T, 1 - y2)],
    "╝": [(0, y2, x2 + T, T), (0, y1, x1 + T, T), (x2, 0, T, y2 + T), (x1, 0, T, y1 + T)],
    "╚": [(x1, y2, 1 - x1, T), (x2, y1, 1 - x2, T), (x1, 0, T, y2 + T), (x2, 0, T, y1 + T)],
    " ": [],
}


def render(rows, out, cell_w=54, aspect=2.0, pad=10, ss=4):
    accent, panel = theme_rgb("accent"), theme_rgb("panel_bg")
    cw, ch = cell_w * ss, cell_w * aspect * ss
    img = Image.new("RGBA", (int(cw * len(rows[0])), int(ch * len(rows))), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    last = max(len(rows) - 1, 1)
    for i, row in enumerate(rows):
        colour = mix(accent, panel, 0.3 * (i / last)) + (255,)
        for j, char in enumerate(row):
            for gx, gy, gw, gh in GLYPHS.get(char, []):
                x, y = j * cw + gx * cw, i * ch + gy * ch
                draw.rectangle([x, y, x + gw * cw, y + gh * ch], fill=colour)

    img = img.crop(img.getbbox())
    img = img.resize((img.width // ss, img.height // ss), Image.LANCZOS)
    canvas = Image.new("RGBA", (img.width + pad * 2, img.height + pad * 2), (0, 0, 0, 0))
    canvas.alpha_composite(img, (pad, pad))
    canvas.save(out)
    print(f"{out}  {canvas.width}x{canvas.height}")


if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "SMALL"
    render(banner(which), ROOT / "assets/logo.png")
