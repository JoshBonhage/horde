#!/usr/bin/env python3
"""Render horde's wordmark to a transparent PNG for the README.

Artwork and colours are read straight out of `src/client/ui/logo.rs` and
`src/theme.rs`, so the image cannot drift from what the sidebar draws. Re-run it
after changing either.

    python3 scripts/render-logo.py            # SMALL, the everyday wordmark
    python3 scripts/render-logo.py BIG        # the tall ANSI-shadow one
    python3 scripts/render-logo.py --plain    # wordmark only, no figure

The figure beside it is the greeter's zombie, whose bitmap and palette are
transcribed from horde-full's `src/client/ui/zombie.rs`. Its colours are derived
from the theme by the same rules as there, so it is a rotting version of horde's
own palette rather than a green sticker.

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
    # \b so `bg` cannot match inside `panel_bg`.
    pattern = r"\b" + name + r": rgb\(0x([0-9a-f]{2}), 0x([0-9a-f]{2}), 0x([0-9a-f]{2})\)"
    m = re.search(pattern, THEME)
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


# The greeter's standing pose, from horde-full src/client/ui/zombie.rs (TALL, frame 0).
# One character per pixel; `.` is transparent. The arm reaches right, toward the letters.
ZOMBIE = [
    "..hhhhh.......",
    ".hh####%......",
    ".h#####%......",
    ".#xx%xx%......",
    ".%%x%%e%......",
    ".#%%x##%......",
    "..oxoxo%......",
    "...%##%.......",
    "..CCCccc%CCC%.",
    "..cCcccc%%##b#",
    "..cCoooo%.....",
    "..cCxxxx%%%#%.",
    "..cCoooo%b###b",
    "..cCCxxc%.....",
    "...ccccc......",
    "...cc.cc%.....",
    "...##..cc%....",
    "...##...##....",
    "..o##...%%....",
    ".ooo....CC....",
]


def zombie_palette():
    """`zombie::palette`, in Python. Same derivation, so the figure matches the theme."""
    ui = lambda n: theme_rgb(n)
    bg, text, faint = ui("bg"), ui("text"), ui("text_faint")
    ok, warn, err, border = ui("ok"), ui("warn"), ui("error"), ui("border")
    lum = lambda c: (0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]) / 255
    lit, dark = (bg, text) if lum(bg) > lum(text) else (text, bg)
    ink = dark if lum(bg) > 0.5 else lit
    skin = mix(mix(ok, ink, 0.3), bg, 0.2)
    cloth = mix(faint, ink, 0.2)
    return {
        "#": skin,
        "%": mix(skin, dark, 0.45),
        "o": mix(ink, warn, 0.3),
        "x": mix(mix(dark, (0, 0, 0), 0.35), skin, 0.12),
        "c": cloth,
        "C": mix(cloth, dark, 0.5),
        "b": mix(err, dark, 0.3),
        "e": mix(warn, ink, 0.2),
        "h": mix(border, ink, 0.25),
    }


def draw_zombie(px):
    """The figure at `px` pixels per bitmap pixel, on transparency."""
    pal = zombie_palette()
    img = Image.new("RGBA", (len(ZOMBIE[0]) * px, len(ZOMBIE) * px), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    for y, row in enumerate(ZOMBIE):
        for x, ch in enumerate(row):
            if ch in pal:
                d.rectangle([x * px, y * px, (x + 1) * px - 1, (y + 1) * px - 1],
                            fill=pal[ch] + (255,))
    return img


def wordmark(rows, cell_w=54, aspect=2.0, ss=4):
    """The banner, cropped to its ink, on transparency."""
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
    return img.resize((img.width // ss, img.height // ss), Image.LANCZOS)


def render(rows, out, with_zombie=True, pad=10, gap=44, loom=1.04):
    word = wordmark(rows)

    if not with_zombie:
        art = word
    else:
        # Sized off the wordmark so the pair holds together at any banner size, and a touch
        # taller so the figure looms rather than lining up like a sixth letter.
        px = max(1, round(word.height * loom / len(ZOMBIE)))
        fig = draw_zombie(px)
        art = Image.new("RGBA", (fig.width + gap + word.width, max(fig.height, word.height)),
                        (0, 0, 0, 0))
        # Bottom-aligned: both stand on the same ground line.
        art.alpha_composite(fig, (0, art.height - fig.height))
        art.alpha_composite(word, (fig.width + gap, art.height - word.height))
        art = art.crop(art.getbbox())

    canvas = Image.new("RGBA", (art.width + pad * 2, art.height + pad * 2), (0, 0, 0, 0))
    canvas.alpha_composite(art, (pad, pad))
    canvas.save(out)
    print(f"{out}  {canvas.width}x{canvas.height}")


if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    render(banner(args[0] if args else "SMALL"),
           ROOT / "assets/logo.png",
           with_zombie="--plain" not in sys.argv)
