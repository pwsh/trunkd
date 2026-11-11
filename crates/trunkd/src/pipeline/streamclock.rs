// SPDX-License-Identifier: GPL-3.0-or-later
//! Self-correcting stream clock: drift detection over the sample-count clock.
//!
//! Every timestamp trunkd writes comes from a [`StreamEpoch`](super::StreamEpoch)
//! — a wall-clock instant anchored once when the first block of samples is
//! drained, advanced from then on by *sample count*. That is the right clock
//! for sub-second work (it is exactly as steady as the SDR's reference, with no
//! NTP step or scheduler jitter in it), and the wrong clock for the one thing a
//! sample counter cannot see: **samples that never arrived**.
//!
//! A host stall does exactly that. On 2026-09-08 the OOM killer ran on the
//! laptop recorder; trunkd survived, but ~12 s of samples did not. The sample
//! count fell 12 s behind wall time and stayed there, so for the next 7.7 h
//! every control-channel event and every call was stamped 11.6 s early — the
//! event log's `at` was 12 s behind `date`, and the second recorder's copy of
//! each call started 11.6 s later. Nothing was counted as an overflow (the
//! ring's own counter stayed at 0): the samples were lost before the ring.
//!
//! [`DriftMonitor`] is the fix. Once per second the DSP thread hands it the
//! wall estimate for the newest sample and the current wall clock; when the two
//! disagree by more than [`STREAM_CLOCK_RESYNC_MS`] for
//! [`RESYNC_CONSECUTIVE`] checks in a row it asks for the epoch to be
//! re-anchored, and the correction (signed milliseconds, positive when the
//! stream clock was *behind*) goes into the log, a per-tuner counter, and the
//! event log.
//!
//! # Why a baseline, and not zero
//!
//! The DSP thread reads the newest sample some milliseconds *after* it was
//! received: the ring holds whatever the SDR thread has pushed and the DSP
//! thread has not yet drained. That buffering is normal, constant, and easily
//! larger than a sub-second threshold, so "stream estimate vs. now" is not the
//! error — "stream estimate vs. now, less the steady-state buffering" is. The
//! monitor therefore spends its first [`BASELINE_END_MS`] measuring that offset
//! and treats the mean as zero error, ignoring the first
//! [`BASELINE_START_MS`] while the ring fills and the pipeline settles.
//!
//! # Monotonicity
//!
//! A correction moves the epoch, which moves every *future* timestamp. It is
//! applied at a block boundary and in-progress calls keep the start they were
//! given, so timestamps stay monotonic within a call. The cost is stated
//! plainly: **a call spanning a resync can be up to one correction long or
//! short**, which is the right trade against every call after it being wrong.
//!
//! # Backlog is not loss (review 2026-09-22, R1 — T-B5)
//!
//! The monitor used to be handed the wall estimate of the newest *consumed*
//! sample. Under DSP overload that is wrong twice over: a full ring (1.68 s at
//! 20 Msps) is backlog, not missing samples, and samples the ring *dropped*
//! never advanced the count at all. Both read as "the stream fell behind", so
//! four engines at ~80 % of real time resynced every ~8 s and stamps swung
//! between ~1.5 s early and ~1.7 s late. [`StreamClock`] fixes the inputs:
//!
//! * **Ring drops advance the clock.** A drop is noted where it sits in the
//!   stream (the ring's write position when it was counted) and the epoch is
//!   moved forward by exactly that much when the consumer reaches it, so every
//!   sample after a gap is stamped with its true time instead of early.
//! * **Drift is measured at the newest *produced* sample** — consumed, plus
//!   buffered in the ring, plus dropped-but-not-yet-reached — so a backlog,
//!   however deep, is not an error. What is left is loss *before* the ring
//!   (the 2026-09-08 OOM case) and a sample clock that runs fast or slow.
//! * **Corrections never move a stamp backwards.** A forward correction (lost
//!   samples) is applied at once: jumping forward keeps order. A backward one
//!   (the stream ran ahead) is slewed, at most a quarter of a block's
//!   duration per block — less than the block advances the stream, so
//!   consecutive blocks still stamp in order — and the monitor judges the clock as it will be
//!   once the slew completes, so it never asks twice for the same correction.
//!   [`StampFloor`] then guarantees it outright: no stamp is ever earlier than
//!   one already given to an earlier-or-equal point of the stream.

use std::collections::VecDeque;

/// Disagreement, in milliseconds, above which the stream clock is considered
/// broken rather than merely imprecise.
///
/// Well above any buffering wobble (tens of milliseconds) and well below the
/// smallest stall worth correcting. Drifts under it are never *resynced*:
/// inside a second, the sample clock is the more trustworthy of the two. The
/// steady part of a drift — a sample clock a few ppm off — is taken out
/// instead, a millisecond at a time, by the slow trim
/// ([`DriftMonitor::trim_step`]).
pub(crate) const STREAM_CLOCK_RESYNC_MS: i64 = 1_500;

/// Consecutive over-threshold checks before a resync.
///
/// One check is a suspicion — a wall clock stepped by NTP, a check taken while
/// the DSP thread was descheduled. Three checks a second apart is a stream
/// clock that is genuinely somewhere else.
pub(crate) const RESYNC_CONSECUTIVE: u32 = 3;

/// Start of the baseline window, relative to the first check.
///
/// The ring is still filling before this and the offset is still climbing
/// toward its steady state; averaging that in would bias the baseline low and
/// make normal buffering look like drift.
const BASELINE_START_MS: i64 = 2_000;

/// End of the baseline window, relative to the first check. No resync is
/// considered before it: there is nothing yet to compare against.
const BASELINE_END_MS: i64 = 10_000;

/// Minimum spacing between two resyncs.
///
/// Hysteresis. A correction lands at a block boundary and the very next check
/// should read ~0 error; if it does not (a host that is still stalling), one
/// correction per five seconds is plenty and stops a wobbling host from
/// producing a resync every second.
const MIN_RESYNC_INTERVAL_MS: i64 = 5_000;

/// Checks over which the drift error is averaged before the slow trim looks
/// at it (an exponential average with this time constant, at one check a
/// second). Long enough that buffering wobble, which is fast, averages out;
/// short enough that a few ppm of rate error, which is steady, shows within a
/// minute or two.
const TRIM_AVERAGE_CHECKS: f64 = 30.0;

/// Averaged error, in milliseconds, at which the slow trim starts nudging the
/// epoch toward the wall clock, and below which it stops. The gap between the
/// two is hysteresis, so the trim runs in short bursts rather than every
/// second.
const TRIM_START_MS: f64 = 20.0;
const TRIM_STOP_MS: f64 = 5.0;

/// Most the slow trim moves the epoch per check. A millisecond a second is a
/// thousand ppm, far more than any sample clock is off, and a small enough
/// step that no one call is visibly stretched or shrunk.
const TRIM_STEP_MS: i64 = 1;

/// Watches the sample-count clock against the wall clock and says when to
/// re-anchor.
///
/// Pure: it holds no clock and no epoch of its own, and every decision is a
/// function of the `(now_ms, stream_wall_ms)` pairs it has been given. That is
/// what makes the 12-second-step case a unit test rather than a field report.
#[derive(Debug, Default)]
pub(crate) struct DriftMonitor {
    /// Wall clock at the first check, the origin of the baseline window.
    first_ms: Option<i64>,
    /// Running sum/count of offsets inside the baseline window.
    baseline_sum: i64,
    baseline_n: i64,
    /// The settled offset (`now - stream estimate`), once measured.
    baseline: Option<i64>,
    /// How many consecutive checks have been over threshold.
    breaches: u32,
    /// Wall clock of the last resync, for the hysteresis interval.
    last_resync_ms: Option<i64>,
    /// Exponential average of the under-threshold error, for the slow trim.
    average_error_ms: f64,
    /// Whether the slow trim is part-way through closing a gap.
    trimming: bool,
}

impl DriftMonitor {
    /// Feed one check.
    ///
    /// `stream_wall_ms` is what the epoch says the newest sample's wall-clock
    /// time is; `now_ms` is the wall clock read at the same block boundary.
    /// Returns `Some(correction_ms)` when the epoch should be moved by that
    /// many milliseconds (positive: the stream clock was running behind and its
    /// timestamps were too early). The caller applies the correction; this type
    /// assumes it was applied.
    pub(crate) fn observe(&mut self, now_ms: i64, stream_wall_ms: i64) -> Option<i64> {
        let first = *self.first_ms.get_or_insert(now_ms);
        let phase = now_ms.saturating_sub(first);
        let offset = now_ms.saturating_sub(stream_wall_ms);

        let Some(baseline) = self.baseline else {
            // Still calibrating: collect the settled part of the window, and
            // decide nothing until it closes.
            if phase >= BASELINE_START_MS {
                self.baseline_sum = self.baseline_sum.saturating_add(offset);
                self.baseline_n += 1;
            }
            if phase >= BASELINE_END_MS {
                // A run that produced no in-window samples (a check cadence
                // slower than the window) falls back to this check's offset,
                // which is a far better guess than zero.
                self.baseline = Some(if self.baseline_n > 0 {
                    self.baseline_sum / self.baseline_n
                } else {
                    offset
                });
            }
            return None;
        };

        // Positive error: the stream clock is behind wall time, i.e. samples
        // went missing and every timestamp is that many milliseconds early.
        let error = offset.saturating_sub(baseline);
        if error.abs() <= STREAM_CLOCK_RESYNC_MS {
            self.breaches = 0;
            self.average_error_ms += (error as f64 - self.average_error_ms) / TRIM_AVERAGE_CHECKS;
            return None;
        }
        self.breaches += 1;
        if self.breaches < RESYNC_CONSECUTIVE {
            return None;
        }
        if let Some(last) = self.last_resync_ms {
            if now_ms.saturating_sub(last) < MIN_RESYNC_INTERVAL_MS {
                return None;
            }
        }
        self.breaches = 0;
        self.last_resync_ms = Some(now_ms);
        // The resync closes the whole gap; what the average remembered of the
        // run-up to it no longer describes the clock.
        self.average_error_ms = 0.0;
        self.trimming = false;
        Some(error)
    }

    /// The slow trim's step for this check, in milliseconds (positive: move
    /// the epoch forward, the stamps were early), or 0.
    ///
    /// A resync answers a clock that is *broken* — samples lost, a stall. A
    /// sample clock that is merely a few ppm off never gets there quickly:
    /// on 2026-10-16..22 the two Airspys ran 3.3 and 4.3 ppm slow of the
    /// host's clock, so each crept 290-370 ms a day toward the 1.5 s threshold,
    /// their stamps a growing fraction of a second early and, between their
    /// resyncs, up to 0.65 s apart. The trim takes that steady error out as it
    /// accrues: once the averaged error passes [`TRIM_START_MS`] it moves the
    /// epoch [`TRIM_STEP_MS`] per check until the average is back under
    /// [`TRIM_STOP_MS`]. Inside a second the sample clock still rules; over
    /// minutes the wall clock does.
    ///
    /// The caller applies the step; this type assumes it was applied and
    /// takes it out of the average at once, so the same error is never
    /// corrected twice while the average catches up.
    pub(crate) fn trim_step(&mut self) -> i64 {
        if self.baseline.is_none() || self.breaches > 0 {
            return 0;
        }
        let size = self.average_error_ms.abs();
        if !self.trimming && size > TRIM_START_MS {
            self.trimming = true;
        } else if self.trimming && size < TRIM_STOP_MS {
            self.trimming = false;
        }
        if !self.trimming {
            return 0;
        }
        let step = if self.average_error_ms > 0.0 {
            TRIM_STEP_MS
        } else {
            -TRIM_STEP_MS
        };
        self.average_error_ms -= step as f64;
        step
    }

    /// The averaged under-threshold error, in milliseconds (positive: stamps
    /// early). 0 until the baseline is measured.
    pub(crate) fn average_error_ms(&self) -> i64 {
        self.average_error_ms.round() as i64
    }
}

/// A resync the monitor asked for, and how much of it lands at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Resync {
    /// The whole correction, signed (positive: stamps were early).
    pub(crate) correction_ms: i64,
    /// What to move the epoch by now. The rest of a backward correction is
    /// slewed in over the following blocks by [`StreamClock::before_block`].
    pub(crate) shift_now_ms: i64,
}

/// The DSP thread's stream clock bookkeeping: where the consumer is, which
/// ring drops lie ahead of it, and how much backward correction is still owed.
///
/// Pure, like [`DriftMonitor`]: it owns no epoch and reads no clock. The caller
/// applies every shift it returns to the device's `StreamEpoch`, which is what
/// lets the synthetic-backlog tests drive exactly the code the DSP thread
/// runs.
#[derive(Debug)]
pub(crate) struct StreamClock {
    rate: u64,
    /// Samples read out of the ring so far — the engines' own timeline.
    consumed: u64,
    /// The ring's overflow counter at the last look.
    overflow_seen: u64,
    /// Ring drops not yet reached: `(consumer position, samples)`. The
    /// position is the ring's write position when the drop was counted, so
    /// every sample from there on comes after the gap.
    gaps: VecDeque<(u64, u64)>,
    /// Sum of the samples in `gaps`.
    gap_pending: u64,
    /// Samples of every gap already passed, and the milliseconds of them the
    /// epoch has been moved by (kept separately so rounding never drifts).
    skipped: u64,
    skipped_ms: i64,
    drift: DriftMonitor,
    /// Backward correction still owed (≤ 0), slewed in per block.
    slew_ms: i64,
    /// Most backward correction applied per block.
    slew_step_ms: i64,
    /// Forward trim still owed (≥ 0), applied at the next block.
    trim_forward_ms: i64,
    /// Every slow-trim step taken since the stream started, signed.
    trimmed_ms: i64,
}

/// Cap on remembered ring gaps; beyond it the newest two merge. A drop is
/// counted at most once per block, and the ring holds about a second of
/// blocks, so this is never reached in practice.
const MAX_GAPS: usize = 1_024;

impl StreamClock {
    /// A clock for a `rate` samples/s stream read in blocks of
    /// `block_samples`, whose ring overflow counter reads `overflow_baseline`
    /// now (the counter outlives a restart of the stream).
    pub(crate) fn new(rate: u64, block_samples: usize, overflow_baseline: u64) -> Self {
        let rate = rate.max(1);
        let block_ms = (block_samples as u64).saturating_mul(1000) / rate;
        StreamClock {
            rate,
            consumed: 0,
            overflow_seen: overflow_baseline,
            gaps: VecDeque::new(),
            gap_pending: 0,
            skipped: 0,
            skipped_ms: 0,
            drift: DriftMonitor::default(),
            slew_ms: 0,
            // A quarter of a block, and at least a millisecond: consecutive
            // blocks stay in order while a 1.5 s correction still lands in
            // well under half a minute.
            slew_step_ms: (block_ms as i64 / 4).max(1),
            trim_forward_ms: 0,
            trimmed_ms: 0,
        }
    }

    /// Stream time at the end of a block of `want` samples starting at the
    /// consumer's position — the engines' timeline, which counts only the
    /// samples they were fed.
    pub(crate) fn block_end_ms(&self, want: usize) -> u64 {
        (self.consumed + want as u64) * 1000 / self.rate
    }

    /// Before a block of `want` samples is stamped.
    ///
    /// `buffered` is what the ring holds right now and `overflow_total` the
    /// ring's drop counter. Returns the epoch shift to apply *before* the
    /// block's events are stamped: every ring gap the block reaches (forward)
    /// plus one step of any owed backward correction.
    pub(crate) fn before_block(
        &mut self,
        want: usize,
        buffered: usize,
        overflow_total: u64,
    ) -> i64 {
        let dropped = overflow_total.saturating_sub(self.overflow_seen);
        self.overflow_seen = overflow_total;
        if dropped > 0 {
            let at = self.consumed + buffered as u64;
            if self.gaps.len() >= MAX_GAPS {
                if let Some(last) = self.gaps.back_mut() {
                    last.1 += dropped;
                }
            } else {
                self.gaps.push_back((at, dropped));
            }
            self.gap_pending += dropped;
        }
        let end = self.consumed + want as u64;
        while let Some(&(at, samples)) = self.gaps.front() {
            if at >= end {
                break;
            }
            self.gaps.pop_front();
            self.gap_pending -= samples;
            self.skipped += samples;
        }
        let skipped_ms = (self.skipped.saturating_mul(1000) / self.rate) as i64;
        let mut shift = skipped_ms - self.skipped_ms + self.trim_forward_ms;
        self.skipped_ms = skipped_ms;
        self.trim_forward_ms = 0;
        if self.slew_ms < 0 {
            let step = self.slew_ms.max(-self.slew_step_ms);
            self.slew_ms -= step;
            shift += step;
        }
        shift
    }

    /// The block of `want` samples was read.
    pub(crate) fn commit(&mut self, want: usize) {
        self.consumed += want as u64;
    }

    /// Stream time (engine timeline) of the newest sample the SDR has
    /// delivered: consumed, plus buffered, plus ring drops not yet reached
    /// (which the epoch has not absorbed yet).
    pub(crate) fn newest_ms(&self, buffered: usize) -> u64 {
        (self.consumed + buffered as u64 + self.gap_pending) * 1000 / self.rate
    }

    /// The 1 Hz drift check. `newest_wall_ms` is the epoch's wall time for
    /// [`Self::newest_ms`]; `now_ms` the wall clock.
    pub(crate) fn check(&mut self, now_ms: i64, newest_wall_ms: i64) -> Option<Resync> {
        // Judge the clock as it will be once everything owed has landed — the
        // backward slew and any forward trim — or the same error would be
        // corrected twice.
        let owed = self.slew_ms.saturating_add(self.trim_forward_ms);
        let Some(correction) = self
            .drift
            .observe(now_ms, newest_wall_ms.saturating_add(owed))
        else {
            // No resync: the slow trim may still have a step to take. Forward
            // lands on the next block; backward joins the slew, so it is
            // spread across blocks and never moves a stamp backwards.
            let step = self.drift.trim_step();
            if step > 0 {
                self.trim_forward_ms += step;
            } else if step < 0 {
                self.slew_ms += step;
            }
            self.trimmed_ms += step;
            return None;
        };
        let net = owed.saturating_add(correction);
        self.trim_forward_ms = 0;
        let shift_now_ms = if net >= 0 {
            self.slew_ms = 0;
            net
        } else {
            self.slew_ms = net;
            0
        };
        Some(Resync {
            correction_ms: correction,
            shift_now_ms,
        })
    }

    /// The drift monitor's averaged error right now, in milliseconds
    /// (positive: stamps early). What the slow trim is working against.
    pub(crate) fn average_error_ms(&self) -> i64 {
        self.drift.average_error_ms()
    }

    /// Every slow-trim step taken since the stream started, signed, in
    /// milliseconds. Its slope is the sample clock's rate error against the
    /// host's: 86 ms a day is 1 ppm.
    pub(crate) fn trimmed_ms(&self) -> i64 {
        self.trimmed_ms
    }
}

/// Keeps stamps from ever going backwards along the stream.
///
/// A stamp for stream time at or after the highest one stamped so far is never
/// earlier than that one's wall time. A stamp for an *earlier* stream time — a
/// call's `Ended` naming its last voice frame after newer blocks were routed —
/// is left alone: it is ordered against the stamps of its own past.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StampFloor {
    stream_ms: u64,
    wall_ms: i64,
}

impl Default for StampFloor {
    fn default() -> Self {
        StampFloor {
            stream_ms: 0,
            wall_ms: i64::MIN,
        }
    }
}

impl StampFloor {
    /// The stamp to use for `stream_ms`, whose epoch mapping is `wall_ms`.
    pub(crate) fn stamp(&mut self, stream_ms: u64, wall_ms: i64) -> i64 {
        if stream_ms < self.stream_ms {
            return wall_ms;
        }
        let w = wall_ms.max(self.wall_ms);
        self.stream_ms = stream_ms;
        self.wall_ms = w;
        w
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One second between checks, the cadence the DSP thread uses.
    const STEP: i64 = 1_000;

    /// The steady-state buffering offset the fixtures simulate: the DSP thread
    /// reads the newest sample 250 ms after it arrived. Well over the naive
    /// "estimate vs. now" zero, and the whole reason for the baseline.
    const LATENCY: i64 = 250;

    /// What `run` reports: the wall time of a resync and its correction.
    type Resync = (i64, i64);

    /// Drive `secs` seconds of checks from `t0` with `skew(i)` milliseconds of
    /// missing samples, applying every correction the monitor asks for exactly
    /// as the DSP thread does. Returns the resyncs.
    fn run(m: &mut DriftMonitor, t0: i64, secs: i64, skew: impl Fn(i64) -> i64) -> Vec<Resync> {
        let mut applied = 0;
        let mut out = Vec::new();
        for i in 0..secs {
            let now = t0 + i * STEP;
            // The stream estimate lags by the buffering latency and by every
            // sample that never arrived, less whatever has been corrected.
            let stream = now - LATENCY - skew(i) + applied;
            if let Some(c) = m.observe(now, stream) {
                applied += c;
                out.push((now, c));
            }
        }
        out
    }

    #[test]
    fn a_healthy_stream_never_resyncs() {
        let mut m = DriftMonitor::default();
        let got = run(&mut m, 1_700_000_000_000, 600, |_| 0);
        assert!(got.is_empty(), "no drift must produce no resync: {got:?}");
        assert_eq!(
            m.baseline,
            Some(LATENCY),
            "the baseline is the steady-state buffering offset, not zero"
        );
    }

    #[test]
    fn a_twelve_second_stall_produces_exactly_one_resync() {
        let mut m = DriftMonitor::default();
        let t0 = 1_700_000_000_000;
        // The host stalls at t+60 s; the sample clock never catches up.
        let got = run(&mut m, t0, 600, |i| if i >= 60 { 12_000 } else { 0 });
        assert_eq!(got.len(), 1, "one stall, one correction: {got:?}");
        let (at, correction) = got[0];
        assert!(
            (correction - 12_000).abs() < 50,
            "the correction is the size of the stall, got {correction}"
        );
        assert_eq!(
            at,
            t0 + 62 * STEP,
            "a resync waits for three consecutive breaches (first at i=60)"
        );
    }

    #[test]
    fn a_sub_threshold_wobble_is_left_alone() {
        let mut m = DriftMonitor::default();
        // Half a second of buffering wobble, back and forth, for ten minutes.
        // Inside a second the sample clock is the more precise of the two:
        // correcting here would be chasing the wall clock's noise.
        let got = run(&mut m, 1_700_000_000_000, 600, |i| {
            if i % 2 == 0 {
                500
            } else {
                -500
            }
        });
        assert!(got.is_empty(), "a 0.5 s wobble must not resync: {got:?}");
    }

    #[test]
    fn breaches_must_be_consecutive() {
        let mut m = DriftMonitor::default();
        // Two isolated pairs of over-threshold checks (a stepped wall clock
        // that comes back): the counter resets rather than accumulating.
        let got = run(&mut m, 1_700_000_000_000, 300, |i| {
            if (60..62).contains(&i) || (120..122).contains(&i) {
                9_000
            } else {
                0
            }
        });
        assert!(got.is_empty(), "two is not three: {got:?}");
    }

    #[test]
    fn hysteresis_spaces_out_a_host_that_keeps_stalling() {
        let mut m = DriftMonitor::default();
        let t0 = 1_700_000_000_000;
        // 12 s of samples lost every second, for ever. Corrections cannot keep
        // up, and without the interval this would resync every third check.
        let got = run(
            &mut m,
            t0,
            100,
            |i| if i >= 20 { 12_000 * (i - 19) } else { 0 },
        );
        assert!(!got.is_empty(), "a real, growing gap must still correct");
        for w in got.windows(2) {
            assert!(
                w[1].0 - w[0].0 >= MIN_RESYNC_INTERVAL_MS,
                "resyncs must be spaced by the hysteresis interval: {got:?}"
            );
        }
    }

    #[test]
    fn nothing_is_decided_during_the_baseline_window() {
        let mut m = DriftMonitor::default();
        // A stall inside the first ten seconds has no baseline to be judged
        // against, so it is absorbed rather than corrected.
        let got = run(&mut m, 1_700_000_000_000, 8, |i| {
            if i >= 4 {
                12_000
            } else {
                0
            }
        });
        assert!(got.is_empty(), "no baseline, no verdict: {got:?}");
        assert_eq!(m.baseline, None, "the window has not closed yet");
    }

    #[test]
    fn a_stream_clock_running_ahead_is_corrected_backwards() {
        let mut m = DriftMonitor::default();
        // The mirror image: the epoch was anchored 4 s late (a wall clock that
        // stepped backwards under it), so timestamps run late and the
        // correction is negative.
        let got = run(&mut m, 1_700_000_000_000, 200, |i| {
            if i >= 60 {
                -4_000
            } else {
                0
            }
        });
        assert_eq!(got.len(), 1, "{got:?}");
        assert!((got[0].1 + 4_000).abs() < 50, "got {}", got[0].1);
    }

    // -- The slow trim: a sample clock a few ppm off -------------------------

    /// What a run of a [`StreamClock`] against a sample clock `ppm` slow
    /// (negative: fast) reports.
    struct DriftRun {
        resyncs: Vec<Resync>,
        /// Worst |true arrival − stamp| after the first ten minutes, ms.
        worst_after_settle_ms: f64,
        trimmed_ms: i64,
        /// Whether every block's stamp was at or after the previous one's.
        monotonic: bool,
    }

    /// Drive a 1 kS/s stream in 100-sample blocks for `secs` seconds, exactly
    /// as the DSP thread does: shift the epoch by what `before_block` returns,
    /// stamp the block, and once a second `check` with the wall clock reading
    /// 250 ms after the newest sample. `lost` is a stall: that many samples
    /// never arrive at second `lost.0`.
    fn drift_run(ppm: f64, secs: i64, lost: Option<(i64, u64)>) -> DriftRun {
        const RATE: u64 = 1_000;
        const BLOCK: usize = 100;
        let t0: i64 = 1_790_000_000_000;
        let mut clock = StreamClock::new(RATE, BLOCK, 0);
        let mut epoch = t0;
        let mut out = DriftRun {
            resyncs: Vec::new(),
            worst_after_settle_ms: 0.0,
            trimmed_ms: 0,
            monotonic: true,
        };
        let mut last_stamp = i64::MIN;
        // Samples the SDR has produced (arrival order), including any it lost.
        let mut produced: u64 = 0;
        let arrival = |n: u64| t0 as f64 + n as f64 * 1_000.0 / (RATE as f64 * (1.0 - ppm * 1e-6));
        let blocks_per_s = RATE as usize / BLOCK;
        for s in 0..secs {
            if let Some((at, n)) = lost {
                if s == at {
                    // Lost before the ring: produced, never delivered, never counted.
                    produced += n;
                }
            }
            for _ in 0..blocks_per_s {
                produced += BLOCK as u64;
                epoch += clock.before_block(BLOCK, BLOCK, 0);
                let stamp = epoch + clock.block_end_ms(BLOCK) as i64;
                if stamp < last_stamp {
                    out.monotonic = false;
                }
                last_stamp = stamp;
                let error = arrival(produced) - stamp as f64;
                if s >= 600 && lost.is_none_or(|(at, _)| s < at || s > at + 30) {
                    out.worst_after_settle_ms = out.worst_after_settle_ms.max(error.abs());
                }
                clock.commit(BLOCK);
            }
            let now = arrival(produced) as i64 + 250;
            let newest = epoch + clock.newest_ms(0) as i64;
            if let Some(r) = clock.check(now, newest) {
                epoch += r.shift_now_ms;
                out.resyncs.push((now, r.correction_ms));
            }
        }
        out.trimmed_ms = clock.trimmed_ms();
        out
    }

    #[test]
    fn a_sample_clock_a_few_ppm_slow_is_trimmed_not_resynced() {
        // The Windows recorder's control Airspy, 2026-10-16..21: 4.3 ppm slow
        // of the host. Five days of it.
        let run = drift_run(4.3, 5 * 86_400, None);
        assert!(
            run.resyncs.is_empty(),
            "rate error alone never resyncs now: {:?}",
            run.resyncs
        );
        assert!(
            run.worst_after_settle_ms < 25.0,
            "stamps stay within the trim band of true time, worst {:.1} ms",
            run.worst_after_settle_ms
        );
        // 4.3 ppm of five days is 1 858 ms; the trim has taken all but the band.
        assert!(
            (run.trimmed_ms - 1_858).abs() < 25,
            "the trim total is the rate error, got {} ms",
            run.trimmed_ms
        );
        assert!(run.monotonic);
    }

    #[test]
    fn a_sample_clock_running_fast_is_trimmed_back_without_reversing_a_stamp() {
        let run = drift_run(-3.3, 3 * 86_400, None);
        assert!(run.resyncs.is_empty(), "{:?}", run.resyncs);
        assert!(
            run.worst_after_settle_ms < 25.0,
            "worst {:.1} ms",
            run.worst_after_settle_ms
        );
        assert!(
            run.trimmed_ms < 0,
            "a fast clock is trimmed backwards: {}",
            run.trimmed_ms
        );
        assert!(run.monotonic, "a backward trim is slewed, never stepped");
    }

    #[test]
    fn a_stall_is_still_one_resync_with_the_trim_running() {
        // Twelve seconds of samples lost on top of a 4 ppm drift: the resync
        // takes the stall, the trim keeps taking the drift.
        let run = drift_run(4.0, 3_600, Some((1_200, 12_000)));
        assert_eq!(run.resyncs.len(), 1, "{:?}", run.resyncs);
        assert!((run.resyncs[0].1 - 12_000).abs() < 50, "{:?}", run.resyncs);
        assert!(
            run.worst_after_settle_ms < 25.0,
            "worst {:.1} ms",
            run.worst_after_settle_ms
        );
    }

    // -- StreamClock under a synthetic backlog (T-B5) -------------------------

    /// How the simulated DSP thread feeds its clock.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        /// What the pump does now: [`StreamClock`] and [`StampFloor`].
        Fixed,
        /// What it did before: the newest *consumed* sample into a bare
        /// monitor, ring drops ignored, corrections applied whole.
        Old,
    }

    /// One simulated recorder.
    struct Sim {
        rate: u64,
        block: usize,
        ring_cap: u64,
        /// DSP speed relative to real time (0.8 = processes 80 % of the
        /// stream it is fed).
        speed: f64,
        /// Samples the SDR actually delivers per nominal sample (1.001 = its
        /// clock runs 0.1 % fast).
        produce: f64,
        /// `(start_ms, len_ms)` of a host stall that loses samples *before*
        /// the ring.
        stall: Option<(i64, i64)>,
        secs: i64,
        mode: Mode,
    }

    /// What a run produced.
    struct Run {
        /// `(true wall time of the block's last sample, stamp given)`.
        stamps: Vec<(i64, i64)>,
        resyncs: Vec<i64>,
        dropped: u64,
    }

    impl Sim {
        fn overloaded(mode: Mode) -> Sim {
            Sim {
                rate: 20_000_000,
                block: 64 * 1024,
                ring_cap: 20_000_000 / (64 * 1024) * (64 * 1024),
                speed: 0.8,
                produce: 1.0,
                stall: None,
                secs: 90,
                mode,
            }
        }

        fn run(&self) -> Run {
            const T0: i64 = 1_790_000_000_000;
            let mut clock = StreamClock::new(self.rate, self.block, 0);
            let mut old = DriftMonitor::default();
            let mut floor = StampFloor::default();
            let mut epoch: Option<i64> = None;
            let (mut ring, mut overflow, mut consumed) = (0u64, 0u64, 0u64);
            // `(consumer index one past the last sample written, tick)` per
            // written chunk: when each sample really arrived.
            let mut arrivals: VecDeque<(u64, i64)> = VecDeque::new();
            let mut written = 0u64;
            let mut carry = 0.0f64;
            let mut busy = 0.0f64;
            let block_ms = self.block as f64 * 1000.0 / self.rate as f64;
            let mut out = Run {
                stamps: Vec::new(),
                resyncs: Vec::new(),
                dropped: 0,
            };
            for tick in 0..self.secs * 1000 {
                let now = T0 + tick;
                // -- the SDR: one millisecond of samples --------------------
                carry += self.rate as f64 * self.produce / 1000.0;
                let n = carry as u64;
                carry -= n as f64;
                let stalled = self
                    .stall
                    .is_some_and(|(at, len)| (at..at + len).contains(&tick));
                if !stalled {
                    let take = n.min(self.ring_cap - ring);
                    ring += take;
                    written += take;
                    overflow += n - take;
                    out.dropped += n - take;
                    if take > 0 {
                        arrivals.push_back((written, now));
                    }
                }
                // -- the DSP thread: a millisecond of work ------------------
                let mut budget = 1.0f64;
                while budget > 0.0 {
                    if busy > 0.0 {
                        let spend = busy.min(budget);
                        busy -= spend;
                        budget -= spend;
                        continue;
                    }
                    if ring < self.block as u64 {
                        break;
                    }
                    let want = self.block;
                    let shift = match self.mode {
                        Mode::Fixed => clock.before_block(want, ring as usize, overflow),
                        Mode::Old => 0,
                    };
                    let end_ms = clock.block_end_ms(want);
                    let e = *epoch.get_or_insert(now - end_ms as i64);
                    let e = e + shift;
                    epoch = Some(e);
                    let stamp = match self.mode {
                        Mode::Fixed => floor.stamp(end_ms, e + end_ms as i64),
                        Mode::Old => e + end_ms as i64,
                    };
                    clock.commit(want);
                    consumed += want as u64;
                    ring -= want as u64;
                    while arrivals.front().is_some_and(|&(end, _)| end < consumed) {
                        arrivals.pop_front();
                    }
                    let truth = arrivals.front().map_or(now, |&(_, at)| at);
                    out.stamps.push((truth, stamp));
                    busy = block_ms / self.speed;
                }
                // -- the 1 Hz check ------------------------------------------
                if tick % 1000 == 999 {
                    if let Some(e) = epoch {
                        match self.mode {
                            Mode::Fixed => {
                                let newest = e + clock.newest_ms(ring as usize) as i64;
                                if let Some(r) = clock.check(now, newest) {
                                    epoch = Some(e + r.shift_now_ms);
                                    out.resyncs.push(r.correction_ms);
                                }
                            }
                            Mode::Old => {
                                let newest = e + (consumed * 1000 / self.rate) as i64;
                                if let Some(c) = old.observe(now, newest) {
                                    epoch = Some(e + c);
                                    out.resyncs.push(c);
                                }
                            }
                        }
                    }
                }
            }
            out
        }
    }

    /// Largest `|stamp - truth|` after the monitor's baseline window.
    fn worst_error(run: &Run) -> i64 {
        run.stamps
            .iter()
            .skip_while(|(truth, _)| *truth < 1_790_000_000_000 + 12_000)
            .map(|(truth, stamp)| (stamp - truth).abs())
            .max()
            .expect("stamps after the baseline window")
    }

    fn assert_monotonic(run: &Run) {
        for w in run.stamps.windows(2) {
            assert!(
                w[1].1 >= w[0].1,
                "a stamp went backwards: {} after {}",
                w[1].1,
                w[0].1
            );
        }
    }

    #[test]
    fn the_old_clock_sawtooths_under_a_full_ring() {
        // The synthetic backlog reproduces R1: a DSP thread at 80 % of real
        // time with a full ring and 20 % of the stream dropped, measured the
        // old way, resyncs over and over and stamps swing by seconds.
        let run = Sim::overloaded(Mode::Old).run();
        assert!(run.dropped > 0, "the ring must actually overflow");
        assert!(
            run.resyncs.len() >= 3,
            "the old clock mistook backlog and ring drops for loss: {:?}",
            run.resyncs
        );
        assert!(worst_error(&run) > 1_000, "and stamps were off by seconds");
    }

    #[test]
    fn a_full_ring_neither_resyncs_nor_moves_the_stamps() {
        let run = Sim::overloaded(Mode::Fixed).run();
        assert!(run.dropped > 0, "the ring must actually overflow");
        assert!(
            run.resyncs.is_empty(),
            "backlog and ring drops are not loss: {:?}",
            run.resyncs
        );
        let worst = worst_error(&run);
        assert!(
            worst <= 10,
            "every block is stamped within a few ms of when it arrived, got {worst} ms"
        );
        assert_monotonic(&run);
    }

    #[test]
    fn loss_before_the_ring_is_still_corrected_once_under_backlog() {
        // The 2026-09-08 case on top of an overloaded DSP: twelve seconds of
        // samples that never reached the ring. While the loss is still going
        // on the monitor corrects in hysteresis-spaced steps (as it always
        // has); every one is forward, together they are the stall, and stamps
        // are right again once it is over. The backlog adds nothing.
        let sim = Sim {
            stall: Some((30_000, 12_000)),
            ..Sim::overloaded(Mode::Fixed)
        };
        let run = sim.run();
        assert!(
            run.resyncs.iter().all(|c| *c > 0),
            "lost samples are corrected forwards: {:?}",
            run.resyncs
        );
        let total: i64 = run.resyncs.iter().sum();
        assert!(
            (total - 12_000).abs() < 50,
            "the corrections add up to the stall: {:?}",
            run.resyncs
        );
        let after: Vec<(i64, i64)> = run
            .stamps
            .iter()
            .copied()
            .filter(|(truth, _)| *truth > 1_790_000_000_000 + 50_000)
            .collect();
        let worst = after
            .iter()
            .map(|(t, s)| (s - t).abs())
            .max()
            .expect("stamps");
        assert!(
            worst <= 10,
            "stamps are right after the resync, got {worst} ms"
        );
        assert_monotonic(&run);
    }

    #[test]
    fn a_stream_running_ahead_is_slewed_back_and_never_steps_backwards() {
        // An SDR clock 1 % fast: the sample count runs ahead of wall time by
        // 10 ms a second until the monitor corrects it backwards. That
        // correction must never make a later block stamp before an earlier
        // one, and must not be asked for twice while it is still landing.
        let sim = Sim {
            rate: 2_400_000,
            ring_cap: 2_400_000 / (64 * 1024) * (64 * 1024),
            speed: 2.0,
            produce: 1.01,
            secs: 240,
            ..Sim::overloaded(Mode::Fixed)
        };
        let run = sim.run();
        assert!(!run.resyncs.is_empty(), "the drift is corrected");
        assert!(
            run.resyncs.iter().all(|c| *c < 0),
            "a stream running ahead is corrected backwards: {:?}",
            run.resyncs
        );
        for c in &run.resyncs {
            assert!(
                c.abs() < STREAM_CLOCK_RESYNC_MS + 200,
                "one threshold's worth per correction, never a double one: {:?}",
                run.resyncs
            );
        }
        assert_monotonic(&run);
    }

    #[test]
    fn a_ring_drop_moves_the_clock_only_when_the_consumer_reaches_it() {
        // 1 Msps, 1000-sample blocks: 1 ms per block.
        let mut c = StreamClock::new(1_000_000, 1_000, 0);
        // 5 000 samples buffered; 2 000 dropped after them.
        assert_eq!(c.before_block(1_000, 5_000, 2_000), 0, "not reached yet");
        c.commit(1_000);
        assert_eq!(c.newest_ms(4_000), 7, "consumed 1 + buffered 4 + gap 2 ms");
        for _ in 0..3 {
            assert_eq!(c.before_block(1_000, 4_000, 2_000), 0);
            c.commit(1_000);
        }
        // The block from 4 000 to 5 000 ends at the gap: still before it.
        assert_eq!(c.before_block(1_000, 1_000, 2_000), 0);
        c.commit(1_000);
        // The next block starts after it: the epoch moves by the gap, once.
        assert_eq!(c.before_block(1_000, 1_000, 2_000), 2);
        c.commit(1_000);
        assert_eq!(c.before_block(1_000, 1_000, 2_000), 0);
    }

    #[test]
    fn a_backward_correction_is_paid_out_a_step_per_block() {
        let mut c = StreamClock::new(20_000_000, 64 * 1024, 0);
        c.slew_ms = -10;
        let step = c.slew_step_ms;
        assert_eq!(step, 1, "a quarter of a 3 ms block, at least 1 ms");
        let paid: i64 = (0..20).map(|_| c.before_block(1, 0, 0)).sum();
        assert_eq!(paid, -10, "exactly the correction, no more");
        assert_eq!(c.slew_ms, 0);
    }

    #[test]
    fn the_floor_holds_stamps_in_stream_order() {
        let mut f = StampFloor::default();
        assert_eq!(f.stamp(100, 1_000), 1_000);
        // The epoch stepped back 5 ms: a later point may not stamp earlier.
        assert_eq!(f.stamp(102, 997), 1_000);
        assert_eq!(f.stamp(110, 1_005), 1_005);
        // A stamp for the past is left alone (a call end naming its last
        // voice frame).
        assert_eq!(f.stamp(50, 945), 945);
        assert_eq!(f.stamp(111, 1_006), 1_006);
    }
}
