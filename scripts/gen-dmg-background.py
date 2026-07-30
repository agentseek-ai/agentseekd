#!/usr/bin/env python3
"""Generate a DMG background image for the macOS installer.

Produces a polished 660×400 background with:
  - Dark purple-to-blue gradient (AgentSeek brand colors)
  - "AgentSeek" heading + tagline
  - Semi-transparent drag zone with "Drag to >>"

Icon positions (set in create-dmg):
  App icon:        (180, 170)  — center (244, 234)
  App drop link:   (480, 170)  — center (544, 234)
"""

from PIL import Image, ImageDraw, ImageFont, ImageFilter
import random
import sys
import os

W, H = 660, 400
OUTPUT = sys.argv[1] if len(sys.argv) > 1 else "dmg-background.png"

# Brand gradient (top-left → bottom-right)
GRADIENT_START = (45, 27, 105)    # #2d1b69 deep purple
GRADIENT_MID   = (26, 26, 46)     # #1a1a2e dark navy
GRADIENT_END   = (15, 52, 96)     # #0f3460 deep blue


def load_font(size, bold=False):
    candidates = [
        "/System/Library/Fonts/SFNS.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        "/System/Library/Fonts/SFNSMono.ttf",
        "/Library/Fonts/Arial.ttf",
    ]
    if bold:
        candidates = [
            "/System/Library/Fonts/SFNS-Bold.ttf",
            "/System/Library/Fonts/Helvetica.ttc",
        ] + candidates
    for path in candidates:
        try:
            return ImageFont.truetype(path, size)
        except (OSError, IOError):
            continue
    return ImageFont.load_default()


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))


def main():
    # 1. Gradient background
    img = Image.new('RGB', (W, H))
    px = img.load()
    for y in range(H):
        for x in range(W):
            t = (x / W + y / H) / 2
            if t < 0.5:
                color = lerp(GRADIENT_START, GRADIENT_MID, t * 2)
            else:
                color = lerp(GRADIENT_MID, GRADIENT_END, (t - 0.5) * 2)
            px[x, y] = color

    # 2. Subtle noise for texture
    random.seed(42)
    for _ in range(6000):
        x = random.randint(0, W - 1)
        y = random.randint(0, H - 1)
        r, g, b = px[x, y]
        n = random.randint(-8, 8)
        px[x, y] = (
            max(0, min(255, r + n)),
            max(0, min(255, g + n)),
            max(0, min(255, b + n)),
        )

    img = img.filter(ImageFilter.GaussianBlur(radius=0.3))
    draw = ImageDraw.Draw(img)

    # 3. Heading: "AgentSeek" — gradient text (purple→blue) with shadow
    font_title = load_font(42, bold=True)
    title = "AgentSeek"
    bbox = draw.textbbox((0, 0), title, font=font_title)
    tw, th = bbox[2] - bbox[0], bbox[3] - bbox[1]
    title_x = (W - tw) // 2
    title_y = 35

    # Drop shadow (dark blur behind text)
    shadow = Image.new('L', (tw + 8, th + 8), 0)
    sd = ImageDraw.Draw(shadow)
    sd.text((4 - bbox[0], 4 - bbox[1]), title, fill=255, font=font_title)
    shadow = shadow.filter(ImageFilter.GaussianBlur(radius=3))
    shadow_layer = Image.new('RGBA', (W, H), (0, 0, 0, 0))
    shadow_layer.paste((0, 0, 0, 120), (0, 0), shadow)
    img = Image.alpha_composite(img.convert('RGBA'), shadow_layer).convert('RGB')
    draw = ImageDraw.Draw(img)

    # Build gradient (purple #7C3AED → blue #3B82F6) clipped to text box
    grad_c0 = (124, 58, 237)   # purple
    grad_c1 = (59, 130, 246)   # blue
    gradient = Image.new('RGB', (tw, th))
    gd = ImageDraw.Draw(gradient)
    for gx in range(tw):
        gt = gx / max(tw - 1, 1)
        gc = lerp(grad_c0, grad_c1, gt)
        gd.line([(gx, 0), (gx, th)], fill=gc)

    mask = Image.new('L', (tw, th), 0)
    md = ImageDraw.Draw(mask)
    md.text((-bbox[0], -bbox[1]), title, fill=255, font=font_title)

    img.paste(gradient, (title_x, title_y - bbox[1]), mask)
    draw = ImageDraw.Draw(img)

    # 4. Tagline below heading
    font_tag = load_font(16)
    tagline = "Deploy  ·  Run  ·  Manage"
    bbox_t = draw.textbbox((0, 0), tagline, font=font_tag)
    tw2 = bbox_t[2] - bbox_t[0]
    draw.text(
        ((W - tw2) // 2, title_y + th + 10 - bbox_t[1]),
        tagline,
        fill=(220, 220, 240),
        font=font_tag,
    )

    # 5. Semi-transparent drag zone — tighter, more visible
    #    Icons at x=180, x=480 (centers), y=170. Icon 128px + label ≈ 155px tall.
    overlay = Image.new('RGBA', (W, H), (0, 0, 0, 0))
    od = ImageDraw.Draw(overlay)
    od.rounded_rectangle(
        [85, 120, 575, 300],
        radius=18,
        fill=(255, 255, 255, 40),
    )
    od.rounded_rectangle(
        [85, 120, 575, 300],
        radius=18,
        outline=(255, 255, 255, 60),
        width=1,
    )
    img = Image.alpha_composite(img.convert('RGBA'), overlay).convert('RGB')
    draw = ImageDraw.Draw(img)

    # 6. ">>" — gradient text (purple→blue), centered between icons
    #    Icon centers: x=180 and x=480 → midpoint x=330
    #    Icon center y=170 → text centered at y=170
    font_arrow = load_font(44, bold=True)
    arrows = ">>"

    bbox_a = draw.textbbox((0, 0), arrows, font=font_arrow)
    aw, ah = bbox_a[2] - bbox_a[0], bbox_a[3] - bbox_a[1]

    mid_x = 330
    center_y = 190
    ax = mid_x - aw // 2
    ay = center_y - ah // 2

    # Gradient fill for ">>"
    grad2 = Image.new('RGB', (aw, ah))
    g2d = ImageDraw.Draw(grad2)
    for gx in range(aw):
        gt = gx / max(aw - 1, 1)
        gc = lerp(grad_c0, grad_c1, gt)
        g2d.line([(gx, 0), (gx, ah)], fill=gc)

    mask2 = Image.new('L', (aw, ah), 0)
    m2d = ImageDraw.Draw(mask2)
    m2d.text((-bbox_a[0], -bbox_a[1]), arrows, fill=255, font=font_arrow)

    img.paste(grad2, (ax - bbox_a[0], ay - bbox_a[1]), mask2)
    draw = ImageDraw.Draw(img)

    # 7. Save
    os.makedirs(os.path.dirname(OUTPUT) or '.', exist_ok=True)
    img.save(OUTPUT, 'PNG')
    print(f"Generated: {OUTPUT} ({W}x{H})")


if __name__ == '__main__':
    main()
