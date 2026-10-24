# TODO — DragOS Desktop

## Open: the compositor still dies at startup about one boot in five

**Symptom:** the compositor (pid 3) dies during startup and the console
shows "Display output is not active." Two signatures have been seen:

* the original one: a null read in the dynamic loader,
  `UNRESOLVABLE USER PAGE FAULT: pid=3 cr2=0x8 rip=0x7f000000fde7`
  (`add rcx,[rsi+8]` with rsi zero);
* the current one: a null-ish **write** from inside a library,
  `pid=3 cr2=0x19 rip=0x700000213006 err=0x6` -- `err=0x6` is
  `P=0 W=1 U=1`, so the program itself wrote through a null pointer at
  offset 0x19. It lands immediately after *another* process finished an
  `execve` (`[execve] destroying old cr3`, `[exit_group] pid=2`).

The second signature is a *different* fault, so the first one may well be
fixed; the common factor is still "a page the kernel handed out was not
what the file contains".

**How the original signature was pinned down:** the loader's offset was
disassembled out of the guest image (`debugfs -R "dump
/lib/ld-linux-x86-64.so.2"`), which resolved `0x7f000000fde7` to
`_dl_relocate_object_no_relro+0x340`, i.e. `rsi = map->l_info[DT_SYMTAB]`
was NULL -- a library's dynamic section had been read as garbage. A
per-page kernel diagnostic then showed the zero pages belonged to
`/lib/libsystemd.so.0` and `/lib/libX11.so.6`; `debugfs -R "ex <424>"`
proved those pages were genuine holes in the file, so the reads were
*correct* and the corruption had to come from elsewhere.

**Fixed along the way** (all real, all wrong, none of them individually
proven to be the crash):

1. A file read that *failed* was mapped as a page of zeros.
2. A read that stopped in the *middle* of a page was mapped with a zeroed
   tail.
3. A failed *block-map* lookup in ext4 was reported as a *hole*.
4. The tmpfs DSO cache id was the address of a heap allocation, so a
   closed file's id was reused by the next file of the same size.
5. The ext2/ext4 cache ids were bare inode numbers. An inode number is
   unique only *within* a filesystem, and the guest mounts the root image,
   `disk2.img` and the initrd at once, so the same `(inode, offset)` named
   several files. Ids are now allocated centrally
   (`vfs::next_dso_id`) and combined per filesystem.
6. `dsocache::lookup()` returned a frame and released the lock; the caller
   then took the reference in a second call. A concurrent `release` (last
   unmap) or OOM `evict_lru` could free the frame in between, and the freed
   frame was then re-inserted and mapped. `lookup_and_retain` now does both
   under one lock, and the plain `lookup` is gone.

**Where to look next:** the frame lifetime of a file page across *process
teardown*. The current crash follows another process's `execve`/exit,
which is exactly when `destroy_address_space` walks the page tables and
calls `release_file_page_from_frame_dirty`. Three things there do not add
up:

* `mark_cow()` adds a COW-table count for a `FILE_CACHE` frame, but
  nothing ever subtracts one for it: `destroy_address_space`'s
  `FILE_CACHE` branch `continue`s before `free_leaf`, and
  `cow_write_fault` clears `FILE_CACHE` without a matching decrement. The
  table drifts upward; `free_leaf`'s underflow clamp keeps that from
  turning into a panic, and hides it.
* `retain_from_frame()` (fork) silently does nothing when the frame is not
  in `DSO_CACHE`, so the child can hold a mapping with no reference.
* `fork_address_space` shares `FILE_CACHE` PTEs directly, including
  writable ones, without converting them to COW.

A first experiment was run: **12 boots with `-smp 1`, all clean**, with
the compositor starting every time, against roughly 5 faults in 25 boots
at `-smp 2`. That is suggestive (the chance of 12 clean runs at the
two-CPU rate is about 7%) but it is **not** a proof of a race: with one
CPU the processes no longer overlap, and that hides a page-lifetime bug
just as well as it hides an actual race. What it does say is that the
fault needs concurrency to be observed, so the next step is to find the
specific shared object, not to read more code single-threaded.

Two concrete concurrency suspects, in the order worth checking:

* **No TLB shootdown.** Every PTE rewrite here -- `mark_cow`,
  `cow_write_fault`, the demand-paging install -- finishes with
  `invlpg`/`write_cr3`, which is local to the CPU that ran it. If another
  thread of the same process (on the other CPU) still holds a stale
  *writable* translation for a page that was just turned into a COW copy,
  its write lands in the DSO cache frame itself -- i.e. straight into a
  copy of a library file that every other process maps.
* **A `release` racing a map.** `destroy_address_space` of one process
  decrements cache refs while another process faults the same page; if the
  count is ever one short, the frame is handed back to the allocator while
  the other process still maps it. `retain_from_frame` doing nothing when
  the frame is absent from `DSO_CACHE` is the obvious way to become one
  short.

**Watch the harness when reproducing this.** `cargo run -p tools -- test`
leaves `/test_mode` on the root image, and a later manual boot then runs
the *test* shell instead of the compositor: no `KMS backend started`
line appears and there is never a fault, which reads exactly like "the
bug is gone". Run `build-fs` first (it regenerates `disk.img`), or
`debugfs -w -R "rm /test_mode" disk.img`. A soak that does not print
`backend=1` for most runs is measuring nothing.

**This is not only a compositor problem.** `cargo run -p tools -- test`
is green (20/20, repeated), but `... -- test --gl` is flaky: over several
runs, `drm_flip` and `rainbow` timed out once each, in different runs,
and each of them passed in the runs where the other failed. They are
dynamically linked GL clients, so the same fault reaches any process that
maps a handful of shared libraries. Treat a `--gl` timeout as this bug
before treating it as a GL regression.

(The `mpv_sdl` case that used to sit in that list was not this bug at
all: it ran a binary that is not in the guest and expected a string
nothing prints. It has been replaced by `venus_probe`, which checks the
Venus path and covers the ring-index fix -- see
`fix(virtgpu): honour the ring count a client declares`.)

**Note:** the compositor-side crash handler cannot help with a
kernel-detected fault, because the kernel kills the thread itself
(`exit_current_thread(139)`) rather than raising SIGSEGV. Delivering the
signal first would let the process print its own backtrace; that is
still open.

## Fixed: an intermittent fault killed the compositor at startup

**Symptom:** in roughly one boot in five the compositor dies before it
finishes starting up. The guest then shows "Display output is not
active." on the console, because the kernel took the framebuffer back.

**What is known:**

* The kernel reports it:
  `UNRESOLVABLE USER PAGE FAULT: pid=3 cr2=0x700019848000,
  rip=0x7f000000fc63, err=111`.
* The faulting instruction pointer is in the **dynamic loader**
  (the interpreter is mapped at `0x7f00_0000_0000`), and the faulting
  write goes to `0x700019848000`, which is inside the **mmap region**
  (`mmap_base` is `0x7000_0000_0000`). So: the loader is writing to a
  file-backed mapping and the kernel cannot resolve the page.
* The kernel's DSO cache is the suspect. `file_demand_paging_fault()`
  looks the page up in the cache; on a miss it returns false, and the
  mapped-file branch has no way to re-read the file, so the fault is
  unresolvable. The cache is keyed by two different key spaces -- the
  inode for the mapped-file path and the vnode cache id for the regular
  file path -- and the frame-to-key reverse map is shared between them.
* The kernel does **not** deliver SIGSEGV for this: it calls
  `exit_current_thread(139)` itself, so the compositor's own crash
  handler never runs (the handler now avoids allocating, but it is not
  reached for a kernel-detected fault).

**How it was found:** the kernel's own fault dump (the PTE, the covering
VMA and the neighbouring-page walk) is printed to the debugcon log. It
showed the faulting instruction to be `add rcx,[rsi+8]` with rsi zero --
a null dereference inside the dynamic loader, on data the loader had just
read from a file. The demand-paging path read the page and *ignored the
result*: a read that failed left the buffer zeroed, and the zeroed buffer
was mapped as if it were the file's content.

**How it was fixed:** a read that failed, or that stopped in the middle
of a page, now fails the fault instead of being presented as file
content. Thirty consecutive boots without a crash, against roughly one
in four before.

**Note:** the compositor-side crash handler cannot help with a
kernel-detected fault, because the kernel kills the thread itself
(`exit_current_thread(139)`) rather than raising SIGSEGV. Delivering
the signal first would let the process print its own backtrace; that is
still open.


The "foot terminal content does not update" issue is **fixed**. The root
causes and fixes are recorded in the commit messages of
`mywima: keep the terminal alive and repaint it`,
`mywima: window lifecycle, keyboard focus and on-screen placement`,
`mywima: animate a window moving to its new place` and
`mywima: stop drawing a frame that shows nothing new`.

Summary of what was actually wrong (none of it was a compositor redraw bug):

1. **The client quit on `EAGAIN`.** `wayland_client`'s read guard returns
   `Err(WouldBlock)` when the socket has no data after all. The client
   treated that as fatal and exited, destroying its `xdg_toplevel` — so the
   terminal *disappeared* and stopped reacting to the keyboard. The compositor
   was fine; the client had left.
2. **The client deadlocked on its own buffer release.** It refused to draw
   again until the compositor released the buffer it had just committed, but
   a compositor only releases a buffer when the client commits a
   *replacement*. One frame, then a deadlock. The client now paces on
   `wl_surface.frame()` callbacks, which is the protocol-sanctioned "you may
   redraw now" signal.
3. **The compositor kept the keyboard focus on a dead surface.**
   `toplevel_destroyed` only fires on an explicit `xdg_toplevel.destroy`, and
   the `wl_surface` usually outlives it — so the dead window stayed in the
   workspace and the focus was never given back. The destroyed window is now
   removed explicitly.
4. **Windows could hang off the screen.** Floating windows were centred
   while their size was still unknown and were never re-clamped; the resize
   grabs had no upper bound and one of them clamped the position to a
   *negative* x.
5. **A window that says it is not resizable was tiled anyway.** xdg-shell has
   no "resizable" boolean, but the de-facto convention is
   `set_min_size(w, h) == set_max_size(w, h)`. Such a window now floats and
   is never configured outside the bounds the client declared.

## Completed this session
- Kernel: `reap_gpu_fences` try_lock + bounded `wait_for` (no system-wide freeze)
- Compositor: `flush_clients` no longer panics on dead sockets
- Compositor: grab-mode button release delivered (mouse works after drag)
- Compositor: status bar geometry fix (output_y offset)
- Compositor: border_gap 10 → 0
- Compositor: floating windows are re-mapped after a fullscreen/maximized
  window goes away (their location is remembered; `Space::unmap_elem` discards
  it, so they used to stay invisible forever)
- Compositor: a fullscreen/maximized window whose client died is detected in
  the render loop and dropped, so the screen is not left reserved for it
- Compositor: windows that declare a fixed size (`set_min_size == set_max_size`)
  float by default instead of being tiled
- Compositor: floating windows are centred on the output without hanging off
  the screen; tiled windows are clamped to the output; resizes are clamped to
  the output and to the 100px minimum
- Compositor: a window that moves to a new place (tiling, floating, a window
  appearing or disappearing) animates into it — position and size, over
  180ms by default (`animation_duration_ms`, 0 switches it off)
- Compositor: the idle frame rate went from ~48 fps to ~1 fps. The title bar
  and the clock are rasterised once instead of once per frame, and the
  terminal only redraws when its content changed
- Client: `EAGAIN` no longer treated as a fatal read error
- Client: redraw paced on frame callbacks instead of buffer release
- Client: one shared memory pool for the whole session (it used to be
  recreated on every resize, which meant a new file and a new set of pages
  for the compositor to map, for every frame of an animation)
- SDL test: handles `SDL_WINDOWEVENT_CLOSE` (close shortcut works)
- Async submit: attempted, reverted (caused regressions); sync path with mitigations retained
