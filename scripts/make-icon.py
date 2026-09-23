"""Draws assets/icon.png (1024×1024): a photo that turns from grey to colour, i.e. a preset
being applied. Run once; the bundle script turns it into AppIcon.icns."""
from PIL import Image, ImageDraw, ImageFilter

S = 2048  # drawn at 2× and downsampled for smooth edges
img = Image.new("RGBA", (S, S), (0, 0, 0, 0))


def lerp(a, b, t):
    return tuple(int(x + (y - x) * t) for x, y in zip(a, b))


def rounded_mask(size, box, radius):
    m = Image.new("L", size, 0)
    ImageDraw.Draw(m).rounded_rectangle(box, radius=radius, fill=255)
    return m


# Background: macOS-style rounded square with a purple vertical gradient.
margin, radius = 100, 460
bg = Image.new("RGBA", (S, S))
top, bottom = (170, 125, 255), (58, 36, 120)
for y in range(S):
    ImageDraw.Draw(bg).line([(0, y), (S, y)], fill=lerp(top, bottom, y / S) + (255,))
img.paste(bg, (0, 0), rounded_mask((S, S), (margin, margin, S - margin, S - margin), radius))

# The photo: a landscape (sky, sun, hills) in a white frame, with a soft shadow.
px0, py0, px1, py1 = 420, 520, 1628, 1528
shadow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
ImageDraw.Draw(shadow).rounded_rectangle((px0 - 30, py0 + 10, px1 + 30, py1 + 70), radius=90, fill=(15, 8, 40, 150))
img.alpha_composite(shadow.filter(ImageFilter.GaussianBlur(40)))
frame = Image.new("RGBA", (S, S), (0, 0, 0, 0))
ImageDraw.Draw(frame).rounded_rectangle((px0 - 40, py0 - 40, px1 + 40, py1 + 40), radius=90, fill=(246, 244, 250, 255))
img.alpha_composite(frame)

scene = Image.new("RGBA", (px1 - px0, py1 - py0))
d = ImageDraw.Draw(scene)
w, h = scene.size
for y in range(h):
    d.line([(0, y), (w, y)], fill=lerp((255, 186, 120), (255, 110, 150), y / h) + (255,))
d.ellipse((w * 0.58, h * 0.16, w * 0.80, h * 0.38), fill=(255, 238, 180, 255))
d.polygon([(0, h), (0, h * 0.62), (w * 0.28, h * 0.40), (w * 0.55, h * 0.66), (w * 0.55, h)], fill=(120, 60, 150, 255))
d.polygon([(w * 0.30, h), (w * 0.62, h * 0.50), (w, h * 0.72), (w, h)], fill=(70, 34, 110, 255))

# Left half as the unedited original: desaturated and flat. Split on a diagonal.
gray = scene.convert("L").point(lambda v: 90 + v * 0.45).convert("RGBA")
split = Image.new("L", scene.size, 0)
ImageDraw.Draw(split).polygon([(0, 0), (w * 0.42, 0), (w * 0.58, h), (0, h)], fill=255)
scene = Image.composite(gray, scene, split)
ImageDraw.Draw(scene).line([(w * 0.42, 0), (w * 0.58, h)], fill=(255, 255, 255, 255), width=18)
img.paste(scene, (px0, py0), rounded_mask(scene.size, (0, 0, w, h), 60))

img.resize((1024, 1024), Image.LANCZOS).save("assets/icon.png")
print("wrote assets/icon.png")
