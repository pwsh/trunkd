// SPDX-License-Identifier: GPL-3.0-or-later
//! What the operating system says about this process and the box it runs on.
//!
//! Every probe here is read-only, cheap enough to run per request, and falls
//! back to `None` rather than failing: an About page that 500s because
//! `/etc/os-release` is missing has failed at the one job it has.
//!
//! Platform coverage is deliberately uneven, in the direction the recorder is
//! actually deployed. Linux answers everything from `/proc` and
//! `/etc/os-release`; Windows answers the version through `RtlGetVersion` and
//! free space through `GetDiskFreeSpaceExW`; anything else gets the compile-time
//! constants and honest `null`s.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set once when the recorder is started by the Windows service control
/// manager, so the page can say *how* this process was launched rather than
/// guessing from its environment.
static WINDOWS_SERVICE: AtomicBool = AtomicBool::new(false);

/// Record that this process is running under the Windows SCM.
///
/// Called from the service host, which is the only code that knows: by the
/// time the recorder is running, a service and a console run look identical
/// from the inside.
pub fn note_windows_service() {
    WINDOWS_SERVICE.store(true, Ordering::Relaxed);
}

/// How this process was launched.
///
/// * `"windows-service"` — the SCM started it (see [`note_windows_service`]).
/// * `"systemd"` — systemd sets `INVOCATION_ID` for every unit it starts, and
///   sets it for nothing else, which makes it the cheapest reliable test.
/// * `"foreground"` — somebody typed `trunkd run`.
pub fn launch_context() -> &'static str {
    if WINDOWS_SERVICE.load(Ordering::Relaxed) {
        "windows-service"
    } else if std::env::var_os("INVOCATION_ID").is_some() {
        "systemd"
    } else {
        "foreground"
    }
}

/// The machine's hostname.
pub fn hostname() -> Option<String> {
    #[cfg(unix)]
    {
        // `/proc/sys/kernel/hostname` is the live value (it follows a
        // `hostnamectl set-hostname`); `/etc/hostname` is the configured one.
        for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
            if let Some(s) = read_trimmed(Path::new(path)) {
                return Some(s);
            }
        }
    }
    for var in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim().to_owned();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// OS name, version, and the kernel/build string.
///
/// One definition per platform rather than one function full of `cfg` blocks:
/// a `cfg`-gated block that `return`s leaves the other platforms' bodies
/// empty, which is exactly the kind of thing that compiles on the machine you
/// are sitting at and not on the one you deploy to.
#[cfg(target_os = "linux")]
pub fn os_info() -> (String, Option<String>, Option<String>) {
    let release = os_release();
    let name = release
        .as_ref()
        .and_then(|r| field(r, "PRETTY_NAME").or_else(|| field(r, "NAME")))
        .unwrap_or_else(|| "Linux".to_owned());
    let version = release
        .as_ref()
        .and_then(|r| field(r, "VERSION_ID").or_else(|| field(r, "VERSION")));
    let kernel = read_trimmed(Path::new("/proc/sys/kernel/osrelease"));
    (name, version, kernel)
}

/// OS name, version, and the build number.
///
/// `RtlGetVersion` is the one version call Windows does not lie about:
/// `GetVersionEx` reports 6.2 to any process without a compatibility manifest,
/// which is every Rust binary.
#[cfg(windows)]
pub fn os_info() -> (String, Option<String>, Option<String>) {
    use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;

    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is a correctly sized, zeroed OSVERSIONINFOW with its size
    // field set, which is the entire contract of RtlGetVersion.
    let status = unsafe { windows_sys::Wdk::System::SystemServices::RtlGetVersion(&mut info) };
    if status != 0 {
        return ("Windows".to_owned(), None, None);
    }
    (
        "Windows".to_owned(),
        Some(format!("{}.{}", info.dwMajorVersion, info.dwMinorVersion)),
        Some(format!("build {}", info.dwBuildNumber)),
    )
}

/// OS name only: everything else needs a platform probe trunkd does not carry.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn os_info() -> (String, Option<String>, Option<String>) {
    (std::env::consts::OS.to_owned(), None, None)
}

/// `/etc/os-release`, whichever of the two standard locations has it.
#[cfg(target_os = "linux")]
fn os_release() -> Option<String> {
    std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .ok()
}

/// One `KEY=value` line out of an os-release file, unquoted.
#[cfg(target_os = "linux")]
fn field(text: &str, key: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().trim_matches('"').to_owned())
        .filter(|v| !v.is_empty())
}

/// Free and total bytes on the filesystem holding `path`.
///
/// The *filesystem's* free space, not the spool's remaining budget: the two
/// are different numbers and it is the first one that decides whether the next
/// call can be written at all.
///
/// A path that does not exist yet walks up to its nearest existing ancestor —
/// a spool directory is created on first use, and "the disk under where it
/// will be" is the useful answer before then.
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    let mut probe = path;
    loop {
        if probe.exists() {
            break;
        }
        probe = probe.parent()?;
    }
    statfs(probe)
}

// The `statvfs` field widths differ by platform and target pointer width
// (`c_ulong` on 32-bit, `u64` on 64-bit Linux), so the casts below are
// redundant on this host and load-bearing on another.
#[allow(clippy::unnecessary_cast)]
#[cfg(unix)]
fn statfs(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` fills a caller-allocated struct; the path is a valid
    // NUL-terminated string and the struct is zeroed before the call.
    let stats = unsafe {
        let mut stats: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stats) != 0 {
            return None;
        }
        stats
    };
    // `f_frsize` is the fragment size the block counts are in; `f_bsize` is a
    // hint for IO sizing and is not always the same number.
    let unit = if stats.f_frsize > 0 {
        stats.f_frsize as u64
    } else {
        stats.f_bsize as u64
    };
    // `f_bavail` is what an unprivileged process may actually use, which is
    // less than `f_bfree` on a filesystem with reserved blocks.
    Some((stats.f_bavail as u64 * unit, stats.f_blocks as u64 * unit))
}

#[cfg(windows)]
fn statfs(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free: u64 = 0;
    let mut total: u64 = 0;
    // SAFETY: the path is NUL-terminated UTF-16 and both out-parameters are
    // owned locals; the call writes them only on success.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            &mut total,
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some((free, total))
}

#[cfg(not(any(unix, windows)))]
fn statfs(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// Read a file and trim it, mapping empty to `None`.
///
/// Unused on Windows, where nothing this module wants lives in a file.
#[cfg_attr(not(unix), allow(dead_code))]
fn read_trimmed(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim().to_owned();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_answers_the_questions_it_can() {
        // Nothing here can be asserted to a value — it is the machine's own
        // truth — but every probe must return *something* usable rather than
        // panicking or hanging, which is what this pins.
        let (name, _version, _kernel) = os_info();
        assert!(!name.is_empty(), "the OS always has a name");
        assert!(
            hostname().is_some_and(|h| !h.is_empty()),
            "every platform trunkd runs on can name itself"
        );
    }

    #[test]
    fn disk_space_answers_for_a_directory_that_does_not_exist_yet() {
        let dir = std::env::temp_dir();
        let (free, total) = disk_space(&dir).expect("the temp dir is on a filesystem");
        assert!(total > 0, "a mounted filesystem has a size");
        assert!(free <= total);

        // The spool is created on first use, so the common case is a path with
        // one or two missing components; it must still report the volume.
        let missing = dir.join("trunkd-about-no-such-dir").join("nor-this");
        assert!(
            disk_space(&missing).is_some(),
            "a not-yet-created directory reports the disk it will live on"
        );
    }

    #[test]
    fn the_launch_context_is_one_of_the_three_known_answers() {
        assert!(matches!(
            launch_context(),
            "windows-service" | "systemd" | "foreground"
        ));
    }
}
