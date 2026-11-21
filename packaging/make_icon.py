#!/usr/bin/env python3
"""Render Mercury's icon (accent planet, orbit ring, highlight) at 256px."""
import sys
from PIL import Image, ImageDraw

S = 256 * 4  # supersample, then downscale for smooth edges
img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
d = ImageDraw.Draw(img)
c = S / 2
# rounded-square backdrop
d.rounded_rectangle([S * 0.04, S * 0.04, S * 0.96, S * 0.96], radius=S * 0.22, fill=(0x12, 0x15, 0x1a, 255))
r_ring = S * 0.33
d.ellipse([c - r_ring, c - r_ring, c + r_ring, c + r_ring], outline=(0x8a, 0x94, 0xa3, 255), width=int(S * 0.025))
r = S * 0.22
d.ellipse([c - r, c - r, c + r, c + r], fill=(0x5e, 0xc8, 0xd8, 255))
h = S * 0.065
hx, hy = c - S * 0.075, c - S * 0.075
d.ellipse([hx - h, hy - h, hx + h, hy + h], fill=(230, 248, 250, 230))
img.resize((256, 256), Image.LANCZOS).save(sys.argv[1] if len(sys.argv) > 1 else "mercury.png")
