"""Regenerates the camera-size JPEG fixtures of cam_jpeg_view's tests
(needs Pillow; run with `python3 -I testdata/gen.py`).

OV2640 frame sizes the badge may be sent, as baseline JPEGs written by
Pillow's libjpeg encoder at quality 60:

* `s240x176.jpg`: HQVGA, 4:2:0 (11 MCU rows of 16).
* `s160x120.jpg`: QQVGA, 4:2:0 (7.5 MCU rows: the last strip has 8 rows).
* `s200x150.jpg`: 4:2:2 like the OV2640's own encoder (8-row MCUs), width
  not a multiple of the MCU width, height not of the MCU height.
* `s320x240.jpg`: QVGA, wider than the panel (refused).
"""
import io
import math
import sys
from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent


def scene(w, h):
    im = Image.new("RGB", (w, h))
    px = im.load()
    for y in range(h):
        for x in range(w):
            r = int(128 + 100 * math.sin(x / (w / 3.1)))
            g = int(40 + 200 * y / max(h - 1, 1))
            b = int(128 + 90 * math.cos((x + y) / (w / 4.3)))
            px[x, y] = (r, g, b)
    d = ImageDraw.Draw(im)
    d.ellipse((w // 5, h // 4, w // 2, h * 3 // 4), fill=(230, 40, 50))
    d.rectangle((w * 3 // 5, h // 6, w * 9 // 10, h // 2), fill=(20, 60, 200))
    # A bright frame on the edge pixels, so a misplaced window shows.
    d.rectangle((0, 0, w - 1, h - 1), outline=(250, 250, 240))
    return im


for name, (w, h), sub in [
    ("s240x176", (240, 176), 2),
    ("s160x120", (160, 120), 2),
    ("s200x150", (200, 150), 1),
    ("s320x240", (320, 240), 2),
]:
    buf = io.BytesIO()
    scene(w, h).save(buf, "JPEG", quality=60, subsampling=sub)
    (HERE / f"{name}.jpg").write_bytes(buf.getvalue())
    print(name, (w, h), len(buf.getvalue()), file=sys.stderr)
