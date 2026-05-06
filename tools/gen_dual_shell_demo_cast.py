#!/usr/bin/env python3
"""Generate split-pane asciinema cast: two interactive `tether` shells.

Both panes are humans in their own `tether` shell, attached to the
same daemon. Whatever either one types is echoed by the device and
broadcast to every session — so each pane sees the same byte stream
in real time. Think of it as `screen -x` for serial consoles.

Output: docs/dual_shell_demo.cast
"""

import os
import random

from _demo_lib import (
    BOLD,
    CYAN,
    DEV,
    DIM,
    GREEN,
    PROMPT_GREEN,
    PROMPT_YELLOW,
    RESET,
    WHITE,
    YELLOW,
    Cast,
    Pane,
    draw_chrome,
)


def both_type(a: Pane, b: Pane, text: str, base: float = 0.05, jitter: float = 0.04) -> None:
    """Write `text` character-by-character to both panes at the same global
    time (simulates the device echo arriving at both sessions together)."""
    for ch in text:
        d = base + random.uniform(0, jitter)
        a.out(ch, d)
        b.out(ch, 0.0)


def both_out(a: Pane, b: Pane, text: str, delay: float = 0.0) -> None:
    a.out(text, delay)
    b.out(text, 0.0)


def main() -> None:
    random.seed(13)
    c = Cast(width=120, height=32, title="Serial Tether — two interactive shells")

    title = (
        f"{BOLD}{WHITE}Serial Tether v0.8.0{RESET}"
        f"{DIM} — two humans, one serial console (`screen -x`-style){RESET}"
    )
    label_a = f"{BOLD}{GREEN}USER A{RESET}{DIM} — interactive shell{RESET}"
    label_b = f"{BOLD}{YELLOW}USER B{RESET}{DIM} — interactive shell{RESET}"
    footer = (
        f"{DIM}# Every byte the device emits is broadcast to every attached session.{RESET}"
        f"   {CYAN}github.com/hulryung/serial-tether{RESET}"
    )
    a, b = draw_chrome(c, title, label_a, label_b, footer)
    c.pause(0.6)

    # Independent shell prompts (each user is in their own terminal).
    a.out(PROMPT_GREEN)
    b.out(PROMPT_YELLOW)
    c.pause(0.4)

    # Both users start `tether` (slightly staggered for realism — A first).
    a.type("tether", base=0.07)
    c.pause(0.2)
    a.out("\n", 0.15)

    b.type("tether", base=0.07)
    c.pause(0.2)
    b.out("\n", 0.15)

    # Each pane prints its own shell banner (independent attach calls).
    a.out(f"{DIM}[tether shell — Ctrl-A Q quit, ? help]{RESET}\n", 0.15)
    a.out("\n", 0.05)
    a.out(DEV, 0.2)

    b.out(f"{DIM}[tether shell — Ctrl-A Q quit, ? help]{RESET}\n", 0.15)
    b.out("\n", 0.05)
    b.out(DEV, 0.2)
    c.pause(0.7)

    # ---- USER A types `version` — both panes see the device echo + reply ----
    both_type(a, b, "version", base=0.08, jitter=0.04)
    c.pause(0.25)
    both_out(a, b, "\n", 0.15)
    c.pause(0.2)
    both_out(a, b, "U-Boot 2025.01-g7c589e7d\n", 0.05)
    both_out(a, b, "(Apr 18 2026 - 21:32:03 +0900)\n", 0.05)
    both_out(a, b, "\n", 0.02)
    both_out(a, b, DEV, 0.2)
    c.pause(0.8)

    # ---- USER B types `printenv mmcdev` — both panes see it ----
    both_type(a, b, "printenv mmcdev", base=0.08, jitter=0.04)
    c.pause(0.25)
    both_out(a, b, "\n", 0.15)
    c.pause(0.15)
    both_out(a, b, "mmcdev=0\n", 0.05)
    both_out(a, b, "\n", 0.02)
    both_out(a, b, DEV, 0.2)
    c.pause(0.8)

    # ---- USER A types `printenv ipaddr` — both see it ----
    both_type(a, b, "printenv ipaddr", base=0.08, jitter=0.04)
    c.pause(0.25)
    both_out(a, b, "\n", 0.15)
    c.pause(0.15)
    both_out(a, b, "ipaddr=192.168.1.42\n", 0.05)
    both_out(a, b, "\n", 0.02)
    both_out(a, b, DEV, 0.2)
    c.pause(0.8)

    # ---- USER B does a quick `bdinfo` (multi-line output) ----
    both_type(a, b, "bdinfo", base=0.08, jitter=0.04)
    c.pause(0.25)
    both_out(a, b, "\n", 0.15)
    c.pause(0.15)
    for line in [
        "boot_params = 0x83000100",
        "DRAM bank   = 0x00000000",
        "-> start    = 0x80000000",
        "-> size     = 0x40000000",
        "baudrate    = 115200 bps",
    ]:
        both_out(a, b, line + "\n", 0.04)
    both_out(a, b, "\n", 0.02)
    both_out(a, b, DEV, 0.05)

    c.pause(2.5)

    out_path = os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "docs", "dual_shell_demo.cast"
    )
    duration = c.write(out_path)
    print(f"wrote {out_path}  ({duration:.1f}s, {len(c.events)} events)")


if __name__ == "__main__":
    main()
