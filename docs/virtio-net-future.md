# Virtio-Net Future Work

> **Status:** Skeleton landed in `kernel/src/drivers/virtio/net.rs:1-218` (2 queues rx0/tx1, 32×2048 rx, `VERSION_1` only). QEMU `user,id=n0` + `virtio-net-pci` wired in `tools/src/qemu.rs:51` and PCI probe `kernel/src/arch/x86_64/pci.rs:283`. `curl` + `libcurl` staged in `scripts/build-fs.sh:70,530` and `disk.img`. Boots with `[virtio-net] init ok mac=52:54:00:12:34:56`.

**Goal:** Real TCP via smoltcp `10.0.2.15/24 gw 10.0.2.2` with `curl http://example.com` working over host user net, not just static DNS stub.

**Extra work deferred (ponytail):**
- `kernel/src/net/mod.rs:1` - Replace `Option<SocketSet>` with proper `SocketSet` sharding per-CPU, DHCP instead of static `10.0.2.15`, ARP table, `Device::transmit` actually calls `virtio::net::send` and `poll` reposts rx bufs (currently `poll()` returns `None`, `send()` copies hdr but never notifies host beyond `push`).
- `kernel/src/drivers/virtio/net.rs:1` - Multi-buf rx, `VIRTQ_DESC_F_WRITE` hdr handling, `try_poll` non-blocking, MSI-X/IRQ vs polling, `NET_DEVICE` per-queue locks if throughput >1Gbps.
- `kernel/src/arch/x86_64/syscalls/net.rs:147` - Full `UdpSocket` via smoltcp (currently static `dns_lookup` table `93.184.216.34` fallback), `AF_INET6`, `SO_REUSEADDR`, `TCP_NODELAY`, `shutdown 48`, `poll` `EPOLLHUP` for Inet.
- `kernel/src/fs/vfs.rs:139` - `VNode::InetSocket` per-socket wait queue (currently global `PIPE_WAIT_QUEUE` coarse).
- `scripts/build-fs.sh:70` - `ca-certificates` already staged, but need `iproute2`/`udhcpc` for lease.

**Upgrade path:** When `curl 10.0.2.2` needs real gateway, wire `virtio::net::poll` → `smoltcp::phy::Device::receive` → `Interface::poll` in `kernel/src/time.rs:206`, then replace DNS stub with `smoltcp::socket::DnsSocket`.

**Tech stack:** smoltcp 0.11 `alloc` no_std, virtio-net split virtqueue (spec 5.1).

**Est. effort:** 3-5 days for full TCP, 1 day for RX repost + DNS.
