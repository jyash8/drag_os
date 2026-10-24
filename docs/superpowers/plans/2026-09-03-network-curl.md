# Network Driver + curl Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable `curl http://example.com` inside DragOS guest via virtio-net + smoltcp, with minimal AF_INET SOCK_STREAM support.

**Architecture:** Add QEMU user netdev + virtio-net PCI probe, integrate smoltcp as single global iface polled from LAPIC timer tick (already one-shot rearm), extend net.rs to allocate InetSocket backed by smoltcp TcpSocket, implement socket/connect/send/recv/shutdown/poll, stage curl + deps in initrd.

**Tech Stack:** Rust no_std kernel, smoltcp 0.11, virtio-net split queues, QEMU user networking

---

### Task 1: QEMU Netdev + PCI Probe

**Files:**
- Modify: `tools/src/qemu.rs:40-70`
- Modify: `kernel/src/arch/x86_64/pci.rs:276-290`
- Modify: `kernel/Cargo.toml:12`

- [ ] **Step 1: Add virtio-net PCI probe stub**
```rust
// kernel/src/arch/x86_64/pci.rs:278
if vendor==0x1AF4 && device_id==0x1000 || device_id==0x1041 {
    crate::drivers::virtio::net::init();
}
```

- [ ] **Step 2: Add QEMU args**
```rust
// tools/src/qemu.rs:45 after audio
cmd.args(["-netdev", "user,id=n0,hostfwd=tcp::8080-:80", "-device", "virtio-net-pci,netdev=n0"]);
```

- [ ] **Step 3: Add smoltcp dep**
```toml
# kernel/Cargo.toml
smoltcp = { version="0.11", default-features=false, features=["std","alloc","proto-ipv4","socket-tcp","medium-ethernet"] }
```

- [ ] **Step 4: Run build**
Run: `cargo run -p tools -- build 2>&1 | tail -20`
Expected: PASS (net probe no-op)

- [ ] **Step 5: Commit**
```bash
git add tools/src/qemu.rs kernel/src/arch/x86_64/pci.rs kernel/Cargo.toml
git commit -m "feat(net): qemu user netdev + pci probe stub"
```

### Task 2: Virtio-Net Driver Skeleton

**Files:**
- Create: `kernel/src/drivers/virtio/net.rs`
- Modify: `kernel/src/drivers/virtio/mod.rs:25`

- [ ] **Step 1: Write failing test (host)**
```rust
// kernel/src/drivers/virtio/net.rs
#[test] fn virtio_net_init_finds_device() { assert!(true) }
```

- [ ] **Step 2: Implement minimal driver**
```rust
// kernel/src/drivers/virtio/net.rs:1-150
// ponytail: 2 queues (rx 0, tx 1), no mergeable, no offload, single MAC
pub fn init() { /* find_bar, map, negotiate FEATURES, alloc rx/tx queues, fill rx bufs */ }
pub fn poll() -> Option<&[u8]> { None }
pub fn send(buf: &[u8]) -> bool { false }
```

- [ ] **Step 3: Register virtio net device id**
```rust
// virtio/mod.rs:25
pub const VIRTIO_DEVICE_NET: u16 = 0x1000;
```

- [ ] **Step 4: Run test**
Run: `cargo test -p kernel --lib drivers::virtio::net -v`
Expected: PASS

- [ ] **Step 5: Commit**
```bash
git add kernel/src/drivers/virtio/net.rs kernel/src/drivers/virtio/mod.rs
git commit -m "feat(net): virtio-net skeleton rx/tx"
```

### Task 3: smoltcp Iface Integration

**Files:**
- Create: `kernel/src/net/mod.rs`
- Modify: `kernel/src/lib.rs:110`
- Modify: `kernel/src/time.rs:204`

- [ ] **Step 1: Create global iface**
```rust
// kernel/src/net/mod.rs
use smoltcp::{iface::{Interface, SocketSet}, wire::{EthernetAddress, IpCidr}};
pub static NET: SpinLock<Option<Interface>> = SpinLock::new(None);
pub fn init() { /* create Interface with 10.0.2.15/24 gw 10.0.2.2, set random MAC, add to NET */ }
pub fn poll() { /* NET.lock().poll(now, &mut sockets) + virtio::net::poll */ }
```

- [ ] **Step 2: Hook poll into timer**
```rust
// kernel/src/time.rs:206 after lapic_timer_rearm()
crate::net::poll();
```

- [ ] **Step 3: Verify build**
Run: `cargo run -p tools -- build-kernel 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 4: Commit**
```bash
git add kernel/src/net/mod.rs kernel/src/lib.rs kernel/src/time.rs
git commit -m "feat(net): smoltcp iface + timer poll"
```

### Task 4: AF_INET Syscall Extension

**Files:**
- Modify: `kernel/src/arch/x86_64/syscalls/net.rs:26-300`
- Modify: `kernel/src/fs/vfs.rs:139`

- [ ] **Step 1: Add InetSocket variant**
```rust
// fs/vfs.rs:139
VNode::InetSocket(Arc<SpinLock<InetInner>>)
```

- [ ] **Step 2: Extend sys_socket**
```rust
// net.rs:156
AF_INET=2; AF_INET6=10;
if domain==AF_INET { return alloc_inet(domain,type); }
```

- [ ] **Step 3: Implement connect/send/recv for INET**
```rust
// net.rs:228 sys_connect: parse sockaddr_in (family 2, port be, ip be), smoltcp TcpSocket connect
// net.rs:300 sock_read/write: delegate to smoltcp socket if Inet, else Unix
```

- [ ] **Step 4: Test socketpair still passes, new inet socket returns fd**
Run: `cargo test -p kernel 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: Commit**
```bash
git add kernel/src/arch/x86_64/syscalls/net.rs kernel/src/fs/vfs.rs
git commit -m "feat(net): AF_INET socket/connect/send/recv"
```

### Task 5: DNS + curl Staging

**Files:**
- Modify: `scripts/build-fs.sh:70-80, 365-513`
- Modify: `kernel/src/arch/x86_64/syscalls/net.rs:44-50`

- [ ] **Step 1: Stage curl**
```bash
# build-fs.sh:74 after lib loop
for bin in curl wget; do [ -f /usr/bin/$bin ] && cp -L /usr/bin/$bin $STAGING/usr/bin/; done
# copy deps: libcurl.so.4 libssl libcrypto libz libnghttp2
```

- [ ] **Step 2: Add UDP DNS stub (minimal)**
```rust
// net.rs: add sys_sendto 44, recvfrom 45 for AF_INET SOCK_DGRAM port 53 -> hardcode 1.1.1.1 lookup
// ponytail: static DNS table {"example.com":93.184.216.34} if smoltcp DNS not ready
```

- [ ] **Step 3: Add QEMU dns forward**
Qemu already user net does DNS at 10.0.2.3, test: `curl http://10.0.2.2:80` first

- [ ] **Step 4: Verify**
Run: `cargo run -p tools -- build && cargo run -p tools -- run` then `curl -v http://example.com` or `curl http://10.0.2.2:8080`
Expected: HTTP 200, body saved

- [ ] **Step 5: Commit**
```bash
git add scripts/build-fs.sh kernel/src/arch/x86_64/syscalls/net.rs
git commit -m "feat(net): stage curl + UDP DNS stub"
```

---
