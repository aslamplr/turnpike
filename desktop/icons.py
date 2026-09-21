#!/usr/bin/env python3
"""Generate the desktop app's icons.

Dev-only asset generation, run once. Needs Pillow, plus `sips` and `iconutil`
(both ship with macOS) for the .icns.

  python3 desktop/icons.py
"""

import os
import shutil
import subprocess
import sys

from PIL import Image, ImageDraw

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "src-tauri", "icons")

# A deep indigo ground with a cool light mark. Deliberately not a brand: this is
# an internal dev build, and the icon only has to be legible at 32px.
BG = (23, 27, 48, 255)
FG = (236, 241, 255, 255)

MASTER = 1024


def rounded_square(size, radius_ratio=0.225):
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    r = max(1, int(size * radius_ratio))
    d.rounded_rectangle([0, 0, size - 1, size - 1], radius=r, fill=BG)
    return img


def draw_chevrons(draw, size, color, stroke):
    """Two right-pointing chevrons: a gateway forwarding a request."""
    top, bot = size * 0.30, size * 0.70
    mid = size * 0.50
    reach = size * 0.135
    for x in (size * 0.30, size * 0.545):
        draw.polygon(
            [
                (x, top),
                (x + stroke, top),
                (x + stroke + reach, mid),
                (x + stroke, bot),
                (x, bot),
                (x + reach, mid),
            ],
            fill=color,
        )


def app_icon(size):
    img = rounded_square(size)
    draw_chevrons(ImageDraw.Draw(img), size, FG, size * 0.105)
    return img


def tray_icon(size):
    """Monochrome on transparent, for `icon_as_template(true)`.

    macOS uses the alpha channel as a mask, so only the shape matters — the color
    here is irrelevant, and black keeps it readable in any image viewer.
    """
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw_chevrons(ImageDraw.Draw(img), size, (0, 0, 0, 255), size * 0.115)
    return img


def main():
    os.makedirs(OUT, exist_ok=True)

    master = app_icon(MASTER)
    master.save(os.path.join(OUT, "icon.png"))

    for name, size in [("32x32.png", 32), ("128x128.png", 128), ("128x128@2x.png", 256)]:
        app_icon(size).save(os.path.join(OUT, name))

    tray_icon(44).save(os.path.join(OUT, "tray.png"))

    # .ico carries its own size list.
    app_icon(256).save(
        os.path.join(OUT, "icon.ico"),
        sizes=[(16, 16), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )

    # .icns has to go through an .iconset + iconutil.
    if shutil.which("iconutil") and shutil.which("sips"):
        iconset = os.path.join(OUT, "icon.iconset")
        shutil.rmtree(iconset, ignore_errors=True)
        os.makedirs(iconset)
        for base in (16, 32, 128, 256, 512):
            app_icon(base).save(os.path.join(iconset, f"icon_{base}x{base}.png"))
            app_icon(base * 2).save(os.path.join(iconset, f"icon_{base}x{base}@2x.png"))
        subprocess.run(
            ["iconutil", "-c", "icns", iconset, "-o", os.path.join(OUT, "icon.icns")],
            check=True,
        )
        shutil.rmtree(iconset, ignore_errors=True)
    else:
        print("iconutil/sips unavailable — skipped icon.icns", file=sys.stderr)

    print(f"wrote icons to {OUT}")


if __name__ == "__main__":
    main()
