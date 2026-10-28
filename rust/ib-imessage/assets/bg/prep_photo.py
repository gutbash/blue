# Prepare a photo for an iOS chat background (ib-imessage set_background).
#
# Usage: python -I prep_photo.py <source image> <out dir>
# Writes <out>/layer.heic (1292x2796, the captured template's portrait layer)
# and <out>/watch.png (820x1003 RGBA, the watch preview), and prints JSON
# {"luminance", "clock_luminance", "is_high_key"} on stdout.
#
# Needs Pillow and pillow-heif. The derived values approximate Apple's: the
# template's luminance (0.132) and clock-area luminance (0.637) were not
# reproduced exactly by any standard formula, so these are best fits.
import json
import os
import sys

from PIL import Image, ImageOps
import pillow_heif

pillow_heif.register_heif_opener()

LAYER = (1292, 2796)
WATCH = (820, 1003)
# Watch preview: about full layer width, from y=600 (best fit to the capture).
WATCH_CROP = (0, 600, 1290, 600 + 1578)
# Clock region in layer pixels (Contents.json timeFrame, bottom-left origin).
CLOCK = (177, 344, 1113, 618)


def lin(c):
    return c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4


def mean_lum(img, linear):
    raw = img.convert("RGB").resize((64, 64)).tobytes()
    total = 0.0
    for i in range(0, len(raw), 3):
        r, g, b = raw[i] / 255, raw[i + 1] / 255, raw[i + 2] / 255
        if linear:
            r, g, b = lin(r), lin(g), lin(b)
        total += 0.2126 * r + 0.7152 * g + 0.0722 * b
    return total / (64 * 64)


def main():
    src, out = sys.argv[1], sys.argv[2]
    im = Image.open(src)
    im = ImageOps.exif_transpose(im)
    icc = im.info.get("icc_profile")
    im = im.convert("RGB")
    layer = ImageOps.fit(im, LAYER, Image.LANCZOS, centering=(0.5, 0.5))
    os.makedirs(out, exist_ok=True)
    kw = {"quality": 85}
    if icc:
        kw["icc_profile"] = icc
    layer.save(os.path.join(out, "layer.heic"), format="HEIF", **kw)
    watch = layer.crop(WATCH_CROP).resize(WATCH, Image.LANCZOS).convert("RGBA")
    watch.save(os.path.join(out, "watch.png"), format="PNG")
    lum = mean_lum(watch, True)
    print(json.dumps({
        "luminance": lum,
        "clock_luminance": mean_lum(layer.crop(CLOCK), False),
        # [VERIFY] Apple's threshold is unknown.
        "is_high_key": lum > 0.5,
    }))


if __name__ == "__main__":
    main()
