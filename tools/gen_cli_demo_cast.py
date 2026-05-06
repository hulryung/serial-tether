#!/usr/bin/env python3
"""Generate split-pane asciinema cast: human shell + plain CLI commands.

  Left pane:  a human in an interactive `tether` shell
  Right pane: another terminal running quick `tether` CLI commands
              (run / ports / config) — no JSON, just plain text I/O,
              the way you'd use it from a shell script or by hand.

Both clients are attached to the same daemon, so when the right pane
sends a command via `tether run`, the human's shell pane sees the
device echo it in real-time.

Output: docs/cli_demo.cast
"""

import os
import random

from _demo_lib import (
    BOLD,
    CYAN,
    DEV,
    DIM,
    GREEN,
    PROMPT_CYAN,
    PROMPT_GREEN,
    RESET,
    WHITE,
    Cast,
    draw_chrome,
)


def main() -> None:
    random.seed(11)
    c = Cast(width=120, height=32, title="Serial Tether — multi-client (CLI + shell)")

    title = (
        f"{BOLD}{WHITE}Serial Tether v0.7.0{RESET}"
        f"{DIM} — one daemon, two clients on /dev/ttys001{RESET}"
    )
    label_h = f"{BOLD}{GREEN}HUMAN{RESET}{DIM} — interactive shell{RESET}"
    label_c = f"{BOLD}{CYAN}SCRIPTER{RESET}{DIM} — one-shot CLI commands{RESET}"
    footer = (
        f"{DIM}# Scripted commands echo live into the human's shell.{RESET}"
        f"   {CYAN}github.com/hulryung/serial-tether{RESET}"
    )
    left, right = draw_chrome(c, title, label_h, label_c, footer)
    c.pause(0.6)

    left.out(PROMPT_GREEN)
    right.out(PROMPT_CYAN)
    c.pause(0.4)

    # ---- LEFT: enter interactive shell ----
    left.type("tether", base=0.07)
    c.pause(0.25)
    left.out("\n", 0.15)
    left.out(f"{DIM}[tether shell — Ctrl-A Q quit, ? help]{RESET}\n", 0.2)
    left.out("\n", 0.05)
    left.out(DEV, 0.3)
    c.pause(0.6)

    # ---- RIGHT: scripted `version` via `tether run` ----
    right.type('tether run "version" \\', base=0.04)
    right.out("\n", 0.15)
    right.type('  -u "uboot > " --literal --newline crlf', base=0.04)
    c.pause(0.25)
    right.out("\n", 0.15)
    c.pause(0.4)

    # The right pane's command echoes into the left pane (device echo).
    left.out("version\n", 0.05)
    c.pause(0.1)
    left.out("U-Boot 2025.01-g7c589e7d\n", 0.05)
    left.out("(Apr 18 2026 - 21:32:03 +0900)\n", 0.05)
    left.out("\n", 0.02)
    left.out(DEV, 0.05)

    # Right gets the matched output (text mode — `before` to stdout, summary to stderr).
    right.out("U-Boot 2025.01-g7c589e7d\n", 0.05)
    right.out("(Apr 18 2026 - 21:32:03 +0900)\n", 0.05)
    right.out("\n", 0.02)
    right.out(f'{DIM}[matched: "uboot > ", 18ms]{RESET}\n', 0.05)
    right.out(PROMPT_CYAN, 0.25)
    c.pause(0.7)

    # ---- RIGHT: scripted `printenv mmcdev` ----
    right.type('tether run "printenv mmcdev" \\', base=0.04)
    right.out("\n", 0.15)
    right.type('  -u "uboot > " --literal --newline crlf', base=0.04)
    c.pause(0.25)
    right.out("\n", 0.15)
    c.pause(0.4)

    # Echo into left pane.
    left.out("printenv mmcdev\n", 0.05)
    c.pause(0.1)
    left.out("mmcdev=0\n", 0.05)
    left.out("\n", 0.02)
    left.out(DEV, 0.05)

    right.out("mmcdev=0\n", 0.05)
    right.out("\n", 0.02)
    right.out(f'{DIM}[matched: "uboot > ", 22ms]{RESET}\n', 0.05)
    right.out(PROMPT_CYAN, 0.25)
    c.pause(0.6)

    # ---- RIGHT: list_ports (v0.7) ----
    right.type("tether ports", base=0.06)
    c.pause(0.2)
    right.out("\n", 0.15)
    right.out("/dev/ttys001  (unknown)\n", 0.05)
    right.out("/dev/cu.usbserial-1132400  (usb)\n", 0.05)
    right.out("  product=Quad RS232-HS, usb=0403:6011\n", 0.05)
    right.out(PROMPT_CYAN, 0.25)
    c.pause(0.6)

    # ---- RIGHT: read live config (no flags = read-only) ----
    right.type("tether config", base=0.06)
    c.pause(0.2)
    right.out("\n", 0.15)
    for line in [
        "path:         /dev/ttys001",
        "baud:         115200",
        "framing:      8N1",
        "flow_control: none",
        "connected:    true",
    ]:
        right.out(line + "\n", 0.04)
    right.out(PROMPT_CYAN, 0.25)
    c.pause(2.5)

    out_path = os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "docs", "cli_demo.cast"
    )
    duration = c.write(out_path)
    print(f"wrote {out_path}  ({duration:.1f}s, {len(c.events)} events)")


if __name__ == "__main__":
    main()
