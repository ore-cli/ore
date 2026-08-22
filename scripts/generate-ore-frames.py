#!/usr/bin/env python3
"""Bake the ore crystal into the TUI's onboarding animation frames.

Upstream ships ten variants of the Codex wordmark under `codex-rs/tui/frames/`.
ore adds its own sets *alongside* them -- the rotating quartz crystal rendered
by `scripts/ore_art.py` -- and selects those instead. Upstream's frames and
their constants stay in the tree so the fork carries no deletion diff and
upstream's edits to them merge instead of conflicting.

The variant mechanic is unchanged, so the `.` key still cycles looks; here the
variants are the renderer's character ramps.

    scripts/generate-ore-frames.py            regenerate every variant
    scripts/generate-ore-frames.py --check    fail if the committed frames differ

The frame files are committed, so this only needs re-running when the art or the
ramp list changes.

Geometry matches what the TUI expects and what upstream's frames used: exactly
`ROWS` lines of exactly `COLS` characters, space-padded, with no trailing
newline. `welcome.rs` splits the frame on newlines and renders one ratatui
`Line` per row, so ragged rows would render ragged.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
ART = REPO_ROOT / "scripts" / "ore_art.py"
FRAMES_DIR = REPO_ROOT / "codex-rs" / "tui" / "frames"

# Frame geometry. Widths are *visible* columns; the frames carry ANSI colour, so
# escape sequences are excluded when measuring and padding.
#
# Three sizes so the crystal can scale to the terminal instead of being a fixed
# postage stamp on a large screen or overflowing a small one. `welcome.rs` picks
# the largest that fits. Character cells are about twice as tall as they are
# wide, so each size keeps cols ~= 2.3x rows to stay visually square.
SIZES: list[tuple[str, int, int]] = [
    ("small", 38, 16),
    ("medium", 46, 20),
    ("large", 62, 26),
]
FRAME_COUNT = 36

# `--matrix-width 1.25` is the look this fork settled on; the wordmark is
# dropped because the welcome screen already prints "ore" underneath.
COMMON_ARGS = [
    # `--dump` writes to a pipe, where the renderer would drop colour.
    "--force-color",
    "--label",
    "",
    "--matrix-width",
    "1.25",
    "--ss",
    "3",
]

# One variant per character ramp, so `.` on the welcome screen cycles the look.
# `shade` is omitted: it leans on block glyphs that render inconsistently across
# terminal fonts.
RAMPS = ["mineral", "blocks", "fine"]

# `--dump` prints each frame, then a blank line, the rule, and another blank
# line. Consuming those blank lines matters: leaving one behind prepends a
# phantom row to every frame after the first, which shifts the crystal down
# by one and reads as a jump when the loop wraps.
SEPARATOR = re.compile(r"\n\n-{10,}\n\n")
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def visible_len(line: str) -> int:
    """Width of a line as rendered, ignoring ANSI colour sequences."""
    return len(ANSI.sub("", line))


def render(ramp: str, cols: int, rows: int) -> list[str]:
    """Render FRAME_COUNT evenly spaced frames for one ramp at one size."""
    result = subprocess.run(
        [
            sys.executable,
            str(ART),
            "--dump",
            str(FRAME_COUNT),
            "--width",
            str(cols),
            "--height",
            str(rows),
            "--ramp",
            ramp,
            *COMMON_ARGS,
        ],
        capture_output=True,
        text=True,
        check=True,
    )
    frames = [f for f in SEPARATOR.split(result.stdout) if f.strip()]
    if len(frames) != FRAME_COUNT:
        raise SystemExit(f"{ramp}: expected {FRAME_COUNT} frames, got {len(frames)}")
    return [normalize(f, cols, rows) for f in frames]


def normalize(frame: str, cols: int, rows: int) -> str:
    """Pad a rendered frame to exactly rows x cols, preserving its framing.

    Absolute row positions are kept as the renderer produced them. An earlier
    version trimmed blank rows and re-centred each frame on its own content,
    which made the crystal bob up and down as its silhouette changed through the
    spin -- most visible as a jump where the loop wraps. The renderer already
    draws into a fixed box around a fixed centre, so the frames only need
    padding, never repositioning.

    Padding is computed from visible width, not string length: every coloured
    cell carries an escape sequence, so `len()` would over-count wildly and the
    crystal would collapse to the left edge.
    """
    lines = frame.split("\n")

    if len(lines) > rows:
        lines = lines[:rows]
    lines.extend([""] * (rows - len(lines)))

    out = []
    for line in lines:
        width = visible_len(line)
        if width > cols:
            # Trim by visible cells so an escape sequence is never cut in half.
            kept, seen = [], 0
            for token in re.split(r"(\x1b\[[0-9;]*m)", line):
                if ANSI.fullmatch(token):
                    kept.append(token)
                    continue
                room = cols - seen
                if room <= 0:
                    continue
                kept.append(token[:room])
                seen += len(token[:room])
            line = "".join(kept)
            width = visible_len(line)
        out.append(line + " " * (cols - width))
    return "\n".join(out)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--check",
        action="store_true",
        help="verify committed frames match; do not write",
    )
    args = parser.parse_args()

    stale: list[str] = []
    for size_name, cols, rows in SIZES:
        for ramp in RAMPS:
            frames = render(ramp, cols, rows)
            out_dir = FRAMES_DIR / f"ore-{size_name}-{ramp}"
            if not args.check:
                out_dir.mkdir(parents=True, exist_ok=True)
            for index, frame in enumerate(frames, start=1):
                path = out_dir / f"frame_{index}.txt"
                if args.check:
                    if not path.is_file() or path.read_text(encoding="utf-8") != frame:
                        stale.append(str(path.relative_to(REPO_ROOT)))
                else:
                    path.write_text(frame, encoding="utf-8")
            if not args.check:
                print(
                    f"  wrote {len(frames)} frames -> {out_dir.relative_to(REPO_ROOT)}"
                )

    if args.check:
        if stale:
            print(f"\033[31m{len(stale)} frame file(s) out of date\033[0m")
            for path in stale[:10]:
                print(f"  - {path}")
            return 1
        print("\033[32mframes are up to date.\033[0m")
    return 0


if __name__ == "__main__":
    sys.exit(main())
