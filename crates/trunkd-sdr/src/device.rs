// SPDX-License-Identifier: GPL-3.0-or-later
//! The [`TunerDevice`] trait and its supporting descriptor/config/event types.

use trunkd_core::chan::BoundedReceiver;
use trunkd_core::{DeviceId, Hz, Metrics, Result, SampleRate};

use crate::stream::{StreamConfig, TunerStream};

/// Static description of a discoverable device, returned by
/// [`TunerDevice::enumerate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDescriptor {
    /// Stable identifier (driver key, serial, or file path).
    pub id: DeviceId,
    /// Backend/driver name, e.g. `"soapy:hackrf"` or `"iqfile"`.
    pub driver: String,
    /// Human-readable label for logs and the control API.
    pub label: String,
    /// Backend-specific arguments, e.g. the Soapy args string `driver=hackrf`.
    pub args: String,
}

/// Requested tuner configuration applied on open and by re-tuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TunerConfig {
    /// Center (tune) frequency.
    pub center_freq: Hz,
    /// Complex sample rate.
    pub sample_rate: SampleRate,
    /// Gain in dB. `None` requests automatic/hardware-default gain.
    pub gain: Option<f64>,
    /// Frequency correction in parts-per-million.
    pub ppm: f64,
}

impl TunerConfig {
    /// A minimal config given a center frequency and sample rate.
    pub fn new(center_freq: Hz, sample_rate: SampleRate) -> Self {
        TunerConfig {
            center_freq,
            sample_rate,
            gain: None,
            ppm: 0.0,
        }
    }
}

/// Out-of-band events emitted by a running tuner stream.
///
/// These flow on a bounded drop-oldest channel toward the supervisor; they are
/// never on the sample hot path.
#[derive(Debug, Clone, PartialEq)]
pub enum TunerEvent {
    /// The device re-tuned to a new center frequency.
    Retuned(Hz),
    /// The sample source reached its end (relevant for file playback).
    EndOfStream,
    /// A recoverable error occurred; the string describes it.
    Error(String),
}

/// Receiving half of a tuner's event channel.
pub type TunerEvents = BoundedReceiver<TunerEvent>;

/// A signal source that can be enumerated, configured, tuned, and streamed.
///
/// Implementors spawn exactly one reader thread in [`TunerDevice::start`] that
/// pushes samples into the ring buffer with drop-and-count on overflow.
pub trait TunerDevice: Send {
    /// Discover devices this backend can open. Cheap and side-effect free.
    fn enumerate() -> Vec<DeviceDescriptor>
    where
        Self: Sized;

    /// The descriptor this instance was opened from.
    fn descriptor(&self) -> &DeviceDescriptor;

    /// The currently applied configuration.
    fn config(&self) -> &TunerConfig;

    /// Re-tune to a new center frequency without restarting the stream where
    /// the backend supports it.
    fn set_center_freq(&mut self, freq: Hz) -> Result<()>;

    /// Adjust gain in dB.
    fn set_gain(&mut self, gain: f64) -> Result<()>;

    /// Start streaming: spawn the reader thread and return the consumer side of
    /// the ring plus the event channel. Consumes the device handle; stopping is
    /// via [`TunerStream::stop`].
    fn start(self: Box<Self>, stream: StreamConfig, metrics: &Metrics) -> Result<TunerStream>
    where
        Self: Sized;
}

/// Compose the effective Soapy args for a configured device.
///
/// An explicit args string always wins; otherwise the args are built from the
/// driver name plus, when given, a serial to pin one board of several.
pub fn soapy_args(explicit: Option<&str>, driver: Option<&str>, serial: Option<&str>) -> String {
    if let Some(a) = explicit {
        let a = a.trim();
        if !a.is_empty() {
            return a.to_owned();
        }
    }
    let mut out = String::new();
    if let Some(d) = driver.map(str::trim).filter(|d| !d.is_empty()) {
        out.push_str("driver=");
        out.push_str(d);
    }
    if let Some(s) = serial.map(str::trim).filter(|s| !s.is_empty()) {
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str("serial=");
        out.push_str(s);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::soapy_args;

    #[test]
    fn composition_prefers_explicit_args_then_driver_and_serial() {
        assert_eq!(
            soapy_args(Some("driver=hackrf,serial=abc"), Some("airspy"), None),
            "driver=hackrf,serial=abc"
        );
        assert_eq!(soapy_args(None, Some("airspy"), None), "driver=airspy");
        assert_eq!(
            soapy_args(None, Some("rtlsdr"), Some("00000001")),
            "driver=rtlsdr,serial=00000001"
        );
        // A blank explicit string falls through rather than opening "".
        assert_eq!(
            soapy_args(Some("  "), Some("sdrplay"), None),
            "driver=sdrplay"
        );
        // Nothing configured composes to nothing; the recorder rejects that.
        assert_eq!(soapy_args(None, None, Some("x")), "serial=x");
        assert_eq!(soapy_args(None, None, None), "");
    }
}
