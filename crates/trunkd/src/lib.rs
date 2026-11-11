// SPDX-License-Identifier: GPL-3.0-or-later
//! `trunkd` — a P25 trunked-radio recorder.
//!
//! The binary is a thin CLI over this library. Everything the recorder does
//! lives behind [`recorder::run_once`], which owns the whole pipeline — SDR,
//! DSP, call assembly, uplink and site observations — for the duration of one
//! run. Keeping it callable as a function is what lets the integration tests
//! replay a wideband capture end to end in-process, against a mock Keyup,
//! rather than spawning and screen-scraping the binary.
//!
//! * [`about`] — build, dependency, library and host provenance for the
//!   panel's `/about` page.
//! * [`config`] — the TOML document and its validation.
//! * [`logging`] — tracing setup (console plus optional rolling files).
//! * [`import`] — sdrtrunk playlist → `trunkd.toml`.
//! * [`recorder`] — the run loop, supervision and status line.
//! * `pipeline` — the threads: SDR → DSP → calls → uplink (ARCHITECTURE §2).
//! * [`ppmmemory`] — the per-board frequency-correction memory, re-exported
//!   from the pipeline because `trunkd check` prints it.

pub mod about;
pub mod config;
pub mod import;
pub mod logging;
mod pipeline;
pub mod recorder;
#[cfg(test)]
mod testalloc;

pub use pipeline::ppmmemory;
pub use recorder::{run_once, run_once_with_reload, RunSummary, UplinkStatsSummary};
