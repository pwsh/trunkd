// SPDX-License-Identifier: GPL-3.0-or-later
//! P25 Phase 1 (C4FM / LSM) receive-side decoder engine.
//!
//! Implemented purely from `docs/P25P1-SPEC.md` (clean-room recreation).
//! Complex baseband IQ in at channel rate; decoded trunking messages, call
//! events and raw 144-bit IMBE voice frames out.

pub mod alias;
pub mod bits;
pub mod call;
pub mod channel;
pub mod demod;
pub mod dsp;
pub mod edac;
pub mod event;
pub mod framer;
pub mod frames;
pub mod iden;
pub mod interleave;
pub mod inventory;
pub mod lrrp;
pub mod message;
pub mod payload;
pub mod site;
pub mod trunking;

pub use alias::{AliasMemory, TalkerAliasAssembler};
pub use bits::{BitBuf, Dibit};
pub use call::{CallAssembler, CallRecord};
pub use channel::{ChannelRole, P25P1Channel, P25P1WidebandChannel};
pub use dsp::{Ddc, DemodMode};
pub use event::P25Event;
pub use inventory::{InventorySnapshot, SiteInventory};
pub use lrrp::{contains_lrrp_transport, extract_lrrp, extract_lrrp_scan, LrrpFix};
pub use site::{CallId, EngineRole, GrantInfo, SiteConfig, SiteEngine, SiteEvent};
pub use trunking::{SharedTracker, TrunkingTracker};

/// Errors returned when constructing an engine channel.
///
/// Returned by the channel constructors and by the DSP builders
/// (`FrontEnd::new`, `Ddc::try_new`) alike.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum P25Error {
    /// The channel-rate sample rate is at or below the engine's floor.
    #[error("sample rate {rate} Hz too low (must be > {min} Hz)")]
    SampleRateTooLow {
        /// Rate that was requested.
        rate: u32,
        /// Exclusive lower bound.
        min: u32,
    },
    /// The wideband capture rate is not an integer multiple of the channel
    /// rate (the DDC cascade has no fractional resampler).
    #[error("sample rate {sample_rate} Hz must be an integer multiple of channel rate {channel_rate} Hz")]
    RateNotDivisible {
        /// Wideband capture rate.
        sample_rate: u32,
        /// Requested P25 channel rate.
        channel_rate: u32,
    },
    /// The requested channel rate cannot describe a P25 channel (zero).
    #[error("invalid channel rate (must be > 0)")]
    InvalidChannelRate,
    /// The wideband capture rate is zero.
    #[error("invalid sample rate (must be > 0)")]
    InvalidSampleRate,
    /// The channel-rate front end cannot be designed alias-free: the decimated
    /// rate is below twice the demodulator's stopband edge (16 kHz LSM,
    /// 13 kHz C4FM).
    #[error("decimated rate {decimated_rate} Hz is below the {required} Hz Nyquist rate needed for the stopband edge")]
    RateBelowNyquist {
        /// Rate the front end would run at after decimation.
        decimated_rate: u32,
        /// Twice the stopband edge.
        required: u32,
    },
    /// A site engine was attached to a shared channel bank built for a
    /// different capture (sample rate, centre frequency or prototype length).
    #[error("site configuration does not match the shared channel bank: {what} differs")]
    ChannelBankMismatch {
        /// Which parameter disagrees.
        what: &'static str,
    },
}
