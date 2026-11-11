// SPDX-License-Identifier: GPL-3.0-or-later
//! Core newtypes: frequencies, sample rates, sample aliases and identifiers.

use serde::{Deserialize, Serialize};

/// A frequency in hertz.
///
/// P25 systems live in the hundreds of MHz, comfortably inside `u64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Hz(pub u64);

impl Hz {
    /// Construct from a whole number of hertz.
    pub const fn new(v: u64) -> Self {
        Hz(v)
    }

    /// Construct from kilohertz.
    pub const fn khz(v: u64) -> Self {
        Hz(v * 1_000)
    }

    /// Construct from megahertz.
    pub const fn mhz(v: u64) -> Self {
        Hz(v * 1_000_000)
    }

    /// The value in hertz.
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// The value in megahertz as an `f64` (for display/planning).
    pub fn as_mhz(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }
}

impl std::fmt::Display for Hz {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.6} MHz", self.as_mhz())
    }
}

/// A complex sample rate in samples per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SampleRate(pub u32);

impl SampleRate {
    /// The rate in samples per second.
    pub const fn as_u32(self) -> u32 {
        self.0
    }

    /// The rate as an `f64` for duration math.
    pub fn as_f64(self) -> f64 {
        self.0 as f64
    }

    /// Wall-clock duration represented by `n` complex samples at this rate.
    pub fn duration_for(self, n: u64) -> std::time::Duration {
        if self.0 == 0 {
            return std::time::Duration::ZERO;
        }
        std::time::Duration::from_secs_f64(n as f64 / self.as_f64())
    }
}

impl std::fmt::Display for SampleRate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} sps", self.0)
    }
}

/// One complex baseband sample, interleaved I/Q as `f32`.
///
/// This is the currency of the whole DSP chain; the SDR layer produces it and
/// every downstream stage consumes it.
pub type IqSample = num_complex::Complex<f32>;

/// A stable, human-meaningful identifier for a configured device.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(pub String);

impl DeviceId {
    /// Construct from anything string-like.
    pub fn new(id: impl Into<String>) -> Self {
        DeviceId(id.into())
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for DeviceId {
    fn from(s: &str) -> Self {
        DeviceId(s.to_owned())
    }
}
