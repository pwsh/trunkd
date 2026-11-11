// SPDX-License-Identifier: GPL-3.0-or-later
//! `trunkd-sdr` — the SDR source layer.
//!
//! One [`TunerDevice`] trait abstracts every signal source. A dedicated OS
//! thread per device does nothing but move samples into a lock-free SPSC ring
//! (`rtrb`); when the ring is full samples are **dropped and counted**, never
//! buffered without bound (ARCHITECTURE §2).
//!
//! Backends are cargo features:
//! - [`iqfile`](iqfile) (default) — play back interleaved IQ files as a virtual
//!   tuner; the test source.
//! - [`hackrf`](hackrf) (default) — a HackRF One driven through the stock
//!   `hackrf_transfer` CLI, with [`portapack`](portapack) mode switching for
//!   boards running the PortaPack/Mayhem firmware. Default because it needs
//!   nothing at build time.
//! - [`soapy`](soapy) (default) — every SDR SoapySDR has a module for (Airspy
//!   R2/Mini, SDRplay RSP, RTL-SDR, ...). The SoapySDR C ABI is bound by hand
//!   and `dlopen`ed at run time, so like `hackrf` it costs nothing at build
//!   time; only a machine that actually configures a Soapy device needs
//!   `libSoapySDR` installed.

pub mod device;
pub mod stream;

#[cfg(feature = "iqfile")]
pub mod iqfile;

#[cfg(feature = "hackrf")]
pub mod hackrf;

#[cfg(feature = "hackrf")]
pub mod portapack;

#[cfg(feature = "soapy")]
pub mod soapy;

pub use device::{soapy_args, DeviceDescriptor, TunerConfig, TunerDevice, TunerEvent};
pub use stream::{ring_metric_names, GainRequest, GainSlot, RetuneSlot, StreamConfig, TunerStream};

#[cfg(feature = "iqfile")]
pub use iqfile::{IqFileTuner, IqFormat, Lockstep, Pacing};

#[cfg(feature = "hackrf")]
pub use hackrf::{HackRfConfig, HackRfControl, HackRfTuner};

#[cfg(feature = "soapy")]
pub use soapy::{
    runtime_report as soapy_runtime_report, GainElementRange, SoapyOptions, SoapyRuntime,
    SoapyTuner,
};

/// A `libloading` error with the loader's own reason attached.
///
/// Since libloading 0.9 an error displays only as `dlopen failed` /
/// `dlsym failed` (or `LoadLibraryExW failed` on Windows); what the operator
/// needs — `cannot open shared object file: No such file or directory`, the
/// Windows error code — is the error's `source()`. Every message that names a
/// library or symbol goes through here so that reason is never dropped.
#[cfg(any(feature = "soapy", feature = "hackrf"))]
pub(crate) fn dl_error(e: &libloading::Error) -> String {
    use std::error::Error as _;
    match e.source() {
        Some(reason) => format!("{e}: {reason}"),
        None => e.to_string(),
    }
}

#[cfg(all(test, any(feature = "soapy", feature = "hackrf")))]
mod dl_error_tests {
    #[test]
    fn a_failed_load_keeps_the_loaders_reason() {
        // A name no system has: this exercises the loader's error path only and
        // never touches a real library or device.
        let name = "libtrunkd-test-no-such-library.so.0";
        // SAFETY: the load fails, so no initializer ever runs.
        let e = unsafe { libloading::Library::new(name) }.expect_err("must not load");
        let msg = super::dl_error(&e);
        assert!(
            msg.len() > e.to_string().len(),
            "the reason was dropped: {msg:?}"
        );
        #[cfg(unix)]
        assert!(msg.contains(name), "dlerror names the file: {msg:?}");
    }
}

/// Enumerate every device the compiled-in hardware backends can discover.
///
/// File sources are absent by design: they are opened by explicit path, never
/// discovered. Backends that are not compiled in contribute nothing.
pub fn enumerate_all() -> Vec<DeviceDescriptor> {
    #[allow(unused_mut)]
    let mut found: Vec<DeviceDescriptor> = Vec::new();
    #[cfg(feature = "hackrf")]
    found.extend(<hackrf::HackRfTuner as TunerDevice>::enumerate());
    #[cfg(feature = "soapy")]
    found.extend(<soapy::SoapyTuner as TunerDevice>::enumerate());
    found
}
