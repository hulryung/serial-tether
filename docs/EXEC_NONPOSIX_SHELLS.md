# `exec` on non-POSIX consoles (U-Boot): failure analysis & fix guide

Status: guidance for implementation. Everything below marked **[HW-verified]**
was reproduced on real hardware (TOPST AI M.2, TCC750x, U-Boot 2022.01 with
hush parser) on 2026-07-02 via a live tether session.

## 1. Why the marker wrapper exists — keep it

`wrap_exec_command()` (tether.rs) currently emits:

```
echo "TETHEREXECBE""G<tag>"; <cmd>; __trc=$?; echo "TETHEREXECEN""D<tag>=$__trc"
```

Every piece has a job, and the design is sound for its purpose:

- **Begin/end markers** frame the command's output on a stream that has no
  framing: the serial buffer mixes command echo, output, prompts, kernel logs,
  and other clients' traffic. Without markers you cannot reliably extract
  "just this command's output".
- **Split quotes (`BE""G`)** keep the *typed/echoed* command line from ever
  containing the marker contiguously — only the shell's *evaluated* echo output
  does. This also survives terminal line-wrapping of long echoed commands.
- **Random 12-hex tag** prevents collisions with stale buffer content, previous
  runs, and concurrent clients.
- **`=$?` on the end marker** carries the device-side exit status in-band, so
  `tether exec` can pass it through as its own exit code.
- **End marker as completion signal** is far more robust than prompt-regex
  matching (prompts vary, appear inside output, get customized).

None of this should be removed. The problem is only that the wrapper assumes a
POSIX shell.

## 2. Exact failure on U-Boot **[HW-verified]**

Observed transcript (`tether exec "mdio list"` against a U-Boot prompt):

```
echo "TETHEREXECBE""G84afe0650db7"; mdio list; __trc=$?; echo "TETHEREXECEN""D84afe0650db7=$__trc"
TETHEREXECBEG84afe0650db7
gmac@13000000:
0 - RealTek RTL8211E <--> gmac@13000000
Unknown command '__trc=0' - try 'help'
TETHEREXECEND84afe0650db7=$__trc
```

Failure chain, step by step:

1. `;` separation, double quotes, and adjacent-string concatenation all **work**
   in U-Boot hush — the BEG marker prints correctly and `mdio list` runs.
2. `__trc=$?` — hush *does* expand `$?` (→ `__trc=0`) but U-Boot has no
   standalone variable assignment (needs `setenv`), so it becomes
   `Unknown command '__trc=0'`. Junk on the console, and `$?` is now clobbered
   (it reflects the failed assignment, not `<cmd>`).
3. In the final echo, `$__trc` is undefined; U-Boot hush leaves undefined vars
   **literal**, so the device prints `TETHEREXECEND<tag>=$__trc`.
4. The client's end regex is `TETHEREXECEND<tag>=(-?[0-9]+)` — requires digits.
   `=$__trc` never matches, so the client **hangs for the full 5 s timeout**
   and then reports `exec timed out — no end-marker seen`, which is doubly
   misleading: the marker text *was* seen, and the command actually ran fine.

A second, independent trap **[HW-verified]**: with `--newline crlf`, U-Boot
executes every command **twice** — CR runs the line, the trailing LF arrives as
an empty line, and U-Boot's CLI repeats the last command on an empty line. Any
U-Boot mode must default to `cr`.

## 3. Fixes, in priority order

### 3.1 Unify the wrapper — drop `__trc` (one wrapper for POSIX *and* hush)

```
echo "TETHEREXECBE""G<tag>"; <cmd>; echo "TETHEREXECEN""D<tag>=$?"
```

`$?` inside the echo's argument is expanded *before* echo runs, so it still
captures `<cmd>`'s status — the `__trc` temp var buys nothing in POSIX sh and
is exactly the part that breaks U-Boot.

**[HW-verified]** on U-Boot 2022.01 hush:

```
=> mdio list; echo "TESTEN""D_tag=$?"
gmac@13000000:
0 - RealTek RTL8211E <--> gmac@13000000
TESTEND_tag=0

=> definitely_not_a_command; echo "TESTEN""D_tag=$?"
Unknown command 'definitely_not_a_command' - try 'help'
TESTEND_tag=1

=> false; echo "st=$?"
st=1
```

Quoted `$?` expansion, split-quote concatenation, and failure statuses all
behave correctly. With this wrapper, `exec` works unmodified on both Linux
shells and hush-enabled U-Boot (which is the norm; `CONFIG_HUSH_PARSER` has
been default-on in mainline for years).

Caveat: U-Boots built *without* hush (plain CLI) support neither `;` nor
quotes. `exec` cannot work there at all — those consoles stay on
`run`/`send`/`expect`, and the error path in 3.2 should say so.

### 3.2 Tolerant end matching + honest errors (kills the silent 5 s hang)

- Loosen the end pattern to `TETHEREXECEND<tag>=(\S*)` for *detection*, then
  parse the capture as an integer separately:
  - digits → exit code as today;
  - anything else (e.g. literal `$__trc`, PowerShell `False`) → command output
    is still extracted correctly, `exit_code` becomes **unknown**: `null` in
    `--json`, plus a one-line stderr hint:
    `device shell did not report a numeric status (non-POSIX console?) — see docs/EXEC_NONPOSIX_SHELLS.md`.
- On timeout, inspect the captured buffer before printing the generic message:
  - end-marker text present but unparsable → the hint above, *immediately*;
  - `Unknown command` present → "device looks like a U-Boot/raw console; use
    `run \"<cmd>\" -u '<prompt-regex>' --newline cr` or set `shell=` on the
    device (3.3)".
- Bug while you're there: `print_exec_result` does `parse().unwrap_or(0)` — a
  failed parse silently reports **success**. Make `exit_code` an
  `Option<u8>`; never fabricate 0.

### 3.3 Per-device console personality

Extend `-D` inline settings (and the same keys in daemon config):

```
-D board=/dev/cu.usbserial-0001,shell=uboot,prompt='=> $',newline=cr
```

- `shell=posix|uboot|none` (default `posix`):
  - `posix` / `uboot`: same unified wrapper (3.1); differ only in defaults
    (`uboot` forces `newline=cr`) and in error hints;
  - `none`: `exec` refuses immediately with the `run`/`send`/`expect` hint
    instead of timing out.
- `prompt=` gives `run` (and `sync`) a default `-u`, so agents can just say
  `tether -d board run "mdio list"` — today every caller must re-derive the
  prompt regex and newline mode by trial and error (this cost a real debugging
  session two round-trips before the first command ran).

### 3.4 Optional: one-shot probe

`tether -d X probe` (or automatic on first `exec` failure): send
`echo "PRO""BE<tag>=$?"` with `newline=cr`, classify from the reply —
`PROBE<tag>=0` ⇒ POSIX/hush shell; literal `$?` or `Unknown command` ⇒ U-Boot
family; raw byte echo/nothing ⇒ `shell=none` — and cache the personality on
the daemon-side device entry. Nice-to-have; 3.1–3.3 already fix the pain.

### 3.5 Tests

The integration mock (`tests/integration.rs`, "TETHEREXEC" fake device) only
models a POSIX shell. Add a fake-U-Boot personality:

- expands `$?` but answers `x=0`-style assignments with
  `Unknown command 'x=0' - try 'help'`;
- leaves undefined `$var` literal in echo output;
- repeats the previous command when it receives an empty line (this is the
  CRLF double-execution regression test);
- assert: unified wrapper returns correct output + status; old-style wrapper
  yields `exit_code: null` *without* waiting for the timeout; `--newline crlf`
  against this mock is caught.

## 4. Agent-facing usage (documented behavior after the fix)

- Linux shell console: `tether -d <id> exec "<cmd>"` — unchanged.
- U-Boot console: works with `exec` once `shell=uboot` (or after 3.1, even
  without it, as long as `--newline cr`); until then:
  `tether -d <id> run "<cmd>" -u "=> $" --newline cr`.
- Never `--newline crlf` toward U-Boot (double execution).
- Update `tether agents` cookbook + AGENT_USAGE.md accordingly once implemented.
