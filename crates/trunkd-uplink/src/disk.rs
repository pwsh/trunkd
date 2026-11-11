// SPDX-License-Identifier: GPL-3.0-or-later
//! Free space on the volume a spool lives on, and the eviction pass that uses
//! it.
//!
//! The byte cap ([`crate::UplinkConfig::spool_cap_bytes`]) bounds what *trunkd*
//! keeps; it says nothing about the rest of the filesystem. A recorder whose
//! disk is being filled by system logs, an operator's `dd`, or a second
//! recorder on the same box hits `ENOSPC` long before its own spool reaches its
//! cap — and a recorder that cannot write is one that loses calls it has
//! already decoded and paid for. So there are two limits, and they answer
//! different questions:
//!
//! * **cap** — how much of this volume trunkd is allowed to use;
//! * **floor** ([`crate::UplinkConfig::spool_min_free_bytes`]) — how much of
//!   this volume must stay free *for everyone*, no matter who filled it.
//!
//! Both evict the oldest spooled entries first, for the same reason: the newest
//! call is the one someone is still waiting to hear.
//!
//! Free space is read with `statvfs`, `f_bavail * f_frsize` — the space an
//! unprivileged process can actually use, not `f_bfree`, which includes the
//! blocks reserved for root. On Windows the same question is asked of
//! `GetDiskFreeSpaceExW`, whose "available to the caller" figure is the
//! quota-aware analogue. If that call fails, or the platform offers neither,
//! the floor is *skipped* for that pass and logged once: a bad reading must
//! never be a reason to delete a call.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// How a spool learns the free space on its volume.
///
/// A plain `fn` pointer rather than a boxed closure so a [`crate::Spool`] stays
/// `Clone + Debug` and copies into the worker thread for free. Tests substitute
/// a fake so they measure the injected number, not the host's disk.
pub type FreeSpaceFn = fn(&Path) -> io::Result<u64>;

/// Bytes available to an unprivileged process on the filesystem holding `path`.
///
/// `f_bavail * f_frsize`: the root-reserved blocks in `f_bfree` are not ours to
/// spend, and counting them would let the spool run the volume into the
/// reservation before the floor noticed.
#[cfg(unix)]
// The `statvfs` field types differ by platform (`u32` on some 32-bit targets,
// `u64` on Linux), so the casts are load-bearing on some and redundant on
// others.
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
pub fn available_space(path: &Path) -> io::Result<u64> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let mut stat = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `c_path` is a NUL-terminated C string that outlives the call, and
    // `statvfs` either fills `stat` completely or returns non-zero.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call returned 0, so the struct is initialised.
    let stat = unsafe { stat.assume_init() };
    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

/// Bytes available to the calling user on the volume holding `path`.
///
/// `GetDiskFreeSpaceExW`'s first out-parameter, the Win32 analogue of
/// `f_bavail * f_frsize`: it is quota-aware, reporting what *this* caller may
/// still write rather than the volume's total free space, which is the number
/// the floor cares about.
///
/// A `path` that does not exist fails with `ERROR_PATH_NOT_FOUND` (3) or
/// `ERROR_FILE_NOT_FOUND` (2); both map to [`io::ErrorKind::NotFound`], which
/// is what `measure` keys on to recreate a pruned spool directory and try
/// again.
#[cfg(windows)]
pub fn available_space(path: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            directory_name: *const u16,
            free_bytes_available_to_caller: *mut u64,
            total_number_of_bytes: *mut u64,
            total_number_of_free_bytes: *mut u64,
        ) -> i32;
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available: u64 = 0;
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call, the
    // out-pointer is a valid `u64`, and the two unused out-pointers may be null
    // per the Win32 contract. `ULARGE_INTEGER` is a 64-bit unsigned value.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(available)
}

/// Fallback for platforms with neither `statvfs` nor `GetDiskFreeSpaceExW`, so
/// the floor is disabled.
///
/// Returning an error rather than `u64::MAX` keeps the "never evict on a
/// reading we do not trust" rule in one place — the caller already skips the
/// floor when the reading fails.
#[cfg(not(any(unix, windows)))]
pub fn available_space(_path: &Path) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "free-space measurement is not implemented for this platform",
    ))
}

/// The unsupported-platform notice is worth saying, but only once.
static UNSUPPORTED_LOGGED: AtomicBool = AtomicBool::new(false);
/// Likewise a failing `statvfs`: a broken mount would otherwise log on every
/// sweep, forever.
static ERROR_LOGGED: AtomicBool = AtomicBool::new(false);

/// Read free space, degrading to `None` (floor skipped) on any failure.
///
/// Logged once per process per class of failure: an unreadable mount point is a
/// standing condition, not an event, and the sweep runs every 30 s.
fn measure(dir: &Path, free_space: FreeSpaceFn) -> Option<u64> {
    let first = free_space(dir);
    // A missing directory is not a broken mount: an empty spool's directory
    // tree can be pruned away (or never created yet), and the right answer is
    // to put it back and measure, not to disable the floor with a warning.
    let result = match first {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let _ = std::fs::create_dir_all(dir);
            free_space(dir)
        }
        other => other,
    };
    match result {
        Ok(free) => Some(free),
        Err(e) if e.kind() == io::ErrorKind::Unsupported => {
            if !UNSUPPORTED_LOGGED.swap(true, Ordering::Relaxed) {
                tracing::info!(
                    reason = %e,
                    "free-space floor disabled on this platform; the byte cap still applies"
                );
            }
            None
        }
        Err(e) => {
            if !ERROR_LOGGED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    dir = %dir.display(),
                    error = %e,
                    "could not read free space; the spool's free-space floor is not being enforced"
                );
            }
            None
        }
    }
}

/// What one enforcement pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sweep {
    /// Entries dropped because the spool was over its byte cap.
    pub cap_evicted: u64,
    /// Entries dropped because the volume was below its free-space floor.
    pub floor_evicted: u64,
    /// Bytes the spool occupies now.
    pub bytes: u64,
    /// Free bytes on the volume after the pass, or `None` when it could not be
    /// measured (in which case the floor was not enforced this pass).
    pub free_bytes: Option<u64>,
}

impl Sweep {
    /// Entries dropped for either reason.
    pub fn evicted(&self) -> u64 {
        self.cap_evicted + self.floor_evicted
    }
}

/// Apply the byte cap and then the free-space floor to `entries`, which must be
/// **oldest first**.
///
/// Generic over the entry type so the call spool and the event spool share one
/// implementation of the policy — they differ only in what an entry is and how
/// it is deleted.
///
/// The floor loop credits itself the bytes it deletes rather than re-reading
/// free space after every unlink: one `statvfs` per sweep instead of one per
/// file, and a reading that does not depend on when the filesystem gets around
/// to accounting for the deletions. `min_free_bytes == 0` disables the floor
/// but still takes the reading, because the gauge is worth publishing either
/// way.
pub(crate) fn enforce<E>(
    entries: &[E],
    bytes_of: impl Fn(&E) -> u64,
    remove: impl Fn(&E),
    dir: &Path,
    cap_bytes: u64,
    min_free_bytes: u64,
    free_space: FreeSpaceFn,
) -> Sweep {
    let mut total: u64 = entries.iter().map(&bytes_of).sum();
    let mut cap_evicted = 0u64;
    let mut next = 0usize;
    while next < entries.len() && total > cap_bytes {
        let entry = &entries[next];
        remove(entry);
        total = total.saturating_sub(bytes_of(entry));
        cap_evicted += 1;
        next += 1;
    }

    let mut free = measure(dir, free_space);
    let mut floor_evicted = 0u64;
    if min_free_bytes > 0 {
        if let Some(free) = free.as_mut() {
            while next < entries.len() && *free < min_free_bytes {
                let entry = &entries[next];
                let bytes = bytes_of(entry);
                remove(entry);
                total = total.saturating_sub(bytes);
                *free = free.saturating_add(bytes);
                floor_evicted += 1;
                next += 1;
            }
        }
    }

    Sweep {
        cap_evicted,
        floor_evicted,
        bytes: total,
        free_bytes: free,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(unix, windows))]
    fn the_platform_call_reports_something_plausible_for_the_temp_dir() {
        let free = available_space(&std::env::temp_dir()).expect("free space");
        // Nothing to assert about the number itself; that it is not absurd is
        // the whole claim.
        assert!(free > 0, "temp dir reports {free} bytes free");
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn a_missing_path_is_an_error_not_a_panic() {
        assert!(available_space(Path::new("/nonexistent/trunkd/spool")).is_err());
    }
}
