#!/usr/bin/env python3
"""Render the PWA icon set from the committed artwork.

The source is `scripts/zapslab-icon-64.png`, a 64x64 pixel-art crate. Icons are
upscaled with nearest-neighbour sampling so the hard pixel edges stay crisp;
non-integer multiples (the 180px Apple icon) are rendered at the next integer
scale and then resampled down so no row of pixels is wider than its neighbour.

Every icon is flattened onto the theme background (#11111b, the manifest's
`background_color`) and saved as an opaque RGB PNG: iOS paints transparent
pixels black, and an opaque icon looks the same in every launcher instead of
picking up whatever backdrop the OS uses.

Requires Pillow (`python3 -m pip install --user pillow`).
"""
import math
import pathlib
import sys

from PIL import Image

ROOT = pathlib.Path(__file__).resolve().parent.parent
SRC = ROOT / "scripts" / "zapslab-icon-64.png"
OUT = ROOT / "web" / "icons"

BACKGROUND = (17, 17, 27, 255)  # #11111b, matches manifest background_color
# Maskable icons must keep their content inside the central 80% safe circle.
MASKABLE_SAFE_FRACTION = 0.8


def load_source() -> Image.Image:
    if not SRC.exists():
        sys.exit(f"missing source artwork: {SRC}")
    img = Image.open(SRC).convert("RGBA")
    if img.width != img.height:
        sys.exit(f"source artwork must be square, got {img.width}x{img.height}")
    return img


def _on_background(mark: Image.Image, size: int) -> Image.Image:
    canvas = Image.new("RGBA", (size, size), BACKGROUND)
    canvas.alpha_composite(
        mark, ((size - mark.width) // 2, (size - mark.height) // 2)
    )
    return canvas


def icon(src: Image.Image, size: int) -> Image.Image:
    """Full-bleed icon: the artwork fills the canvas, bar its own margins."""
    # Integer scale up, then (only when the size is not a multiple) downsample
    # to the exact size. Upscaling straight to a fractional size with NEAREST
    # would give neighbouring pixels different widths.
    scale = max(1, math.ceil(size / src.width))
    mark = src.resize((src.width * scale, src.height * scale), Image.NEAREST)
    if mark.width != size:
        mark = mark.resize((size, size), Image.LANCZOS)
    return _on_background(mark, size)


def maskable(src: Image.Image, size: int) -> Image.Image:
    """Opaque, full-bleed icon with the artwork inside the safe circle."""
    # Fit the mark's bounding box inside the safe circle: the farthest pixel
    # from the centre is a bbox corner, at half its diagonal.
    left, top, right, bottom = src.getbbox()
    half_diagonal = math.hypot(right - left, bottom - top) / 2
    radius = size * MASKABLE_SAFE_FRACTION / 2
    scale = max(1, int(radius / half_diagonal))
    mark = src.resize((src.width * scale, src.height * scale), Image.NEAREST)
    return _on_background(mark, size)


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    src = load_source()
    icons = {
        "icon-192.png": icon(src, 192),
        "icon-512.png": icon(src, 512),
        "maskable-512.png": maskable(src, 512),
        "apple-touch-icon.png": icon(src, 180),
    }
    for name, img in icons.items():
        path = OUT / name
        img.convert("RGB").save(path, "PNG", optimize=True)
        print(f"wrote {path.relative_to(ROOT)} ({img.width}x{img.height})")


if __name__ == "__main__":
    main()
