use std::path::Path;
use std::process::{Child, Command, Stdio};

#[derive(Debug, Clone)]
pub struct QemuConfig {
    pub memory: String,
    pub smp: u32,
    pub gdb: bool,
    pub no_audio: bool,
    pub trace: bool,
    pub headless: bool,
    pub gl: bool,
    pub extra_args: Vec<String>,
}

impl Default for QemuConfig {
    fn default() -> Self {
        Self {
            memory: "4G".to_string(),
            smp: 2,
            gdb: false,
            no_audio: false,
            trace: false,
            headless: false,
            // virgl host GPU is the default (virtio-vga-gl); guest
            // auto-selects virpipe via the esp virgl flag. Software
            // rendering (llvmpipe) is too slow for video/compositing.
            gl: true,
            extra_args: Vec::new(),
        }
    }
}

pub fn clean_test_mode(root: &Path) {
    let _ = std::fs::remove_file(root.join("esp/efi/boot/test_mode"));
    let disk_img = root.join("disk.img");
    if disk_img.exists() {
        let _ = Command::new("debugfs")
            .args(["-w", "-R", "rm /test_mode", disk_img.to_str().unwrap()])
            .output();
    }
}

impl QemuConfig {
    pub fn build_command(&self, root: &Path) -> Command {
        let mut cmd = Command::new("qemu-system-x86_64");

        // Use KVM acceleration if available
        if Path::new("/dev/kvm").exists() {
            cmd.arg("-enable-kvm");
        }

        cmd.arg("-smp").arg(self.smp.to_string());
        cmd.arg("-m").arg(&self.memory);

        if !self.no_audio {
            // Pick the audio backend instead of assuming one exists.
            //
            // `pa,id=snd0` makes QEMU *abort at startup* when it cannot reach a
            // PulseAudio server ("could not stat pidfile
            // /run/user/1000/pulse/pid"), so asking for it unconditionally
            // meant `cargo run -p tools -- run` failed outright on any machine
            // without a running pipewire/pulse daemon — including this one. The
            // guest is not using sound, so degrade to QEMU's null backend
            // rather than refusing to boot.
            //
            // A server is considered reachable if PULSE_SERVER is set, or the
            // per-user pulse *pidfile* exists. It must be the pidfile and not
            // just the directory: QEMU stats `$XDG_RUNTIME_DIR/pulse/pid` and
            // aborts if that is missing, even when a `native` socket sits
            // happily in the same directory. Testing the directory is how this
            // check previously passed a machine that then refused to boot.
            let pulse_present = std::env::var_os("PULSE_SERVER").is_some()
                || std::env::var_os("XDG_RUNTIME_DIR")
                    .map(|d| Path::new(&d).join("pulse").join("pid").exists())
                    .unwrap_or(false);
            let use_pa = !self.headless && pulse_present;
            if self.headless || !pulse_present {
                if !self.headless {
                    eprintln!(
                        "[tools] no PulseAudio server found; using the null audio \
                         backend (pass --no-audio to omit the device entirely)"
                    );
                }
            }
            let audiodev = if use_pa { "pa,id=snd0" } else { "none,id=snd0" };
            cmd.args([
                "-audiodev",
                audiodev,
                "-machine",
                "pc,pcspk-audiodev=snd0,i8042=off",
                "-device",
                "AC97,audiodev=snd0",
            ]);
        }

        cmd.args([
            "-netdev",
            "user,id=n0,hostfwd=tcp::8080-:80,hostfwd=tcp::2222-:22",
            "-device",
            "virtio-net-pci,netdev=n0",
        ]);

        // VirGL flag: create/remove esp/efi/boot/virgl for guest auto-detect
        let virgl_flag = root.join("esp/efi/boot/virgl");
        if self.gl {
            let _ = std::fs::create_dir_all(virgl_flag.parent().unwrap());
            let _ = std::fs::write(&virgl_flag, b"");
        } else {
            let _ = std::fs::remove_file(&virgl_flag);
        }

        // Ensure test_mode flag is never present in interactive runs
        if !self.headless {
            clean_test_mode(root);
        }

        let ovmf_code = root.join("OVMF_CODE.fd");
        let ovmf_vars = root.join("OVMF_VARS.fd");
        let esp_dir = root.join("esp");
        let disk_img = root.join("disk.img");

        cmd.args([
            "-drive",
            &format!(
                "if=pflash,format=raw,readonly=on,file={}",
                ovmf_code.display()
            ),
            "-drive",
            &format!(
                "if=pflash,format=raw,readonly=on,file={}",
                ovmf_vars.display()
            ),
            "-drive",
            &format!("format=raw,file=fat:rw:{}", esp_dir.display()),
            "-drive",
            &format!("file={},format=raw,if=none,id=vd0", disk_img.display()),
            "-device",
            "virtio-blk-pci,drive=vd0",
            "-device",
            "edu",
            "-debugcon",
            "file:uefi_debug.log",
            "-global",
            "isa-debugcon.iobase=0x402",
            "-serial",
            "mon:stdio",
        ]);

        if self.gl {
            // virgl host GPU: requires host virglrenderer + GL display.
            // ponytail: venus=true enables the Venus Vulkan capset (guest
            // Vulkan via virgl); blob+hostmem are its prerequisites.
            // Harmless when the guest doesn't use Vulkan.
            cmd.args(["-device", "virtio-vga-gl,edid=on,xres=1280,yres=800,venus=true,blob=true,hostmem=256M"]);
            // DRAGOS_TEST_GTK=1 runs a *test* with a real on-screen window even
            // though the harness drives QEMU headlessly. Needed whenever the
            // thing under test is a picture: a virtio-gpu scanout cannot be
            // captured by `screendump` (QEMU reports "no surface" once the
            // guest programs it), so the only way to see the frame is to give
            // it a window and let a human look.
            let want_window = std::env::var("DRAGOS_TEST_GTK").ok().as_deref() == Some("1");
            // ponytail: DRAGOS_QEMU_DISPLAY overrides the display backend for
            // experiments that must sweep it ("egl-headless,gl=on",
            // "sdl,gl=on", "gtk,gl=es", ...). Unset = behavior unchanged.
            // Needed because VNC is incompatible with the GL context on QEMU
            // 11.1, so egl-headless + screendump is the only scripted capture
            // channel for a virgl scanout.
            if let Ok(spec) = std::env::var("DRAGOS_QEMU_DISPLAY") {
                cmd.args(["-display", &spec]);
            } else if !self.headless || want_window {
                cmd.args(["-display", "gtk,gl=on"]);
                // ponytail: DRAGOS_FULLSCREEN=1 for unattended visual checks.
                // A virgl scanout cannot be captured with `screendump` (the host
                // reports "no surface" once the guest programs the scanout), so
                // the only oracle is a real window. But a normal window is
                // unreliable as an oracle for exactly the same reason: the
                // compositor may stack another window over it, and a screen
                // capture then silently measures the wrong pixels. It already
                // cost one run -- two "measurements" that were a browser.
                //
                // Fullscreen removes the ambiguity, because nothing can be
                // stacked above a fullscreen window. It is opt-in rather than
                // the default because it takes over the user's display, which
                // is not something a build tool should do unasked.
                //
                // The spelling is `-full-screen`, not `-fullscreen`; QEMU 11.1
                // rejects the latter outright ("invalid option") and also
                // rejects `gtk,...,fullscreen=on` ("Parameter 'fullscreen' is
                // unexpected"). It is a top-level option, not a display
                // suboption, so it must be a separate argv entry.
                if std::env::var("DRAGOS_FULLSCREEN").ok().as_deref() == Some("1") {
                    cmd.arg("-full-screen");
                }
            } else {
                // ponytail: single GL display; a later `-display none` would
                // override it and strip GL (virgl then never negotiates).
                // egl-headless *is* a display backend, so it creates a graphic
                // console surface and `screendump` works. The later headless
                // branch must not then add `-display none` on top of it, which
                // is what made the monitor answer "no surface".
                cmd.args(["-display", "egl-headless,gl=on"]);
            }
        } else {
            cmd.args(["-device", "virtio-vga,xres=1280,yres=800"]);
            if !self.headless {
                cmd.args(["-display", "gtk"]);
            }
        }

        // QEMU's built-in trace backend. `DRAGOS_TRACE=1` turns on every
        // virtio_gpu_* event, which is the only host-side visibility we have
        // without patching QEMU -- and the flicker is a host-side problem
        // (docs/virgl-vs-linux.md §10: draws are correct and symmetric, the guest
        // encodings are verified byte-for-byte, so the fault is in what QEMU does
        // with them).
        //
        // `-d trace:<events>` selects the trace *log* backend; `-D` names the
        // file. Without -D the events go to stderr and drown the serial console.
        //
        // Events include virtio_gpu_cmd_set_scanout, virtio_gpu_cmd_res_flush,
        // virtio_gpu_cmd_ctx_res_attach and virtio_gpu_fence_{ctrl,resp}.
        if std::env::var("DRAGOS_TRACE").ok().as_deref() == Some("1") {
            let path = std::env::var("DRAGOS_TRACE_FILE")
                .unwrap_or_else(|_| "/tmp/opencode/qemu-trace.log".to_string());
            // Must be `-trace enable=...,file=...` and NOT `-d trace:... -D ...`:
            // `-D` names the single log file shared by ALL `-d` domains, so it
            // collides with DRAGOS_QEMU_DLOG and whichever comes last wins for
            // both. Measured: the trace file was never created and the DLOG
            // file existed but stayed empty while virtio-gpu traffic flowed.
            cmd.arg("-trace");
            cmd.arg(format!("enable=virtio_gpu*,file={}", path));
        }

        let mon_sock = "/tmp/dragos-mon.sock";
        let _ = std::fs::remove_file(mon_sock);
        cmd.args(["-monitor", &format!("unix:{},server,nowait", mon_sock)]);

        // QMP as well as HMP. The HMP socket is a line editor that reads one
        // byte at a time and accepts a single client, which makes it a poor
        // control channel: it echoes every character back and drops the
        // connection after a command, so a script asking two questions gets a
        // BrokenPipe rather than the second answer. QMP is newline-delimited
        // JSON with a real event stream, so `{"execute":"screendump"}` and
        // `human-monitor-command` are both reliable and scriptable.
        let qmp_sock = "/tmp/dragos-qmp.sock";
        let _ = std::fs::remove_file(qmp_sock);
        cmd.args(["-qmp", &format!("unix:{},server,nowait", qmp_sock)]);

        cmd.args([
            "-device",
            "qemu-xhci",
            "-device",
            "usb-kbd",
            "-device",
            "usb-tablet",
            "-no-reboot",
        ]);

        // `-d int,cpu_reset` writes QEMU's own view of every interrupt and,
        // critically, every CPU reset, to a separate log. This exists because
        // the guest dies *silently* under `-no-reboot`: a triple fault resets
        // the machine, QEMU exits, and nothing in the guest's own output says
        // so. Zero KERNEL PANIC, zero BADRIP, log simply stops growing. Two
        // distinct deaths have been seen with that same signature (one right
        // after an execve of a Wayland client, one in steady-state rendering),
        // and neither could be told apart from a deadlock or a halted CPU by
        // inspecting the guest -- which is guesswork.
        //
        // `cpu_reset` is the discriminator: it tells us the machine was reset
        // rather than wedged, and `int` shows the last interrupts before the
        // reset, which is the only way to see what the CPU was doing.
        if std::env::var("DRAGOS_RESETLOG").ok().as_deref() == Some("1") {
            let path = std::env::var("DRAGOS_RESETLOG_FILE")
                .unwrap_or_else(|_| "/tmp/opencode/qemu_reset.log".to_string());
            cmd.arg("-d");
            cmd.arg("int,cpu_reset");
            cmd.arg("-D");
            cmd.arg(&path);
        }

        if self.trace {
            cmd.args(["-d", "int,cpu_reset", "-D", "qemu_trace.log"]);
        }
        // ponytail: DRAGOS_QEMU_DLOG=file captures qemu_log (guest_errors etc).
        if let Ok(path) = std::env::var("DRAGOS_QEMU_DLOG") {
            cmd.args(["-d", "guest_errors", "-D", &path]);
        }

        if self.gdb {
            cmd.args(["-s", "-S"]);
        }

        if self.headless {
            // A GL run already asked for `egl-headless` above; adding
            // `-display none` here would override it and remove the graphic
            // console, so `screendump` has nothing to read. `-nographic`
            // likewise implies `-display none`, so neither is safe here.
            if !self.gl {
                cmd.args(["-display", "none", "-nographic"]);
            }
        }

        for arg in &self.extra_args {
            cmd.arg(arg);
        }

        cmd.current_dir(root);
        cmd
    }

    pub fn run_interactive(&self, root: &Path) -> Result<(), String> {
        clean_test_mode(root);
        println!(
            "==> [tools] Launching QEMU (RAM: {}, SMP: {}, GDB: {})",
            self.memory, self.smp, self.gdb
        );
        let mut cmd = self.build_command(root);

        let status = cmd
            .status()
            .map_err(|e| format!("Failed to start QEMU: {}", e))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("QEMU exited with status: {}", status))
        }
    }

    pub fn spawn_headless(&self, root: &Path) -> Result<Child, String> {
        let mut config = self.clone();
        config.headless = true;
        // In headless mode, build_command automatically switches the audiodev
        // backend to `none,id=snd0` (null audio backend) so QEMU runs without
        // requiring a host PulseAudio daemon.
        // ponytail: --gl tests opt in via DRAGOS_TEST_GL=1 (virgl headless needs host EGL).
        if std::env::var("DRAGOS_TEST_GL").ok().as_deref() == Some("1") {
            config.gl = true;
        }
        let mut cmd = config.build_command(root);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        // ponytail: DRAGOS_QEMU_STDERR=file captures QEMU stderr (virgl errors)
        // instead of the default pipe (which is unread and dropped).
        if let Ok(path) = std::env::var("DRAGOS_QEMU_STDERR") {
            let f = std::fs::File::create(&path)
                .map_err(|e| format!("Failed to create {}: {}", path, e))?;
            cmd.stderr(Stdio::from(f));
        } else {
            cmd.stderr(Stdio::piped());
        }
        cmd.spawn()
            .map_err(|e| format!("Failed to spawn headless QEMU: {}", e))
    }
}
