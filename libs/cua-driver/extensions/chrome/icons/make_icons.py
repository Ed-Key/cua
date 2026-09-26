"""Render the extension icons from the cua agent cursor.

CURSOR is the Lottie path from
libs/cua-driver/rust/crates/cursor-overlay/assets/build_default_theme.py
(vertex, in-tangent, out-tangent), filled Cua blue with a white outline and
the same soft glow as the on-screen cursor. Small sizes use a tighter crop and
a lighter glow so the shape stays legible in the toolbar.

    python3 make_icons.py      # needs: pip install cairosvg
"""
from pathlib import Path

import cairosvg

CURSOR = [((55, 30), (0, 0), (-7, -2)), ((43, 41), (-1, -8), (0, 0)), ((64, 98), (0, 0), (3, 8)),
          ((77, 99), (-4, 7), (0, 0)), ((86, 79), (0, 0), (2, -4)), ((95, 70), (-4, 2), (0, 0)),
          ((108, 63), (0, 0), (7, -4)), ((107, 50), (7, 3), (0, 0))]
BLUE = "#5EC0E8"
GLOW = [(44, 2), (36, 2.4), (29, 3), (23, 3.8), (18, 4.8), (14, 6), (10, 7.5), (7, 9.5)]


def cursor_path() -> str:
    d = f"M{CURSOR[0][0][0]},{CURSOR[0][0][1]}"
    for k, (vertex, _, out) in enumerate(CURSOR):
        nxt = CURSOR[(k + 1) % len(CURSOR)]
        c1 = (vertex[0] + out[0], vertex[1] + out[1])
        c2 = (nxt[0][0] + nxt[1][0], nxt[0][1] + nxt[1][1])
        d += f" C{c1[0]},{c1[1]} {c2[0]},{c2[1]} {nxt[0][0]},{nxt[0][1]}"
    return d + " Z"


def icon_svg(size: int) -> str:
    d = cursor_path()
    small = size <= 32
    glow_scale = 0.5 if small else 1.0
    glow = "".join(
        f'<path d="{d}" fill="none" stroke="{BLUE}" stroke-width="{w}" '
        f'stroke-opacity="{o * glow_scale / 100}" stroke-linejoin="round"/>'
        for w, o in GLOW
    )
    view = "30 20 88 88" if small else "12 12 108 108"
    stroke = 7 if small else 5
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="{view}">'
        f'{glow}<path d="{d}" fill="{BLUE}" stroke="#fff" stroke-width="{stroke}" stroke-linejoin="round"/></svg>'
    )


if __name__ == "__main__":
    here = Path(__file__).parent
    for size in (16, 32, 48, 128):
        cairosvg.svg2png(bytestring=icon_svg(size).encode(), write_to=str(here / f"icon{size}.png"),
                         output_width=size, output_height=size)
        print(f"icon{size}.png")
