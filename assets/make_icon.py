"""Draws scrubr's icon: the logo's five bars on an amber tile.

Run from this folder with Python and Pillow:  python make_icon.py
It writes scrubr.ico (every size Windows asks for), scrubr-256.png and scrubr-1024.png.
After changing the .ico, make scrubr.res again with the Windows SDK's resource compiler:
    rc /nologo /fo scrubr.res scrubr.rc
"""
from PIL import Image, ImageDraw

AMBER_TOP = (226, 132, 16)      # a little lighter than the logo's #CE7100, for the top edge
AMBER_FOOT = (198, 104, 0)
CREAM = (255, 248, 238)
# The logo: five bars on a 16-unit square. x, top, bottom.
BARS = [(2, 7, 9), (5, 4.5, 11.5), (8, 2, 14), (11, 5, 11), (14, 7, 9)]


def draw(size):
    """The icon at `size` pixels, drawn eight times larger and scaled down."""
    over = 8
    n = size * over
    # Small sizes get a fuller tile, a bigger logo and fatter bars, so they still read.
    small = size <= 24 or size == 32
    inset = 0 if small else n * 0.035
    radius = n * (0.20 if small else 0.225)
    fill = 0.66                             # how much of the tile the logo spans
    stroke = 1.7                            # the bars' width, in the logo's units

    tile = Image.new('RGBA', (n, n), (0, 0, 0, 0))
    shade = Image.new('RGBA', (n, n))
    for y in range(n):
        t = y / (n - 1)
        row = tuple(round(a + (b - a) * t) for a, b in zip(AMBER_TOP, AMBER_FOOT)) + (255,)
        ImageDraw.Draw(shade).line([(0, y), (n, y)], fill=row)
    mask = Image.new('L', (n, n), 0)
    ImageDraw.Draw(mask).rounded_rectangle([inset, inset, n - 1 - inset, n - 1 - inset], radius=radius, fill=255)
    tile.paste(shade, (0, 0), mask)

    if small:
        # At the sizes of a title bar the bars are set on whole pixels, or they blur into one
        # another: scale the tile down first, then draw each bar as a block. The 32-pixel icon
        # is the 16-pixel one doubled, because that is the one a window is given for its title
        # bar and Windows halves it there.
        tile = tile.resize((size, size), Image.LANCZOS)
        pen = ImageDraw.Draw(tile)
        base, twice = (size // 2, 2) if size == 32 else (size, 1)
        wide = round(base / 8)
        gap = max(1, round(base / 20))
        left = (base - (5 * wide + 4 * gap)) // 2
        for i, (_, top, foot) in enumerate(BARS):
            tall = max(2, round((foot - top + 1.6) / 16 * base * 0.86 / 2) * 2)
            x = left + i * (wide + gap)
            y = (base - tall) // 2
            pen.rectangle([x * twice, y * twice, (x + wide) * twice - 1, (y + tall) * twice - 1], fill=CREAM + (255,))
        return tile

    pen = ImageDraw.Draw(tile)
    unit = n * fill / 16
    left = (n - 16 * unit) / 2
    half = stroke * unit / 2
    for x, top, foot in BARS:
        cx = left + x * unit
        pen.rounded_rectangle([cx - half, left + top * unit - half, cx + half, left + foot * unit + half], radius=half, fill=CREAM + (255,))
    return tile.resize((size, size), Image.LANCZOS)


sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
images = {size: draw(size) for size in sizes}
images[256].save('scrubr.ico', format='ICO', sizes=[(s, s) for s in sizes], append_images=[images[s] for s in sizes if s != 256], bitmap_format='bmp')
images[256].save('scrubr-256.png')
draw(1024).save('scrubr-1024.png')
print('wrote scrubr.ico with', len(sizes), 'sizes, scrubr-256.png and scrubr-1024.png')
