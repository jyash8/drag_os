use std::path::{Path, PathBuf};
use std::process::Command;

pub fn project_root() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap());
    let mut p = manifest_dir;
    while p.ends_with("tools") || p.ends_with("crates") {
        p = p.parent().unwrap().to_path_buf();
    }
    p
}

pub fn build_fs(root: &Path) -> Result<(), String> {
    println!("==> [tools] Building filesystem (initrd.tar & disk.img)");
    let script = root.join("scripts/build-fs.sh");
    if !script.exists() {
        return Err(format!("Script not found: {}", script.display()));
    }
    let status = Command::new("bash")
        .arg(&script)
        .current_dir(root)
        .status()
        .map_err(|e| format!("Failed to execute build-fs.sh: {}", e))?;

    if status.success() {
        println!("==> [tools] Filesystem build successful");
        Ok(())
    } else {
        Err(format!("build-fs.sh failed with exit status: {}", status))
    }
}

pub fn build_kernel(root: &Path) -> Result<(), String> {
    println!("==> [tools] Building DragOS kernel (release, x86_64-unknown-none)");
    let args = vec![
        "build",
        "--release",
        "-p",
        "kernel",
        "--target",
        "x86_64-unknown-none",
    ];
    let status = Command::new("cargo")
        .args(&args)
        .current_dir(root)
        .status()
        .map_err(|e| format!("Failed to run cargo build for kernel: {}", e))?;

    if !status.success() {
        return Err(format!("Kernel cargo build failed with exit status: {}", status));
    }

    let src = root.join("target/x86_64-unknown-none/release/kernel");
    let esp_dir = root.join("esp/efi/boot");
    std::fs::create_dir_all(&esp_dir)
        .map_err(|e| format!("Failed to create esp directory {}: {}", esp_dir.display(), e))?;

    let dst_kernel = esp_dir.join("kernel");
    let dst_elf = esp_dir.join("kernel.elf");

    std::fs::copy(&src, &dst_kernel)
        .map_err(|e| format!("Failed to copy kernel to {}: {}", dst_kernel.display(), e))?;
    std::fs::copy(&src, &dst_elf)
        .map_err(|e| format!("Failed to copy kernel to {}: {}", dst_elf.display(), e))?;

    println!("==> [tools] Kernel deployed to {}", dst_elf.display());
    Ok(())
}

pub fn build_bootloader(root: &Path) -> Result<(), String> {
    println!("==> [tools] Building Drago bootloader (release, x86_64-unknown-uefi)");
    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "-p",
            "bootloader",
            "--target",
            "x86_64-unknown-uefi",
        ])
        .current_dir(root)
        .status()
        .map_err(|e| format!("Failed to run cargo build for bootloader: {}", e))?;

    if !status.success() {
        return Err(format!("Bootloader cargo build failed with exit status: {}", status));
    }

    let src = root.join("target/x86_64-unknown-uefi/release/bootloader.efi");
    let esp_dir = root.join("esp/efi/boot");
    std::fs::create_dir_all(&esp_dir)
        .map_err(|e| format!("Failed to create esp directory {}: {}", esp_dir.display(), e))?;

    let dst = esp_dir.join("bootx64.efi");
    std::fs::copy(&src, &dst)
        .map_err(|e| format!("Failed to copy bootloader to {}: {}", dst.display(), e))?;

    println!("==> [tools] Bootloader deployed to {}", dst.display());
    Ok(())
}

pub fn build_all(root: &Path) -> Result<(), String> {
    build_fs(root)?;
    build_kernel(root)?;
    build_bootloader(root)?;
    Ok(())
}
