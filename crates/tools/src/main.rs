mod builder;
mod qemu;
mod test_runner;

use std::env;
use std::process::exit;

fn print_help() {
    println!(
        r#"DragOS Unified Tool (tools)

USAGE:
    cargo run -p tools -- <COMMAND> [OPTIONS]

COMMANDS:
    build               Build filesystem (initrd), kernel, and bootloader
    build-kernel        Build kernel (release, x86_64-unknown-none)
    build-bootloader    Build bootloader (release, x86_64-unknown-uefi)
    build-fs            Build filesystem staging (initrd.tar & disk.img)
    run / qemu          Launch QEMU (2GB RAM, SMP 2, VirtIO, Audio)
    ssh [ARGS...]       SSH into DragOS kernel over forwarded port 2222
    test [NAME]         Run automated test suite (e.g. `test all`, `test fork_test`)

OPTIONS:
    --gdb               (for run/qemu) Start QEMU with GDB stub on port 1234
    --no-audio          (for run/qemu) Disable audio devices
    --no-gl             (for run/qemu) Software display (virtio-vga + gtk, no virgl)
    --sdl               (for run/qemu) Use SDL display frontend instead of GTK when GL is on
    --trace             (for run/qemu) Enable CPU reset/interrupt tracing
                        (virgl host GPU is always on: virtio-vga-gl + gtk,gl=on)
    --mem <SIZE>        (for run/qemu) Custom memory size (default: 2G)
    --smp <N>           (for run/qemu) Custom SMP core count (default: 2)
    --verbose           (for test) Show full serial output during tests
    --help, -h          Print this help message
"#
    );
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let root = builder::project_root();

    if args.is_empty() {
        print_help();
        return;
    }

    match args[0].as_str() {
        "build" => {
            if let Err(e) = builder::build_all(&root) {
                eprintln!("\x1b[31m[ERROR] {}\x1b[0m", e);
                exit(1);
            }
        }
        "build-kernel" => {
            if let Err(e) = builder::build_kernel(&root) {
                eprintln!("\x1b[31m[ERROR] {}\x1b[0m", e);
                exit(1);
            }
        }
        "build-bootloader" => {
            if let Err(e) = builder::build_bootloader(&root) {
                eprintln!("\x1b[31m[ERROR] {}\x1b[0m", e);
                exit(1);
            }
        }
        "build-fs" => {
            if let Err(e) = builder::build_fs(&root) {
                eprintln!("\x1b[31m[ERROR] {}\x1b[0m", e);
                exit(1);
            }
        }
        "run" | "qemu" => {
            qemu::clean_test_mode(&root);
            let mut config = qemu::QemuConfig::default();
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--gdb" => config.gdb = true,
                    "--no-audio" => config.no_audio = true,
                    // ponytail: virgl-vs-2D isolation for display artifacts
                    // (guest sends identical bytes; only host read path differs).
                    "--no-gl" => config.gl = false,
                    "--sdl" => config.sdl = true,
                    "--trace" => config.trace = true,
                    "--mem" if i + 1 < args.len() => {
                        config.memory = args[i + 1].clone();
                        i += 1;
                    }
                    "--smp" if i + 1 < args.len() => {
                        if let Ok(n) = args[i + 1].parse() {
                            config.smp = n;
                        }
                        i += 1;
                    }
                    other => config.extra_args.push(other.to_string()),
                }
                i += 1;
            }

            if let Err(e) = config.run_interactive(&root) {
                eprintln!("\x1b[31m[ERROR] {}\x1b[0m", e);
                exit(1);
            }
        }
        "ssh" => {
            let dev_key = root.join(".dragos_dev_key");
            if !dev_key.exists() {
                eprintln!("\x1b[33m[WARN] Developer key .dragos_dev_key not found. Generating now...\x1b[0m");
                let status = std::process::Command::new("ssh-keygen")
                    .args(["-t", "ed25519", "-f", dev_key.to_str().unwrap(), "-N", "", "-C", "dragos-dev", "-q"])
                    .status();
                if status.is_err() || !status.unwrap().success() {
                    eprintln!("\x1b[31m[ERROR] Failed to generate developer key.\x1b[0m");
                    exit(1);
                }
            }

            let mut cmd = std::process::Command::new("ssh");
            cmd.arg("-i").arg(&dev_key)
                .args([
                    "-p", "2222",
                    "-o", "StrictHostKeyChecking=no",
                    "-o", "UserKnownHostsFile=/dev/null",
                    "-o", "LogLevel=ERROR",
                "-t",
                ]);

            let pass_through_args = &args[1..];
            if pass_through_args.is_empty() {
                cmd.arg("root@localhost");
            } else {
                // If user specified custom user@host or flags, or remote commands
                let mut has_target = false;
                for a in pass_through_args {
                    if a.contains('@') || a == "localhost" {
                        has_target = true;
                    }
                }
                if !has_target {
                    cmd.arg("root@localhost");
                }
                cmd.args(pass_through_args);
            }

            match cmd.status() {
                Ok(status) => {
                    if let Some(code) = status.code() {
                        exit(code);
                    }
                }
                Err(e) => {
                    eprintln!("\x1b[31m[ERROR] Failed to execute host ssh command: {}\x1b[0m", e);
                    exit(1);
                }
            }
        }
        "test" => {
            let mut target = None;
            let mut verbose = false;

            for arg in &args[1..] {
                if arg == "--verbose" || arg == "-v" {
                    verbose = true;
                } else if arg == "--gl" {
                    unsafe { std::env::set_var("DRAGOS_TEST_GL", "1") };
                } else if !arg.starts_with('-') && target.is_none() {
                    target = Some(arg.as_str());
                }
            }

            if let Err(e) = test_runner::run_all_tests(&root, target, verbose) {
                eprintln!("\x1b[31m[ERROR] {}\x1b[0m", e);
                exit(1);
            }
        }
        "--help" | "-h" | "help" => {
            print_help();
        }
        unknown => {
            eprintln!("\x1b[31mUnknown command: '{}'\x1b[0m\n", unknown);
            print_help();
            exit(1);
        }
    }
}
