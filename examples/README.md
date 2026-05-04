# Examples

Real-world automation scripts an AI agent can read, copy, and adapt. Each
example follows the canonical pattern from [AGENTS.md](../AGENTS.md): one
`tether sync` to detect the prompt, then `tether run` per command with that
prompt as the anchor.

| File | What it does |
|---|---|
| [`detect-shell.sh`](detect-shell.sh)             | Probe the device, classify it (U-Boot / Linux root / busybox / MCU / unknown) |
| [`uboot-bootinfo.sh`](uboot-bootinfo.sh)         | Read `version`, `bdinfo`, `printenv` on a U-Boot device, print as JSON |
| [`set-env-and-verify.sh`](set-env-and-verify.sh) | Set a U-Boot env var, persist with `saveenv`, verify the new value |
| [`parse-help.sh`](parse-help.sh)                 | Run `help`, extract the available commands as a list, decide what to do next |
| [`wait-for-boot.sh`](wait-for-boot.sh)           | Issue `reset`, wait for the device to come back to its prompt (e.g. after kernel boot) |

All scripts assume `tether` is on `$PATH` and the daemon is reachable on the
default socket (`/tmp/tetherd.sock`). Override with `TETHER_SOCK=/path` if needed.

```sh
# Run all of them against a real board:
TETHERD_DEVICE=/dev/tty.usbserial-XXXX bash examples/detect-shell.sh
```
