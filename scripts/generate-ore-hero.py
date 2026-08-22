#!/usr/bin/env python3
"""Render the spinning ore crystal to the animated GIF used as the README hero.

Upstream's README hero is OpenAI's screenshot, showing OpenAI's branding, and
cannot be rebranded. This bakes a replacement from the same renderer the TUI
uses -- `scripts/ore_art.py`, the source of truth for the crystal -- by
rasterising each ANSI frame with a monospace font into a looping GIF.

    scripts/generate-ore-hero.py                 # write .github/ore-splash.gif
    scripts/generate-ore-hero.py --out /tmp/x.gif --frames 12   # a quick look

The rotation matches `ore_art.py --speed 0.8`, its default, so the hero turns
at the pace you would see running the renderer yourself. GIF delays are whole
centiseconds, so `--frames` decides how finely that period is sliced: more
frames means less judder and a proportionally larger file.

Not wired into `just ore-check`: it needs Pillow and gifsicle, and its output
only changes when someone deliberately changes the art. Run it by hand:

    uv run --with pillow scripts/generate-ore-hero.py

The frame is 1.595:1, the proportions of the screenshot this replaces, and
`--font-size` sets how big that frame is: the cell metrics come from the face
at that size, and the canvas is whatever the 87x32 grid needs. 17 px gives
870x545.

Nothing enforces a size ceiling: the repo's 500 KiB blob policy exempts this
file by allowlist. `--colors 32` is chosen on measurement -- against 48 and 64
it held the same glyph coverage and saved ~200 KiB, because drawing at 1:1
leaves few enough distinct colours that the palette no longer limits fidelity.
"""

from __future__ import annotations

import argparse
import math
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
ART = REPO_ROOT / "scripts" / "ore_art.py"
DEFAULT_OUT = REPO_ROOT / ".github" / "ore-splash.gif"

# sRGB 081937 -- the deep navy the crystal was tuned against.
BACKGROUND = (0x08, 0x19, 0x37)

# Menlo is macOS' terminal font and carries the whole `blocks` ramp. Override
# with --font on other platforms; any monospace TTF/TTC will do. Cell metrics
# are measured from the face at runtime rather than assumed.
DEFAULT_FONT = "/System/Library/Fonts/Menlo.ttc"

# Grid, chosen for the target aspect ratio -- see the module docstring.
COLS = 87
ROWS = 32

# 1898x1190, the screenshot this replaces. Cell metrics are derived from it,
# and the canvas is padded (never resampled) to land on it exactly.
TARGET_ASPECT = 1898 / 1190

# Draw at the published size instead of rendering large and downsampling.
#
# Downsampling looked like free quality and was the opposite. Resampling a
# 1px stroke of a dim ramp step averages it with the background it sits on,
# and the palette step then rounds the result the rest of the way -- so the
# faintest characters simply vanish, and the art shows holes. It also *tripled*
# the colour count, 3.2k -> 12.2k, because every glyph edge became its own
# gradient. Fewer colours quantise better and compress smaller, so drawing at
# 1:1 is both more faithful and cheaper. Raise --font-size for a bigger file.

# `blocks` deliberately, not the TUI's `mineral`: mineral's ramp tops out in
# the block-drawing characters, whose glyphs are shorter than the 41 px cell,
# so bright regions rasterise as a grid of rectangles with visible seams.
# `blocks` stops at `@` and stays clean.
RAMP = "blocks"

# The renderer's default gain is tuned for a terminal, where the darkest ramp
# steps still read. Against 081937 in a browser they vanish, so lift it.
GAIN = 1.4

# ore_art.py's own default spin, in radians per second: one rotation every
# ~7.9s. Matching it is the point -- the GIF should look like the thing you
# get from running the renderer, not a sped-up version of it.
#
# A GIF cannot have the renderer's 30fps, so --frames buys smoothness with
# file size, roughly linearly. The defaults sit at 90 frames, or 4 degrees of
# rotation per step, which is where the judder stops reading as judder. Below
# ~60 it looks like stop-motion; above ~120 the file doubles for a difference
# nobody notices at this rotation speed.
#
# Counter-intuitively, a *coarser* character grid does not help: fewer, larger
# glyphs mean more solid area changing between frames, which costs more than
# the fine grid's sparse noise over a flat background. 87x32 was measured
# smaller than 68x25 at every frame count tried.
SPEED = 0.8

SGR = re.compile(r"\x1b\[([0-9;]*)m")
FRAME_SEPARATOR = re.compile(r"\n\n-{10,}\n\n")


def die(msg: str) -> None:
    sys.exit(f"\033[31merror:\033[0m {msg}")


def cell_metrics(font, cols: int, rows: int) -> tuple[int, int, int]:
    """Measure the face: (cell width, cell height, glyph y-offset).

    Width is the advance, so glyphs tile without overlap. Height follows from
    the target aspect ratio rather than the font's line box, because the grid
    has to fill a 1.595:1 frame. `top` centres the `#` ink box in the cell --
    without it the ramp hangs low and clips against the row beneath.
    """
    cell_w = round(font.getlength("#"))
    cell_h = round(cell_w / (TARGET_ASPECT * rows / cols))
    left, top, _right, bottom = font.getbbox("#")
    del left, _right
    return cell_w, cell_h, round((cell_h - (bottom - top)) / 2) - top


def dump_frames(count: int, cols: int, rows: int, char_aspect: float) -> list[str]:
    """Run the renderer and split its output into ANSI frames."""
    result = subprocess.run(
        [
            sys.executable,
            str(ART),
            "--dump",
            str(count),
            "--width",
            str(cols),
            "--height",
            str(rows),
            "--char-aspect",
            f"{char_aspect:.4f}",
            "--matrix-width",
            "1.25",
            "--ramp",
            RAMP,
            "--gain",
            str(GAIN),
            "--ss",
            "3",
            # The wordmark renders *below* the grid, so it would fall outside
            # the canvas. The README says "ore" in text right underneath.
            "--label",
            "",
            "--force-color",
        ],
        capture_output=True,
        text=True,
        check=True,
    )
    frames = [f for f in FRAME_SEPARATOR.split(result.stdout) if f.strip()]
    if len(frames) != count:
        die(f"expected {count} frames from ore_art.py, got {len(frames)}")
    return frames


def parse_frame(frame: str) -> list[list[tuple[str, tuple[int, int, int] | None]]]:
    """Split one ANSI frame into rows of (character, colour) cells."""
    rows = []
    for line in frame.split("\n"):
        row: list[tuple[str, tuple[int, int, int] | None]] = []
        colour: tuple[int, int, int] | None = None
        pos = 0
        for match in SGR.finditer(line):
            row.extend((ch, colour) for ch in line[pos : match.start()])
            body = match.group(1)
            parts = body.split(";")
            if body in ("", "0"):
                colour = None
            elif len(parts) == 5 and parts[0] == "38" and parts[1] == "2":
                colour = (int(parts[2]), int(parts[3]), int(parts[4]))
            pos = match.end()
        row.extend((ch, colour) for ch in line[pos:])
        rows.append(row)
    return rows


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT)
    parser.add_argument("--frames", type=int, default=90, help="frames per rotation")
    parser.add_argument(
        "--font-size", type=int, default=17, help="px; sets the output size"
    )
    parser.add_argument("--speed", type=float, default=SPEED, help="spin, radians/sec")
    parser.add_argument("--colors", type=int, default=32, help="GIF palette size")
    parser.add_argument("--lossy", type=int, default=30, help="gifsicle --lossy level")
    parser.add_argument("--cols", type=int, default=COLS, help="character grid width")
    parser.add_argument("--rows", type=int, default=ROWS, help="character grid height")
    parser.add_argument("--font", default=DEFAULT_FONT)
    args = parser.parse_args()

    # GIF timing is whole centiseconds, so the achievable rotation is quantised
    # by frame count. Report what we actually got rather than what was asked.
    delay = max(2, round(100 * (2 * math.pi / args.speed) / args.frames))
    period = delay * args.frames / 100

    try:
        from PIL import Image, ImageDraw, ImageFont
    except ModuleNotFoundError:
        die(
            "Pillow is required; try `uv run --with pillow scripts/generate-ore-hero.py`"
        )

    if shutil.which("gifsicle") is None:
        die("gifsicle is required (brew install gifsicle)")
    if not Path(args.font).is_file():
        die(f"font not found: {args.font} (pass --font)")

    font = ImageFont.truetype(args.font, args.font_size, index=0)
    cell_w, cell_h, top = cell_metrics(font, args.cols, args.rows)
    art = (args.cols * cell_w, args.rows * cell_h)

    # Pad rather than resize: rounding cell heights to whole pixels leaves the
    # grid a hair off 1.595:1, and a one-pixel resize would resample every
    # glyph in the frame to fix it. Background padding costs nothing.
    out_size = (art[0], round(art[0] / TARGET_ASPECT))
    offset = (0, (out_size[1] - art[1]) // 2)

    images = []
    for frame in dump_frames(args.frames, args.cols, args.rows, cell_w / cell_h):
        image = Image.new("RGB", out_size, BACKGROUND)
        draw = ImageDraw.Draw(image)
        for y, row in enumerate(parse_frame(frame)):
            for x, (char, colour) in enumerate(row):
                if char != " " and colour is not None:
                    draw.text(
                        (x * cell_w + offset[0], y * cell_h + top + offset[1]),
                        char,
                        font=font,
                        fill=colour,
                    )
        images.append(image)

    args.out.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(suffix=".gif", delete=False) as handle:
        staged = Path(handle.name)
    try:
        images[0].save(
            staged,
            save_all=True,
            append_images=images[1:],
            duration=delay * 10,
            loop=0,
            optimize=True,
        )
        subprocess.run(
            [
                "gifsicle",
                "-O3",
                "--colors",
                str(args.colors),
                f"--lossy={args.lossy}",
                f"--delay={delay}",
                "--loop=0",
                str(staged),
                "-o",
                str(args.out),
            ],
            check=True,
        )
    finally:
        staged.unlink(missing_ok=True)

    size_kib = args.out.stat().st_size / 1024
    resolved = args.out.resolve()
    shown = (
        resolved.relative_to(REPO_ROOT)
        if resolved.is_relative_to(REPO_ROOT)
        else resolved
    )
    print(f"wrote {shown}")
    print(
        f"  {out_size[0]}x{out_size[1]}, {args.frames} frames, "
        f"{period:.1f}s per rotation, {size_kib:.0f} KiB"
    )
    # Nothing enforces a ceiling -- GitHub's own limit is 100 MB, and the repo's
    # 500 KiB blob policy exempts this file by allowlist. The only real cost is
    # how long a reader waits for the README to paint, so this is a nudge, not
    # a rule. Raise it deliberately if the art needs the room.
    if size_kib > 1536:
        print(f"\033[33mwarning:\033[0m {size_kib / 1024:.1f} MiB is a slow README")
    return 0


if __name__ == "__main__":
    sys.exit(main())
