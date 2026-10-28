"""Regenerates the libjpeg-encoded fixtures (needs Pillow and ffmpeg).

Each `<name>.jpg` is written by Pillow's libjpeg encoder and `<name>.rgb`
holds libjpeg's own decode of it (raw RGB888, row-major) as the reference
for tests/fixtures.rs. libjpeg upsamples chroma with a triangle filter, so
for the subsampled files `<name>.rep.rgb` is a second reference that
replicates chroma like mw-jpeg: ffmpeg decodes the raw Y, Cb and Cr planes
(its own IDCT, no colour conversion) and this script replicates the chroma
and converts with libjpeg's fixed-point constants. `fuel.jpg` is the
camera-like frame embedded by examples/fuel_probe.rs.
"""
import io
import math
import random
import subprocess
import sys
from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent


def scene(w, h, noise, seed):
    rnd = random.Random(seed)
    im = Image.new("RGB", (w, h))
    px = im.load()
    for y in range(h):
        for x in range(w):
            r = int(128 + 100 * math.sin(x / (w / 3.1)))
            g = int(255 * y / max(h - 1, 1))
            b = int(128 + 90 * math.cos((x + y) / (w / 4.3)))
            n = rnd.randint(-noise, noise) if noise else 0
            px[x, y] = tuple(max(0, min(255, v + n)) for v in (r, g, b))
    d = ImageDraw.Draw(im)
    d.ellipse((w // 5, h // 4, w // 2, h * 3 // 4), fill=(230, 40, 50))
    d.rectangle((w * 3 // 5, h // 6, w * 9 // 10, h // 2), fill=(20, 60, 200))
    d.line((0, h - 1, w - 1, 0), fill=(250, 250, 240), width=max(1, w // 40))
    return im


def clamp(v):
    return max(0, min(255, v))


def replicated(path, w, h, hs, vs):
    """ffmpeg's raw planes of `path`, chroma replicated, as RGB888."""
    fmt = {(2, 2): "yuvj420p", (2, 1): "yuvj422p"}[(hs, vs)]
    raw = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", str(path), "-f", "rawvideo",
         "-pix_fmt", fmt, "-"], check=True, capture_output=True).stdout
    cw, ch = -(-w // hs), -(-h // vs)
    y, cb, cr = raw[:w * h], raw[w * h:w * h + cw * ch], raw[w * h + cw * ch:]
    out = bytearray()
    for j in range(h):
        for i in range(w):
            yy = y[j * w + i]
            b = cb[(j // vs) * cw + i // hs] - 128
            r = cr[(j // vs) * cw + i // hs] - 128
            half = 1 << 15
            out += bytes((clamp(yy + ((91881 * r + half) >> 16)),
                          clamp(yy + ((-22554 * b - 46802 * r + half) >> 16)),
                          clamp(yy + ((116130 * b + half) >> 16))))
    return bytes(out)


def save(name, im, **kw):
    buf = io.BytesIO()
    im.save(buf, "JPEG", **kw)
    data = buf.getvalue()
    (HERE / f"{name}.jpg").write_bytes(data)
    if name not in ("fuel", "pil_progressive"):
        ref = Image.open(io.BytesIO(data)).convert("RGB")
        (HERE / f"{name}.rgb").write_bytes(ref.tobytes())
        sampling = {1: (2, 1), 2: (2, 2)}.get(kw.get("subsampling"))
        if sampling and im.mode == "RGB":
            rep = replicated(HERE / f"{name}.jpg", *im.size, *sampling)
            (HERE / f"{name}.rep.rgb").write_bytes(rep)
    print(name, im.size, len(data), file=sys.stderr)


def main():
    smooth = scene(45, 31, 0, 1)
    save("pil_444", scene(37, 29, 6, 2), quality=90, subsampling=0)
    save("pil_422", smooth.resize((41, 23)), quality=85, subsampling=1)
    save("pil_420", smooth, quality=85, subsampling=2)
    save("pil_gray", scene(19, 13, 4, 3).convert("L"), quality=75)
    save("pil_420_rst", scene(50, 34, 0, 4), quality=80, subsampling=2,
         restart_marker_blocks=3)
    save("pil_progressive", smooth, quality=80, progressive=True)
    save("fuel", scene(240, 240, 10, 5), quality=80, subsampling=2)


if __name__ == "__main__":
    main()
