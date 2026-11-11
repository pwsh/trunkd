// SPDX-License-Identifier: GPL-3.0-or-later
//! Power squelch: noise-floor tracker with dB thresholds and attack/hang
//! timing, plus the discriminator-noise veto.
//!
//! The engine measures the channel in blocks of [`BLOCK_SAMPLES`] input
//! samples ([`BLOCK_MS`] ms) and calls [`PowerSquelch::update`] once per
//! block with the block's channel power and discriminator noise.
//!
//! - **Floor**: a slow minimum tracker on the block power in dB: it follows
//!   the power down with a 2 s time constant and up with 30 s, and only
//!   while the squelch is closed, so a call never lifts its own floor.
//! - **Open**: power at least `open_db` above the floor for `attack_ms`
//!   (consecutive blocks), *and* the discriminator noise over those blocks
//!   below [`VETO_NOISE_RMS`]. A run of loud blocks that fails the noise
//!   check is energy with no FM carrier under it (noise, splatter,
//!   intermod); it counts once in [`PowerSquelch::false_opens`] and keeps
//!   being re-checked over a sliding `attack_ms` window, so a real carrier
//!   that follows without a gap still opens.
//! - **Close**: once open, a block is *present* while power stays above
//!   `close_db` (hysteresis). The squelch closes after `hang_ms` of
//!   consecutive absent blocks; a present block within the hang resumes.

use crate::{SquelchParams, INPUT_RATE_HZ};

/// Input samples per squelch block (5 ms at 25 ksps).
pub const BLOCK_SAMPLES: usize = 125;

/// Squelch block length, ms.
pub const BLOCK_MS: u32 = (BLOCK_SAMPLES as u32 * 1000) / INPUT_RATE_HZ;

/// Floor tracker time constant downward (power below the floor), s.
const FLOOR_DOWN_S: f32 = 2.0;

/// Floor tracker time constant upward (power above the floor), s.
const FLOOR_UP_S: f32 = 30.0;

/// Discriminator noise (RMS of the 6–10 kHz band, normalised units, mean
/// over the attack window) at or above which an opening is vetoed.
///
/// Calibrated on synthetic signals through the engine's chain (per-block
/// means; SNR in 12.5 kHz):
///
/// | input               | 12.5 kHz channel | 25 kHz channel |
/// |---------------------|------------------|----------------|
/// | white noise alone   | 0.70 (0.52–0.91) | 0.67 (0.53–0.83) |
/// | carrier, 6 dB SNR   | 0.15 (max 0.42)  | 0.38 (max 0.50) |
/// | carrier, 10 dB SNR  | 0.07             | 0.22           |
/// | carrier, 20 dB SNR  | 0.02             | 0.07           |
///
/// (ranges: 1st–99th percentile of single 5 ms blocks; audio content makes
/// no difference. The 12.5 kHz column is for the 4 / 6.5 kHz channel
/// filter.) Over the 8-block attack window the spread shrinks about
/// threefold, so 0.5 sits ≥ 5 σ from noise and from a 6 dB carrier in both
/// bandwidths; the squelch's own `open_db` (8 dB) is the binding limit for
/// weak carriers.
pub const VETO_NOISE_RMS: f32 = 0.5;

/// Smallest power reported, dBFS (silence would otherwise be -inf).
pub const MIN_POWER_DBFS: f32 = -200.0;

/// Mean-square power in dBFS, clamped at [`MIN_POWER_DBFS`].
pub fn power_dbfs(mean_square: f32) -> f32 {
    if mean_square <= 0.0 {
        return MIN_POWER_DBFS;
    }
    (10.0 * mean_square.log10()).max(MIN_POWER_DBFS)
}

/// What the squelch decided for one block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SquelchBlock {
    /// Block power, dBFS.
    pub power_dbfs: f32,
    /// Block power above the floor, dB.
    pub snr_db: f32,
    /// The squelch is open (from the end of the attack to the end of the
    /// hang).
    pub open: bool,
    /// Open and this block is above `close_db` (carrier present).
    pub present: bool,
    /// Opened at this block: the opening run began this many blocks before
    /// the current one (`attack` blocks minus one at minimum), i.e. the
    /// carrier came up at block `current - rose`.
    pub rose: Option<u32>,
    /// Closed at this block: the absent run (the hang) began this many
    /// blocks before the current one.
    pub closed: Option<u32>,
    /// This block starts a new above-`open_db` run while closed (a good
    /// moment to reset the tone detectors).
    pub run_started: bool,
}

/// Carrier/SNR squelch for one channel. See the module docs.
#[derive(Clone, Debug)]
pub struct PowerSquelch {
    open_db: f32,
    close_db: f32,
    attack_blocks: u32,
    hang_blocks: u32,
    a_down: f32,
    a_up: f32,
    floor_db: Option<f32>,
    open: bool,
    /// Closed: consecutive blocks above `open_db`.
    run: u32,
    /// The current run has already been counted as a false open.
    run_vetoed: bool,
    /// Discriminator noise of the last `attack_blocks` blocks (ring).
    noise: Vec<f32>,
    noise_pos: usize,
    /// Open: consecutive absent blocks.
    absent: u32,
    false_opens: u64,
}

impl PowerSquelch {
    /// Squelch with the thresholds and timing of `p`.
    pub fn new(p: &SquelchParams) -> Self {
        let attack_blocks = p.attack_ms.div_ceil(BLOCK_MS).max(1);
        let hang_blocks = p.hang_ms.div_ceil(BLOCK_MS).max(1);
        let block_s = BLOCK_MS as f32 / 1000.0;
        PowerSquelch {
            open_db: p.open_db,
            close_db: p.close_db,
            attack_blocks,
            hang_blocks,
            a_down: 1.0 - (-block_s / FLOOR_DOWN_S).exp(),
            a_up: 1.0 - (-block_s / FLOOR_UP_S).exp(),
            floor_db: None,
            open: false,
            run: 0,
            run_vetoed: false,
            noise: vec![0.0; attack_blocks as usize],
            noise_pos: 0,
            absent: 0,
            false_opens: 0,
        }
    }

    /// Blocks of attack.
    pub fn attack_blocks(&self) -> u32 {
        self.attack_blocks
    }

    /// Blocks of hang.
    pub fn hang_blocks(&self) -> u32 {
        self.hang_blocks
    }

    /// Tracked noise floor, dBFS ([`MIN_POWER_DBFS`] before the first block).
    pub fn floor_dbfs(&self) -> f32 {
        self.floor_db.unwrap_or(MIN_POWER_DBFS)
    }

    /// The squelch is open.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Openings vetoed by the discriminator noise meter so far.
    pub fn false_opens(&self) -> u64 {
        self.false_opens
    }

    /// One block: `power_dbfs` of the filtered IQ and `noise_rms` from the
    /// discriminator noise meter.
    pub fn update(&mut self, power_dbfs: f32, noise_rms: f32) -> SquelchBlock {
        let floor = *self.floor_db.get_or_insert(power_dbfs);
        let snr_db = power_dbfs - floor;
        let mut out = SquelchBlock {
            power_dbfs,
            snr_db,
            open: self.open,
            present: false,
            rose: None,
            closed: None,
            run_started: false,
        };

        self.noise[self.noise_pos] = noise_rms;
        self.noise_pos = (self.noise_pos + 1) % self.noise.len();

        if self.open {
            if snr_db > self.close_db {
                self.absent = 0;
                out.present = true;
            } else {
                self.absent += 1;
                if self.absent >= self.hang_blocks {
                    out.closed = Some(self.absent - 1);
                    self.open = false;
                    out.open = false;
                    self.absent = 0;
                    self.run = 0;
                    self.run_vetoed = false;
                }
            }
            return out;
        }

        // Closed: track the floor, look for an opening run.
        let a = if power_dbfs < floor {
            self.a_down
        } else {
            self.a_up
        };
        self.floor_db = Some(floor + a * (power_dbfs - floor));

        if snr_db >= self.open_db {
            self.run += 1;
            out.run_started = self.run == 1;
            if self.run >= self.attack_blocks {
                let mean_noise = self.noise.iter().sum::<f32>() / self.noise.len() as f32;
                if mean_noise < VETO_NOISE_RMS {
                    self.open = true;
                    self.absent = 0;
                    out.open = true;
                    out.present = true;
                    // After a veto the sliding window, not the whole run, is
                    // what passed.
                    let span = if self.run_vetoed {
                        self.attack_blocks
                    } else {
                        self.run
                    };
                    out.rose = Some(span - 1);
                    self.run = 0;
                    self.run_vetoed = false;
                } else if !self.run_vetoed {
                    self.run_vetoed = true;
                    self.false_opens += 1;
                }
            }
        } else {
            self.run = 0;
            self.run_vetoed = false;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sq() -> PowerSquelch {
        PowerSquelch::new(&SquelchParams::default())
    }

    #[test]
    fn timing_constants() {
        assert_eq!(BLOCK_MS, 5);
        let s = sq();
        assert_eq!(s.attack_blocks(), 8);
        assert_eq!(s.hang_blocks(), 200);
    }

    #[test]
    fn opens_after_attack_and_closes_after_hang() {
        let mut s = sq();
        for _ in 0..400 {
            assert!(!s.update(-60.0, 0.6).open);
        }
        let mut opened = None;
        for i in 0..20 {
            let b = s.update(-45.0, 0.02);
            if let Some(r) = b.rose {
                opened = Some((i, r));
                break;
            }
        }
        // Opens at the 8th loud block; the run began 7 blocks earlier.
        assert_eq!(opened, Some((7, 7)));
        // Hysteresis: 6 dB above the floor is still present.
        assert!(s.update(-54.0, 0.02).present);
        let mut closed = None;
        for i in 0..300 {
            let b = s.update(-60.0, 0.6);
            if let Some(k) = b.closed {
                closed = Some((i, k));
                break;
            }
            assert!(b.open && !b.present);
        }
        assert_eq!(closed, Some((199, 199)));
        assert!(!s.is_open());
    }

    #[test]
    fn rekey_within_hang_continues() {
        let mut s = sq();
        for _ in 0..100 {
            s.update(-60.0, 0.6);
        }
        for _ in 0..20 {
            s.update(-45.0, 0.02);
        }
        assert!(s.is_open());
        for _ in 0..150 {
            assert!(s.update(-60.0, 0.6).closed.is_none());
        }
        let b = s.update(-45.0, 0.02);
        assert!(b.present && b.rose.is_none());
    }

    #[test]
    fn noise_burst_is_vetoed_once() {
        let mut s = sq();
        for _ in 0..100 {
            s.update(-60.0, 0.6);
        }
        for _ in 0..200 {
            let b = s.update(-45.0, 0.65);
            assert!(!b.open);
        }
        assert_eq!(s.false_opens(), 1);
        // A real carrier right after the burst (no gap) still opens; its
        // start is the sliding window, not the burst's start.
        let mut rose = None;
        for _ in 0..20 {
            if let Some(r) = s.update(-45.0, 0.02).rose {
                rose = Some(r);
                break;
            }
        }
        assert_eq!(rose, Some(7));
        assert_eq!(s.false_opens(), 1);
    }

    #[test]
    fn floor_tracks_down_fast_up_slow_and_freezes_while_open() {
        let mut s = sq();
        s.update(-40.0, 0.6);
        for _ in 0..(2 * 2 * 1000 / 5) {
            s.update(-60.0, 0.6);
        }
        // Two time constants down: within 20 * e^-2 ≈ 2.7 dB.
        assert!(s.floor_dbfs() < -57.0, "{}", s.floor_dbfs());
        for _ in 0..(10 * 1000 / 5) {
            s.update(-60.0, 0.6);
        }
        // Open on a carrier for 10 s: once open, the floor does not move.
        for _ in 0..8 {
            s.update(-30.0, 0.01);
        }
        assert!(s.is_open());
        let f0 = s.floor_dbfs();
        for _ in 0..2000 {
            s.update(-30.0, 0.01);
        }
        assert_eq!(s.floor_dbfs(), f0);
        // 6 dB louder noise for 2 s while closed (vetoed): the floor rises
        // only a little (30 s up time constant).
        let mut s = sq();
        for _ in 0..400 {
            s.update(-60.0, 0.6);
        }
        for _ in 0..400 {
            s.update(-54.0, 0.6);
        }
        assert!(s.floor_dbfs() < -59.0, "{}", s.floor_dbfs());
    }

    #[test]
    fn power_dbfs_clamps() {
        assert_eq!(power_dbfs(0.0), MIN_POWER_DBFS);
        assert!((power_dbfs(1.0)).abs() < 1e-6);
        assert!((power_dbfs(0.01) + 20.0).abs() < 1e-4);
    }
}
