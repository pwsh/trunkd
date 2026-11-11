// SPDX-License-Identifier: GPL-3.0-or-later
//! The native libraries this process can reach, asked what they are.
//!
//! trunkd takes almost no build-time native dependencies: SoapySDR and
//! libhackrf are `dlopen`ed at run time, which is what makes one binary work on
//! a box with an Airspy, a box with a HackRF and a box with neither. The price
//! is that "which SoapySDR is this actually using?" has no answer you can read
//! off the binary — and that question, with its cousin "why is my SDRplay
//! module not being found?", is behind most of the deployment time this page
//! exists to save.
//!
//! # Nothing here opens a device
//!
//! Loading a shared library is not opening a radio. Every call below is a
//! version getter or a directory listing:
//!
//! * SoapySDR — `getAPIVersion` / `getLibVersion` / `getABIVersion` /
//!   `getRootPath`, then `listModules` (a filesystem scan) and
//!   `getModuleVersion` (a lookup in the registry a module fills in when it is
//!   loaded). No `enumerate`, no `make`, no stream.
//! * libhackrf — `hackrf_library_version` / `hackrf_library_release`, both
//!   static strings; not even `hackrf_init` is called.
//! * LAME — statically linked, so it is a plain function call.
//!
//! # Missing is not broken
//!
//! Every entry reports `available: false` with the reason rather than failing
//! the request. A recorder with only Soapy devices *should* have no libhackrf,
//! and a page that shouted about it would train an operator to ignore it.

use serde_json::{json, Value};

/// The whole `libraries` section.
///
/// Ordered most- to least- likely to be the thing you are looking at: the SDR
/// runtime that decides whether the recorder can see its radios, then the
/// board-specific library, then the encoder, then what the Rust standard
/// library thinks it is running on.
pub fn libraries() -> Value {
    json!({
        "soapysdr": soapysdr(),
        "libhackrf": libhackrf(),
        "mp3_encoder": mp3_encoder(),
        "rust_std": rust_std(),
    })
}

/// The SoapySDR runtime: versions, install prefix, and every module it sees.
fn soapysdr() -> Value {
    #[cfg(feature = "soapy")]
    {
        match trunkd_sdr::soapy_runtime_report() {
            Ok(r) => json!({
                "available": true,
                "library": r.library,
                "resolved_path": r.resolved_path,
                "api_version": r.api_version,
                "lib_version": r.lib_version,
                "abi_version": r.abi_version,
                "root_path": r.root_path,
                "modules": r.modules.iter().map(|(path, version)| json!({
                    "path": path,
                    "version": version,
                })).collect::<Vec<_>>(),
                "module_count": r.modules.len(),
            }),
            // The load error carries the install advice `trunkd check` prints,
            // which is a page's worth of help in one string.
            Err(e) => not_available(e),
        }
    }
    #[cfg(not(feature = "soapy"))]
    {
        not_available("the `soapy` feature is not compiled into this build")
    }
}

/// libhackrf, which only the PortaPack reset path needs.
fn libhackrf() -> Value {
    #[cfg(feature = "hackrf")]
    {
        match trunkd_sdr::portapack::library_version() {
            Some((version, release)) => json!({
                "available": true,
                "version": version,
                "release": release,
            }),
            None => not_available(
                "libhackrf could not be loaded; only the PortaPack reset path needs it",
            ),
        }
    }
    #[cfg(not(feature = "hackrf"))]
    {
        not_available("the `hackrf` feature is not compiled into this build")
    }
}

/// LAME, linked in rather than loaded.
fn mp3_encoder() -> Value {
    let (name, version) = trunkd_call::mp3_encoder_version();
    json!({
        "available": true,
        "name": name,
        "version": version,
        "linkage": "static",
    })
}

/// What the Rust standard library reports about the target it was built for.
fn rust_std() -> Value {
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "family": std::env::consts::FAMILY,
        "target": env!("TRUNKD_TARGET"),
        "pointer_width": usize::BITS,
    })
}

/// The shape every unavailable library reports, so the page can render one
/// row-and-a-reason without special cases.
fn not_available(reason: impl Into<String>) -> Value {
    json!({ "available": false, "reason": reason.into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_library_reports_availability_one_way_or_the_other() {
        let libs = libraries();
        for name in ["soapysdr", "libhackrf", "mp3_encoder"] {
            let entry = &libs[name];
            let available = entry["available"]
                .as_bool()
                .unwrap_or_else(|| panic!("{name} did not report availability: {entry}"));
            if available {
                assert!(entry.get("reason").is_none());
            } else {
                assert!(
                    entry["reason"].as_str().is_some_and(|r| !r.is_empty()),
                    "{name} is unavailable without saying why: {entry}"
                );
            }
        }
    }

    #[test]
    fn a_missing_sdr_library_is_reported_rather_than_fatal() {
        // Whether libSoapySDR is installed on the machine running the tests is
        // not something a test may assume; what it may insist on is that both
        // answers are well-formed and neither panics.
        let soapy = libraries()["soapysdr"].clone();
        match soapy["available"].as_bool() {
            Some(true) => assert!(
                soapy["library"].as_str().is_some_and(|s| !s.is_empty()),
                "a loaded library names itself: {soapy}"
            ),
            Some(false) => assert!(soapy["reason"].as_str().is_some()),
            None => panic!("no availability flag: {soapy}"),
        }
    }

    #[test]
    fn the_encoder_and_the_std_target_are_always_known() {
        let libs = libraries();
        assert_eq!(libs["mp3_encoder"]["name"], "libmp3lame");
        assert!(
            libs["mp3_encoder"]["version"]
                .as_str()
                .is_some_and(|v| !v.is_empty()),
            "LAME is linked in, so it can always be asked"
        );
        assert_eq!(libs["rust_std"]["os"], std::env::consts::OS);
        assert!(libs["rust_std"]["target"]
            .as_str()
            .is_some_and(|t| !t.is_empty()));
    }
}
