//! Thread settings (no file involved)

/// Calling thread → background priority: CPU nice 10, I/O best-effort level 7 (merges yield disk
/// + cores to serving reads)
///
/// - hint: failures ignored
/// - I/O class honoured by BFQ / mq-deadline only (`none`, common on NVMe, ignores it)
/// - sound: plain syscalls on the calling thread's own id, no memory passed
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(crate) fn background_priority() {
    const IOPRIO_WHO_PROCESS: libc::c_int = 1;
    const IOPRIO_CLASS_BE: libc::c_int = 2;
    const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
    const LOWEST_BE_LEVEL: libc::c_int = 7;
    const NICE: libc::c_int = 10;

    unsafe {
        let thread = libc::gettid();
        let io_priority = (IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | LOWEST_BE_LEVEL;
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, thread, io_priority);
        libc::setpriority(libc::PRIO_PROCESS as _, thread as _, NICE);
    }
}

/// No thread priorities off Linux
#[cfg(not(target_os = "linux"))]
pub(crate) fn background_priority() {}
