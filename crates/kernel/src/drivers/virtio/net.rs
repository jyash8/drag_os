// ponytail: 2 queues (rx 0, tx 1), no mergeable, no offload, single MAC
// ponytail: single global SpinLock, O(1) rx/tx, repost rx on poll

use core::ptr::{copy_nonoverlapping, read_volatile};

use shared::memory::address::VirtAddr;

use crate::arch::x86_64::pci::find_device;
use crate::arch::sync::spinlock::SpinLock;

use super::{
    find_cap, CommonCfg, DmaBuf, VirtQueue, CAP_COMMON_CFG, CAP_DEVICE_CFG, CAP_NOTIFY_CFG,
    REG_DEVICE_FEATURE, REG_DEVICE_FEATURE_SELECT, REG_DEVICE_STATUS, REG_DRIVER_FEATURE,
    REG_DRIVER_FEATURE_SELECT, REG_QUEUE_ENABLE, REG_QUEUE_SELECT, STATUS_ACKNOWLEDGE,
    STATUS_DRIVER, STATUS_DRIVER_OK, STATUS_FEATURES_OK, VIRTIO_DEVICE_NET,
    VIRTIO_DEVICE_NET_LEGACY, VIRTIO_VENDOR_ID, VIRTQ_DESC_F_WRITE,
};

const RX_BUF_SIZE: usize = 2048; // 1514 + virtio_net_hdr + pad, backed by one page
const RX_BUFS: usize = 32;
// TEMP-DIAG: trying 12 (mrg_rxbuf-sized) header — pcap shows QEMU eats 2 frame
// bytes on TX, i.e. it parses a 12-byte vnet header, not 10.
pub const VIRTIO_NET_HDR: usize = 12;

pub(crate) struct NetDevice {
    rx_queue: VirtQueue,
    tx_queue: VirtQueue,
    rx_bufs: alloc::vec::Vec<DmaBuf>,
    tx_buf: DmaBuf,
    #[allow(dead_code)]
    next_rx: usize,
    mac: [u8; 6],
}

impl NetDevice {
    fn init() -> Option<Self> {
        let addr = find_device(VIRTIO_VENDOR_ID, VIRTIO_DEVICE_NET)
            .or_else(|| find_device(VIRTIO_VENDOR_ID, VIRTIO_DEVICE_NET_LEGACY))?;

        let common_cap = find_cap(&addr, CAP_COMMON_CFG)?;
        let notify_cap = find_cap(&addr, CAP_NOTIFY_CFG)?;
        let device_cap = find_cap(&addr, CAP_DEVICE_CFG)?;

        let mut bar_virts: [Option<VirtAddr>; 8] = [None; 8];
        let mut get_bar = |i: u8| -> Option<VirtAddr> {
            let idx = i as usize;
            if let Some(v) = bar_virts[idx] {
                return Some(v);
            }
            let (v, _) = addr.map_bar(u32::from(i)).ok()?;
            bar_virts[idx] = Some(v);
            Some(v)
        };

        let common = CommonCfg::new(VirtAddr::new(
            get_bar(common_cap.bar)?.0 + u64::from(common_cap.offset),
        ));
        let notify = VirtAddr::new(get_bar(notify_cap.bar)?.0 + u64::from(notify_cap.offset));
        let device_cfg = VirtAddr::new(get_bar(device_cap.bar)?.0 + u64::from(device_cap.offset));

        unsafe { common.write_u8(REG_DEVICE_STATUS, STATUS_ACKNOWLEDGE) };

        let dev_hi = unsafe {
            common.write_u32(REG_DEVICE_FEATURE_SELECT, 1);
            common.read_u32(REG_DEVICE_FEATURE)
        };
        let _dev_lo = unsafe {
            common.write_u32(REG_DEVICE_FEATURE_SELECT, 0);
            common.read_u32(REG_DEVICE_FEATURE)
        };
        if dev_hi & 1 == 0 {
            return None; // need VIRTIO_F_VERSION_1
        }
        // ponytail: clear MRG_RXBUF(15), GSO etc — keep only VERSION_1
        unsafe {
            common.write_u32(REG_DRIVER_FEATURE_SELECT, 1);
            common.write_u32(REG_DRIVER_FEATURE, 1);
            common.write_u32(REG_DRIVER_FEATURE_SELECT, 0);
            common.write_u32(REG_DRIVER_FEATURE, 0);
        }

        unsafe {
            common.write_u8(
                REG_DEVICE_STATUS,
                STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK,
            );
        }
        let status = unsafe { common.read_u8(REG_DEVICE_STATUS) };
        if status & STATUS_FEATURES_OK == 0 {
            return None;
        }

        // rx queue 0
        unsafe { common.write_u16(REG_QUEUE_SELECT, 0) };
        let mut rx_queue = VirtQueue::new(&common, notify, notify_cap.notify_off_multiplier)?;
        unsafe { common.write_u16(REG_QUEUE_ENABLE, 1) };

        // tx queue 1
        unsafe { common.write_u16(REG_QUEUE_SELECT, 1) };
        let tx_queue = VirtQueue::new(&common, notify, notify_cap.notify_off_multiplier)?;
        unsafe { common.write_u16(REG_QUEUE_ENABLE, 1) };

        unsafe {
            common.write_u8(
                REG_DEVICE_STATUS,
                STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
            );
        }

        // device config: MAC at offset 0 (6 bytes)
        let mut mac = [0u8; 6];
        for i in 0..6 {
            mac[i] = unsafe { read_volatile((device_cfg.0 + i as u64) as *const u8) };
        }

        let mut rx_bufs = alloc::vec::Vec::with_capacity(RX_BUFS);
        for _ in 0..RX_BUFS {
            rx_bufs.push(DmaBuf::new(RX_BUF_SIZE)?);
        }
        let tx_buf = DmaBuf::new(RX_BUF_SIZE)?;

        // fill rx: post all RX_BUFS into the RX queue
        for i in 0..RX_BUFS {
            rx_queue.post_buffer(
                i as u16,
                rx_bufs[i].phys().0,
                RX_BUF_SIZE as u32,
                VIRTQ_DESC_F_WRITE,
            );
        }
        rx_queue.notify();

        crate::dprintln!(
            "[virtio-net] init ok mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        );

        Some(Self {
            rx_queue,
            tx_queue,
            rx_bufs,
            tx_buf,
            next_rx: 0,
            mac,
        })
    }

    fn poll_rx(&mut self) -> Option<(usize, usize)> {
        while let Some((id, len)) = self.rx_queue.try_pop_used() {
            RX_HIT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let buf_id = id as usize;
            if buf_id < self.rx_bufs.len() {
                if (len as usize) > VIRTIO_NET_HDR {
                    return Some((buf_id, len as usize));
                } else {
                    // Malformed or empty, repost immediately
                    self.repost_rx(buf_id);
                }
            }
        }
        None
    }

    fn repost_rx(&mut self, buf_id: usize) {
        if buf_id < self.rx_bufs.len() {
            self.rx_queue.post_buffer(
                buf_id as u16,
                self.rx_bufs[buf_id].phys().0,
                RX_BUF_SIZE as u32,
                VIRTQ_DESC_F_WRITE,
            );
            self.rx_queue.notify();
        }
    }

    fn send_bytes(&mut self, buf: &[u8]) -> bool {
        TX_ATTEMPT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if buf.len() + VIRTIO_NET_HDR > self.tx_buf.len() {
            return false;
        }
        unsafe {
            core::ptr::write_bytes(self.tx_buf.as_mut_ptr(), 0, VIRTIO_NET_HDR);
            copy_nonoverlapping(buf.as_ptr(), self.tx_buf.as_mut_ptr().add(VIRTIO_NET_HDR), buf.len());
        }
        let parts = [(self.tx_buf.phys().0, (buf.len() + VIRTIO_NET_HDR) as u32, 0u16)];
        if self.tx_queue.push(&parts).is_err() {
            return false;
        }
        // ponytail: blocking wait, single-flight; smoltcp calls from timer so bounded
        let ok = self.tx_queue.wait().is_ok();
        if ok {
            TX_OK.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        ok
    }
}

pub(crate) static NET_DEVICE: SpinLock<Option<NetDevice>> = SpinLock::new(None);

// ponytail: net activity counters, printed in the 8s timer stats line.
pub static TX_ATTEMPT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static TX_OK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static RX_HIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn init() {
    if let Some(dev) = NetDevice::init() {
        *NET_DEVICE.lock() = Some(dev);
    } else {
        crate::dprintln!("[virtio-net] init FAILED or not present");
    }
}

pub fn poll_rx() -> Option<(usize, usize)> {
    let mut g = NET_DEVICE.try_lock()?;
    let dev = g.as_mut()?;
    dev.poll_rx()
}

pub fn repost_rx(buf_id: usize) {
    if let Some(mut g) = NET_DEVICE.try_lock() {
        if let Some(dev) = g.as_mut() {
            dev.repost_rx(buf_id);
        }
    }
}

pub fn with_rx_buf<R, F: FnOnce(&mut [u8]) -> R>(buf_id: usize, f: F) -> Option<R> {
    let mut g = NET_DEVICE.try_lock()?;
    let dev = g.as_mut()?;
    let buf = dev.rx_bufs.get_mut(buf_id)?;
    Some(f(unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.len()) }))
}

pub fn poll() -> Option<&'static [u8]> {
    None
}

pub fn send(buf: &[u8]) -> bool {
    let mut g = match NET_DEVICE.try_lock() {
        Some(g) => g,
        None => return false,
    };
    let Some(dev) = g.as_mut() else { return false };
    dev.send_bytes(buf)
}

pub fn mac() -> Option<[u8; 6]> {
    NET_DEVICE.lock().as_ref().map(|d| d.mac)
}

