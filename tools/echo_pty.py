#!/usr/bin/env python3
"""Virtual device used by the smoke test.

Opens a PTY pair, writes the slave path to stdout, then echoes whatever the
master receives in a "prompt# " style response.

usage:
    python3 tools/echo_pty.py
    # prints: PTY=/dev/ttysXXX  (pass this path as `-D` to tetherd)
"""

import os
import pty
import select
import signal
import sys
import termios
import tty


def main():
    master, slave = pty.openpty()
    # Put the slave in raw mode (turn off ECHO/ICANON/OPOST) to avoid loopback amplification.
    tty.setraw(slave, when=termios.TCSANOW)
    slave_path = os.ttyname(slave)
    sys.stdout.write(f"PTY={slave_path}\n")
    sys.stdout.flush()

    # Initial banner + prompt.
    os.write(master, b"hello from echo_pty\r\n# ")

    line = bytearray()

    def shutdown(_sig=None, _frm=None):
        try:
            os.close(master)
        finally:
            sys.exit(0)

    signal.signal(signal.SIGINT, shutdown)
    signal.signal(signal.SIGTERM, shutdown)

    while True:
        r, _, _ = select.select([master], [], [], 1.0)
        if master not in r:
            continue
        try:
            data = os.read(master, 4096)
        except OSError as e:
            sys.stderr.write(f"[echo_pty] read err: {e}\n")
            sys.stderr.flush()
            return
        if not data:
            sys.stderr.write("[echo_pty] EOF on master\n")
            sys.stderr.flush()
            return
        sys.stderr.write(f"[echo_pty] got {len(data)}B: {data!r}\n")
        sys.stderr.flush()
        # Echo back to the master (telnet/serial style).
        os.write(master, data)
        for b in data:
            if b in (10, 13):
                cmd = bytes(line).decode("utf-8", "replace").strip()
                line.clear()
                if cmd == "":
                    os.write(master, b"# ")
                elif cmd == "version":
                    os.write(master, b"\nv0.1.0-mock\r\n# ")
                elif cmd == "echo":
                    os.write(master, b"\nready\r\n# ")
                elif cmd.startswith("say "):
                    os.write(master, b"\n" + cmd[4:].encode() + b"\r\n# ")
                else:
                    os.write(master, b"\nunknown: " + cmd.encode() + b"\r\n# ")
            else:
                line.append(b)


if __name__ == "__main__":
    main()
