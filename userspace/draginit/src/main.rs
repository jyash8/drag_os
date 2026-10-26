// draginit: minimal PID 1 for DragOS.
//
// Reads /etc/draginit/*.toml (one daemon each), spawns them in filename
// order, reaps exits, restarts per policy. Never exits: a dead PID 1 halts
// the guest, so a crashing daemon (e.g. mywima SIGSEGV) must not take us down.
//
// Test harness contract: with DRAGOS_TEST_MODE=1 in our environment (set by
// the kernel when built with the `test_mode` feature), print [SHELL_READY]
// and exec an interactive shell instead of starting daemons.

use std::collections::HashMap;
use std::ffi::CString;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Duration;

const CONFIG_DIR: &str = "/etc/draginit";

#[derive(Debug, serde::Deserialize)]
struct ConfigFile {
    daemon: Daemon,
}

#[derive(Debug, serde::Deserialize)]
struct Daemon {
    name: String,
    path: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    /// Wait this long before first spawn (lets seatd bind before mywima).
    #[serde(default)]
    delay_ms: u64,
    /// never | always | on-failure
    #[serde(default = "default_never")]
    restart: String,
    #[serde(default = "default_restart_delay")]
    restart_delay_ms: u64,
    /// Run the daemon as this uid/gid (dropped in the child before exec).
    /// unset = stay root (PID 1 itself always stays root).
    #[serde(default)]
    uid: Option<u32>,
    #[serde(default)]
    gid: Option<u32>,
}

fn default_never() -> String {
    "never".to_string()
}

fn default_restart_delay() -> u64 {
    2000
}

fn log(msg: &str) {
    println!("[draginit] {}", msg);
    let _ = std::io::stdout().flush();
}

fn load_configs() -> Vec<Daemon> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(CONFIG_DIR) {
        Ok(e) => e,
        Err(e) => {
            log(&format!("no {} ({}), starting nothing", CONFIG_DIR, e));
            return out;
        }
    };
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "toml").unwrap_or(false))
        .collect();
    files.sort();
    for path in files {
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                log(&format!("skip {}: {}", path.display(), e));
                continue;
            }
        };
        match toml::from_str::<ConfigFile>(&text) {
            Ok(cfg) => {
                log(&format!(
                    "daemon {} -> {} {:?}",
                    cfg.daemon.name, cfg.daemon.path, cfg.daemon.args
                ));
                out.push(cfg.daemon);
            }
            Err(e) => log(&format!("bad {}: {}", path.display(), e)),
        }
    }
    out
}

/// Spawn via raw fork+execve. std::process::Command does a pipe handshake
/// around exec that never completes on DragOS (parent blocks forever even
/// though the child runs). Raw fork returns immediately; we reap with
/// waitpid below. Single-threaded here, so fork is safe.
fn spawn_daemon(d: &Daemon, initial: bool) -> Option<i32> {
    // Initial spawn honors delay_ms (ordering, e.g. seatd before mywima);
    // restarts use restart_delay_ms instead (handled by the caller).
    if initial && d.delay_ms > 0 {
        std::thread::sleep(Duration::from_millis(d.delay_ms));
    }
    let path = CString::new(d.path.clone()).ok()?;
    let mut arg_ptrs: Vec<CString> = Vec::with_capacity(d.args.len() + 1);
    arg_ptrs.push(CString::new(d.path.clone()).unwrap_or(path.clone()));
    for a in &d.args {
        arg_ptrs.push(CString::new(a.clone()).ok()?);
    }
    // Child-only env overrides, applied after fork so PID 1's env stays clean.
    let env_pairs: Vec<(CString, CString)> = d
        .env
        .iter()
        .filter_map(|(k, v)| Some((CString::new(k.clone()).ok()?, CString::new(v.clone()).ok()?)))
        .collect();

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        log(&format!("fork for {} failed", d.name));
        return None;
    }
    if pid == 0 {
        // Child: drop to the configured uid/gid before exec, so a session
        // daemon runs unprivileged instead of as root. PID 1 itself stays
        // root; only the spawned child changes identity. setgid must come
        // before setuid (dropping uid first can drop the privilege needed to
        // change gid).
        if let Some(gid) = d.gid {
            unsafe { libc::setgid(gid) };
        }
        if let Some(uid) = d.uid {
            unsafe { libc::setuid(uid) };
        }
        // Child-only env overrides, applied after fork so PID 1's env stays clean.
        for (k, v) in &env_pairs {
            unsafe { libc::setenv(k.as_ptr(), v.as_ptr(), 1) };
        }
        let mut argv: Vec<*const libc::c_char> = arg_ptrs.iter().map(|s| s.as_ptr()).collect();
        argv.push(std::ptr::null());
        unsafe {
            libc::execv(path.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        }
    }
    let running_as = d.uid.map_or_else(|| "root".to_string(), |u| u.to_string());
    log(&format!(
        "started {} (pid {}) uid={}",
        d.name,
        pid,
        running_as
    ));
    Some(pid)
}

/// Non-blocking reap of any child. Returns (pid, exit_code) on reaped child.
fn reap_child() -> Option<(i32, String)> {
    let mut status: libc::c_int = 0;
    let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
    if pid <= 0 {
        return None;
    }
    let code = if libc::WIFEXITED(status) {
        format!("exit {}", libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        format!("signal {}", libc::WTERMSIG(status))
    } else {
        "stopped".to_string()
    };
    Some((pid, code))
}

/// Base environment every session needs (mirrors the old init.sh exports).
fn bootstrap_env() {
    // ponytail: fixed DragOS values, no config for what never changes.
    unsafe {
        std::env::set_var("PATH", "/bin:/usr/bin:/usr/local/bin:/usr/sbin:/sbin");
        std::env::set_var("HOME", "/root");
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        std::env::set_var("XDG_CONFIG_HOME", "/root/.config");
        std::env::set_var("XCURSOR_THEME", "Adwaita");
        std::env::set_var("XCURSOR_SIZE", "24");
        std::env::set_var("XCURSOR_PATH", "/usr/share/icons");
        // ponytail: Mesa shader cache (saves ~2s on second boot).
        std::env::set_var("MESA_SHADER_CACHE_DIR", "/tmp");
        std::env::set_var("MESA_SHADER_CACHE_MAX_SIZE", "32M");
    }
    let _ = std::fs::create_dir_all("/tmp");
    // /tmp must be world-writable (sticky) so uid-1000 daemons can allocate
    // SHM files (foot, mesa shader cache, ...). create_dir_all leaves it as
    // 0755 root-owned by default, which blocks them.
    unsafe {
        let path = std::ffi::CString::new("/tmp").unwrap();
        libc::chmod(path.as_ptr(), 0o1777);
    }
    // Mount the ESP so virgl auto-detect works (best effort, like init.sh).
    let _ = std::fs::create_dir_all("/efi");
    for dev in ["/dev/vda1", "/dev/sda1"] {
        let dev_c = std::ffi::CString::new(dev).unwrap();
        let dir_c = std::ffi::CString::new("/efi").unwrap();
        let fst_c = std::ffi::CString::new("vfat").unwrap();
        let r = unsafe { libc::mount(dev_c.as_ptr(), dir_c.as_ptr(), fst_c.as_ptr(), 0, std::ptr::null()) };
        if r == 0 {
            break;
        }
    }
    // VirGL: QEMU --gl leaves esp/efi/boot/virgl; use virpipe when present.
    let virgl = std::path::Path::new("/efi/efi/boot/virgl").exists()
        || std::path::Path::new("/efi/EFI/BOOT/virgl").exists();
    unsafe {
        if virgl {
            std::env::set_var("MESA_LOADER_DRIVER_OVERRIDE", "virtio_gpu");
            std::env::set_var("GALLIUM_DRIVER", "virgl");
            std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "0");
            log("virgl detected, using virtio_gpu");
        } else {
            std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
        }
    }
    // sshd for hostfwd tcp::2222-:22 live SSH (best effort).
    let _ = std::fs::create_dir_all("/run/sshd");
    let _ = std::fs::create_dir_all("/var/empty");
    for sshd in ["/usr/sbin/sshd", "/sbin/sshd"] {
        if std::path::Path::new(sshd).exists() {
            let d = Daemon {
                name: "sshd".to_string(),
                path: sshd.to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                delay_ms: 0,
                restart: "never".to_string(),
                restart_delay_ms: 2000,
                uid: None,
                gid: None,
            };
            // sshd daemonizes itself; restart=never since the direct child exits.
            spawn_daemon(&d, true);
            break;
        }
    }
}

/// The serial console, as an owned `File`.
///
/// `cargo run -p tools -- test` drives the guest over QEMU's serial port
/// (`-serial mon:stdio`) and reads its stdout. A write to stdout here is a
/// `TtyDevice` write, which reaches the VT cell buffer and debugcon but *never*
/// COM1 -- so `[SHELL_READY]` and every test result line were invisible to the
/// harness, which then timed out waiting for a prompt. `/dev/ttyS0` is the port
/// itself, so test-mode output is written there directly.
///
/// Returns None when the node is absent. Only `exec_shell` calls this, and that
/// runs on the test-mode path alone, so an interactive boot never opens the port
/// and is unaffected. The fallback still matters: a kernel without the node must
/// yield a shell rather than a dead PID 1.
fn serial_console() -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    // O_WRONLY without O_CREAT: never fabricate the device, and never truncate.
    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(0)
        .open("/dev/ttyS0")
        .ok()
}

fn exec_shell() -> ! {
    // The marker the harness gates on, on whichever sink it is watching. It is
    // written to the serial port when that node exists (test mode) and to stdout
    // otherwise, so an interactive boot is unaffected.
    let marker = b"[SHELL_READY]\n";
    match serial_console() {
        Some(mut serial) => {
            use std::io::Write;
            let _ = serial.write_all(marker);
            let _ = serial.flush();
        }
        None => {
            print!("[SHELL_READY]\n");
            let _ = std::io::stdout().flush();
        }
    }

    // Mirror init.sh: pty-hosted shell when available, plain sh otherwise.
    let has_ptyrun = std::path::Path::new("/bin/ptyrun").exists();
    let shell = if has_ptyrun { "/bin/ptyrun" } else { "/bin/sh" };

    let mut cmd = Command::new(shell);
    if has_ptyrun {
        cmd.arg("/bin/sh");
    }
    // In test mode the shell's stdout/stderr go to the serial port, so the
    // harness sees everything the shell prints. ptyrun keeps the VT for its own
    // stdin -- serial input reaches the guest through the COM1 IRQ into VT 1 --
    // so the shell still gets a real pty for termios and job control.
    if let Some(serial) = serial_console() {
        cmd.stdout(std::process::Stdio::from(serial));
        // A second handle for stderr: Stdio::from needs its own File, and
        // duplicating the fd would need libc. Re-opening the node is cheap and
        // keeps this dependency-free.
        if let Some(serial_err) = serial_console() {
            cmd.stderr(std::process::Stdio::from(serial_err));
        }
    }

    let err = cmd.exec();
    // exec() only returns on failure; without a shell PID 1 has nothing to do.
    println!("[draginit] FATAL: cannot exec shell: {}", err);
    let _ = std::io::stdout().flush();
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

struct Running {
    cfg: Daemon,
    pid: i32,
}

fn main() {
    bootstrap_env();

    // Give the unprivileged session a runtime directory it can own, mirroring
    // Linux's /run/user/<uid>. draginit runs as root, so it may create and
    // chown it; session daemons (spawned as uid 1000 below) then have a place
    // for their sockets and runtime files.
    unsafe {
        if libc::mkdir(b"/run/user\0".as_ptr() as *const _, 0o700) != 0
            && *libc::__errno_location() != libc::EEXIST
        {
            log("failed to mkdir /run/user");
        }
        if libc::mkdir(b"/run/user/1000\0".as_ptr() as *const _, 0o700) != 0
            && *libc::__errno_location() != libc::EEXIST
        {
            log("failed to mkdir /run/user/1000");
        } else if libc::chown(b"/run/user/1000\0".as_ptr() as *const _, 1000, 1000) != 0 {
            log("failed to chown /run/user/1000");
        }
    }

    // Test harness path: interactive shell, no daemons.
    let is_test_mode = std::env::var("DRAGOS_TEST_MODE").ok().as_deref() == Some("1")
        || std::path::Path::new("/efi/efi/boot/test_mode").exists()
        || std::path::Path::new("/efi/EFI/BOOT/test_mode").exists()
        || std::path::Path::new("/test_mode").exists();

    if is_test_mode {
        exec_shell();
    }

    log("starting daemons");

    let mut running: Vec<Running> = Vec::new();
    for d in load_configs() {
        if let Some(pid) = spawn_daemon(&d, true) {
            running.push(Running { cfg: d, pid });
        }
    }

    // Reap loop. waitpid(-1, WNOHANG) reaps, so no zombies. PID 1 never exits.
    loop {
        while let Some((pid, code)) = reap_child() {
            if let Some(pos) = running.iter().position(|r| r.pid == pid) {
                let r = running.remove(pos);
                log(&format!("{} (pid {}) exited ({})", r.cfg.name, pid, code));
                if r.cfg.restart == "always" {
                    log(&format!("restarting {} in {}ms", r.cfg.name, r.cfg.restart_delay_ms));
                    std::thread::sleep(Duration::from_millis(r.cfg.restart_delay_ms));
                    if let Some(new_pid) = spawn_daemon(&r.cfg, false) {
                        running.push(Running { cfg: r.cfg, pid: new_pid });
                    }
                }
                // on-failure: ponytail: same as never for v1; crash loops
                // belong in a supervisor with backoff, not here yet.
            } else {
                log(&format!("reaped unknown pid {} ({})", pid, code));
            }
        }
        // All daemons gone and none restarting: park, don't exit (PID 1
        // exiting halts the guest).
        std::thread::sleep(Duration::from_millis(100));
    }
}
