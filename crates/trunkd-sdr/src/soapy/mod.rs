// SPDX-License-Identifier: GPL-3.0-or-later
//! `soapy` backend: every SDR SoapySDR has a module for — Airspy R2/Mini,
//! SDRplay RSP series, RTL-SDR, and more — driven through the SoapySDR C ABI
//! loaded at **run time**.
//!
//! # No build-time dependency
//!
//! The old implementation wrapped the `soapysdr` crate, which needs bindgen,
//! libclang, pkg-config and the SoapySDR headers *to compile*. That forced the
//! feature off by default and left the backend unreachable on stock builds.
//! This one binds the C ABI by hand ([`ffi`]) and `dlopen`s the library on
//! first use, so `soapy` is a default feature: nothing is needed to build, and
//! a machine that will never see a Soapy device pays one failed `dlopen`.
//!
//! # Shape
//!
//! [`SoapyTuner::open_with`] applies configuration in a fixed order — sample
//! rate, bandwidth, antenna, frequency, gain mode, overall gain, per-element
//! gains, driver settings — so a per-element gain always wins over the overall
//! one, and logs the device's own view of itself (driver/hardware keys, actual
//! rate and bandwidth, antennas, and every gain element with its range) at
//! `info`. That log line is how an operator learns what to put in `gains`.
//!
//! [`TunerDevice::start`] spawns one thread that reads `CF32` with a 250 ms
//! timeout and pushes into the ring with drop-and-count on overflow
//! (ARCHITECTURE §2). SoapySDR's *own* overflow (`SOAPY_SDR_OVERFLOW`: the
//! driver lost samples before we saw them) is counted separately as
//! `sdr.{id}.soapy_overflows`.
//!
//! # The sample-rate readback guard
//!
//! Soapy drivers coerce an unsupported rate to a nearby supported one and
//! return success. That is fatal here: the channelizer derives its bin count
//! from `rate / 12500` and every recorded channel would sit at the wrong
//! frequency with no error anywhere. So [`SoapyTuner::open_with`] reads the
//! rate back and refuses to open unless it matches the request to within 1 Hz,
//! reporting the device's supported ranges in the error.

pub mod ffi;
pub mod kwargs;

use std::collections::BTreeMap;
use std::ffi::{c_int, c_longlong, c_void};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;

use num_complex::Complex;
use trunkd_core::chan::bounded;
use trunkd_core::{DeviceId, Error, Hz, IqSample, Metrics, Result};

use crate::device::{DeviceDescriptor, TunerConfig, TunerDevice, TunerEvent};
use crate::stream::{StreamConfig, StreamParts, TunerStream};

use ffi::{Api, SoapySDRDevice, SoapySDRRange, SoapySDRStream, SOAPY_SDR_RX};
use kwargs::{cstr, format_args as format_kwargs, Kwargs};

pub use crate::device::soapy_args as compose_soapy_args;

/// The receive channel trunkd uses. Every supported device is single-channel
/// for our purposes; a second RX on an RSPduo would be a separate `[[device]]`.
const RX_CHANNEL: usize = 0;

/// Stream sample format: interleaved 32-bit float I/Q, matching [`IqSample`].
const FORMAT_CF32: &str = "CF32";

/// `readStream` timeout. Long enough that an idle device does not spin, short
/// enough that a stop request is honoured promptly.
const READ_TIMEOUT_US: std::ffi::c_long = 250_000;

/// Fallback transfer size when the driver reports no stream MTU.
const DEFAULT_MTU: usize = 1 << 14;

/// Tolerance, in Hz, for the sample-rate readback guard.
const RATE_TOLERANCE_HZ: f64 = 1.0;

/// Driver-specific knobs that have no home in the backend-generic
/// [`TunerConfig`].
///
/// All optional: an empty `SoapyOptions` reproduces the old behaviour (rate,
/// frequency, overall gain).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SoapyOptions {
    /// Analog filter bandwidth in Hz. Left alone when `None`; an RSPdx wants it
    /// set explicitly or it filters away the edges of a wide capture.
    pub bandwidth_hz: Option<f64>,
    /// RX antenna/port name, e.g. `"Antenna B"` on an RSPdx.
    pub rx_port: Option<String>,
    /// Hardware AGC. `None` leaves the driver default.
    pub agc: Option<bool>,
    /// Per-element gains by name, e.g. `LNA`/`MIX`/`VGA` on an Airspy.
    /// Applied *after* the overall gain, so these win.
    pub gains: BTreeMap<String, f64>,
    /// Driver settings passed to `writeSetting`, e.g. `biastee=false`.
    pub settings: BTreeMap<String, String>,
}

/// One configuration call [`SoapyTuner::open_with`] will make, in order.
///
/// Splitting the plan from its execution makes the ordering rule — per-element
/// gains after the overall gain, settings last — testable without hardware.
#[derive(Debug, Clone, PartialEq)]
pub enum OpenStep {
    /// `setSampleRate`, then the readback guard.
    SampleRate(f64),
    /// `setBandwidth`.
    Bandwidth(f64),
    /// `setAntenna`.
    Antenna(String),
    /// `setFrequency`, already ppm-corrected if the driver has no correction.
    Frequency(f64),
    /// `setGainMode` (hardware AGC).
    GainMode(bool),
    /// `setGain` (the driver's overall/distributed gain).
    Gain(f64),
    /// `setGainElement`.
    GainElement(String, f64),
    /// `writeSetting`.
    Setting(String, String),
}

/// One RX gain element the driver exposes, with the range it accepts.
///
/// Reported up to the control plane so keyup can offer an operator the
/// element names and limits the *actual* board has, rather than a guess: this
/// is the same information the open-time info log prints, in a form a status
/// POST can carry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GainElementRange {
    /// The driver's own element name (`LNA`, `MIX`, `VGA`, `IFGR`, ...).
    pub name: String,
    /// Lowest accepted value in dB.
    pub min: f64,
    /// Highest accepted value in dB.
    pub max: f64,
    /// Step in dB; 0 when the driver reports a continuous range.
    pub step: f64,
}

/// Effective tune frequency for a ppm correction.
///
/// A tuner told to receive `f` with a crystal that is `ppm` parts-per-million
/// fast must be *asked* for `f * (1 + ppm/1e6)`.
pub fn corrected_freq(freq: Hz, ppm: f64) -> f64 {
    freq.as_u64() as f64 * (1.0 + ppm / 1_000_000.0)
}

/// The ordered configuration plan for one open.
pub fn open_plan(config: &TunerConfig, options: &SoapyOptions) -> Vec<OpenStep> {
    let mut plan = vec![OpenStep::SampleRate(config.sample_rate.as_f64())];
    if let Some(bw) = options.bandwidth_hz {
        plan.push(OpenStep::Bandwidth(bw));
    }
    if let Some(port) = &options.rx_port {
        plan.push(OpenStep::Antenna(port.clone()));
    }
    plan.push(OpenStep::Frequency(corrected_freq(
        config.center_freq,
        config.ppm,
    )));
    if let Some(agc) = options.agc {
        plan.push(OpenStep::GainMode(agc));
    }
    if let Some(gain) = config.gain {
        plan.push(OpenStep::Gain(gain));
    }
    for (name, value) in &options.gains {
        plan.push(OpenStep::GainElement(name.clone(), *value));
    }
    for (key, value) in &options.settings {
        plan.push(OpenStep::Setting(key.clone(), value.clone()));
    }
    plan
}

/// Whether a readback sample rate is the one that was asked for.
pub fn rate_matches(requested: f64, actual: f64) -> bool {
    (requested - actual).abs() <= RATE_TOLERANCE_HZ
}

/// The error text for a coerced sample rate.
pub fn rate_mismatch_message(
    args: &str,
    requested: f64,
    actual: f64,
    ranges: &[SoapySDRRange],
) -> String {
    let supported = if ranges.is_empty() {
        "the driver reports no sample-rate ranges".to_owned()
    } else {
        let list: Vec<String> = ranges
            .iter()
            .map(|r| {
                if r.minimum == r.maximum {
                    format!("{:.0}", r.minimum)
                } else if r.step > 0.0 {
                    format!("{:.0}–{:.0} step {:.0}", r.minimum, r.maximum, r.step)
                } else {
                    format!("{:.0}–{:.0}", r.minimum, r.maximum)
                }
            })
            .collect();
        format!("supported: {}", list.join(", "))
    };
    format!(
        "soapy '{args}': requested sample_rate {requested:.0} Hz but the device \
         settled on {actual:.0} Hz. trunkd needs the exact rate — the channelizer \
         derives its bin count from rate/12500, so a coerced rate silently moves \
         every channel. Pick a rate the hardware really supports ({supported})."
    )
}

/// A `*mut SoapySDRDevice` with an owner.
///
/// The pointer is `Send` because the SoapySDR device object is only ever
/// touched by whichever single thread holds this handle: the opening thread up
/// to [`TunerDevice::start`], and the reader thread after it.
struct DeviceHandle {
    ptr: *mut SoapySDRDevice,
    api: Arc<Api>,
}

// SAFETY: see the type docs — ownership is moved, never shared, and SoapySDR
// device objects support being used from a thread other than the opener.
unsafe impl Send for DeviceHandle {}

impl Drop for DeviceHandle {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: `ptr` came from `makeStrArgs` and is unmade exactly once,
            // here, because `DeviceHandle` is not `Clone`.
            let rc = unsafe { (self.api.unmake)(self.ptr) };
            if rc != 0 {
                tracing::warn!(rc, "SoapySDRDevice_unmake failed");
            }
            self.ptr = std::ptr::null_mut();
        }
    }
}

/// A tuner backed by a SoapySDR device.
pub struct SoapyTuner {
    descriptor: DeviceDescriptor,
    config: TunerConfig,
    options: SoapyOptions,
    api: Arc<Api>,
    device: DeviceHandle,
    /// Whether [`SoapyTuner::apply_plan`] pushed `ppm` into the driver's own
    /// frequency correction. Kept as a field, not a local, because the reader
    /// thread has to make the same choice for every runtime retune: correct
    /// the frequency itself, or leave it nominal because the hardware already
    /// does.
    hw_ppm: bool,
}

/// What the library reports about itself, for `trunkd check`.
///
/// `Err` carries the load failure, including install advice, so the CLI can
/// tell "no SoapySDR installed" apart from "installed but no devices found".
pub fn library_status() -> std::result::Result<String, String> {
    let api = ffi::api()?;
    Ok(match api.version() {
        Some(v) => format!("{} (SoapySDR {v})", api.lib_name),
        None => api.lib_name.clone(),
    })
}

/// Everything the SoapySDR runtime says about itself, for the About page.
///
/// Built by `dlopen`ing the library and calling its version and module
/// getters. **No device is opened and no driver is loaded**: `listModules`
/// reads the module search path off the filesystem, and `getModuleVersion`
/// answers from the registry a module fills in when something else loads it,
/// so a module nobody has needed yet simply reports no version.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SoapyRuntime {
    /// The candidate name or path that `dlopen` accepted.
    pub library: String,
    /// The full path of the mapped object, where the platform can say.
    pub resolved_path: Option<String>,
    /// `SoapySDR_getAPIVersion`, e.g. `"0.8.1"`.
    pub api_version: Option<String>,
    /// `SoapySDR_getLibVersion` — the distribution's own version string.
    pub lib_version: Option<String>,
    /// `SoapySDR_getABIVersion` — what a module must match to be loadable.
    pub abi_version: Option<String>,
    /// `SoapySDR_getRootPath` — the install prefix modules live under.
    pub root_path: Option<String>,
    /// Each module file found, with the version it has registered (if any).
    pub modules: Vec<(String, Option<String>)>,
}

/// Query the SoapySDR runtime, or say why it could not be.
///
/// `Err` carries the same load failure (and install advice) that
/// [`library_status`] reports, so a page can print it verbatim instead of
/// inventing a shorter, less useful message.
pub fn runtime_report() -> std::result::Result<SoapyRuntime, String> {
    let api = ffi::api()?;
    Ok(SoapyRuntime {
        resolved_path: mapped_library_path(&api.lib_name),
        library: api.lib_name.clone(),
        api_version: Some(api.api_version()).filter(|v| !v.is_empty()),
        lib_version: api.version(),
        abi_version: api.abi_version(),
        root_path: api.root_path(),
        modules: api.modules(),
    })
}

/// The path the dynamic loader actually mapped, when the OS will tell us.
///
/// `dlopen("libSoapySDR.so.0.8")` says nothing about *which* copy of the
/// library was found, and "which copy" is the question behind most SoapySDR
/// mysteries (a PothosSDR bundle shadowing a distribution package, a bundled
/// library next to the executable, an `LD_LIBRARY_PATH` left over from a
/// shell). Linux answers it in `/proc/self/maps`; elsewhere the candidate name
/// is the best that can honestly be said, so this returns `None` and the page
/// shows the name alone.
fn mapped_library_path(lib_name: &str) -> Option<String> {
    // An operator-supplied absolute path is already the answer.
    if std::path::Path::new(lib_name).is_absolute() {
        return Some(lib_name.to_owned());
    }
    if !cfg!(target_os = "linux") {
        return None;
    }
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    maps.lines()
        .filter_map(|l| l.split_once('/').map(|(_, rest)| format!("/{rest}")))
        .find(|p| {
            std::path::Path::new(p)
                .file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.starts_with("libSoapySDR."))
        })
}

impl SoapyTuner {
    /// Open a Soapy device by args string, e.g. `driver=airspy`.
    pub fn open(args: &str, config: TunerConfig) -> Result<Self> {
        Self::open_with(args, config, SoapyOptions::default())
    }

    /// Open with driver-specific options; see [`SoapyOptions`].
    pub fn open_with(args: &str, config: TunerConfig, options: SoapyOptions) -> Result<Self> {
        let api = ffi::api().map_err(Error::device)?;
        let c_args = cstr(args);
        // SAFETY: `makeStrArgs` takes a NUL-terminated args string and returns
        // a device or null (with the reason in `lastError`).
        let ptr = unsafe { (api.make_str_args)(c_args.as_ptr()) };
        if ptr.is_null() {
            return Err(Error::device(format!(
                "soapy open '{args}' failed: {}",
                api.last_error_text()
            )));
        }
        let device = DeviceHandle {
            ptr,
            api: api.clone(),
        };

        let mut tuner = SoapyTuner {
            descriptor: DeviceDescriptor {
                id: DeviceId::new(format!("soapy:{args}")),
                driver: format!("soapy:{args}"),
                label: format!("SoapySDR {args}"),
                args: args.to_owned(),
            },
            config,
            options,
            api,
            device,
            hw_ppm: false,
        };
        tuner.apply_plan(args)?;
        tuner.describe_and_log(args);
        Ok(tuner)
    }

    /// Run [`open_plan`] against the device, then guard the sample rate.
    fn apply_plan(&mut self, args: &str) -> Result<()> {
        let plan = open_plan(&self.config, &self.options);
        let dev = self.device.ptr;
        let api = &self.api;
        // Whether the driver applied ppm itself; if it did, the plan's
        // pre-corrected frequency must not be used.
        let hw_ppm = self.config.ppm != 0.0 && self.apply_hardware_ppm()?;

        for step in plan {
            match step {
                OpenStep::SampleRate(rate) => {
                    // SAFETY (every arm): `dev` is a live device from
                    // `makeStrArgs`, the direction/channel are valid, and each
                    // string argument outlives its call.
                    self.check(
                        unsafe { (api.set_sample_rate)(dev, SOAPY_SDR_RX, RX_CHANNEL, rate) },
                        "set sample rate",
                    )?;
                    let actual = unsafe { (api.get_sample_rate)(dev, SOAPY_SDR_RX, RX_CHANNEL) };
                    if !rate_matches(rate, actual) {
                        return Err(Error::device(rate_mismatch_message(
                            args,
                            rate,
                            actual,
                            &self.sample_rate_ranges(),
                        )));
                    }
                }
                OpenStep::Bandwidth(bw) => {
                    self.check(
                        unsafe { (api.set_bandwidth)(dev, SOAPY_SDR_RX, RX_CHANNEL, bw) },
                        "set bandwidth",
                    )?;
                }
                OpenStep::Antenna(name) => {
                    let c = cstr(&name);
                    self.check(
                        unsafe { (api.set_antenna)(dev, SOAPY_SDR_RX, RX_CHANNEL, c.as_ptr()) },
                        "set antenna",
                    )?;
                }
                OpenStep::Frequency(corrected) => {
                    let hz = if hw_ppm {
                        self.config.center_freq.as_u64() as f64
                    } else {
                        corrected
                    };
                    self.check(
                        unsafe {
                            (api.set_frequency)(dev, SOAPY_SDR_RX, RX_CHANNEL, hz, std::ptr::null())
                        },
                        "set frequency",
                    )?;
                }
                OpenStep::GainMode(agc) => {
                    let has = unsafe { (api.has_gain_mode)(dev, SOAPY_SDR_RX, RX_CHANNEL) };
                    if has {
                        self.check(
                            unsafe { (api.set_gain_mode)(dev, SOAPY_SDR_RX, RX_CHANNEL, agc) },
                            "set gain mode",
                        )?;
                    } else if agc {
                        tracing::warn!(device = %args, "agc requested but this driver has no gain mode; ignoring");
                    }
                }
                OpenStep::Gain(gain) => {
                    self.check(
                        unsafe { (api.set_gain)(dev, SOAPY_SDR_RX, RX_CHANNEL, gain) },
                        "set gain",
                    )?;
                }
                OpenStep::GainElement(name, value) => {
                    let c = cstr(&name);
                    self.check(
                        unsafe {
                            (api.set_gain_element)(dev, SOAPY_SDR_RX, RX_CHANNEL, c.as_ptr(), value)
                        },
                        &format!("set gain element '{name}'"),
                    )?;
                }
                OpenStep::Setting(key, value) => {
                    let (ck, cv) = (cstr(&key), cstr(&value));
                    self.check(
                        unsafe { (api.write_setting)(dev, ck.as_ptr(), cv.as_ptr()) },
                        &format!("write setting '{key}'"),
                    )?;
                }
            }
        }
        // Recorded for the reader thread's runtime retunes, which must repeat
        // this same choice between a pre-corrected and a nominal frequency.
        self.hw_ppm = hw_ppm;
        Ok(())
    }

    /// Push the ppm correction into the hardware when the driver supports one.
    /// Returns whether it did; if not, the caller pre-corrects the frequency.
    fn apply_hardware_ppm(&self) -> Result<bool> {
        let (dev, api) = (self.device.ptr, &self.api);
        // SAFETY: live device, valid direction/channel.
        let has = unsafe { (api.has_frequency_correction)(dev, SOAPY_SDR_RX, RX_CHANNEL) };
        if !has {
            return Ok(false);
        }
        self.check(
            unsafe {
                (api.set_frequency_correction)(dev, SOAPY_SDR_RX, RX_CHANNEL, self.config.ppm)
            },
            "set frequency correction",
        )?;
        Ok(true)
    }

    /// Turn a C return code into a `Result`, attaching the library's own text.
    fn check(&self, rc: c_int, what: &str) -> Result<()> {
        if rc == 0 {
            return Ok(());
        }
        let detail = self.api.last_error_text();
        let detail = if detail.is_empty() {
            ffi::error_text(rc).to_owned()
        } else {
            detail
        };
        Err(Error::device(format!("soapy {what}: rc {rc} ({detail})")))
    }

    /// The device's supported sample-rate ranges (empty if it reports none).
    fn sample_rate_ranges(&self) -> Vec<SoapySDRRange> {
        let (dev, api) = (self.device.ptr, &self.api);
        let mut len: usize = 0;
        // SAFETY: the call fills `len` and returns a `SoapySDR_free`-able array
        // of that many ranges, or null.
        unsafe {
            let p = (api.get_sample_rate_range)(dev, SOAPY_SDR_RX, RX_CHANNEL, &mut len);
            if p.is_null() {
                return Vec::new();
            }
            let out = std::slice::from_raw_parts(p, len).to_vec();
            (api.free)(p.cast());
            out
        }
    }

    /// The RX gain elements this device exposes, with their ranges.
    ///
    /// Public because the control plane reports them: an operator setting
    /// `gains` from a GUI needs the element names and limits of the board in
    /// front of them, and only the open knows those.
    pub fn gain_element_ranges(&self) -> Vec<GainElementRange> {
        self.gain_elements()
            .into_iter()
            .map(|(name, r)| GainElementRange {
                name,
                min: r.minimum,
                max: r.maximum,
                step: r.step,
            })
            .collect()
    }

    /// Gain element names paired with their ranges.
    fn gain_elements(&self) -> Vec<(String, SoapySDRRange)> {
        let (dev, api) = (self.device.ptr, &self.api);
        let mut len: usize = 0;
        // SAFETY: `listGains` fills `len` and returns a strings array freed by
        // `take_strings`; `getGainElementRange` takes one of those names.
        unsafe {
            let p = (api.list_gains)(dev, SOAPY_SDR_RX, RX_CHANNEL, &mut len);
            kwargs::take_strings(api, p, len)
                .into_iter()
                .map(|name| {
                    let c = cstr(&name);
                    let r = (api.get_gain_element_range)(dev, SOAPY_SDR_RX, RX_CHANNEL, c.as_ptr());
                    (name, r)
                })
                .collect()
        }
    }

    /// Available RX antenna/port names.
    fn antennas(&self) -> Vec<String> {
        let (dev, api) = (self.device.ptr, &self.api);
        let mut len: usize = 0;
        // SAFETY: as `gain_elements`.
        unsafe {
            let p = (api.list_antennas)(dev, SOAPY_SDR_RX, RX_CHANNEL, &mut len);
            kwargs::take_strings(api, p, len)
        }
    }

    /// Fill in the descriptor label from the hardware and log what we opened.
    ///
    /// This is the line operators read to choose `gains`, `rx_port` and
    /// `settings`, so it carries every element range the driver exposes.
    fn describe_and_log(&mut self, args: &str) {
        let (dev, api) = (self.device.ptr, &self.api);
        // SAFETY: both key getters return `SoapySDR_free`-able strings;
        // `getHardwareInfo` returns kwargs owned by us, released by
        // `kwargs_clear`.
        let (driver_key, hw_key, hw_info) = unsafe {
            let d = kwargs::take_string(api, (api.get_driver_key)(dev));
            let h = kwargs::take_string(api, (api.get_hardware_key)(dev));
            let mut info = (api.get_hardware_info)(dev);
            let map = kwargs::read_kwargs(&info);
            (api.kwargs_clear)(&mut info);
            (d, h, map)
        };
        // SAFETY: live device, valid direction/channel.
        let (rate, bandwidth, freq) = unsafe {
            (
                (api.get_sample_rate)(dev, SOAPY_SDR_RX, RX_CHANNEL),
                (api.get_bandwidth)(dev, SOAPY_SDR_RX, RX_CHANNEL),
                (api.get_frequency)(dev, SOAPY_SDR_RX, RX_CHANNEL),
            )
        };
        let gains: Vec<String> = self
            .gain_elements()
            .into_iter()
            .map(|(n, r)| format!("{n} {:.1}–{:.1} dB", r.minimum, r.maximum))
            .collect();

        if !hw_key.is_empty() {
            self.descriptor.label = hw_key.clone();
        } else if let Some(l) = hw_info.get("label") {
            self.descriptor.label = l.clone();
        }
        if !driver_key.is_empty() {
            self.descriptor.driver = format!("soapy:{}", driver_key.to_lowercase());
        }

        tracing::info!(
            device = %args,
            driver = %driver_key,
            hardware = %hw_key,
            info = %format_kwargs(&hw_info),
            sample_rate = rate,
            bandwidth_hz = bandwidth,
            frequency_hz = freq,
            antennas = %self.antennas().join(", "),
            gain_elements = %gains.join(", "),
            "opened SoapySDR device"
        );
    }
}

impl TunerDevice for SoapyTuner {
    fn enumerate() -> Vec<DeviceDescriptor> {
        let api = match ffi::api() {
            Ok(api) => api,
            Err(e) => {
                tracing::debug!(error = %e, "soapy enumerate skipped");
                return Vec::new();
            }
        };
        let empty = cstr("");
        let mut len: usize = 0;
        // SAFETY: `enumerateStrArgs` fills `len` and returns a kwargs list of
        // that length, released below with `kwargs_list_clear`.
        let found = unsafe {
            let p = (api.enumerate_str_args)(empty.as_ptr(), &mut len);
            if p.is_null() {
                return Vec::new();
            }
            let out: Vec<Kwargs> = (0..len).map(|i| kwargs::read_kwargs(&*p.add(i))).collect();
            (api.kwargs_list_clear)(p, len);
            out
        };

        found
            .into_iter()
            .map(|map| {
                let args = format_kwargs(&map);
                let driver = map.get("driver").cloned().unwrap_or_default();
                let label = map
                    .get("label")
                    .cloned()
                    .filter(|l| !l.is_empty())
                    .unwrap_or_else(|| args.clone());
                DeviceDescriptor {
                    id: DeviceId::new(format!("soapy:{args}")),
                    driver: format!("soapy:{driver}"),
                    label,
                    args,
                }
            })
            .collect()
    }

    fn descriptor(&self) -> &DeviceDescriptor {
        &self.descriptor
    }

    fn config(&self) -> &TunerConfig {
        &self.config
    }

    fn set_center_freq(&mut self, freq: Hz) -> Result<()> {
        let hz = corrected_freq(freq, self.config.ppm);
        // SAFETY: live device; a null kwargs pointer means "no tuning hints".
        let rc = unsafe {
            (self.api.set_frequency)(
                self.device.ptr,
                SOAPY_SDR_RX,
                RX_CHANNEL,
                hz,
                std::ptr::null(),
            )
        };
        self.check(rc, "set frequency")?;
        self.config.center_freq = freq;
        Ok(())
    }

    fn set_gain(&mut self, gain: f64) -> Result<()> {
        // SAFETY: live device, valid direction/channel.
        let rc = unsafe { (self.api.set_gain)(self.device.ptr, SOAPY_SDR_RX, RX_CHANNEL, gain) };
        self.check(rc, "set gain")?;
        self.config.gain = Some(gain);
        Ok(())
    }

    fn start(self: Box<Self>, stream: StreamConfig, metrics: &Metrics) -> Result<TunerStream> {
        let (event_tx, event_rx) = bounded::<TunerEvent>(stream.event_capacity);
        let (mut writer, parts) = StreamParts::new(&self.descriptor.id, stream, metrics, event_rx);
        let stop = parts.stop.clone();

        let overflow_metric = metrics.counter(&format!(
            "sdr.{}.soapy_overflows",
            self.descriptor.id.as_str()
        ));
        let label = self.descriptor.label.clone();
        let name = self.descriptor.id.as_str().to_owned();
        let api = self.api.clone();
        let device = self.device;
        // This backend can retune a running stream: the reader thread owns the
        // device handle, so it can call `setFrequency` between reads with no
        // new sharing and no restart.
        let retune = parts.retune_slot();
        retune.mark_supported();
        // And it can change gain, AGC and ppm the same way, for the same
        // reason: every one of those is a call on the device handle this
        // thread owns, so none of them needs a restart.
        let gain_slot = parts.gain_slot();
        gain_slot.mark_supported();
        let (hw_ppm, ppm) = (self.hw_ppm, self.config.ppm);
        let start_freq = self.config.center_freq;

        let handle = thread::Builder::new()
            .name(format!("soapy:{name}"))
            .spawn(move || {
                // The handle moves in here and is unmade when this thread ends.
                let dev = device.ptr;
                let fmt = cstr(FORMAT_CF32);
                let channels = [RX_CHANNEL];
                // SAFETY: a live device, the CF32 format string, and a
                // one-element channel array whose length is passed alongside.
                let stream_ptr: *mut SoapySDRStream = unsafe {
                    (api.setup_stream)(
                        dev,
                        SOAPY_SDR_RX,
                        fmt.as_ptr(),
                        channels.as_ptr(),
                        channels.len(),
                        std::ptr::null(),
                    )
                };
                if stream_ptr.is_null() {
                    event_tx.send(TunerEvent::Error(format!(
                        "setupStream: {}",
                        api.last_error_text()
                    )));
                    return;
                }
                // SAFETY: `stream_ptr` is the stream just set up on `dev`.
                let rc = unsafe { (api.activate_stream)(dev, stream_ptr, 0, 0, 0) };
                if rc != 0 {
                    event_tx.send(TunerEvent::Error(format!(
                        "activateStream: rc {rc} ({})",
                        api.last_error_text()
                    )));
                    // SAFETY: closing a stream that was set up but not started.
                    unsafe { (api.close_stream)(dev, stream_ptr) };
                    return;
                }

                // SAFETY: live device and stream.
                let mtu = unsafe { (api.get_stream_mtu)(dev, stream_ptr) };
                let mtu = if mtu == 0 { DEFAULT_MTU } else { mtu };
                let mut buf: Vec<IqSample> = vec![Complex::new(0.0, 0.0); mtu];

                // The frequency and correction this thread is currently on.
                // A runtime ppm change on a driver with no hardware frequency
                // correction has to be applied by re-tuning, so the thread has
                // to remember what it is nominally tuned to.
                let mut ppm = ppm;
                let mut hw_ppm = hw_ppm;
                let mut nominal = start_freq;

                while !stop.load(Ordering::Relaxed) {
                    // Runtime gain / AGC / ppm, checked on the same pass as a
                    // retune and for the same reason: these are device calls
                    // that only this thread may make. Each failure is a warn,
                    // never the end of the stream — a board still delivering
                    // samples at the old gain beats a teardown.
                    if let Some(req) = gain_slot.take() {
                        if let Some(agc) = req.agc {
                            // SAFETY: live device, valid direction/channel.
                            let rc = unsafe {
                                (api.set_gain_mode)(dev, SOAPY_SDR_RX, RX_CHANNEL, agc)
                            };
                            if rc != 0 {
                                tracing::warn!(device = %label, rc, agc, error = %api.last_error_text(), "soapy set gain mode failed");
                            }
                        }
                        if let Some(g) = req.gain {
                            // SAFETY: live device, valid direction/channel.
                            let rc =
                                unsafe { (api.set_gain)(dev, SOAPY_SDR_RX, RX_CHANNEL, g) };
                            if rc != 0 {
                                tracing::warn!(device = %label, rc, gain = g, error = %api.last_error_text(), "soapy set gain failed");
                            }
                        }
                        // After the overall gain, exactly as the open plan
                        // orders them, so a per-element value still wins.
                        for (name, value) in &req.gains {
                            let c = cstr(name);
                            // SAFETY: live device; `c` is a NUL-terminated
                            // element name owned for the duration of the call.
                            let rc = unsafe {
                                (api.set_gain_element)(
                                    dev,
                                    SOAPY_SDR_RX,
                                    RX_CHANNEL,
                                    c.as_ptr(),
                                    *value,
                                )
                            };
                            if rc != 0 {
                                tracing::warn!(device = %label, rc, element = %name, value, error = %api.last_error_text(), "soapy set gain element failed");
                            }
                        }
                        if let Some(new_ppm) = req.ppm {
                            ppm = new_ppm;
                            // SAFETY: live device, valid direction/channel.
                            let has = unsafe {
                                (api.has_frequency_correction)(dev, SOAPY_SDR_RX, RX_CHANNEL)
                            };
                            if has {
                                // SAFETY: as above.
                                let rc = unsafe {
                                    (api.set_frequency_correction)(
                                        dev,
                                        SOAPY_SDR_RX,
                                        RX_CHANNEL,
                                        new_ppm,
                                    )
                                };
                                hw_ppm = rc == 0;
                                if rc != 0 {
                                    tracing::warn!(device = %label, rc, ppm = new_ppm, error = %api.last_error_text(), "soapy set frequency correction failed");
                                }
                            }
                            if !hw_ppm {
                                // No hardware correction: re-ask for the same
                                // nominal frequency, pre-corrected by the new
                                // ppm, which is exactly what the open does.
                                retune.request(nominal);
                            }
                        }
                    }

                    // Runtime retune, checked once per pass — `read_stream`
                    // below times out every 250 ms, so a request is applied
                    // within that even on a silent device. A failure is a
                    // warning, not the end of the stream: the board keeps
                    // delivering samples on the old frequency, which is worth
                    // far more than a teardown.
                    if let Some(want) = retune.take() {
                        nominal = want;
                        let hz = if hw_ppm {
                            nominal.as_u64() as f64
                        } else {
                            corrected_freq(nominal, ppm)
                        };
                        // SAFETY: `dev` is the live device this thread owns; a
                        // null kwargs pointer means "no tuning hints".
                        let rc = unsafe {
                            (api.set_frequency)(dev, SOAPY_SDR_RX, RX_CHANNEL, hz, std::ptr::null())
                        };
                        if rc == 0 {
                            event_tx.send(TunerEvent::Retuned(nominal));
                        } else {
                            tracing::warn!(
                                device = %label,
                                rc,
                                hz,
                                error = %api.last_error_text(),
                                "soapy retune failed; staying on the current frequency"
                            );
                        }
                    }

                    let mut flags: c_int = 0;
                    let mut time_ns: c_longlong = 0;
                    let buffs: [*mut c_void; 1] = [buf.as_mut_ptr().cast()];
                    // SAFETY: `buffs[0]` points at `mtu` `Complex<f32>` values,
                    // which is exactly the CF32 layout the stream writes, and
                    // `mtu` elements are what we ask for.
                    let ret = unsafe {
                        (api.read_stream)(
                            dev,
                            stream_ptr,
                            buffs.as_ptr(),
                            buf.len(),
                            &mut flags,
                            &mut time_ns,
                            READ_TIMEOUT_US,
                        )
                    };
                    if ret > 0 {
                        writer.write_batch(&buf[..ret as usize]);
                    } else if ret == 0 || ret == ffi::SOAPY_SDR_TIMEOUT {
                        // Nothing ready this window; benign.
                        continue;
                    } else if ret == ffi::SOAPY_SDR_OVERFLOW {
                        overflow_metric.inc();
                    } else {
                        let text = ffi::error_text(ret);
                        tracing::warn!(device = %label, rc = ret, error = text, "soapy read error");
                        event_tx.send(TunerEvent::Error(format!("readStream: rc {ret} ({text})")));
                        break;
                    }
                }

                // Deactivate, then close the stream, then let `device` drop
                // (which unmakes it) — strictly in that order, and all on the
                // thread that owns the handle. Anything else hands a driver a
                // device it is still streaming from.
                //
                // Windows note: `unmake` runs the driver's destructor, and
                // **libairspy 1.0.10 crashes there when it is paired with
                // libusb 1.0.30 or newer**. `airspy_close()` calls
                // `libusb_close()` before `free_transfers()`, and since libusb
                // 1.0.28 `libusb_free_transfer()` unrefs the device the
                // transfer was submitted against — so it walks a device
                // reference the just-closed handle already dropped, and takes
                // the process down with an access violation. That is not this
                // teardown order's doing: airspy-tools' own `airspy_rx.exe`
                // faults identically on exit with the same two libraries, and
                // both are clean against libusb 1.0.24. The fix belongs to
                // whatever ships the DLLs — pin libusb 1.0.24 next to
                // libairspy; see docs/sdr-hardware.md.
                //
                // SAFETY: `dev` and `stream_ptr` are the live device and the
                // stream set up on it above, and each is torn down exactly once.
                unsafe {
                    (api.deactivate_stream)(dev, stream_ptr, 0, 0);
                    (api.close_stream)(dev, stream_ptr);
                }
                drop(device);
            })
            .map_err(|e| Error::device(format!("failed to spawn soapy thread: {e}")))?;

        Ok(parts.into_stream(handle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trunkd_core::SampleRate;

    fn cfg() -> TunerConfig {
        let mut c = TunerConfig::new(Hz(855_000_000), SampleRate(10_000_000));
        c.gain = Some(20.0);
        c
    }

    #[test]
    fn the_plan_applies_element_gains_after_the_overall_gain() {
        let mut opts = SoapyOptions {
            bandwidth_hz: Some(8_000_000.0),
            rx_port: Some("Antenna B".into()),
            agc: Some(false),
            ..Default::default()
        };
        opts.gains.insert("LNA".into(), 12.0);
        opts.gains.insert("VGA".into(), 12.0);
        opts.settings.insert("biastee".into(), "false".into());

        let plan = open_plan(&cfg(), &opts);
        assert_eq!(
            plan,
            vec![
                OpenStep::SampleRate(10_000_000.0),
                OpenStep::Bandwidth(8_000_000.0),
                OpenStep::Antenna("Antenna B".into()),
                OpenStep::Frequency(855_000_000.0),
                OpenStep::GainMode(false),
                OpenStep::Gain(20.0),
                OpenStep::GainElement("LNA".into(), 12.0),
                OpenStep::GainElement("VGA".into(), 12.0),
                OpenStep::Setting("biastee".into(), "false".into()),
            ]
        );
    }

    #[test]
    fn a_bare_config_plans_only_rate_and_frequency() {
        let mut c = cfg();
        c.gain = None;
        assert_eq!(
            open_plan(&c, &SoapyOptions::default()),
            vec![
                OpenStep::SampleRate(10_000_000.0),
                OpenStep::Frequency(855_000_000.0),
            ]
        );
    }

    #[test]
    fn ppm_moves_the_planned_tune_frequency() {
        let mut c = cfg();
        c.ppm = 50.0;
        // 50 ppm at 855 MHz is 42.75 kHz — more than three 12.5 kHz bins.
        assert!((corrected_freq(c.center_freq, c.ppm) - 855_042_750.0).abs() < 1e-3);
        assert!(matches!(
            open_plan(&c, &SoapyOptions::default())[1],
            OpenStep::Frequency(f) if (f - 855_042_750.0).abs() < 1e-6
        ));
    }

    #[test]
    fn the_rate_guard_allows_a_hertz_of_slop_and_no_more() {
        assert!(rate_matches(10_000_000.0, 10_000_000.0));
        assert!(rate_matches(10_000_000.0, 9_999_999.5));
        assert!(!rate_matches(10_000_000.0, 9_999_998.0));
        // The RTL-SDR case that motivated the guard: 2.56 Msps coerced.
        assert!(!rate_matches(2_560_000.0, 2_400_000.0));
    }

    #[test]
    fn the_rate_error_names_the_request_the_result_and_the_ranges() {
        let ranges = [
            SoapySDRRange {
                minimum: 2_000_000.0,
                maximum: 10_000_000.0,
                step: 0.0,
            },
            SoapySDRRange {
                minimum: 250_000.0,
                maximum: 250_000.0,
                step: 0.0,
            },
        ];
        let msg = rate_mismatch_message("driver=sdrplay", 8_000_001.0, 8_000_000.0, &ranges);
        assert!(msg.contains("driver=sdrplay"), "{msg}");
        assert!(msg.contains("8000001"), "{msg}");
        assert!(msg.contains("8000000"), "{msg}");
        assert!(msg.contains("2000000–10000000"), "{msg}");
        assert!(msg.contains("250000"), "{msg}");

        let none = rate_mismatch_message("driver=x", 1.0, 2.0, &[]);
        assert!(none.contains("no sample-rate ranges"), "{none}");
    }

    #[test]
    fn error_codes_map_to_text() {
        assert_eq!(ffi::error_text(ffi::SOAPY_SDR_TIMEOUT), "timeout");
        assert_eq!(ffi::error_text(ffi::SOAPY_SDR_STREAM_ERROR), "stream error");
        assert_eq!(ffi::error_text(ffi::SOAPY_SDR_CORRUPTION), "corruption");
        assert_eq!(ffi::error_text(ffi::SOAPY_SDR_OVERFLOW), "overflow");
        assert_eq!(
            ffi::error_text(ffi::SOAPY_SDR_NOT_SUPPORTED),
            "not supported"
        );
        assert_eq!(ffi::error_text(ffi::SOAPY_SDR_TIME_ERROR), "time error");
        assert_eq!(ffi::error_text(ffi::SOAPY_SDR_UNDERFLOW), "underflow");
        assert_eq!(ffi::error_text(-99), "unknown error");
    }

    #[test]
    fn a_missing_library_is_reported_with_install_advice_and_the_override() {
        let hint = ffi::install_hint();
        assert!(hint.contains("PothosSDR"), "{hint}");
        assert!(hint.contains("soapysdr-tools"), "{hint}");
        assert!(hint.contains(ffi::LIB_ENV), "{hint}");
    }

    #[test]
    fn composed_args_are_re_exported_for_the_recorder() {
        assert_eq!(
            compose_soapy_args(None, Some("airspy"), None),
            "driver=airspy"
        );
    }

    /// Hardware/library smoke test: `--ignored` because a build host need not
    /// have libSoapySDR. On a host that does but has no driver modules, this
    /// must return an empty list rather than fail.
    #[test]
    #[ignore = "requires libSoapySDR at run time"]
    fn soapy_library_loads_and_enumerates() {
        let status = library_status().expect("libSoapySDR should load");
        println!("SoapySDR library: {status}");
        let devices = <SoapyTuner as TunerDevice>::enumerate();
        println!("enumerated {} device(s)", devices.len());
        for d in &devices {
            println!("  [{}] {} ({})", d.driver, d.label, d.args);
        }
    }
}
