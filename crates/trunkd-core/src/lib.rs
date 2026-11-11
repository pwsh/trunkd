// SPDX-License-Identifier: GPL-3.0-or-later
//! `trunkd-core` — foundational, dependency-light types shared across the whole
//! trunkd engine.
//!
//! Nothing in this crate performs DSP or I/O of its own; it defines the
//! vocabulary (frequencies, timestamps, identifiers), the anti-unbounded-queue
//! primitives ([`chan`]), the [`metrics`] registry, and the [`Error`] taxonomy
//! that the rest of the workspace builds on.

pub mod chan;
pub mod error;
pub mod metrics;
pub mod skip;
pub mod time;
pub mod types;

pub use error::{Error, Result};
pub use metrics::{Metric, MetricKind, MetricSnapshot, Metrics};
pub use skip::{SkipList, SkipSpec};
pub use time::Timestamp;
pub use types::{DeviceId, Hz, IqSample, SampleRate};
