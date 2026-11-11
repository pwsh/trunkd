// SPDX-License-Identifier: GPL-3.0-or-later
//! PortaPack/Mayhem mode switching for a HackRF that boots into the UI.
//!
//! A HackRF One inside a PortaPack normally comes up running the Mayhem
//! firmware: the radio is owned by the PortaPack's own UI and the board appears
//! on USB only as a CDC serial console (`1d50:6018`, typically `/dev/ttyACM0`)
//! presenting the `ch> ` shell. `hackrf_info` finds nothing in that state.
//!
//! Two directions:
//!
//! - [`ensure_hackrf_mode`] writes `hackrf` to the Mayhem console, which hands
//!   the radio to the HackRF firmware; the board re-enumerates as `1d50:6089`
//!   after ~1–3 s and `hackrf_transfer` can then drive it (see
//!   [`crate::hackrf`]).
//! - [`restore_portapack`] resets the board, which reboots it back into the
//!   PortaPack UI.
//!
//! The reset path is the one place trunkd needs libhackrf: the stock CLI tools
//! shipped with 2023.01.1 expose no reset (`hackrf_debug` covers registers, LED
//! and UI state only). Rather than take a build-time dependency on libhackrf —
//! the exact thing [`crate::hackrf`]'s CLI-pipe design exists to avoid — the
//! library is `dlopen`ed at run time via `libloading` and the four-call
//! `hackrf_init` / `hackrf_open` / `hackrf_reset` / `hackrf_exit` sequence is
//! invoked. A machine without libhackrf simply gets an error from this one
//! function; everything else in the crate still builds and runs.

use std::ffi::{c_char, c_int, c_void};
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use trunkd_core::{Error, Result};

/// The CDC console a PortaPack usually presents on Linux.
pub const DEFAULT_PORTAPACK_PORT: &str = "/dev/ttyACM0";

/// Shared-object names tried, in order, when `dlopen`ing libhackrf.
const LIBHACKRF_CANDIDATES: [&str; 4] = [
    "libhackrf.so.0",
    "libhackrf.so",
    "libhackrf.0.dylib",
    "hackrf.dll",
];

/// Poll interval while waiting for the board to re-enumerate.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Whether `hackrf_info` currently finds a board.
///
/// This is the authoritative "is the radio available to us" test: it is exactly
/// what `hackrf_transfer` needs to succeed.
pub fn hackrf_present() -> bool {
    hackrf_present_with("hackrf_info")
}

/// [`hackrf_present`] with an explicit `hackrf_info` binary (tests, odd installs).
pub fn hackrf_present_with(info_bin: &str) -> bool {
    match Command::new(info_bin).output() {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            out.status.success() && text.contains("Found HackRF")
        }
        Err(e) => {
            tracing::debug!(bin = info_bin, error = %e, "hackrf_info not runnable");
            false
        }
    }
}

/// Put the board into HackRF mode, returning as soon as `hackrf_info` sees it.
///
/// A no-op (immediate `Ok`) when a board is already present, so it is safe to
/// call unconditionally at startup. Otherwise `port` is configured raw at
/// 115200 baud and the Mayhem console command `hackrf` is written to it; the
/// board then re-enumerates, which takes roughly 1–3 s.
///
/// Errors if the port does not exist, cannot be configured or written, or if no
/// board appears within `timeout`.
pub fn ensure_hackrf_mode(port: &Path, timeout: Duration) -> Result<()> {
    if hackrf_present() {
        tracing::debug!("hackrf already present; no mode switch needed");
        return Ok(());
    }
    if !port.exists() {
        return Err(Error::device(format!(
            "portapack console {} not found and no HackRF present",
            port.display()
        )));
    }

    configure_tty(port)?;

    // Open read-write: the CDC gadget only runs its console while the host has
    // the port open with DTR asserted, and a write followed by an immediate
    // close is observably ignored — the command sits in the firmware's buffer
    // until some later opener asserts DTR again. So the handle is deliberately
    // held open across the whole wait, and only dropped once the board has
    // switched (at which point the tty is gone anyway).
    let mut console = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(port)
        .map_err(|e| Error::device(format!("open {}: {e}", port.display())))?;
    // Give the gadget a moment to notice DTR before the command arrives.
    std::thread::sleep(POLL_INTERVAL);
    console
        .write_all(b"hackrf\r\n")
        .and_then(|()| console.flush())
        .map_err(|e| Error::device(format!("write {}: {e}", port.display())))?;
    tracing::info!(port = %port.display(), "sent 'hackrf' to the PortaPack console");

    let deadline = Instant::now() + timeout;
    loop {
        std::thread::sleep(POLL_INTERVAL);
        if hackrf_present() {
            drop(console);
            tracing::info!("board is in HackRF mode");
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::device(format!(
                "board did not enter HackRF mode within {timeout:?}"
            )));
        }
    }
}

/// Put `port` in raw mode at 115200 baud.
///
/// Shelling out to `stty` keeps the crate free of a termios dependency for a
/// one-shot setup step that happens at most once per process.
fn configure_tty(port: &Path) -> Result<()> {
    let status = Command::new("stty")
        .arg("-F")
        .arg(port)
        .args(["115200", "raw", "-echo"])
        .status()
        .map_err(|e| Error::device(format!("stty: {e}")))?;
    if !status.success() {
        return Err(Error::device(format!(
            "stty -F {} failed with {status}",
            port.display()
        )));
    }
    Ok(())
}

/// Reset the board, which reboots a PortaPack back into its UI.
///
/// Implemented by `dlopen`ing libhackrf and calling
/// `hackrf_init` → `hackrf_open` → `hackrf_reset` → `hackrf_exit`; the stock
/// CLI tools expose no reset. The reset takes effect immediately, so the USB
/// device disappears while this call returns.
///
/// Errors if libhackrf cannot be loaded, no board is open-able, or a call
/// fails.
pub fn restore_portapack() -> Result<()> {
    let lib = load_libhackrf()?;
    // SAFETY: the symbols below are libhackrf's documented C API and are given
    // their real signatures; `device` is only ever written by `hackrf_open` and
    // read back by `hackrf_reset`, both while the library is loaded.
    unsafe {
        let init: libloading::Symbol<unsafe extern "C" fn() -> c_int> =
            lib.get(b"hackrf_init\0").map_err(|e| {
                Error::device(format!("libhackrf: hackrf_init: {}", crate::dl_error(&e)))
            })?;
        let open: libloading::Symbol<unsafe extern "C" fn(*mut *mut c_void) -> c_int> =
            lib.get(b"hackrf_open\0").map_err(|e| {
                Error::device(format!("libhackrf: hackrf_open: {}", crate::dl_error(&e)))
            })?;
        let reset: libloading::Symbol<unsafe extern "C" fn(*mut c_void) -> c_int> =
            lib.get(b"hackrf_reset\0").map_err(|e| {
                Error::device(format!("libhackrf: hackrf_reset: {}", crate::dl_error(&e)))
            })?;
        let exit: libloading::Symbol<unsafe extern "C" fn() -> c_int> =
            lib.get(b"hackrf_exit\0").map_err(|e| {
                Error::device(format!("libhackrf: hackrf_exit: {}", crate::dl_error(&e)))
            })?;

        let rc = init();
        if rc != 0 {
            return Err(Error::device(format!("hackrf_init failed: {rc}")));
        }

        let mut device: *mut c_void = std::ptr::null_mut();
        let rc = open(&mut device);
        if rc != 0 || device.is_null() {
            exit();
            return Err(Error::device(format!("hackrf_open failed: {rc}")));
        }

        let rc = reset(device);
        // The board is already rebooting; tear the library down either way.
        exit();
        if rc != 0 {
            return Err(Error::device(format!("hackrf_reset failed: {rc}")));
        }
    }
    tracing::info!("board reset; PortaPack UI should be back");
    Ok(())
}

/// What libhackrf says about itself, without touching a board.
///
/// `(library version, release)` — libhackrf reports two strings: the library's
/// own version (`hackrf_library_version`, the git describe of the host tools)
/// and the release it belongs to (`hackrf_library_release`, e.g.
/// `"2024.02.1"`). Both are static strings; neither call initialises the
/// library, enumerates USB or opens hardware, which is what makes this safe to
/// call from a diagnostics page on a running recorder.
///
/// `None` when libhackrf cannot be loaded at all — the common case on a box
/// that only has Soapy devices, and not something to report as a fault.
pub fn library_version() -> Option<(Option<String>, Option<String>)> {
    let lib = load_libhackrf().ok()?;
    // SAFETY: both symbols are libhackrf's documented C API, take no
    // arguments, and return pointers to static NUL-terminated strings owned by
    // the library, which stays loaded for the duration of the reads below.
    unsafe {
        let read = |name: &[u8]| -> Option<String> {
            let f: libloading::Symbol<unsafe extern "C" fn() -> *const c_char> =
                lib.get(name).ok()?;
            let p = f();
            if p.is_null() {
                return None;
            }
            let s = std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
            (!s.is_empty()).then_some(s)
        };
        let version = read(b"hackrf_library_version\0");
        let release = read(b"hackrf_library_release\0");
        // Nothing at all means the library is too old to say; that is a
        // different answer from "no library", so keep the tuple.
        Some((version, release))
    }
}

/// `dlopen` libhackrf, trying the usual platform names.
fn load_libhackrf() -> Result<libloading::Library> {
    let mut last = String::new();
    for name in LIBHACKRF_CANDIDATES {
        // SAFETY: loading libhackrf runs its (empty) initializers; the library
        // has no side effects until one of its functions is called.
        match unsafe { libloading::Library::new(name) } {
            Ok(lib) => {
                tracing::debug!(lib = name, "loaded libhackrf");
                return Ok(lib);
            }
            Err(e) => last = format!("{name}: {}", crate::dl_error(&e)),
        }
    }
    Err(Error::device(format!(
        "could not load libhackrf (needed to reset the board): {last}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_port_without_hardware_is_an_error() {
        // Only meaningful when no board is attached; with one present the
        // function short-circuits to Ok, which is also correct.
        let r = ensure_hackrf_mode(
            Path::new("/dev/definitely-not-a-tty"),
            Duration::from_secs(1),
        );
        if hackrf_present() {
            assert!(r.is_ok(), "a present board makes this a no-op");
        } else {
            assert!(r.is_err(), "missing console must be reported");
        }
    }

    #[test]
    fn presence_probe_tolerates_a_missing_binary() {
        assert!(!hackrf_present_with("/no/such/hackrf_info"));
    }
}
