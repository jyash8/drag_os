# P4 — TTY Line Discipline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the pty pipe pair into a real terminal: canonical-mode line buffering with ERASE/KILL, signal characters (ISIG), and output processing — so shells get line editing, `^C` works, and full-screen apps (vim, less) survive.

**Architecture:** All discipline lives in `kernel/src/fs/pty.rs` (`PtyPair`), gated on the existing `termios` flags that are currently decorative. Raw mode (`ICANON` clear) keeps today's byte-pipe behavior exactly. Current state after this week's fix: ECHO + ICRNL + ONLCR implemented in `master_write`/`slave_write`; everything below starts from there.

**Tech Stack:** `kernel/src/fs/pty.rs` (350 lines), `Termios`/`WinSize` structs already defined, `TIOCSWINSZ` handling wherever ioctls dispatch pty ioctls (find via `grep -rn TIOCSWINSZ kernel/src`). Independent of P1–P3; do anytime.

---

### File map

- Modify: `kernel/src/fs/pty.rs` — input queue, canonical buffer, signal delivery, output processing, `TIOCGWINSZ/TIOCSWINSZ` + `SIGWINCH`.
- Reference: `initrd/bin/sh` behavior (dash or busybox ash — check `disk_folders/` provenance) for acceptance tests.

---

### Task 1: Canonical input queue (ICANON + ERASE/KILL/VEOF)

**Files:**
- Modify: `kernel/src/fs/pty.rs`

- [ ] **Step 1: Add the line buffer.** `PtyPair` gains `canon_buf: VecDeque<u8>` (reuse `VecDeque` import). `master_write` in ICANON mode appends to `canon_buf` instead of `m2s`, processing per byte:
  - `VERASE` (default 0x7f, from `termios.c_cc[2]`): pop one byte from `canon_buf`; if `ECHOE`, echo `\b \b` (3 bytes) into `s2m`.
  - `VKILL` (0x15): clear `canon_buf`; if `ECHOK`, echo `^U\n`-ish (`\r\n` suffices).
  - `\n` (after ICRNL mapping) or `VEOF` (0x04): commit — drain `canon_buf` into `m2s` (append `\n` unless VEOF with empty buffer, which delivers EOF/0 bytes), wake readers.
  - else: push byte; echo per ECHO rules (already implemented).
- [ ] **Step 2: `slave_read` unchanged** — it already reads `m2s`, which is now line-committed in canonical mode. Raw mode path stays byte-identical to today.
- [ ] **Step 3: Verify.** Boot, type `sl`, Backspace, `ls`, Enter in foot: echo shows `sl`→`s`→`sls`… i.e. erase visibly removes the char, and the fixed `?`-on-backspace artifact is gone. `Ctrl+U` clears the line.

Run: `cargo run -p tools -- build-kernel 2>&1 | tail -1 && cargo run -p tools -- build-fs 2>&1 | tail -1`

- [ ] **Step 4: Commit**

```bash
git add kernel/src/fs/pty.rs
git commit -m "feat(tty): canonical mode with ERASE/KILL/VEOF"
```

---

### Task 2: Signal characters (ISIG: VINTR/VQUIT/VSUSP)

**Files:**
- Modify: `kernel/src/fs/pty.rs`, process-group signal delivery path (`grep -rn "fn.*signal\|SIGINT" kernel/src/tasks.rs` for the existing send API).

- [ ] **Step 1: Map control chars to signals.** In `master_write` ICANON/raw input processing, when `ISIG` set: `VINTR` (0x03) → deliver `SIGINT` to `pgrp` (the `PtyPair.pgrp` field already exists — verify it is set on slave open / `setsid`+`setpgid` path, fix if zero); `VQUIT` (0x1C) → `SIGQUIT`; `VSUSP` (0x1A) → `SIGTSTP`. Consumed bytes are NOT queued and (with `ECHOCTL` semantics) echo as `^C` only — keep minimal: no echo for signal chars.
- [ ] **Step 2: Verify.** Run `sleep 30` in foot, press `^C`: process dies, prompt returns. `^Z` suspends (fg/bg only if the shell supports job control — dash does not by default non-interactive; accept stop + `jobs` listing as pass).

Run: `cargo run -p tools -- build-kernel 2>&1 | tail -1`

- [ ] **Step 3: Commit**

```bash
git add kernel/src/fs/pty.rs
git commit -m "feat(tty): ISIG signal characters to foreground pgrp"
```

---

### Task 3: Winsize + SIGWINCH, job-control basics

**Files:**
- Modify: `kernel/src/fs/pty.rs`, pty ioctl dispatch, foot resize path.

- [ ] **Step 1: `TIOCSWINSZ`.** foot already rebuilds its grid on toplevel Configure (tiling work). Add `ioctl(master, TIOCSWINSZ, &winsize{rows, cols})` in foot's Configure handler (`/home/jyash8/Projects/mywima/src/bin/foot.rs`, next to the `Term::new(cols, rows)` rebuild). Kernel: store into `PtyPair.winsize`, deliver `SIGWINCH` (28) to `pgrp`.
- [ ] **Step 2: `TIOCGWINSZ`** returns the stored size (check current behavior — likely zeros/80x24 default; make it truthful).
- [ ] **Step 3: Verify.** `stty size` in foot prints the actual grid size; resize via tiling (spawn second terminal) changes it live. Full-screen `vi` (if present in initrd/busybox) renders to the right dimensions.

Run: mywima rebuild + `cargo run -p tools -- build-fs 2>&1 | tail -1`

- [ ] **Step 4: Commit**

```bash
git add kernel/src/fs/pty.rs
git commit -m "feat(tty): winsize from compositor tile size with SIGWINCH"
```

---

### Task 4: Flown control + remaining iflags (only on demand)

`IXON` (`^S/^Q`), `ISTRIP`, `INLCR`, `ECHOCTL/ECHOKE` refinements: implement when a real program visibly breaks without them, not before. Each is a 5-line addition to the `master_write` byte loop with the same test loop (type the affected key, observe). Do not batch-speculate them.

---

## Self-review

- Spec coverage: canonical editing, signals, winsize each have a task; flow control correctly deferred.
- No placeholders: exact control-char indices into `c_cc`, exact echo bytes, exact verify rituals.
- Type consistency: `canon_buf` added once in Task 1, reused by Tasks 2–3.
