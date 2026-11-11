// SPDX-License-Identifier: GPL-3.0-or-later
//! Best-effort scheduling priority for the DSP thread.
//!
//! The DSP thread is the one thread in the recorder that cannot be late: the
//! SDR never pauses, and every millisecond the thread is descheduled comes out
//! of the ring. A `cargo` build running beside a recorder on the same machine
//! stalled it for about two seconds and cost 4.6 M samples — a deeper ring
//! (`[[device]] ring_seconds`) absorbs that, and asking the scheduler to
//! prefer this thread is the other half of the answer.
//!
//! **This is best effort and never fatal.** `SCHED_FIFO` needs `CAP_SYS_NICE`
//! (or an `RLIMIT_RTPRIO` allowance), which an ordinary user does not have; the
//! call then fails with `EPERM` and the recorder carries on exactly as before,
//! having said so once. Nothing here is required for correct recording.
//!
//! The priority requested is the **lowest** real-time priority the system
//! offers, not the highest: the goal is to outrank ordinary `SCHED_OTHER` work
//! like a compiler, not to outrank the kernel's own real-time threads. Linux's
//! real-time throttling (95% of each period by default) still bounds a runaway.

/// Ask the scheduler to treat the calling thread as real-time.
///
/// Logs once — at info for the expected "not permitted", at warn for anything
/// else — and always returns. On a non-Linux target this does nothing.
pub(crate) fn raise_current_thread(device: &str) {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `sched_setscheduler(0, ...)` addresses the calling thread and
        // reads one initialized `sched_param`; it has no other effect on the
        // process's memory.
        let (ok, err) = unsafe {
            let priority = libc::sched_get_priority_min(libc::SCHED_FIFO);
            let param = libc::sched_param {
                sched_priority: priority,
            };
            let rc = libc::sched_setscheduler(0, libc::SCHED_FIFO, &param);
            (rc == 0, std::io::Error::last_os_error())
        };
        if ok {
            tracing::info!(device, "DSP thread running at real-time priority");
        } else if err.raw_os_error() == Some(libc::EPERM) {
            // The ordinary case for an unprivileged run. Said once, at info,
            // because it is a fact about the deployment rather than a fault:
            // `setcap cap_sys_nice+ep` on the binary, or a `RLIMIT_RTPRIO`
            // allowance, is what would change it.
            tracing::info!(
                device,
                "no permission for real-time scheduling; running at normal priority \
                 (grant CAP_SYS_NICE to reduce overflow under load)"
            );
        } else {
            tracing::warn!(device, error = %err, "could not raise DSP thread priority");
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = device;
    }
}
