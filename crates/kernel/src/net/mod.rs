// ponytail: single global SocketSet, 10.0.2.15/24 gw 10.0.2.2, MAC 52:54:00:12:34:56
// ponytail: single global Interface + SocketSet, static IP, no DHCP, smoltcp default ARP
use alloc::vec;
use core::sync::atomic::Ordering;

use smoltcp::{
    iface::{Config, Interface, SocketSet, SocketStorage},
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    time::Instant,
    wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address},
};

use crate::arch::sync::spinlock::SpinLock;

pub static NET: SpinLock<Option<Interface>> = SpinLock::new(None);
// ponytail: Option wrapper because SocketSet::new is not const — init() replaces None with Some
pub static SOCKETS: SpinLock<Option<SocketSet<'static>>> = SpinLock::new(None);

// smoltcp Device wrapping virtio-net
struct VirtioNetDevice;

struct VirtioRxToken {
    buf_id: usize,
    len: usize,
}
struct VirtioTxToken;

impl RxToken for VirtioRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // ponytail: copy out under the driver lock, then RELEASE it before f.
        // f drives smoltcp, which may transmit a reply (ARP/SYN-ACK) through
        // this same NET_DEVICE lock — holding it across f made every reply's
        // try_lock fail, silently dropping it (tx stayed 0 while RX worked).
        let mut copied = crate::drivers::virtio::net::with_rx_buf(self.buf_id, |buf| {
            buf[crate::drivers::virtio::net::VIRTIO_NET_HDR..self.len].to_vec()
        })
        .expect("rx buf invalid");
        let r = f(&mut copied);
        crate::drivers::virtio::net::repost_rx(self.buf_id);
        r
    }
}

impl TxToken for VirtioTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf[..len]);
        // virtio-net adds its own header internally
        let _ = crate::drivers::virtio::net::send(&buf[..len]);
        r
    }
}

impl Device for VirtioNetDevice {
    type RxToken<'a> = VirtioRxToken where Self: 'a;
    type TxToken<'a> = VirtioTxToken where Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let (buf_id, len) = crate::drivers::virtio::net::poll_rx()?;
        Some((VirtioRxToken { buf_id, len }, VirtioTxToken))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(VirtioTxToken)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps.max_burst_size = Some(32);
        caps
    }
}

pub fn init() {
    let mac_bytes = crate::drivers::virtio::net::mac().unwrap_or([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    let hw = HardwareAddress::Ethernet(EthernetAddress(mac_bytes));
    let config = Config::new(hw);
    let mut device = VirtioNetDevice;
    let mut iface = Interface::new(config, &mut device, Instant::from_millis(0));
    iface
        .update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24));
        });
    let _ = iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2));
    *NET.lock() = Some(iface);
    *SOCKETS.lock() = Some(SocketSet::new(vec![] as alloc::vec::Vec<SocketStorage<'static>>));
}

pub fn poll() {
    // timer holds no locks — NET/SOCKETS are innermost, safe; try_lock avoids deadlock if re-entered
    let Some(mut net_guard) = NET.try_lock() else { return };
    let Some(iface) = net_guard.as_mut() else { return };
    let Some(mut sock_guard) = SOCKETS.try_lock() else { return };
    let Some(sockets) = sock_guard.as_mut() else { return };
    let mut device = VirtioNetDevice;
    let now = Instant::from_millis(crate::time::TICKS.load(Ordering::Relaxed) as i64);
    let activity = iface.poll(now, &mut device, sockets);
    if activity {
        crate::tasks::wake_pipe_waiters();
    }
}
