use super::*;

pub(super) fn sys_gettimeofday(arg1: u64) -> u64 {
    let tv = arg1 as *mut u8;
    if !validate_user_write_range(tv as u64, 16) {
        return EFAULT;
    }
    let ns = crate::time::realtime_nanos();
    let sec = ns.div_euclid(1_000_000_000);
    let usec = ns.rem_euclid(1_000_000_000) / 1_000;
    let buf = unsafe { core::slice::from_raw_parts_mut(tv, 16) };
    crate::time::pack_timeval(buf, sec, usec);
    0
}

pub(super) fn sys_clock_gettime(clockid: i32, arg2: u64) -> u64 {
    let tp = arg2 as *mut u8;
    if !validate_user_write_range(tp as u64, 16) {
        return EFAULT;
    }
    let buf = unsafe { core::slice::from_raw_parts_mut(tp, 16) };
    match clockid {
        0 | 5 => {
            // CLOCK_REALTIME (0), CLOCK_REALTIME_COARSE (5)
            let ns = crate::time::realtime_nanos();
            let sec = ns.div_euclid(1_000_000_000);
            let nsec = ns.rem_euclid(1_000_000_000);
            crate::time::pack_timespec(buf, sec, nsec);
            0
        }
        1 | 4 | 6 | 7 => {
            // CLOCK_MONOTONIC (1), CLOCK_MONOTONIC_RAW (4), CLOCK_MONOTONIC_COARSE (6), CLOCK_BOOTTIME (7)
            let ticks = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed);
            let ns = crate::time::monotonic_nanos(ticks) as i64;
            let sec = ns.div_euclid(1_000_000_000);
            let nsec = ns.rem_euclid(1_000_000_000);
            crate::time::pack_timespec(buf, sec, nsec);
            0
        }
        _ => EINVAL,
    }
}

pub(super) fn sys_clock_getres(clockid: i32, arg2: u64) -> u64 {
    let tp = arg2 as *mut u8;
    if (tp as u64) != 0 && !validate_user_write_range(tp as u64, 16) {
        return EFAULT;
    }
    if (tp as u64) != 0 {
        let buf = unsafe { core::slice::from_raw_parts_mut(tp, 16) };
        match clockid {
            0 | 1 | 4 | 5 | 6 | 7 => {
                crate::time::pack_timespec(buf, 0, 1_000_000);
                0
            }
            _ => EINVAL,
        }
    } else {
        0
    }
}

