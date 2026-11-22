#!/usr/bin/env python3
"""Generate placeholder PWA icons (stdlib only, no external deps).

Replace web/icons/*.png with real artwork whenever it exists; the manifest and
index.html already point at these filenames.
"""
import pathlib
import struct
import zlib

BG = (17, 17, 27, 255)      # #11111b
FG = (137, 180, 250, 255)   # #89b4fa

OUT = pathlib.Path(__file__).resolve().parent.parent / "web" / "icons"


def write_png(path: pathlib.Path, size: int, rgba: bytes) -> None:
    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data))
        )

    raw = b"".join(
        b"\x00" + rgba[y * size * 4 : (y + 1) * size * 4] for y in range(size)
    )
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def make(size: int, fg_frac: float, name: str) -> None:
    """Dark square with a centered light square (fg_frac of the icon size)."""
    lo = int(size * (0.5 - fg_frac / 2))
    hi = int(size * (0.5 + fg_frac / 2))
    rows = []
    for y in range(size):
        row = bytearray()
        for x in range(size):
            row += bytes(FG if (lo <= x < hi and lo <= y < hi) else BG)
        rows.append(bytes(row))
    write_png(OUT / name, size, b"".join(rows))


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    make(192, 0.45, "icon-192.png")
    make(512, 0.45, "icon-512.png")
    # Maskable icons must keep content inside the central ~80% safe zone.
    make(512, 0.40, "maskable-512.png")
    make(180, 0.45, "apple-touch-icon.png")
    for p in sorted(OUT.glob("*.png")):
        print(f"wrote {p}")


if __name__ == "__main__":
    main()
