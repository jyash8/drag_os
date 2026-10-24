# DragOS

A small, deterministic x86_64 operating system written in Rust.

DragOS boots via UEFI into a `no_std` kernel and runs real Linux ELF binaries
(glibc and musl) through a Linux-compatible syscall layer. It includes a
Wayland compositor, DRM/VirtIO graphics (VirGL + Venus), audio, networking,
and a D-Bus based desktop portal.

## Highlights

- **Deterministic microkernel** — explicit lock ordering, one-shot LAPIC timer,
  no hidden preemption, and every execution path is traceable.
- **Linux ABI compatible** — process/memory/fs/signal/epoll/inotify/ptrace/
  io_uring syscalls plus DRM and ALSA ioctls, so unmodified userspace runs.
- **Graphics** — VirtIO-GPU with DRM/KMS, VirGL and Venus (Vulkan) acceleration,
  DMA-BUF, and a Smithay-based compositor.
- **Desktop stack** — xdg-shell, layer-shell, xdg-desktop-portal (OpenURI,
  Screenshot, Notification, GlobalShortcuts, Settings, Trash, Secret), PipeWire
  and ALSA audio.
- **Filesystems** — VFS, ext2/ext4, ramfs, devfs, FUSE skeleton, DAC permissions.
- **SMP** — per-CPU GDT/TSS, AP bring-up via trampoline, preemptive scheduler.

## Layout

```
crates/kernel/       core kernel (arch, memory, fs, drivers, syscalls)
crates/shared/       boot protocol, loader and paging shared with the bootloader
crates/drm-uapi/     DRM ioctl ABI, pinned to libdrm
crates/tools/        build/run/test driver (`cargo run -p tools -- ...`)
crates/nerdfont-bitmap/  embedded bitmap font for the compositor
userspace/draginit/  PID 1: mounts, session, shell
userspace/xdg-portal/  desktop portal (hand-rolled D-Bus client)
initrd/              guest test programs and their sources
scripts/             build-fs, QEMU, DRM pin verification
docs/                design notes and specs
modules/             loadable module fixtures
```

## Building

Requires a nightly Rust toolchain, `nasm`, QEMU, and the usual UEFI firmware
(`OVMF_CODE.fd` / `OVMF_VARS.fd`).

```sh
cargo run -p tools -- build          # build fs, kernel and bootloader
cargo run -p tools -- build-kernel   # kernel only (release, x86_64-unknown-none)
cargo run -p tools -- build-fs       # stage the filesystem (initrd.tar, disk.img)
cargo run -p tools -- run            # boot in QEMU (2 GB RAM, SMP 2, VirtIO, audio)
cargo run -p tools -- test           # run the automated test suite
```

Use `cargo run -p tools -- --help` for the full command list.

## Status

Work in progress. See [`AGENTS.md`](AGENTS.md) for the kernel invariants that
changes must preserve, and the build/test quick reference.

## License

No license has been chosen yet.
