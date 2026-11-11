// SPDX-License-Identifier: GPL-3.0-or-later
//! Runtime binding to the SoapySDR C ABI.
//!
//! trunkd deliberately takes **no build-time dependency** on SoapySDR: no
//! bindgen, no libclang, no pkg-config, no `libSoapySDR` headers. The **0.8** C
//! ABI is stable and small, so the handful of entry points the recorder needs
//! are declared here by hand and resolved with `dlopen` + `dlsym` (via
//! `libloading`) the first time a Soapy device is touched. This is
//! the same posture as [`crate::portapack`]'s libhackrf loader, scaled up to a
//! whole backend, and it is what lets `soapy` be a *default* feature: a build
//! machine needs nothing, and a run-time machine needs only the shared library
//! the distribution already ships.
//!
//! The load is attempted once per process and cached in [`api`], so a host with
//! no SoapySDR installed pays one failed `dlopen` and then answers from a
//! cached error string.
//!
//! # Why 0.8 and not 0.7
//!
//! The signatures below are 0.8's, and 0.7 is **not** compatible with them:
//! `SoapySDRDevice_setupStream` changed shape between the two, from
//! `int setupStream(dev, SoapySDRStream **stream, dir, ...)` — returning a code
//! with the stream handed back through an out-pointer — to
//! `SoapySDRStream *setupStream(dev, dir, ...)`. Every symbol resolves either
//! way, so a 0.7 library would load happily and then scribble over the stack on
//! the first `setupStream` call. Loading therefore checks
//! `SoapySDR_getAPIVersion` and refuses anything below 0.8 (see
//! [`api_version_ok`]), and only 0.8-or-later library names are tried.

use std::ffi::{c_char, c_int, c_long, c_longlong, c_void, CStr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Receive direction (`SOAPY_SDR_RX`).
pub const SOAPY_SDR_RX: c_int = 1;

/// `readStream` returned no samples within the timeout (benign).
pub const SOAPY_SDR_TIMEOUT: c_int = -1;
/// A non-recoverable stream error.
pub const SOAPY_SDR_STREAM_ERROR: c_int = -2;
/// The stream data is corrupt.
pub const SOAPY_SDR_CORRUPTION: c_int = -3;
/// The device overflowed its own buffers: samples were lost before us.
pub const SOAPY_SDR_OVERFLOW: c_int = -4;
/// The requested operation is not supported by this driver.
pub const SOAPY_SDR_NOT_SUPPORTED: c_int = -5;
/// The requested time could not be honoured.
pub const SOAPY_SDR_TIME_ERROR: c_int = -6;
/// The stream underflowed (transmit direction).
pub const SOAPY_SDR_UNDERFLOW: c_int = -7;

/// Human text for a `readStream`/device error code.
pub fn error_text(code: c_int) -> &'static str {
    match code {
        SOAPY_SDR_TIMEOUT => "timeout",
        SOAPY_SDR_STREAM_ERROR => "stream error",
        SOAPY_SDR_CORRUPTION => "corruption",
        SOAPY_SDR_OVERFLOW => "overflow",
        SOAPY_SDR_NOT_SUPPORTED => "not supported",
        SOAPY_SDR_TIME_ERROR => "time error",
        SOAPY_SDR_UNDERFLOW => "underflow",
        _ => "unknown error",
    }
}

/// Opaque `SoapySDRDevice`.
pub enum SoapySDRDevice {}
/// Opaque `SoapySDRStream`.
pub enum SoapySDRStream {}

/// `SoapySDRKwargs`: a parallel key/value string list.
#[repr(C)]
pub struct SoapySDRKwargs {
    /// Number of entries.
    pub size: usize,
    /// `size` NUL-terminated keys.
    pub keys: *mut *mut c_char,
    /// `size` NUL-terminated values.
    pub vals: *mut *mut c_char,
}

/// `SoapySDRRange`: an inclusive min/max with an optional step (0 = continuous).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SoapySDRRange {
    /// Lower bound.
    pub minimum: f64,
    /// Upper bound.
    pub maximum: f64,
    /// Step, or 0 for a continuous range.
    pub step: f64,
}

/// Shared-object names tried, in order, after the `TRUNKD_SOAPYSDR_LIB`
/// override.
///
/// The versioned 0.8 soname comes first because it is the one this binding is
/// written against; `libSoapySDR.so` (an unversioned dev symlink, which may
/// point anywhere) and the Windows/macOS names follow and are version-checked
/// after loading. 0.7 is deliberately absent — see the module docs.
pub const LIB_CANDIDATES: [&str; 4] = [
    "libSoapySDR.so.0.8",
    "libSoapySDR.so",
    "SoapySDR.dll",
    "libSoapySDR.dylib",
];

/// The lowest SoapySDR ABI this binding's signatures match.
pub const REQUIRED_ABI: &str = "0.8";

/// Whether an API version string names an ABI these signatures work with.
///
/// Only the leading `major.minor` matters and the separator after it is
/// whatever the build used (`"0.8.200"`, `"0.8-3"`); anything that does not
/// start with two dot-separated numbers is rejected rather than assumed good,
/// because guessing wrong here means memory corruption, not a bad error
/// message.
pub fn api_version_ok(s: &str) -> bool {
    let mut parts = s.trim().splitn(2, '.');
    let Some(Ok(major)) = parts.next().map(str::parse::<u32>) else {
        return false;
    };
    let Some(rest) = parts.next() else {
        return false;
    };
    let minor_digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let Ok(minor) = minor_digits.parse::<u32>() else {
        return false;
    };
    major > 0 || minor >= 8
}

/// Environment variable holding a full path to `libSoapySDR`, tried first.
pub const LIB_ENV: &str = "TRUNKD_SOAPYSDR_LIB";

/// SoapySDR's own module-search override, read when it first loads modules.
pub const PLUGIN_PATH_ENV: &str = "SOAPY_SDR_PLUGIN_PATH";

/// Where a redistributable bundle keeps its driver modules, relative to the
/// directory holding the executable.
pub const BUNDLED_MODULE_SUBDIR: &str = "SoapySDR/modules0.8";

/// The library file name a bundle ships next to the executable.
///
/// Windows' loader would find `SoapySDR.dll` in the exe's directory on its own,
/// but naming the full path makes the bundle win over a PothosSDR install that
/// happens to be on `PATH`, which is the whole point of shipping one.
#[cfg(windows)]
const BUNDLED_LIB_NAME: &str = "SoapySDR.dll";
#[cfg(not(windows))]
const BUNDLED_LIB_NAME: &str = "libSoapySDR.so.0.8";

/// The bundled module directory for an executable living in `exe_dir`.
pub fn bundled_module_dir(exe_dir: &Path) -> PathBuf {
    // A `/`-joined literal keeps the constant readable; `Path::join` on Windows
    // is happy to take it and produces a mixed separator the loader accepts.
    exe_dir.join(BUNDLED_MODULE_SUBDIR)
}

/// The value to write to `SOAPY_SDR_PLUGIN_PATH`, or `None` to leave it alone.
///
/// `current` is the variable's present value: an operator who set it explicitly
/// always wins, because they are pointing at driver modules we know nothing
/// about. Otherwise a bundled module directory next to the executable is used —
/// SoapySDR's module search path is baked in at *its* build time, so a
/// relocated bundle would find no drivers at all without this.
pub fn plugin_path_override(exe_dir: &Path, current: Option<&str>) -> Option<PathBuf> {
    if current.is_some_and(|v| !v.trim().is_empty()) {
        return None;
    }
    let dir = bundled_module_dir(exe_dir);
    dir.is_dir().then_some(dir)
}

/// The full path of a bundled SoapySDR library next to the executable, if any.
pub fn bundled_library_path(exe_dir: &Path) -> Option<PathBuf> {
    let lib = exe_dir.join(BUNDLED_LIB_NAME);
    lib.is_file().then_some(lib)
}

/// The candidate list, in try order: operator override, bundled library, then
/// the bare names the platform loader resolves against its own search path.
///
/// Pure so the ordering can be tested without a filesystem or an environment.
fn compose_candidates(env_override: Option<&str>, bundled: Option<&Path>) -> Vec<String> {
    let mut names: Vec<String> = Vec::with_capacity(LIB_CANDIDATES.len() + 2);
    if let Some(path) = env_override.map(str::trim).filter(|p| !p.is_empty()) {
        names.push(path.to_owned());
    }
    if let Some(p) = bundled {
        names.push(p.to_string_lossy().into_owned());
    }
    names.extend(LIB_CANDIDATES.iter().map(|s| (*s).to_owned()));
    names
}

/// Directory holding the running executable, when the OS will say.
fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// Point SoapySDR at a bundled module directory, if there is one and the
/// operator has not chosen otherwise.
///
/// Safe to do here rather than in `main`: SoapySDR reads
/// `SOAPY_SDR_PLUGIN_PATH` when it first *loads modules*, which happens on the
/// first factory call — after our `dlopen`, never during it.
fn apply_bundled_plugin_path() {
    let Some(dir) = exe_dir() else { return };
    let current = std::env::var(PLUGIN_PATH_ENV).ok();
    let Some(path) = plugin_path_override(&dir, current.as_deref()) else {
        return;
    };
    std::env::set_var(PLUGIN_PATH_ENV, &path);
    tracing::debug!(path = %path.display(), "using bundled SoapySDR modules");
}

/// The resolved SoapySDR entry points, plus the `Library` that owns them.
///
/// Every field is a plain `unsafe extern "C"` function pointer copied out of a
/// `libloading::Symbol` at load time; the `Library` is kept in the same struct
/// so the pointers stay valid for as long as the `Api` lives (which, being
/// behind a process-wide `OnceLock`, is forever).
pub struct Api {
    _lib: libloading::Library,
    /// The candidate name (or `TRUNKD_SOAPYSDR_LIB` path) that loaded.
    pub lib_name: String,
    /// `SoapySDR_getAPIVersion`. Required: it is the ABI gate.
    pub get_api_version: unsafe extern "C" fn() -> *const c_char,
    /// `SoapySDR_getLibVersion`, if the library exports it.
    pub get_lib_version: Option<unsafe extern "C" fn() -> *const c_char>,
    /// `SoapySDR_getABIVersion`, if the library exports it. Diagnostics only:
    /// the gate is [`api_version_ok`] on the API version, because a 0.7
    /// library reports an ABI string this code would have to interpret
    /// identically anyway.
    pub get_abi_version: Option<unsafe extern "C" fn() -> *const c_char>,
    /// `SoapySDR_getRootPath`: the install prefix modules are looked for under.
    pub get_root_path: Option<unsafe extern "C" fn() -> *const c_char>,
    /// `SoapySDR_listModules` (free with `strings_clear`): the module files the
    /// search path contains. Listing them reads the filesystem; it does not
    /// load a driver and never touches hardware.
    pub list_modules: Option<unsafe extern "C" fn(*mut usize) -> *mut *mut c_char>,
    /// `SoapySDR_getModuleVersion` (free with `free`): the version a module
    /// registered when it was loaded, or empty if it has not been.
    pub get_module_version: Option<unsafe extern "C" fn(*const c_char) -> *mut c_char>,

    /// `SoapySDRDevice_enumerateStrArgs`
    pub enumerate_str_args: unsafe extern "C" fn(*const c_char, *mut usize) -> *mut SoapySDRKwargs,
    /// `SoapySDRKwargsList_clear`
    pub kwargs_list_clear: unsafe extern "C" fn(*mut SoapySDRKwargs, usize),
    /// `SoapySDRKwargs_clear`
    pub kwargs_clear: unsafe extern "C" fn(*mut SoapySDRKwargs),
    /// `SoapySDRKwargs_get`
    pub kwargs_get: unsafe extern "C" fn(*const SoapySDRKwargs, *const c_char) -> *const c_char,
    /// `SoapySDRStrings_clear`: takes a **pointer to** the array
    /// (`char ***elems`), frees each string and the array, and nulls it.
    pub strings_clear: unsafe extern "C" fn(*mut *mut *mut c_char, usize),
    /// `SoapySDR_free`
    pub free: unsafe extern "C" fn(*mut c_void),

    /// `SoapySDRDevice_makeStrArgs`
    pub make_str_args: unsafe extern "C" fn(*const c_char) -> *mut SoapySDRDevice,
    /// `SoapySDRDevice_unmake`
    pub unmake: unsafe extern "C" fn(*mut SoapySDRDevice) -> c_int,
    /// `SoapySDRDevice_lastError`
    pub last_error: unsafe extern "C" fn() -> *const c_char,
    /// `SoapySDRDevice_getDriverKey`
    pub get_driver_key: unsafe extern "C" fn(*mut SoapySDRDevice) -> *mut c_char,
    /// `SoapySDRDevice_getHardwareKey`
    pub get_hardware_key: unsafe extern "C" fn(*mut SoapySDRDevice) -> *mut c_char,
    /// `SoapySDRDevice_getHardwareInfo` (returns by value; free with `kwargs_clear`)
    pub get_hardware_info: unsafe extern "C" fn(*mut SoapySDRDevice) -> SoapySDRKwargs,

    /// `SoapySDRDevice_setSampleRate`
    pub set_sample_rate: unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, f64) -> c_int,
    /// `SoapySDRDevice_getSampleRate`
    pub get_sample_rate: unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize) -> f64,
    /// `SoapySDRDevice_getSampleRateRange` (free the array with `free`)
    pub get_sample_rate_range:
        unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, *mut usize) -> *mut SoapySDRRange,

    /// `SoapySDRDevice_setFrequency`
    pub set_frequency: unsafe extern "C" fn(
        *mut SoapySDRDevice,
        c_int,
        usize,
        f64,
        *const SoapySDRKwargs,
    ) -> c_int,
    /// `SoapySDRDevice_getFrequency`
    pub get_frequency: unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize) -> f64,
    /// `SoapySDRDevice_hasFrequencyCorrection`
    pub has_frequency_correction: unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize) -> bool,
    /// `SoapySDRDevice_setFrequencyCorrection`
    pub set_frequency_correction:
        unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, f64) -> c_int,

    /// `SoapySDRDevice_hasGainMode`
    pub has_gain_mode: unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize) -> bool,
    /// `SoapySDRDevice_setGainMode`
    pub set_gain_mode: unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, bool) -> c_int,
    /// `SoapySDRDevice_setGain`
    pub set_gain: unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, f64) -> c_int,
    /// `SoapySDRDevice_setGainElement`
    pub set_gain_element:
        unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, *const c_char, f64) -> c_int,
    /// `SoapySDRDevice_listGains` (free with `strings_clear`)
    pub list_gains:
        unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, *mut usize) -> *mut *mut c_char,
    /// `SoapySDRDevice_getGainElementRange` (returns by value)
    pub get_gain_element_range:
        unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, *const c_char) -> SoapySDRRange,

    /// `SoapySDRDevice_setBandwidth`
    pub set_bandwidth: unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, f64) -> c_int,
    /// `SoapySDRDevice_getBandwidth`
    pub get_bandwidth: unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize) -> f64,

    /// `SoapySDRDevice_listAntennas` (free with `strings_clear`)
    pub list_antennas:
        unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, *mut usize) -> *mut *mut c_char,
    /// `SoapySDRDevice_setAntenna`
    pub set_antenna:
        unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, *const c_char) -> c_int,

    /// `SoapySDRDevice_writeSetting`
    pub write_setting:
        unsafe extern "C" fn(*mut SoapySDRDevice, *const c_char, *const c_char) -> c_int,

    /// `SoapySDRDevice_setupStream`
    pub setup_stream: unsafe extern "C" fn(
        *mut SoapySDRDevice,
        c_int,
        *const c_char,
        *const usize,
        usize,
        *const SoapySDRKwargs,
    ) -> *mut SoapySDRStream,
    /// `SoapySDRDevice_activateStream`
    pub activate_stream: unsafe extern "C" fn(
        *mut SoapySDRDevice,
        *mut SoapySDRStream,
        c_int,
        c_longlong,
        usize,
    ) -> c_int,
    /// `SoapySDRDevice_deactivateStream`
    pub deactivate_stream:
        unsafe extern "C" fn(*mut SoapySDRDevice, *mut SoapySDRStream, c_int, c_longlong) -> c_int,
    /// `SoapySDRDevice_closeStream`
    pub close_stream: unsafe extern "C" fn(*mut SoapySDRDevice, *mut SoapySDRStream) -> c_int,
    /// `SoapySDRDevice_getStreamMTU`
    pub get_stream_mtu: unsafe extern "C" fn(*mut SoapySDRDevice, *mut SoapySDRStream) -> usize,
    /// `SoapySDRDevice_readStream`
    pub read_stream: unsafe extern "C" fn(
        *mut SoapySDRDevice,
        *mut SoapySDRStream,
        *const *mut c_void,
        usize,
        *mut c_int,
        *mut c_longlong,
        c_long,
    ) -> c_int,
}

impl Api {
    /// The library's `lastError()` as an owned string (empty when unset).
    ///
    /// # Safety
    /// Must be called on a thread that has just made a failing SoapySDR call;
    /// the returned pointer is thread-local storage owned by the library.
    pub fn last_error_text(&self) -> String {
        // SAFETY: `last_error` takes no arguments and returns a pointer to
        // library-owned, NUL-terminated thread-local storage (or null).
        unsafe {
            let p = (self.last_error)();
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }

    /// The library's `SoapySDR_getAPIVersion`, e.g. `"0.8.200"`. This is the
    /// ABI gate's input, not a marketing version.
    pub fn api_version(&self) -> String {
        // SAFETY: takes no arguments and returns a static NUL-terminated
        // string owned by the library.
        unsafe {
            let p = (self.get_api_version)();
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }

    /// A version string for logs and `trunkd check`, when the library says.
    pub fn version(&self) -> Option<String> {
        let read = |f: Option<unsafe extern "C" fn() -> *const c_char>| -> Option<String> {
            let f = f?;
            // SAFETY: both version getters take no arguments and return a
            // static NUL-terminated string owned by the library.
            unsafe {
                let p = f();
                if p.is_null() {
                    None
                } else {
                    Some(CStr::from_ptr(p).to_string_lossy().into_owned())
                }
            }
        };
        read(self.get_lib_version).or_else(|| Some(self.api_version()).filter(|v| !v.is_empty()))
    }

    /// `SoapySDR_getABIVersion` — the string a module must match exactly for
    /// SoapySDR to load it. The single most useful line when a driver the
    /// operator swears they installed does not appear.
    pub fn abi_version(&self) -> Option<String> {
        // SAFETY: takes no arguments, returns a static NUL-terminated string.
        unsafe { static_string((self.get_abi_version?)()) }
    }

    /// `SoapySDR_getRootPath` — the install prefix modules are searched under.
    pub fn root_path(&self) -> Option<String> {
        // SAFETY: as above.
        unsafe { static_string((self.get_root_path?)()) }
    }

    /// The module files SoapySDR can see, each with the version it registered.
    ///
    /// Reads the module search path; it does not load a driver and opens no
    /// device. A module that has not been loaded reports no version, which is
    /// the honest answer — the version is registered *by* the module, so there
    /// is nothing to read until something has needed it.
    ///
    /// An empty vector means either "the library does not export
    /// `SoapySDR_listModules`" or "no modules are installed"; the caller
    /// distinguishes those by whether it also got a root path.
    pub fn modules(&self) -> Vec<(String, Option<String>)> {
        let Some(list) = self.list_modules else {
            return Vec::new();
        };
        let mut len: usize = 0;
        // SAFETY: `listModules` writes the element count through `len` and
        // returns an array of that many NUL-terminated strings, owned by the
        // library and freed below with the matching `SoapySDRStrings_clear`.
        unsafe {
            let paths = list(&mut len);
            if paths.is_null() {
                return Vec::new();
            }
            let mut out = Vec::with_capacity(len);
            for i in 0..len {
                let p = *paths.add(i);
                if p.is_null() {
                    continue;
                }
                let path = CStr::from_ptr(p).to_string_lossy().into_owned();
                let version = self.module_version(p);
                out.push((path, version));
            }
            let mut paths = paths;
            (self.strings_clear)(&mut paths, len);
            out
        }
    }

    /// The version one module registered, freeing the library's copy.
    ///
    /// # Safety
    /// `path` must be a NUL-terminated string naming a module, as handed back
    /// by `SoapySDR_listModules`.
    unsafe fn module_version(&self, path: *const c_char) -> Option<String> {
        let f = self.get_module_version?;
        let p = f(path);
        if p.is_null() {
            return None;
        }
        let v = CStr::from_ptr(p).to_string_lossy().into_owned();
        (self.free)(p.cast());
        (!v.is_empty()).then_some(v)
    }
}

/// Copy a library-owned static string, mapping null and empty to `None`.
///
/// # Safety
/// `p` must be null or a NUL-terminated string that outlives this call.
unsafe fn static_string(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = CStr::from_ptr(p).to_string_lossy().into_owned();
    (!s.is_empty()).then_some(s)
}

/// The advice printed when no SoapySDR library can be loaded.
pub fn install_hint() -> String {
    format!(
        "no SoapySDR {REQUIRED_ABI} library could be loaded. Install the runtime: \
         Linux — `apt install libsoapysdr0.8 soapysdr-tools` plus the driver modules \
         (`soapysdr-module-airspy`, `soapysdr-module-rtlsdr`, SoapySDRPlay3 for SDRplay); \
         Windows — install the PothosSDR bundle and put its bin directory \
         (C:\\Program Files\\PothosSDR\\bin for a winget install, \
         C:\\PothosSDR\\bin for the manual installer) on PATH. \
         A self-contained bundle needs no install: a SoapySDR library next to \
         trunkd.exe, with its drivers in a SoapySDR\\modules0.8 folder beside \
         it, is picked up automatically ({PLUGIN_PATH_ENV} is set for you \
         unless you set it yourself). \
         Set {LIB_ENV}=/full/path/to/libSoapySDR.so to point at a library elsewhere."
    )
}

/// Resolve one symbol, copying the function pointer out of the `Symbol`.
macro_rules! sym {
    ($lib:expr, $name:literal, $t:ty) => {{
        // SAFETY: the name is a documented SoapySDR C entry point and `$t` is
        // its C signature; the pointer is copied out but the owning `Library`
        // is stored in the same struct, so it outlives every call.
        let s: libloading::Symbol<$t> = unsafe { $lib.get(concat!($name, "\0").as_bytes()) }
            .map_err(|e| {
                format!(
                    "the SoapySDR library is missing symbol {}: {}",
                    $name,
                    crate::dl_error(&e)
                )
            })?;
        *s
    }};
}

/// Optional symbol: `None` when the library does not export it.
macro_rules! opt_sym {
    ($lib:expr, $name:literal, $t:ty) => {{
        // SAFETY: as `sym!`, but a missing symbol is not an error.
        let s: Result<libloading::Symbol<$t>, _> =
            unsafe { $lib.get(concat!($name, "\0").as_bytes()) };
        s.ok().map(|s| *s)
    }};
}

/// Try to `dlopen` one candidate and bind every symbol.
fn bind(lib: libloading::Library, lib_name: String) -> Result<Api, String> {
    Ok(Api {
        lib_name,
        get_api_version: sym!(
            lib,
            "SoapySDR_getAPIVersion",
            unsafe extern "C" fn() -> *const c_char
        ),
        get_lib_version: opt_sym!(
            lib,
            "SoapySDR_getLibVersion",
            unsafe extern "C" fn() -> *const c_char
        ),
        get_abi_version: opt_sym!(
            lib,
            "SoapySDR_getABIVersion",
            unsafe extern "C" fn() -> *const c_char
        ),
        get_root_path: opt_sym!(
            lib,
            "SoapySDR_getRootPath",
            unsafe extern "C" fn() -> *const c_char
        ),
        list_modules: opt_sym!(
            lib,
            "SoapySDR_listModules",
            unsafe extern "C" fn(*mut usize) -> *mut *mut c_char
        ),
        get_module_version: opt_sym!(
            lib,
            "SoapySDR_getModuleVersion",
            unsafe extern "C" fn(*const c_char) -> *mut c_char
        ),

        enumerate_str_args: sym!(
            lib,
            "SoapySDRDevice_enumerateStrArgs",
            unsafe extern "C" fn(*const c_char, *mut usize) -> *mut SoapySDRKwargs
        ),
        kwargs_list_clear: sym!(
            lib,
            "SoapySDRKwargsList_clear",
            unsafe extern "C" fn(*mut SoapySDRKwargs, usize)
        ),
        kwargs_clear: sym!(
            lib,
            "SoapySDRKwargs_clear",
            unsafe extern "C" fn(*mut SoapySDRKwargs)
        ),
        kwargs_get: sym!(
            lib,
            "SoapySDRKwargs_get",
            unsafe extern "C" fn(*const SoapySDRKwargs, *const c_char) -> *const c_char
        ),
        strings_clear: sym!(
            lib,
            "SoapySDRStrings_clear",
            unsafe extern "C" fn(*mut *mut *mut c_char, usize)
        ),
        free: sym!(lib, "SoapySDR_free", unsafe extern "C" fn(*mut c_void)),

        make_str_args: sym!(
            lib,
            "SoapySDRDevice_makeStrArgs",
            unsafe extern "C" fn(*const c_char) -> *mut SoapySDRDevice
        ),
        unmake: sym!(
            lib,
            "SoapySDRDevice_unmake",
            unsafe extern "C" fn(*mut SoapySDRDevice) -> c_int
        ),
        last_error: sym!(
            lib,
            "SoapySDRDevice_lastError",
            unsafe extern "C" fn() -> *const c_char
        ),
        get_driver_key: sym!(
            lib,
            "SoapySDRDevice_getDriverKey",
            unsafe extern "C" fn(*mut SoapySDRDevice) -> *mut c_char
        ),
        get_hardware_key: sym!(
            lib,
            "SoapySDRDevice_getHardwareKey",
            unsafe extern "C" fn(*mut SoapySDRDevice) -> *mut c_char
        ),
        get_hardware_info: sym!(
            lib,
            "SoapySDRDevice_getHardwareInfo",
            unsafe extern "C" fn(*mut SoapySDRDevice) -> SoapySDRKwargs
        ),

        set_sample_rate: sym!(
            lib,
            "SoapySDRDevice_setSampleRate",
            unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, f64) -> c_int
        ),
        get_sample_rate: sym!(
            lib,
            "SoapySDRDevice_getSampleRate",
            unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize) -> f64
        ),
        get_sample_rate_range: sym!(
            lib,
            "SoapySDRDevice_getSampleRateRange",
            unsafe extern "C" fn(
                *mut SoapySDRDevice,
                c_int,
                usize,
                *mut usize,
            ) -> *mut SoapySDRRange
        ),

        set_frequency: sym!(
            lib,
            "SoapySDRDevice_setFrequency",
            unsafe extern "C" fn(
                *mut SoapySDRDevice,
                c_int,
                usize,
                f64,
                *const SoapySDRKwargs,
            ) -> c_int
        ),
        get_frequency: sym!(
            lib,
            "SoapySDRDevice_getFrequency",
            unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize) -> f64
        ),
        has_frequency_correction: sym!(
            lib,
            "SoapySDRDevice_hasFrequencyCorrection",
            unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize) -> bool
        ),
        set_frequency_correction: sym!(
            lib,
            "SoapySDRDevice_setFrequencyCorrection",
            unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, f64) -> c_int
        ),

        has_gain_mode: sym!(
            lib,
            "SoapySDRDevice_hasGainMode",
            unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize) -> bool
        ),
        set_gain_mode: sym!(
            lib,
            "SoapySDRDevice_setGainMode",
            unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, bool) -> c_int
        ),
        set_gain: sym!(
            lib,
            "SoapySDRDevice_setGain",
            unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, f64) -> c_int
        ),
        set_gain_element: sym!(
            lib,
            "SoapySDRDevice_setGainElement",
            unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, *const c_char, f64) -> c_int
        ),
        list_gains: sym!(
            lib,
            "SoapySDRDevice_listGains",
            unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, *mut usize) -> *mut *mut c_char
        ),
        get_gain_element_range: sym!(
            lib,
            "SoapySDRDevice_getGainElementRange",
            unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, *const c_char) -> SoapySDRRange
        ),

        set_bandwidth: sym!(
            lib,
            "SoapySDRDevice_setBandwidth",
            unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, f64) -> c_int
        ),
        get_bandwidth: sym!(
            lib,
            "SoapySDRDevice_getBandwidth",
            unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize) -> f64
        ),

        list_antennas: sym!(
            lib,
            "SoapySDRDevice_listAntennas",
            unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, *mut usize) -> *mut *mut c_char
        ),
        set_antenna: sym!(
            lib,
            "SoapySDRDevice_setAntenna",
            unsafe extern "C" fn(*mut SoapySDRDevice, c_int, usize, *const c_char) -> c_int
        ),

        write_setting: sym!(
            lib,
            "SoapySDRDevice_writeSetting",
            unsafe extern "C" fn(*mut SoapySDRDevice, *const c_char, *const c_char) -> c_int
        ),

        setup_stream: sym!(
            lib,
            "SoapySDRDevice_setupStream",
            unsafe extern "C" fn(
                *mut SoapySDRDevice,
                c_int,
                *const c_char,
                *const usize,
                usize,
                *const SoapySDRKwargs,
            ) -> *mut SoapySDRStream
        ),
        activate_stream: sym!(
            lib,
            "SoapySDRDevice_activateStream",
            unsafe extern "C" fn(
                *mut SoapySDRDevice,
                *mut SoapySDRStream,
                c_int,
                c_longlong,
                usize,
            ) -> c_int
        ),
        deactivate_stream: sym!(
            lib,
            "SoapySDRDevice_deactivateStream",
            unsafe extern "C" fn(
                *mut SoapySDRDevice,
                *mut SoapySDRStream,
                c_int,
                c_longlong,
            ) -> c_int
        ),
        close_stream: sym!(
            lib,
            "SoapySDRDevice_closeStream",
            unsafe extern "C" fn(*mut SoapySDRDevice, *mut SoapySDRStream) -> c_int
        ),
        get_stream_mtu: sym!(
            lib,
            "SoapySDRDevice_getStreamMTU",
            unsafe extern "C" fn(*mut SoapySDRDevice, *mut SoapySDRStream) -> usize
        ),
        read_stream: sym!(
            lib,
            "SoapySDRDevice_readStream",
            unsafe extern "C" fn(
                *mut SoapySDRDevice,
                *mut SoapySDRStream,
                *const *mut c_void,
                usize,
                *mut c_int,
                *mut c_longlong,
                c_long,
            ) -> c_int
        ),

        _lib: lib,
    })
}

/// The candidate library names to try, override first.
fn candidates() -> Vec<String> {
    let env_override = std::env::var(LIB_ENV).ok();
    let bundled = exe_dir().as_deref().and_then(bundled_library_path);
    compose_candidates(env_override.as_deref(), bundled.as_deref())
}

fn load() -> Result<Arc<Api>, String> {
    // Before the first dlopen, so a relocated bundle's drivers are findable.
    apply_bundled_plugin_path();

    let mut attempts: Vec<String> = Vec::new();
    for name in candidates() {
        // SAFETY: loading libSoapySDR runs its initializers, which register the
        // built-in factories; the library has no other side effects until one
        // of its functions is called.
        match unsafe { libloading::Library::new(&name) } {
            Ok(lib) => {
                let api = bind(lib, name.clone()).map_err(|e| format!("{name}: {e}"))?;
                // A 0.7 library exports every symbol we bind but hands the
                // stream back through an out-pointer, so calling it with these
                // signatures would corrupt memory. Refuse, loudly.
                let version = api.api_version();
                if !api_version_ok(&version) {
                    return Err(format!(
                        "{name}: SoapySDR ABI {REQUIRED_ABI} is required; found \
                         {}. Install a {REQUIRED_ABI} runtime (Linux: libsoapysdr0.8; \
                         Windows: a current PothosSDR bundle), or point {LIB_ENV} at one.",
                        if version.is_empty() {
                            "no version"
                        } else {
                            &version
                        }
                    ));
                }
                tracing::debug!(lib = %name, %version, lib_version = ?api.version(), "loaded SoapySDR");
                return Ok(Arc::new(api));
            }
            Err(e) => attempts.push(format!("{name}: {}", crate::dl_error(&e))),
        }
    }
    Err(format!(
        "{} (tried {})",
        install_hint(),
        attempts.join("; ")
    ))
}

/// The process-wide SoapySDR binding, loaded on first use.
///
/// A host without the library gets one failed `dlopen` sweep and a cached
/// error string thereafter: enumeration stays cheap on machines that will
/// never have a Soapy device.
pub fn api() -> Result<Arc<Api>, String> {
    static API: OnceLock<Result<Arc<Api>, String>> = OnceLock::new();
    API.get_or_init(load).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private directory for one test, in the repo's usual style.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("trunkd-soapy-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn a_bundled_module_dir_becomes_the_plugin_path() {
        let dir = scratch("plugin");
        // No modules folder yet: nothing to point at.
        assert_eq!(plugin_path_override(&dir, None), None);

        std::fs::create_dir_all(bundled_module_dir(&dir)).expect("modules dir");
        assert_eq!(
            plugin_path_override(&dir, None),
            Some(bundled_module_dir(&dir))
        );
        // Blank is "unset" as far as SoapySDR is concerned, so we may fill it.
        assert!(plugin_path_override(&dir, Some("  ")).is_some());
        // An operator's explicit choice is never overridden.
        assert_eq!(plugin_path_override(&dir, Some("C:\\drivers")), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_bundled_library_is_found_only_when_it_is_there() {
        let dir = scratch("lib");
        assert_eq!(bundled_library_path(&dir), None);
        let lib = dir.join(BUNDLED_LIB_NAME);
        std::fs::write(&lib, b"not really a library").expect("write lib");
        assert_eq!(bundled_library_path(&dir), Some(lib));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_override_outranks_the_bundle_which_outranks_the_bare_names() {
        let bundled = Path::new("/opt/trunkd/libSoapySDR.so.0.8");

        let all = compose_candidates(Some("/custom/libSoapySDR.so.0.8"), Some(bundled));
        assert_eq!(all[0], "/custom/libSoapySDR.so.0.8");
        assert_eq!(all[1], bundled.to_string_lossy());
        assert_eq!(all[2..], LIB_CANDIDATES);

        // A blank override is not an override.
        let no_env = compose_candidates(Some("   "), Some(bundled));
        assert_eq!(no_env[0], bundled.to_string_lossy());

        let bare = compose_candidates(None, None);
        assert_eq!(bare, LIB_CANDIDATES);
    }

    #[test]
    fn the_abi_gate_accepts_0_8_and_later() {
        assert!(api_version_ok("0.8.200"));
        assert!(api_version_ok("0.8-3"));
        assert!(api_version_ok("0.8"));
        assert!(api_version_ok("0.9.0"));
        assert!(api_version_ok("1.0.0"));
        assert!(api_version_ok("  0.8.1  "));
    }

    #[test]
    fn the_abi_gate_rejects_0_7_and_nonsense() {
        // 0.7's setupStream has a different shape; loading it would corrupt
        // memory rather than fail, which is exactly what this gate prevents.
        assert!(!api_version_ok("0.7.2"));
        assert!(!api_version_ok("0.7"));
        assert!(!api_version_ok("0.6.1"));
        assert!(!api_version_ok(""));
        assert!(!api_version_ok("unknown"));
        assert!(!api_version_ok("0"));
        assert!(!api_version_ok("0."));
        assert!(!api_version_ok("v0.8.0"));
        assert!(!api_version_ok(".8"));
    }
}
