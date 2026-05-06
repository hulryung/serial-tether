#!/usr/bin/env python3
"""Generate split-pane asciinema cast: one daemon, two boards.

  Left pane:  `tether shell -d board0`  watching board 0
  Right pane: `tether shell -d board1`  watching board 1

A single `tetherd` process owns both serial ports and addresses each
by an operator-chosen device id. Output streams stay isolated per
device — typing in one pane only echoes into that device's traffic.

Output: docs/multi_device_demo.cast
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
    draw_chrome,
)


def main() -> None:
    random.seed(19)
    c = Cast(width=120, height=32, title="Serial Tether — multi-device daemon")

    title = (
        f"{BOLD}{WHITE}Serial Tether v0.8.0{RESET}"
        f"{DIM} — one daemon, two boards (`tetherd -D board0=... -D board1=...`){RESET}"
    )
    label_a = f"{BOLD}{GREEN}board0{RESET}{DIM}  /dev/ttyUSB0 @ 115200{RESET}"
    label_b = f"{BOLD}{YELLOW}board1{RESET}{DIM}  /dev/ttyUSB1 @ 921600{RESET}"
    footer = (
        f"{DIM}# One process, N ports — clients address devices by id (`tether -d <id>`).{RESET}"
        f"   {CYAN}github.com/hulryung/serial-tether{RESET}"
    )
    a, b = draw_chrome(c, title, label_a, label_b, footer)
    c.pause(0.6)

    # Each pane in its own terminal — independent shell prompts.
    a.out(PROMPT_GREEN)
    b.out(PROMPT_YELLOW)
    c.pause(0.4)

    # Both operators attach to the same daemon, different devices.
    a.type("tether shell -d board0", base=0.06)
    a.out("\n", 0.15)
    b.type("tether shell -d board1", base=0.06)
    b.out("\n", 0.15)

    a.out(f"{DIM}[tether shell — Ctrl-A Q quit, ? help]{RESET}\n", 0.15)
    a.out("\n", 0.05)
    a.out(DEV, 0.2)

    b.out(f"{DIM}[tether shell — Ctrl-A Q quit, ? help]{RESET}\n", 0.15)
    b.out("\n", 0.05)
    # board1 has a different prompt to make device isolation visually obvious.
    b.out(f"{ORANGE_HIGHLIGHT}buildroot login: {RESET}", 0.2)
    c.pause(0.6)

    # ---- board0: U-Boot work ----
    a.type("version", base=0.08)
    c.pause(0.2)
    a.out("\n", 0.15)
    a.out("U-Boot 2025.01-g7c589e7d\n", 0.05)
    a.out("(Apr 18 2026 - 21:32:03 +0900)\n", 0.05)
    a.out("\n", 0.02)
    a.out(DEV, 0.2)
    c.pause(0.5)

    # ---- board1: Linux login (totally different output stream) ----
    b.type("root", base=0.07)
    c.pause(0.2)
    b.out("\n", 0.15)
    b.out(f"{ORANGE_HIGHLIGHT}Password:{RESET} ", 0.2)
    b.type("******", base=0.06)
    b.out("\n", 0.2)
    b.out("\n# ", 0.15)
    c.pause(0.4)

    # ---- board0: continue U-Boot work ----
    a.type("printenv ipaddr", base=0.07)
    c.pause(0.2)
    a.out("\n", 0.15)
    a.out("ipaddr=192.168.1.42\n", 0.05)
    a.out("\n", 0.02)
    a.out(DEV, 0.2)
    c.pause(0.5)

    # ---- board1: uname -a (must stay within the 58-col right pane) ----
    b.type("uname -sr", base=0.07)
    c.pause(0.2)
    b.out("\n", 0.15)
    b.out("Linux 6.6.0\n", 0.05)
    b.out("# ", 0.2)
    c.pause(0.6)

    # ---- board1: another command to fill the panel ----
    b.type("cat /etc/hostname", base=0.07)
    c.pause(0.2)
    b.out("\n", 0.15)
    b.out("board1\n", 0.05)
    b.out("# ", 0.2)
    c.pause(0.5)

    # ---- board0: a v0.7 live config change — only this device sees it ----
    a.out(f"\n{DIM}[device config_changed: 921600 8N1 flow=none]{RESET}\n", 0.1)
    a.out(DEV, 0.05)
    c.pause(0.5)

    # ---- board1: meanwhile, modem-status check (v0.8 Ctrl-A L) ----
    b.out(f"\n{DIM}[lines: CTS=1 DSR=1 RI=0 DCD=1]{RESET}\n", 0.1)
    b.out("# ", 0.05)
    c.pause(2.5)

    out_path = os.path.join(
        os.path.dirname(os.path.abspath(__file__)),
        "..",
        "docs",
        "multi_device_demo.cast",
    )
    duration = c.write(out_path)
    print(f"wrote {out_path}  ({duration:.1f}s, {len(c.events)} events)")


# Use a different colour for board1's Linux prompt so the visual contrast
# between the two panes lands instantly. Defined inline (not in _demo_lib)
# because no other demo needs it.
ORANGE_HIGHLIGHT = "\x1b[38;5;208m"


if __name__ == "__main__":
    main()
