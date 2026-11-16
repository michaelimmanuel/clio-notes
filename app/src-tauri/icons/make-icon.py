"""Draws the Clio mark (a C made of six stripes) and the 1024 px app icon.

Run from the repository root:

    python app/src-tauri/icons/make-icon.py
    cd app && npx tauri icon src-tauri/icons/icon-source.png

The first writes `icon-source.png` and `mark.svg` (the mark alone, in
`currentColor`, also inlined in `app/index.html`); the second makes every
icon size from the PNG.
"""

from pathlib import Path

from PIL import Image, ImageChops, ImageDraw

HERE = Path(__file__).resolve().parent

# The mark, in its own units: 259 wide, 214 high.
W, H = 259, 214
OUTER_R = 85  # radius of the C's two left corners
BARS = [(0, 22), (36, 22), (72, 26), (116, 26), (156, 22), (192, 22)]  # (top, height)
MIDDLE = {2, 3}  # bars cut by the C's opening
HOLE_CX, HOLE_CY, HOLE_R = 122, 107, 52  # the opening's round left side

BACKGROUND = (17, 17, 17, 255)
INK = (255, 255, 255, 255)


def svg() -> str:
    bars = []
    for i, (y, h) in enumerate(BARS):
        rect = f'<rect x="-40" y="{y}" width="{W + 40}" height="{h}" rx="{h / 2}"/>'
        bars.append(rect if i not in MIDDLE else rect.replace("<rect ", '<rect mask="url(#clio-hole)" '))
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" fill="currentColor">
  <defs>
    <clipPath id="clio-c">
      <path d="M{OUTER_R} 0H{W}V{H}H{OUTER_R}A{OUTER_R} {OUTER_R} 0 0 1 0 {H - OUTER_R}V{OUTER_R}A{OUTER_R} {OUTER_R} 0 0 1 {OUTER_R} 0Z"/>
    </clipPath>
    <mask id="clio-hole" maskUnits="userSpaceOnUse" x="-40" y="0" width="{W + 80}" height="{H}">
      <rect x="-40" y="0" width="{W + 80}" height="{H}" fill="#fff"/>
      <circle cx="{HOLE_CX}" cy="{HOLE_CY}" r="{HOLE_R}" fill="#000"/>
      <rect x="{HOLE_CX}" y="{HOLE_CY - HOLE_R}" width="{W}" height="{2 * HOLE_R}" fill="#000"/>
    </mask>
  </defs>
  <g clip-path="url(#clio-c)">
    {chr(10).join('    ' + b for b in bars).strip()}
  </g>
</svg>
"""


def mark_mask(scale: float) -> Image.Image:
    """The mark as a white-on-black mask, `scale` pixels per unit."""
    size = (round(W * scale), round(H * scale))
    s = lambda v: v * scale  # noqa: E731

    outer = Image.new("L", size, 0)
    ImageDraw.Draw(outer).rounded_rectangle(
        (0, 0, s(W) + s(OUTER_R), s(H) - 1), radius=s(OUTER_R), fill=255
    )
    hole = Image.new("L", size, 0)
    d = ImageDraw.Draw(hole)
    d.ellipse((s(HOLE_CX - HOLE_R), s(HOLE_CY - HOLE_R), s(HOLE_CX + HOLE_R), s(HOLE_CY + HOLE_R)), fill=255)
    d.rectangle((s(HOLE_CX), s(HOLE_CY - HOLE_R), s(W) + 1, s(HOLE_CY + HOLE_R)), fill=255)

    plain = Image.new("L", size, 0)
    cut = Image.new("L", size, 0)
    for i, (y, h) in enumerate(BARS):
        target = cut if i in MIDDLE else plain
        ImageDraw.Draw(target).rounded_rectangle(
            (-s(40), s(y), s(W) - 1, s(y + h) - 1), radius=s(h / 2), fill=255
        )
    cut = ImageChops.subtract(cut, hole)
    return ImageChops.multiply(ImageChops.lighter(plain, cut), outer)


def icon(size: int = 1024, supersample: int = 4) -> Image.Image:
    big = size * supersample
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    # A rounded square, as macOS and Windows show app icons.
    ImageDraw.Draw(img).rounded_rectangle((0, 0, big - 1, big - 1), radius=round(big * 0.225), fill=BACKGROUND)
    scale = big * 0.56 / W
    mask = mark_mask(scale)
    ink = Image.new("RGBA", mask.size, INK)
    img.paste(ink, ((big - mask.width) // 2, (big - mask.height) // 2), mask)
    return img.resize((size, size), Image.LANCZOS)


if __name__ == "__main__":
    icon().save(HERE / "icon-source.png")
    (HERE / "mark.svg").write_text(svg(), encoding="utf-8", newline="\n")
    print("wrote icon-source.png and mark.svg")
