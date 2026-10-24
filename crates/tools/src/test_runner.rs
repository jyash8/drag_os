// Test cases for the DragOS test harness
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc;
use std::thread;

use crate::qemu::QemuConfig;

// Test case configuration
#[derive(Clone, Debug)]
pub struct TestCase {
    pub name: &'static str,
    pub command: &'static str,
    pub expected: &'static [&'static str],
    pub timeout_secs: u64,
    pub gl_only: bool,
}

// All test cases
fn all_tests() -> Vec<TestCase> {
    vec![
        // The kernel's module self-check, asserted from the suite.
        //
        // The marker is printed by the *kernel* during boot, not by the
        // command: the module self-check runs before the compositor takes the
        // screen, and its result used to reach only the debugcon, which the
        // suite cannot see. The command is a shell builtin that does nothing,
        // so the case waits for the boot output and finishes.
        TestCase {
            name: "module_selftest",
            command: "true",
            expected: &["MODULE_SELFTEST_OK"],
            timeout_secs: 60,
            gl_only: false,
        },
        // The D-Bus session bus: start dbus-daemon on a path-based socket
        // (the abstract namespace is not implemented) and query it. This is
        // the foundation that xdg-desktop-portal builds on.
        TestCase {
            name: "session_bus",
            command: "echo DBUS_START_TEST; mkdir -p /run/user/1000; printf '%s\\n' '<busconfig><type>session</type><listen>unix:path=/run/user/1000/bus</listen><auth>EXTERNAL</auth><standard_session_servicedirs/><policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/><allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>' > /tmp/dbus.conf; /usr/bin/dbus-daemon --config-file=/tmp/dbus.conf --nosyslog --nopidfile & sleep 1; echo ALIVE:; ls -la /run/user/1000/bus 2>&1; echo DONE",
            expected: &["DBUS_START_TEST", "ALIVE", "DONE"],
            timeout_secs: 15,
            gl_only: false,
        },
        // The xdg-desktop-portal service: register on the session bus and
        // respond to introspection. This is Phase 1 of the portal.
        TestCase {
            name: "xdg_portal_bus",
            command: "echo PORTAL_START_TEST; mkdir -p /run/user/1000; printf '%s\\n' '<busconfig><type>session</type><listen>unix:path=/run/user/1000/bus</listen><auth>EXTERNAL</auth><standard_session_servicedirs/><policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/><allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>' > /tmp/dbus.conf; /usr/bin/dbus-daemon --config-file=/tmp/dbus.conf --nosyslog --nopidfile & sleep 1; DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus /usr/bin/xdg-portal > /tmp/portal.log 2>&1 & sleep 1; cat /tmp/portal.log; echo ---; dbus-send --address=unix:path=/run/user/1000/bus --dest=org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.DBus.Introspectable.Introspect 2>&1; echo DONE",
            expected: &["PORTAL_START_TEST", "owns org.freedesktop.portal.Desktop", "DONE"],
            timeout_secs: 30,
            gl_only: false,
        },
        // The file-permission checks: a process that drops from root to uid
        // 1000 must lose its access to a root-only file, and keep the access
        // it has by the "other" class. The program creates its own fixtures
        // and chmods them, so the case does not depend on the image's modes.
        TestCase {
            name: "privilege_drop",
            command: "/privdrop",
            expected: &[
                "PRIVDROP_ROOT_OPEN_OK",
                "PRIVDROP_GETGROUPS_OK",
                "PRIVDROP_USER_OPEN_RENDERNODE_OK",
                "PRIVDROP_USER_RENDERNODE_GID_1000",
                "PRIVDROP_USER_EXEC_NOEXEC_EACCES",
                "PRIVDROP_USER_SETUID_WORKS",
                "PRIVDROP_USER_OPEN_EACCES",
                "PRIVDROP_USER_OPEN_OTHER_OK",
                "PRIVDROP_USER_GROUP_VIA_SUPPLEMENTARY_OK",
                "PRIVDROP_USER_GROUP_NOT_MEMBER_EACCES",
                "PRIVDROP_USER_OPEN_WRONLY_EACCES",
                "PRIVDROP_USER_TRAVERSE_EACCES",
                "PRIVDROP_USER_CREATE_IN_0700_DIR_EACCES",
                "PRIVDROP_USER_CREATE_IN_NOWRITEDIR_EACCES",
                "PRIVDROP_USER_CREATE_IN_0777_DIR",
                "PRIVDROP_USER_CREATE_OWNER_1000_2000",
                "PRIVDROP_USER_CREATE_SETGID_DIR",
                "PRIVDROP_USER_CREATE_SETGID_DIR_GID_1000",
                "PRIVDROP_USER_UNLINK_ROOTFILE_EACCES",
                "PRIVDROP_USER_UNLINK_STICKY_EPERM",
                "PRIVDROP_USER_UNLINK_NONSTICKY_OK",
                "PRIVDROP_USER_CHMOD_ROOTFILE_EPERM",
                "PRIVDROP_USER_CHOWN_ROOTFILE_EPERM",
                "PRIVDROP_DONE",
            ],
            timeout_secs: 30,
            gl_only: false,
        },
        // ponytail: sessions, process groups, controlling terminals. asserts
        // values, not just return codes -- a stub returning 0 passes otherwise.
        TestCase {
            name: "job_control",
            command: "/jobctltest",
            expected: &["JOBCTL_DONE failures=0"],
            timeout_secs: 25,
            gl_only: false,
        },
        // ponytail: SIGTSTP/SIGCONT stop/resume and waitpid(WUNTRACED/WCONTINUED).
        TestCase {
            name: "job_control_stop",
            command: "/jobctltest_stop",
            expected: &["STOP_RESUME_DONE failures=0"],
            timeout_secs: 25,
            gl_only: false,
        },
        // tmux static binary test - basic version check
        TestCase {
            name: "tmux",
            command: "/tmux -V",
            expected: &["tmux 3.4"],
            timeout_secs: 60,
            gl_only: false,
        },
        // ponytail: mmap and COW test
        TestCase {
            name: "mmap_cow",
            command: "/mmap_test",
            expected: &["mmap_test: first bytes:", "mmap_test: COW ok"],
            timeout_secs: 25,
            gl_only: false,
        },
        // ponytail: eventfd basic functionality
        TestCase {
            name: "eventfd_basic",
            command: "/eventfd_test",
            expected: &["[eventfd_test] starting", "EVENTFD TEST PASSED"],
            timeout_secs: 25,
            gl_only: false,
        },
        // ponytail: ptrace (TRACEME / ATTACH stop, GETREGS, PEEKTEXT,
        // GETSIGINFO, CONT, kill-interception) basic functionality.
        TestCase {
            name: "ptrace_basic",
            command: "/ptrace_test",
            expected: &["[ptrace_test] starting", "PTRACE_TEST_PASS"],
            timeout_secs: 25,
            gl_only: false,
        },
        // ponytail: epoll basic functionality
        TestCase {
            name: "epoll_basic",
            command: "/epoll_test",
            expected: &["[epoll_test] starting", "EPOLL TEST PASSED"],
            timeout_secs: 25,
            gl_only: false,
        },
        // ponytail: timerfd basic functionality
        TestCase {
            name: "timerfd_basic",
            command: "/timerfd_test",
            expected: &["[timerfd_test] starting", "TIMERFD TEST PASSED"],
            timeout_secs: 25,
            gl_only: false,
        },
        // ponytail: VFS tmpfs sysfs test
        TestCase {
            name: "vfs_tmpfs_sysfs",
            command: "/vfstest",
            expected: &["tmpfs roundtrip: hello tmpfs", "mkdir ok", "uevent:"],
            timeout_secs: 25,
            gl_only: false,
        },
        // ponytail: mprotect fault test
        TestCase {
            name: "mprotect_fault",
            command: "/mprotect_fault_test",
            expected: &["[mprotect_fault_test] starting", "MPROTECT_SIGSEGV_PASS"],
            timeout_secs: 25,
            gl_only: false,
        },
        // ponytail: FPU concurrency test
        TestCase {
            name: "fpu_concurrency",
            command: "/fpu_concurrency_test",
            expected: &["[fpu_concurrency_test] starting", "FPU_CONCURRENCY_PASS"],
            timeout_secs: 25,
            gl_only: false,
        },
        // ponytail: EFAULT security test
        TestCase {
            name: "efault_security",
            command: "/efault_security_test",
            expected: &["[efault_security_test] starting", "EFAULT_SECURITY_PASS"],
            timeout_secs: 25,
            gl_only: false,
        },
        // ponytail: ext4 read/write on disk.img (never /tmp tmpfs or initrd).
        // Static musl binary; stages mkdir/create/stat/truncate/fsync/sync.
        TestCase {
            name: "ext4_rw",
            command: "/ext4test",
            expected: &[
                "MKDIR_OK",
                "DOT_OK",
                "BASIC_OK",
                "STAT_OK",
                "OTRUNC_OK",
                "TRUNC_OK",
                "FTRUNC_OK",
                "SYNC_OK",
                "EXT4TEST_PASS",
            ],
            timeout_secs: 25,
            gl_only: false,
        },
        // ponytail: VirtIO-Net TCP via QEMU gateway; host fixture serves
        // /token.txt on 10.0.2.2:18080 (bound by `spawn_net_fixture` in
        // run_all_tests when this test is selected).
        TestCase {
            name: "net_probe",
            command: "/netprobe",
            expected: &[
                "NETPROBE_CONNECTED",
                "NETPROBE_GOT",
                "NETPROBE_PASS",
            ],
            timeout_secs: 40,
            gl_only: false,
        },
        // ponytail: real UDP DNS via QEMU resolver at 10.0.2.3.
        TestCase {
            name: "dns_probe",
            command: "/dnstest 10.0.2.3",
            expected: &["DNSTEST_SENT", "DNSTEST_PASS"],
            timeout_secs: 40,
            gl_only: false,
        },
        // AC97 PCM Audio test (/dev/dsp)
        TestCase {
            name: "audio_probe",
            command: "/pcm_test",
            expected: &[
                "Playing AC97 Digital PCM Audio Test (/dev/dsp)...",
                "PCM Audio Playback Finished!",
            ],
            timeout_secs: 60,
            gl_only: false,
        },
        TestCase {
            name: "audio_probe",
            command: "/pcm_test",
            expected: &[
                "Playing AC97 Digital PCM Audio Test (/dev/dsp)...",
                "PCM Audio Playback Finished!",
            ],
            timeout_secs: 60,
            gl_only: false,
        },
        TestCase {
            name: "alsa_probe",
            command: "cat /proc/asound/cards 2>&1; echo '=== devices ==='; cat /proc/asound/devices; echo '=== arecord ==='; arecord -l 2>&1; echo EXIT:$?; echo DONE",
            expected: &["AC97", "DONE"],
            timeout_secs: 20,
            gl_only: false,
        },
        // curl dynamic library, DNS resolution & HTTP fetch probe
        TestCase {
            name: "curl_probe",
            command: "/usr/bin/curl -s -v http://www.example.com",
            expected: &["Example Domain"],
            timeout_secs: 35,
            gl_only: false,
        },
        TestCase {
            name: "curl_probe_cern",
            command: "/usr/bin/curl -s -v http://info.cern.ch",
            expected: &["home of the first website"],
            timeout_secs: 35,
            gl_only: false,
        },
        // OpenSSH client dynamic library & version probe
        TestCase {
            name: "ssh_probe",
            command: "/usr/bin/ssh -V 2>&1",
            expected: &["OpenSSH_"],
            timeout_secs: 25,
            gl_only: false,
        },
        // OpenSSH server configuration & hostkeys validation probe
        TestCase {
            name: "sshd_probe",
            // The marker must NOT appear verbatim in `command`: the pty echoes
            // the typed line, so an inline marker let this test pass on the
            // echo alone even if `sshd -t` failed. `'_'` is removed by the
            // shell, so the command text reads SSHD_CONFIG'_'VALID while the
            // real output is SSHD_CONFIG_VALID.
            command: "/usr/sbin/sshd -t && echo SSHD_CONFIG'_'VALID",
            expected: &["SSHD_CONFIG_VALID"],
            timeout_secs: 25,
            gl_only: false,
        },
        // ponytail: VirGL 3D path; requires `test --gl` (virtio-vga-gl + host EGL).
        TestCase {
            name: "mount_subsystem",
            command: "/mounttest",
            expected: &["MOUNTTEST_DONE failures=0"],
            timeout_secs: 30,
            gl_only: false,
        },
        TestCase {
            name: "virgl_probe",
            command: "/virglprobe",
            expected: &[
                "VIRGL_3D=1",
                "CAPS_OK",
                "RES_OK",
                "MAP_OK",
                "WAIT_OK",
                // A real 3D command submission, not just context setup.
                "EXEC_OK",
            ],
            timeout_secs: 60,
            gl_only: true,
        },
        // ponytail: DRM flip test
        TestCase {
            name: "drm_flip",
            command: "/flip_test",
            expected: &["Page flip test PASSED"],
            timeout_secs: 60,
            gl_only: true,
        },
        // ponytail: rainbow DRM test
        TestCase {
            name: "rainbow",
            command: "/rainbow",
            expected: &["RAINBOW_DONE"],
            timeout_secs: 60,
            gl_only: true,
        },
        // ponytail: gbmscanout test
        TestCase {
            name: "gbmscanout",
            command: "/gbmscanout",
            expected: &["GBMSCANOUT_DONE"],
            timeout_secs: 60,
            gl_only: true,
        },
        // The Venus path, end to end without a compositor: context init with
        // the Venus capset, a blob round-trip, and an out-ring submit.
        //
        // This replaces the "mpv_sdl" entry, which invoked /mpv_sdl -- a
        // binary that is not in the guest -- and expected "MPV_SDL_PASS",
        // which nothing in the tree prints. It therefore failed on every run
        // for a reason that had nothing to do with the GPU, and it left the
        // Venus submit path with no coverage at all.
        //
        // The out-ring submit is the part that matters: a Venus client
        // declares two rings and signals its swapchain image on the "out"
        // ring. The kernel used to validate the ring index against a single
        // ring, so every such submit was rejected with -EINVAL and the
        // client lost the device on its first frame.
        TestCase {
            name: "venus_probe",
            command: "/vkprobe",
            expected: &[
                "VENUS_FOUND=1",
                "VENUS_RING1_OK",
            ],
            timeout_secs: 60,
            gl_only: true,
        },
    ]
}

/// Start a tiny host HTTP fixture for `net_probe`.
///
/// The guest reaches the host as `10.0.2.2` under QEMU user-mode networking
/// (SLIRP maps that address to the host's loopback), so binding
/// `127.0.0.1:18080` serves the guest's `GET /token.txt`. The response body
/// carries the `NETTOKEN` marker `initrd/netprobe.c` looks for.
///
/// Runs on a detached thread for the lifetime of the harness process. Binding
/// failure is reported (and returned as `false`) but is not fatal: `net_probe`
/// then fails exactly as it did before the fixture existed, with the guest log
/// showing `NETPROBE_FAIL: connect`.
fn spawn_net_fixture() -> bool {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = match TcpListener::bind(("127.0.0.1", 18080)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[net_fixture] bind 127.0.0.1:18080 failed: {e}");
            return false;
        }
    };
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Drain the request line/headers (contents don't matter), then
            // reply and close. Content-Length + Connection: close means the
            // guest's read-until-EOF loop terminates cleanly.
            let mut req = [0u8; 1024];
            let _ = stream.read(&mut req);
            let body = "NETTOKEN\n";
            let resp = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    true
}

/// One-time guard so the fixture binds at most once per process.
static NET_FIXTURE: std::sync::Once = std::sync::Once::new();

pub fn run_all_tests(root: &Path, target: Option<&str>, verbose: bool) -> Result<(), String> {
    let all = all_tests();
    
    // Filter by target if specified - collect into owned Vec
    let tests_to_run: Vec<TestCase> = if let Some(t) = target {
        all.into_iter().filter(|tc| tc.name == t).collect()
    } else {
        all
    };
    
    if tests_to_run.is_empty() {
        return Err(format!("No test found matching '{}'", target.unwrap()));
    }
    
    let mut pass_count = 0;
    let mut fail_count = 0;
    let mut skip_count = 0;
    
    for tc in tests_to_run {
        // Check if test requires GL and we don't have it
        if tc.gl_only && std::env::var("DRAGOS_TEST_GL").is_err() {
            println!("[SKIP] {} (requires --gl)", tc.name);
            skip_count += 1;
            continue;
        }
        
        // net_probe expects a host HTTP fixture on 10.0.2.2:18080; bring it up
        // on demand (the guest reaches the host through SLIRP).
        if tc.name == "net_probe" {
            NET_FIXTURE.call_once(|| {
                spawn_net_fixture();
            });
        }

        println!("Running Test [{}]", tc.name);
        println!(" Command: {}", tc.command);
        
        // Create test mode flag in EFI partition (esp/efi/boot/test_mode)
        // and on the root disk (/test_mode). The ESP is not mountable from
        // the guest (no vfat driver, mount(2) previously returned ENOSYS),
        // so /efi/efi/boot/test_mode is unreachable. Writing /test_mode to
        // disk.img via debugfs is the reliable path; draginit detects it and
        // redirects the shell to /dev/ttyS0 so test output reaches COM1.
        let test_mode_path = root.join("esp/efi/boot/test_mode");
        let _ = std::fs::create_dir_all(test_mode_path.parent().unwrap());
        std::fs::write(&test_mode_path, tc.command).map_err(|e| format!("Failed to write test_mode: {}", e))?;
        // Also write /test_mode to disk.img so draginit sees it.
        let disk_img = root.join("disk.img");
        if disk_img.exists() {
            let _ = std::process::Command::new("debugfs")
                .arg("-w")
                .arg("-R")
                .arg(format!("write {} /test_mode", test_mode_path.display()))
                .arg(disk_img)
                .output();
        }
        
        // Configure QEMU for headless test
        let mut config = QemuConfig::default();
        config.headless = true;
        config.memory = "2G".to_string();
        config.smp = 2;
        
        // Enable GL if required
        if tc.gl_only {
            config.gl = true;
            unsafe { std::env::set_var("DRAGOS_TEST_GL", "1"); }
        }
        
        // Spawn QEMU
        let mut child = config.spawn_headless(root).map_err(|e| format!("Failed to spawn QEMU: {}", e))?;
        
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        
        let (tx, rx) = mpsc::channel();
        
        // Read stdout
        let tx_stdout = tx.clone();
        let name = tc.name;
        let verbose_clone = verbose;
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                if let Ok(line) = line {
                    if verbose_clone {
                        println!("[{}] {}", name, line);
                    }
                    let _ = tx_stdout.send(line);
                }
            }
        });
        
        // Read stderr
        let tx_stderr = tx.clone();
        let name = tc.name;
        let verbose_clone = verbose;
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                if let Ok(line) = line {
                    if verbose_clone {
                        eprintln!("[{}] ERR: {}", name, line);
                    }
                    let _ = tx_stderr.send(line);
                }
            }
        });
        
        // Wait for SHELL_READY marker, then send the test command
        let start = Instant::now();
        let timeout = Duration::from_secs(tc.timeout_secs);
        let mut found = vec![false; tc.expected.len()];
        let mut timed_out = false;
        let mut child_dropped = false;
        let mut command_sent = false;
        
        while start.elapsed() < timeout {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    // Check for SHELL_READY marker
                    if !command_sent && line.contains("[SHELL_READY]") {
                        // Send the test command
                        let cmd = format!("{}\n", tc.command);
                        let _ = stdin.write_all(cmd.as_bytes());
                        let _ = stdin.flush();
                        command_sent = true;
                    }
                    
                    for (i, expected) in tc.expected.iter().enumerate() {
                        if line.contains(expected) {
                            found[i] = true;
                        }
                    }
                    // Check if all expected strings found
                    if found.iter().all(|&f| f) {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if start.elapsed() >= timeout {
                        timed_out = true;
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    child_dropped = true;
                    break;
                }
            }
        }
        
        // Kill QEMU
        let _ = child.kill();
        let _ = child.wait();
        
        // Clean up test_mode
        let _ = std::fs::remove_file(&test_mode_path);
        
        let all_found = found.iter().all(|&f| f);
        
        if timed_out {
            println!("\x1b[31m[FAIL] {} - timed out after {}s\x1b[0m", tc.name, tc.timeout_secs);
            fail_count += 1;
        } else if child_dropped && !all_found {
            println!("\x1b[31m[FAIL] {} - QEMU exited unexpectedly\x1b[0m", tc.name);
            fail_count += 1;
        } else if all_found {
            println!("\x1b[32m[PASS] Test '{}' passed cleanly\x1b[0m", tc.name);
            pass_count += 1;
        } else {
            println!("\x1b[31m[FAIL] {} - expected output not found\x1b[0m", tc.name);
            for (i, expected) in tc.expected.iter().enumerate() {
                if !found[i] {
                    println!("  Missing: {}", expected);
                }
            }
            fail_count += 1;
        }
    }
    
    println!("\n=== Test Summary ===");
    println!("Passed: {}", pass_count);
    println!("Failed: {}", fail_count);
    println!("Skipped: {}", skip_count);
    
    if fail_count > 0 {
        return Err(format!("{} test(s) failed", fail_count));
    }
    
    Ok(())
}
