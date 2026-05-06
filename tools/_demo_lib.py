"""Shared helpers for the split-pane asciinema demo generators.

Each demo (split_demo, cli_demo, dual_shell_demo) is a hand-crafted v2
cast file built by writing cursor-position escapes into a single
terminal — same trick tmux uses, but generated deterministically so
the GIF never changes between renders unless we change the script.

This module provides:
  - Cast: the global-timeline emitter
  - Pane: a rectangular region with its own cursor; \\n stays inside it
  - vlen: visible length of a string ignoring ANSI escapes
  - draw_chrome: title bar + pane labels + separator + footer
  - style constants
"""

from __future__ import annotations

import json
import random
import re

ANSI_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")


def vlen(s: str) -> int:
    """Visible length, ignoring ANSI escapes."""
    return len(ANSI_RE.sub("", s))


# ---------------------------------------------------------------------------
# Style
# ---------------------------------------------------------------------------

RESET = "\x1b[0m"
BOLD = "\x1b[1m"
DIM = "\x1b[2m"
GREEN = "\x1b[32m"
CYAN = "\x1b[36m"
YELLOW = "\x1b[33m"
ORANGE = "\x1b[38;5;208m"
WHITE = "\x1b[97m"
RED = "\x1b[31m"
MAGENTA = "\x1b[35m"

PROMPT_GREEN = f"{BOLD}{GREEN}$ {RESET}"
PROMPT_CYAN = f"{BOLD}{CYAN}$ {RESET}"
PROMPT_YELLOW = f"{BOLD}{YELLOW}$ {RESET}"
DEV = f"{ORANGE}uboot > {RESET}"


# ---------------------------------------------------------------------------
# Cast / Pane
# ---------------------------------------------------------------------------


class Cast:
    """Asciinema v2 cast emitter — one global timeline, raw byte stream."""

    def __init__(self, width=120, height=32, title="Serial Tether demo"):
        self.events: list[list] = []
        self.t = 0.0
        self.header = {
            "version": 2,
            "width": width,
            "height": height,
            "title": title,
            "env": {"TERM": "xterm-256color", "SHELL": "/bin/zsh"},
        }
        self.width = width
        self.height = height

    def raw(self, s: str, delay: float = 0.0) -> None:
        self.t += delay
        self.events.append([round(self.t, 3), "o", s])

    def pause(self, secs: float) -> None:
        self.t += secs

    def write(self, path: str) -> float:
        with open(path, "w") as f:
            f.write(json.dumps(self.header) + "\n")
            for e in self.events:
                f.write(json.dumps(e) + "\n")
        return self.t


class Pane:
    """A rectangular slice of the screen with its own cursor.

    Writes route through cursor-position escapes so two Panes can
    interleave on a single terminal without bleeding into each other.
    `\\n` jumps to the next row at the pane's left column (instead of
    column 0 of the screen).
    """

    def __init__(self, cast: Cast, top: int, left: int, width: int, height: int):
        self.cast = cast
        self.top = top
        self.left = left
        self.width = width
        self.height = height
        self.row = top
        self.col = left

    @staticmethod
    def at(row: int, col: int) -> str:
        return f"\x1b[{row};{col}H"

    def out(self, text: str, delay: float = 0.0) -> None:
        parts = text.split("\n")
        seq = self.at(self.row, self.col)
        for i, part in enumerate(parts):
            seq += part
            self.col += vlen(part)
            if i < len(parts) - 1:
                self.row += 1
                self.col = self.left
                seq += self.at(self.row, self.col)
        self.cast.raw(seq, delay)

    def type(self, text: str, base: float = 0.05, jitter: float = 0.04) -> None:
        for ch in text:
            d = base + random.uniform(0, jitter)
            self.out(ch, d)


# ---------------------------------------------------------------------------
# Layout chrome
# ---------------------------------------------------------------------------


def draw_chrome(
    c: Cast,
    title: str,
    label_left: str,
    label_right: str,
    footer: str | None = None,
    sep_col: int = 60,
) -> tuple[Pane, Pane]:
    """Clear the screen, draw the title bar, pane labels, vertical separator,
    and (optional) footer at the very bottom row. Return the two Panes for
    body content.

    Pane geometry (default sep_col=60):
      - Left:  cols 1..sep_col-1, rows 3..height-1   (width = sep_col-1)
      - Right: cols sep_col+1..width, rows 3..height-1
      - Title at row 1, labels at row 2, footer at row=height.
    """
    c.raw("\x1b[2J\x1b[H", 0.1)
    c.raw(Pane.at(1, 1) + title, 0.3)
    c.raw(Pane.at(2, 1) + label_left, 0.05)
    c.raw(Pane.at(2, sep_col + 2) + label_right, 0.05)

    body_top = 3
    body_bot = c.height - (1 if footer else 0)
    sep = ""
    for r in range(body_top, body_bot + 1):
        sep += Pane.at(r, sep_col) + f"{DIM}│{RESET}"
    c.raw(sep, 0.05)

    if footer:
        c.raw(Pane.at(c.height, 1) + footer, 0.0)

    body_height = body_bot - body_top + 1
    left = Pane(c, top=body_top, left=1, width=sep_col - 1, height=body_height)
    right = Pane(
        c,
        top=body_top,
        left=sep_col + 2,
        width=c.width - sep_col - 1,
        height=body_height,
    )
    return left, right
