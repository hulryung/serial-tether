#!/usr/bin/env python3
"""Generate an asciinema v2 cast file showing a typical Serial Tether session.

Why hand-craft instead of `asciinema rec`?  Live recording captures real
typing latency and shell/device timing, which makes a clean reproducible
demo very fiddly. Hand-crafting gives us deterministic timing, exact text,
and zero dependency on a real serial board.

Output: docs/demo.cast (asciinema player v2 format).
"""

import json
import os
import random

# ---------------------------------------------------------------------------
# Cast builder
# ---------------------------------------------------------------------------

class Cast:
    def __init__(self, width=100, height=30, title="Serial Tether demo"):
        self.events = []
        self.t = 0.0
        self.header = {
            "version": 2,
            "width": width,
            "height": height,
            "title": title,
            "env": {"TERM": "xterm-256color", "SHELL": "/bin/zsh"},
        }

    def out(self, s, delay=0.0):
        self.t += delay
        self.events.append([round(self.t, 3), "o", s])

    def pause(self, secs):
        self.t += secs

    def type(self, s, base=0.06, jitter=0.04):
        for ch in s:
            d = base + random.uniform(0, jitter)
            self.out(ch, d)

    def write(self, path):
        with open(path, "w") as f:
            f.write(json.dumps(self.header) + "\n")
            for e in self.events:
                f.write(json.dumps(e) + "\n")
        return self.t

# ---------------------------------------------------------------------------
# Style helpers
# ---------------------------------------------------------------------------

RESET   = "\x1b[0m"
BOLD    = "\x1b[1m"
DIM     = "\x1b[2m"
GREEN   = "\x1b[32m"
BLUE    = "\x1b[34m"
CYAN    = "\x1b[36m"
YELLOW  = "\x1b[33m"
ORANGE  = "\x1b[38;5;208m"
PURPLE  = "\x1b[35m"

PROMPT  = f"{BOLD}{GREEN}$ {RESET}"
DEV     = f"{ORANGE}ASAD SOC => {RESET}"

# ---------------------------------------------------------------------------
# Scenes
# ---------------------------------------------------------------------------

def main():
    random.seed(42)  # reproducible
    c = Cast()

    # ---- Header / intro comment -------------------------------------------------
    c.out(f"{DIM}# Serial Tether — one daemon, multiple clients on a single serial port{RESET}\n", 0.3)
    c.pause(0.7)
    c.out(PROMPT)

    # ---- Scene 1: start the daemon ---------------------------------------------
    c.type("tetherd -D /dev/ttys001 -b 115200 --tcp", base=0.05)
    c.pause(0.4)
    c.out("\n", 0.15)
    c.pause(0.8)

    banner = [
        "",
        f"{BOLD}Serial Tether 0.5.0{RESET}",
        "  device     /dev/ttys001 @ 115200 baud",
        "  buffer     64 KiB",
        f"  auth       {YELLOW}da3e2d294d9d4d6f942d61f0c402938a{RESET}",
        f"             {DIM}(clients: TETHER_AUTH_TOKEN=...){RESET}",
        "",
        "Listening:",
        "  unix       /tmp/tetherd.sock",
        f"  tcp        0.0.0.0:5557    {DIM}(auth required){RESET}",
        "               reachable as:",
        f"                 {CYAN}tcp://127.0.0.1:5557{RESET}   (lo0)",
        f"                 {CYAN}tcp://192.168.1.42:5557{RESET}   (en0)",
        "",
    ]
    for line in banner:
        c.out(line + "\n", 0.04)
    c.pause(1.5)

    # ---- Scene 2: interactive shell --------------------------------------------
    c.out(f"{DIM}# In another terminal — `tether` alone drops into a tio-style shell{RESET}\n")
    c.pause(0.5)
    c.out(PROMPT)
    c.type("tether", base=0.07)
    c.pause(0.3)
    c.out("\n", 0.15)
    c.pause(0.5)

    c.out(f"\r\n{DIM}[tether shell — Ctrl-A then Q to quit]{RESET}\r\n\r\n", 0.3)
    c.pause(0.4)
    c.out(DEV, 0.3)
    c.pause(0.7)

    # type version
    c.type("version", base=0.08)
    c.pause(0.3)
    c.out("\r\n", 0.15)
    c.pause(0.3)
    c.out("\nU-Boot 2025.01-g7c589e7dc280-dirty (Apr 18 2026 - 21:32:03 +0900)\r\n\r\n", 0.05)
    c.out(DEV)
    c.pause(0.9)

    # help (abbreviated)
    c.type("help", base=0.08)
    c.pause(0.3)
    c.out("\r\n", 0.15)
    c.pause(0.3)
    for l in [
        "?         - alias for 'help'",
        "bdinfo    - print Board Info structure",
        "boot      - boot default, i.e., run 'bootcmd'",
        f"{DIM}...{RESET}",
        "printenv  - print environment variables",
        "reset     - Perform RESET of the CPU",
        "version   - print monitor, compiler and linker version",
    ]:
        c.out(l + "\r\n", 0.04)
    c.out("\r\n")
    c.out(DEV)
    c.pause(1.2)

    # quit shell
    c.out("\r\n", 0.4)
    c.pause(0.3)
    c.out(PROMPT)
    c.pause(0.6)

    # ---- Scene 3: agent-style scripted RPC -------------------------------------
    c.out(f"{DIM}# For an agent or shell script — transactional, JSON, race-free{RESET}\n")
    c.pause(0.4)
    c.out(PROMPT)
    c.type('tether --json run "version" --newline crlf -u "ASAD SOC => " --literal', base=0.04)
    c.pause(0.3)
    c.out("\n", 0.15)
    c.pause(0.7)

    json_lines = [
        "{",
        '  "before": "",',
        '  "duration_ms": 15,',
        '  "end_seq": 4726,',
        '  "match": "ASAD SOC => ",',
        '  "match_seq": 4714,',
        '  "matched": true,',
        '  "output": "U-Boot 2025.01-g7c589e7dc280-dirty (Apr 18 2026 - 21:32:03 +0900)\\r\\n\\r\\n",',
        '  "truncated": false',
        "}",
    ]
    for line in json_lines:
        c.out(line + "\n", 0.04)
    c.pause(1.5)

    # ---- Scene 4: tail (god's-eye view) ----------------------------------------
    c.out(PROMPT)
    c.out(f"{DIM}# Humans watching: every byte the agent sends or receives{RESET}\n", 0.3)
    c.pause(0.4)
    c.out(PROMPT)
    c.type("tether tail", base=0.08)
    c.pause(0.3)
    c.out("\n", 0.15)
    c.pause(0.6)
    c.out("U-Boot 2025.01-g7c589e7dc280-dirty (Apr 18 2026 - 21:32:03 +0900)\r\n", 0.1)
    c.pause(0.3)
    c.out(DEV)
    c.pause(0.7)
    c.out(f"\r\n{DIM}^C{RESET}\r\n", 0.4)
    c.pause(0.3)
    c.out(PROMPT)
    c.pause(0.4)

    # ---- Outro ------------------------------------------------------------------
    c.out(f"{DIM}# {RESET}{CYAN}github.com/hulryung/serial-tether{RESET}\n", 0.5)
    c.pause(2.5)

    out_path = os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "docs", "demo.cast"
    )
    duration = c.write(out_path)
    print(f"wrote {out_path}  ({duration:.1f}s, {len(c.events)} events)")


if __name__ == "__main__":
    main()
