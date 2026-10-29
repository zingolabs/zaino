//! Thread settings (no file involved)

/// Calling thread → background priority: CPU nice 10, I/O best-effort level 7 (merges yield disk
/// + cores to serving reads)
///
/// - hint: failures ignored
/// - I/O class honoured by BFQ / mq-deadline only (`none`, common on NVMe, ignores it)
#[cfg(target_os = "linux")]
pub(crate) fn background_priority() {
    const NICE: i32 = 10;

    let thread = rustix::thread::gettid();
    io_priority_lowest(thread);
    let _ = rustix::process::setpriority_process(Some(thread), NICE);
}

/// `ioprio_set(IOPRIO_WHO_PROCESS, thread, best-effort level 7)` (no rustix wrapper)
///
/// - sound: plain syscall on a thread id, no memory passed
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn io_priority_lowest(thread: rustix::process::Pid) {
    const IOPRIO_WHO_PROCESS: libc::c_int = 1;
    const IOPRIO_CLASS_BE: libc::c_int = 2;
    const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
    const LOWEST_BE_LEVEL: libc::c_int = 7;

    let io_priority = (IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | LOWEST_BE_LEVEL;
    unsafe {
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, thread.as_raw_pid(), io_priority);
    }
}

/// No thread priorities off Linux
#[cfg(not(target_os = "linux"))]
pub(crate) fn background_priority() {}
