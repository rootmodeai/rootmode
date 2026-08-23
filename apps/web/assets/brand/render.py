#!/usr/bin/env python3
"""Canonical rootmode marks: X avatar, banner, press variants, OG.

Void #0d0f11 · Paper #e8eaed · Violet #7433f7
Glider cells: (1,0) (2,1) (0,2) (1,2) (2,2)
"""

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont, ImageFilter

GLIDER = [(1, 0), (2, 1), (0, 2), (1, 2), (2, 2)]
VOID = (13, 15, 17, 255)  # #0d0f11
PAPER = (232, 234, 237, 255)  # #e8eaed
VIOLET = (116, 51, 247)  # #7433f7
WHITE = (255, 255, 255, 255)

HERE = Path(__file__).resolve().parent
WEB_ASSETS = HERE.parent
DESKTOP = Path("/Users/chris/Desktop")


def rounded_cell(draw, x, y, s, r, fill):
    draw.rounded_rectangle((x, y, x + s, y + s), radius=r, fill=fill)


def paint_glider(img, cx, cy, cell, gap, fill, glow=False):
    pitch = cell + gap
    cols = [c for c, _ in GLIDER]
    rows = [r for _, r in GLIDER]
    min_c, max_c = min(cols), max(cols)
    min_r, max_r = min(rows), max(rows)
    span_w = (max_c - min_c + 1) * cell + (max_c - min_c) * gap
    span_h = (max_r - min_r + 1) * cell + (max_r - min_r) * gap
    ox = cx - span_w / 2 - min_c * pitch
    oy = cy - span_h / 2 - min_r * pitch
    if glow:
        halo = Image.new("RGBA", img.size, (0, 0, 0, 0))
        hd = ImageDraw.Draw(halo)
        pad = int(cell * 0.55)
        for c, r in GLIDER:
            x = ox + c * pitch - pad
            y = oy + r * pitch - pad
            hd.rounded_rectangle(
                (x, y, x + cell + pad * 2, y + cell + pad * 2),
                radius=int((cell + pad * 2) * 0.28),
                fill=(*VIOLET, 70),
            )
        halo = halo.filter(ImageFilter.GaussianBlur(radius=cell * 0.7))
        img.alpha_composite(halo)
    d = ImageDraw.Draw(img)
    for c, r in GLIDER:
        rounded_cell(
            d,
            ox + c * pitch,
            oy + r * pitch,
            cell,
            max(4, int(cell * 0.18)),
            fill,
        )


def load_font(px, face="bold"):
    futura = "/System/Library/Fonts/Supplemental/Futura.ttc"
    index = {"bold": 2, "medium": 0, "condensed": 3}.get(face, 2)
    try:
        return ImageFont.truetype(futura, px, index=index)
    except OSError:
        try:
            return ImageFont.truetype("/System/Library/Fonts/SFNS.ttf", px)
        except OSError:
            return ImageFont.load_default()


def draw_cx(draw, cx, y, text, font, fill):
    x0, _, x1, _ = draw.textbbox((0, 0), text, font=font)
    draw.text((cx - (x1 - x0) / 2, y), text, font=font, fill=fill)


def purple_diagonal(w, h):
    import math

    side = int(math.hypot(w, h)) + 8
    band = Image.linear_gradient("L").resize((side, side), Image.Resampling.BILINEAR)
    angle = math.degrees(math.atan2(w, -h))
    rotated = band.rotate(angle, resample=Image.Resampling.BICUBIC, expand=True, fillcolor=0)
    rw, rh = rotated.size
    mask = rotated.crop(((rw - w) // 2, (rh - h) // 2, (rw - w) // 2 + w, (rh - h) // 2 + h))
    start, peak = 0.42, 132
    table = []
    for p in range(256):
        t = p / 255.0
        if t <= start:
            table.append(0)
        else:
            u = (t - start) / (1.0 - start)
            u = u * u * (3.0 - 2.0 * u)
            table.append(int(u * peak))
    alpha = mask.point(table)
    wash = Image.new("RGBA", (w, h), (*VIOLET, 255))
    wash.putalpha(alpha)
    return wash.filter(ImageFilter.GaussianBlur(radius=max(8, int(h * 0.06))))


def avatar():
    scale = 2
    size = 800 * scale
    img = Image.new("RGBA", (size, size), VOID)
    wash = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    wd = ImageDraw.Draw(wash)
    m = size // 2
    wd.ellipse(
        (m - size * 0.48, m - size * 0.48, m + size * 0.48, m + size * 0.48),
        fill=(*VIOLET, 70),
    )
    wash = wash.filter(ImageFilter.GaussianBlur(radius=size * 0.2))
    img.alpha_composite(wash)
    paint_glider(img, size / 2, size / 2, cell=size * 0.145, gap=size * 0.038, fill=PAPER, glow=True)
    return img.resize((800, 800), Image.Resampling.LANCZOS).convert("RGB")


def banner():
    scale = 2
    w, h = 1500 * scale, 500 * scale
    img = Image.new("RGBA", (w, h), VOID)
    img.alpha_composite(purple_diagonal(w, h))
    cx = w * 0.68
    paint_glider(img, cx, h * 0.28, cell=h * 0.105, gap=h * 0.027, fill=PAPER, glow=True)
    d = ImageDraw.Draw(img)
    name = load_font(int(h * 0.115), "bold")
    line = load_font(int(h * 0.078), "medium")
    draw_cx(d, cx, h * 0.52, "rootmode", name, PAPER)
    tag = (*PAPER[:3], 230)
    draw_cx(d, cx, h * 0.68, "Decentralised AI", line, tag)
    draw_cx(d, cx, h * 0.80, "Inference Network", line, tag)
    return img.resize((1500, 500), Image.Resampling.LANCZOS).convert("RGB")


def mark(bg, fill, glow=False):
    scale = 2
    size = 800 * scale
    img = Image.new("RGBA", (size, size), bg)
    paint_glider(img, size / 2, size / 2, cell=size * 0.145, gap=size * 0.038, fill=fill, glow=glow)
    return img.resize((800, 800), Image.Resampling.LANCZOS).convert("RGB")


def og():
    scale = 2
    w, h = 1200 * scale, 630 * scale
    img = Image.new("RGBA", (w, h), VOID)
    img.alpha_composite(purple_diagonal(w, h))
    cx = w * 0.62
    paint_glider(img, cx, h * 0.32, cell=h * 0.095, gap=h * 0.024, fill=PAPER, glow=True)
    d = ImageDraw.Draw(img)
    name = load_font(int(h * 0.10), "bold")
    line = load_font(int(h * 0.055), "medium")
    draw_cx(d, cx, h * 0.54, "rootmode", name, PAPER)
    tag = (*PAPER[:3], 230)
    draw_cx(d, cx, h * 0.70, "Decentralised AI Inference Network", line, tag)
    return img.resize((1200, 630), Image.Resampling.LANCZOS).convert("RGB")


def main():
    HERE.mkdir(parents=True, exist_ok=True)
    av = avatar()
    bn = banner()
    av.save(HERE / "profile.png", "PNG", optimize=True)
    bn.save(HERE / "banner.png", "PNG", optimize=True)
    av.save(DESKTOP / "rootmode-x-profile.png", "PNG", optimize=True)
    bn.save(DESKTOP / "rootmode-x-banner.png", "PNG", optimize=True)
    mark(VOID, PAPER, glow=True).save(HERE / "mark-void.png", "PNG", optimize=True)
    mark((*VIOLET, 255), WHITE).save(HERE / "mark-violet.png", "PNG", optimize=True)
    mark(WHITE, VOID).save(HERE / "mark-paper.png", "PNG", optimize=True)
    og().save(WEB_ASSETS / "og.png", "PNG", optimize=True)
    print("wrote", HERE)
    print("wrote", WEB_ASSETS / "og.png")


if __name__ == "__main__":
    main()
