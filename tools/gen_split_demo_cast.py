#!/usr/bin/env python3
"""Generate split-pane asciinema cast: human shell + AI agent (JSON-RPC).

  Left pane:  a human in an interactive `tether` shell
  Right pane: an AI agent calling `tether --json` for scripted RPCs

Hand-rendered with cursor-position escapes (no live recording dep) —
see tools/_demo_lib.py for the Cast/Pane primitives.

Output: docs/split_demo.cast
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
    random.seed(7)
    c = Cast(width=120, height=32, title="Serial Tether — multi-client (agent + shell)")

    title = (
        f"{BOLD}{WHITE}Serial Tether v0.7.0{RESET}"
        f"{DIM} — one daemon, two clients on /dev/ttys001{RESET}"
    )
    label_h = f"{BOLD}{GREEN}HUMAN{RESET}{DIM} — interactive shell{RESET}"
    label_a = f"{BOLD}{CYAN}AGENT{RESET}{DIM} — JSON-RPC over NDJSON{RESET}"
    footer = (
        f"{DIM}# Same daemon, same device — every client sees the same byte stream.{RESET}"
        f"   {CYAN}github.com/hulryung/serial-tether{RESET}"
    )
    left, right = draw_chrome(c, title, label_h, label_a, footer)
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
    c.pause(0.4)

    # ---- RIGHT: status ----
    right.type("tether status --json", base=0.05)
    c.pause(0.2)
    right.out("\n", 0.15)
    for line in [
        "{",
        '  "device": { "path":"/dev/ttys001",',
        '              "baud":115200,',
        '              "connected":true },',
        '  "sessions": [{"mode":"rw"}]',
        "}",
    ]:
        right.out(line + "\n", 0.04)
    right.out(PROMPT_CYAN, 0.25)
    c.pause(0.5)

    # ---- LEFT: human types `version` ----
    left.type("version", base=0.08)
    c.pause(0.25)
    left.out("\n", 0.15)
    c.pause(0.2)
    left.out("U-Boot 2025.01-g7c589e7d\n", 0.05)
    left.out("(Apr 18 2026 - 21:32:03 +0900)\n", 0.05)
    left.out("\n", 0.02)
    left.out(DEV, 0.2)
    c.pause(0.7)

    # ---- RIGHT: agent runs `printenv mmcdev` via `tether run` ----
    right.type("tether --json run \\", base=0.04)
    right.out("\n", 0.15)
    right.type('  "printenv mmcdev" \\', base=0.04)
    right.out("\n", 0.15)
    right.type("  --newline crlf \\", base=0.04)
    right.out("\n", 0.15)
    right.type('  -u "uboot > " --literal', base=0.04)
    c.pause(0.3)
    right.out("\n", 0.15)
    c.pause(0.4)

    # ---- The pivotal moment: agent's command echoes into the human's pane.
    left.out("printenv mmcdev\n", 0.05)
    c.pause(0.15)
    left.out("mmcdev=0\n", 0.05)
    left.out("\n", 0.02)
    left.out(DEV, 0.05)

    for line in [
        "{",
        '  "match": "uboot > ",',
        '  "duration_ms": 18,',
        '  "output": "mmcdev=0\\r\\n",',
        '  "matched": true',
        "}",
    ]:
        right.out(line + "\n", 0.03)
    right.out(PROMPT_CYAN, 0.25)
    c.pause(0.6)

    # ---- LEFT: human keeps typing — both panes still in sync ----
    left.type("printenv ipaddr", base=0.07)
    c.pause(0.3)
    left.out("\n", 0.15)
    left.out("ipaddr=192.168.1.42\n", 0.05)
    left.out("\n", 0.02)
    left.out(DEV, 0.05)
    c.pause(0.5)

    # ---- RIGHT: agent does a v0.7 set_device ----
    right.type("tether config --baud 921600", base=0.05)
    c.pause(0.25)
    right.out("\n", 0.15)
    for line in [
        "path:         /dev/ttys001",
        "baud:         921600",
        "framing:      8N1",
        "flow_control: none",
        "connected:    true",
    ]:
        right.out(line + "\n", 0.04)
    right.out(PROMPT_CYAN, 0.25)
    c.pause(0.5)

    # The human pane also picks up the `device {kind:"config_changed"}`
    # notification (rendered on its own line by the shell).
    left.out(
        f"\n{DIM}[device config_changed: 921600 8N1 flow=none]{RESET}\n",
        0.1,
    )
    left.out(DEV, 0.05)
    c.pause(2.5)

    out_path = os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "docs", "split_demo.cast"
    )
    duration = c.write(out_path)
    print(f"wrote {out_path}  ({duration:.1f}s, {len(c.events)} events)")


if __name__ == "__main__":
    main()
