//! kobject-uevent broadcast for NETLINK_KOBJECT_UEVENT (protocol 15) listeners.
//!
//! ponytail: one global datagram queue, no per-socket queues or multicast
//! groups — every bound AF_NETLINK socket dequeues the same stream. Payloads
//! are raw kobject strings (`ACTION@DEVPATH\0ACTION=..\0DEVPATH=..\0K=V\0..`),
//! no netlink header; the P1T4 listener parses them as-is.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::arch::sync::spinlock::SpinLock;

// ponytail: 64-deep cap, drop-new when full (Linux sheds load under ENOBUFS
// pressure too); keeps a hotplug storm from eating the heap.
const UEVENT_QUEUE_CAP: usize = 64;

static UEVENT_QUEUE: SpinLock<VecDeque<Vec<u8>>> = SpinLock::new(VecDeque::new());

fn push_prop(msg: &mut Vec<u8>, k: &str, v: &str) {
    msg.extend_from_slice(k.as_bytes());
    msg.push(b'=');
    msg.extend_from_slice(v.as_bytes());
    msg.push(0);
}

/// Build a kobject-uevent datagram and wake blocked protocol-15 readers.
/// The queue lock is dropped before the wake (producer pattern: data lock,
/// release, then SCHEDULER via wake_pipe_waiters — never nested).
pub fn broadcast(action: &str, devpath: &str, props: &[(&str, &str)]) {
    let mut msg = Vec::new();
    msg.extend_from_slice(action.as_bytes());
    msg.push(b'@');
    msg.extend_from_slice(devpath.as_bytes());
    msg.push(0);
    push_prop(&mut msg, "ACTION", action);
    push_prop(&mut msg, "DEVPATH", devpath);
    for (k, v) in props {
        push_prop(&mut msg, k, v);
    }
    {
        let mut q = UEVENT_QUEUE.lock();
        if q.len() >= UEVENT_QUEUE_CAP {
            return;
        }
        q.push_back(msg);
    }
    crate::tasks::wake_pipe_waiters();
}

/// Dequeue one datagram for a protocol-15 recvfrom. None = queue empty.
pub fn dequeue() -> Option<Vec<u8>> {
    UEVENT_QUEUE.lock().pop_front()
}

/// Non-empty probe for poll/epoll readiness (leaf lock, short hold).
pub fn has_pending() -> bool {
    !UEVENT_QUEUE.lock().is_empty()
}
