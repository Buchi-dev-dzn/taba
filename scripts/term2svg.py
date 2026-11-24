#!/usr/bin/env python3
"""Render a captured terminal transcript (plain text) into a terminal-style SVG.

Usage: term2svg.py TRANSCRIPT.txt OUT.svg [--title TITLE]

The transcript is copied verbatim; only styling is added:
  - lines starting with "$ " are drawn as a prompt + command
  - lines starting with "taba: " (errors) are drawn in red
  - the words "running" / "exited" / "stopped" are tinted
  - "# ..." lines are drawn as dim comments
The SVG carries its own dark background, so it reads the same on GitHub's
light and dark themes. Standard library only.
"""

import argparse
import html
import re

FONT = "ui-monospace, SFMono-Regular, Menlo, Consolas, 'Liberation Mono', monospace"
FONT_SIZE = 14
CHAR_W = FONT_SIZE * 0.625  # SF Mono is ~0.62em, Menlo ~0.6em; err wide
LINE_H = 20
PAD_X = 22
PAD_TOP = 48  # room for the title bar
PAD_BOTTOM = 18

BG = "#0d1117"
BORDER = "#30363d"
BAR = "#161b22"
FG = "#c9d1d9"
DIM = "#8b949e"
PROMPT = "#3fb950"
CMD = "#e6edf3"
RED = "#ff7b72"
GREEN = "#3fb950"
YELLOW = "#d29922"
HEADER = "#79c0ff"

WORD_COLORS = {"running": GREEN, "exited": DIM, "stopped": YELLOW}
WORD_RE = re.compile(r"\b(running|exited|stopped)\b")
HEADER_RE = re.compile(r"^SERVICE\s+STATUS\s+URL\s+DIRECTORY")


def tspan(text, color, bold=False):
    weight = ' font-weight="bold"' if bold else ""
    return f'<tspan fill="{color}"{weight}>{html.escape(text)}</tspan>'


def render_line(line):
    if line.startswith("$ "):
        return tspan("$ ", PROMPT, True) + tspan(line[2:], CMD, True)
    if line.startswith("# "):
        return tspan(line, DIM)
    if line.startswith("taba: "):
        return tspan(line, RED)
    if HEADER_RE.match(line):
        return tspan(line, HEADER, True)
    out, pos = [], 0
    for m in WORD_RE.finditer(line):
        out.append(tspan(line[pos : m.start()], FG))
        out.append(tspan(m.group(1), WORD_COLORS[m.group(1)]))
        pos = m.end()
    out.append(tspan(line[pos:], FG))
    return "".join(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("transcript")
    ap.add_argument("out")
    ap.add_argument("--title", default="taba")
    args = ap.parse_args()

    with open(args.transcript, encoding="utf-8") as f:
        lines = f.read().rstrip("\n").split("\n")

    cols = max(60, max(len(l) for l in lines))
    width = int(PAD_X * 2 + cols * CHAR_W)
    height = PAD_TOP + LINE_H * len(lines) + PAD_BOTTOM

    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}" role="img" aria-label="{html.escape(args.title)}">',
        f'<rect x="0.5" y="0.5" width="{width - 1}" height="{height - 1}" rx="8" '
        f'fill="{BG}" stroke="{BORDER}"/>',
        f'<path d="M0.5 30 V8.5 A8 8 0 0 1 8.5 0.5 H{width - 8.5} A8 8 0 0 1 {width - 0.5} 8.5 V30 Z" '
        f'fill="{BAR}"/>',
        f'<line x1="0.5" y1="30.5" x2="{width - 0.5}" y2="30.5" stroke="{BORDER}"/>',
        '<circle cx="18" cy="15.5" r="5.5" fill="#ff5f57"/>',
        '<circle cx="36" cy="15.5" r="5.5" fill="#febc2e"/>',
        '<circle cx="54" cy="15.5" r="5.5" fill="#28c840"/>',
        f'<text x="{width / 2}" y="20" text-anchor="middle" fill="{DIM}" '
        f'font-family="{FONT}" font-size="12">{html.escape(args.title)}</text>',
        f'<text font-family="{FONT}" font-size="{FONT_SIZE}" xml:space="preserve">',
    ]
    for i, line in enumerate(lines):
        y = PAD_TOP + LINE_H * i + FONT_SIZE
        parts.append(f'<tspan x="{PAD_X}" y="{y}">{render_line(line)}</tspan>')
    parts += ["</text>", "</svg>", ""]

    with open(args.out, "w", encoding="utf-8") as f:
        f.write("\n".join(parts))


if __name__ == "__main__":
    main()
