// SPDX-License-Identifier: GPL-3.0-or-later
//! Closed-loop automatic frequency (ppm) correction.
//!
//! # The failure this exists for
//!
//! An Airspy R2 whose TCXO is about 1 ppm fast puts the 855 MHz control
//! channel ~900 Hz away from where trunkd tuned. The board streams perfectly,
//! the carrier is plainly there, and the LSM demodulator — which does not pull
//! in past a few hundred Hz of residual offset — never acquires. `[[device]]
//! ppm` fixes it, but only if an operator already knows the number, and it is
//! applied once at open and never revisited. sdrtrunk decodes the same board
//! out of the box because it corrects frequency automatically; this is that.
//!
//! # Shape
//!
//! [`AutoPpm`] is a pure state machine: observations in, retune decisions out,
//! `Instant` injected rather than read. It owns no hardware, does no I/O and
//! allocates nothing, so the DSP thread can run it inline and the rules below
//! can be tested exactly.
//!
//! # The rules, and why they are what they are
//!
//! **Hunting** is the case that matters — a tuner that never locks has no
//! decoder-derived frequency error to work from, so the correction comes from
//! [`SiteStats::hunt_carrier_offset_hz`](trunkd_p25::site::SiteStats), the
//! carrier offset measured straight off the candidate bins. It has to settle
//! ([`HUNT_SETTLE`]) before the first step so the meters have real samples,
//! needs [`HUNT_MIN_SNR_DB`] of spread between candidates to prove something is
//! actually on the air, and only acts past [`HUNT_MIN_OFFSET_HZ`] — below that
//! the demodulator locks anyway and a correction is churn.
//!
//! **Locked** is drift maintenance, so it is deliberately reluctant: a smaller
//! threshold ([`LOCK_MIN_OFFSET_HZ`]) but two consecutive agreeing observations
//! and a long cooldown, because a spurious retune here costs a working lock.
//!
//! The **sign** follows from what the meter measures: a carrier reading +900 Hz
//! means it sits *above* where the tuner is listening, so the hardware must be
//! asked for a frequency 900 Hz higher. The correction is cumulative and
//! clamped to the configured ppm limit; a step that would cross the limit is
//! applied as the remainder and flagged.

use std::time::{Duration, Instant};

/// Hunting must have been under way this long before the first correction, so
/// the carrier meters are reporting a full window rather than a fragment.
pub const HUNT_SETTLE: Duration = Duration::from_secs(3);

/// Minimum spacing between corrections while hunting. Fast, because every
/// second spent hunting is a second of the site not being recorded.
pub const HUNT_COOLDOWN: Duration = Duration::from_secs(3);

/// Below this the LSM demodulator pulls in unaided, so correcting is churn.
pub const HUNT_MIN_OFFSET_HZ: f64 = 300.0;

/// Spread between the strongest and weakest control candidate below which the
/// hunt measurement is noise, not signal.
pub const HUNT_MIN_SNR_DB: f64 = 6.0;

/// Drift threshold on a locked control channel. Lower than the hunting one:
/// the measurement is better (one known-good channel, not a field of
/// candidates) and the goal is to keep a lock rather than to win one.
pub const LOCK_MIN_OFFSET_HZ: f64 = 150.0;

/// Minimum spacing between corrections on a locked channel. Long, because a
/// needless retune here throws away a lock that is already working.
pub const LOCK_COOLDOWN: Duration = Duration::from_secs(10);

/// How far two consecutive locked readings may differ, as a fraction of the
/// larger, and still count as agreeing.
const LOCK_AGREE_TOLERANCE: f64 = 0.5;

/// One stats window's worth of frequency evidence for one device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
    /// Whether the site is still hunting for its control channel.
    pub hunting: bool,
    /// Carrier offset across the control candidates, Hz.
    pub hunt_offset_hz: Option<f64>,
    /// Spread between the strongest and weakest candidate, dB.
    pub hunt_snr_db: f64,
    /// Carrier offset on the locked control channel, Hz.
    pub control_offset_hz: Option<f64>,
}

/// A retune the loop wants applied.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    /// Nominal frequency to ask the tuner for (the configured center plus the
    /// cumulative correction).
    pub nominal_request_hz: u64,
    /// This step alone, Hz.
    pub step_hz: f64,
    /// Cumulative correction after this step, Hz.
    pub total_hz: f64,
    /// The same total as ppm of the configured center.
    pub total_ppm: f64,
    /// The requested step ran into the configured limit and was truncated.
    pub clamped: bool,
}

/// The correction loop for one device.
pub struct AutoPpm {
    /// Configured center frequency; 0 disables the loop outright (there is no
    /// frequency to correct and no denominator for ppm).
    nominal_hz: u64,
    /// The ppm limit, pre-converted to Hz at the nominal frequency.
    limit_hz: f64,
    correction_hz: f64,
    /// The part of `correction_hz` the *hardware* already carries, because the
    /// board was opened on a remembered ppm (see
    /// [`crate::pipeline::ppmmemory`]).
    ///
    /// The loop's retunes are nominal — the backend re-applies its `ppm` on top
    /// — so a seeded correction must be counted in the totals (it is real, and
    /// the limit is a limit on the total) but subtracted from every retune
    /// request, or the board would be asked to move by an offset it is already
    /// tuned off by. Zero for a board with nothing remembered.
    baseline_hz: f64,
    last_applied: Option<Instant>,
    /// When the current hunt began; `None` while locked.
    hunting_since: Option<Instant>,
    /// Previous locked reading, held for the two-in-a-row rule.
    last_locked_hz: Option<f64>,
}

impl AutoPpm {
    /// A loop for a device tuned to `nominal_center_hz`, allowed to move it by
    /// at most `limit_ppm` in total, opened with `applied_ppm` of correction
    /// already dialed into the backend out of this board's remembered offset
    /// (0 for a board with nothing remembered).
    ///
    /// That seed is the loop's starting total, not a free head start: it counts
    /// against the limit exactly as a measured correction would, and the first
    /// hunt reading therefore refines it ("another 40 Hz on top of the 900 we
    /// already applied") instead of re-measuring the whole error from zero and
    /// applying it twice.
    pub fn new(nominal_center_hz: u64, limit_ppm: f64, applied_ppm: f64) -> Self {
        let limit_hz = limit_ppm.max(0.0) * nominal_center_hz as f64 / 1e6;
        let seed = applied_ppm * nominal_center_hz as f64 / 1e6;
        // A seed outside the limit would leave the loop with nowhere to go on
        // its very first step; the caller clamps too, and both are cheap.
        let seed = if seed.is_finite() {
            seed.clamp(-limit_hz, limit_hz)
        } else {
            0.0
        };
        AutoPpm {
            nominal_hz: nominal_center_hz,
            limit_hz,
            correction_hz: seed,
            baseline_hz: seed,
            last_applied: None,
            hunting_since: None,
            last_locked_hz: None,
        }
    }

    /// The cumulative correction so far, in ppm of the nominal center —
    /// including anything seeded in at open. This is the number worth
    /// remembering and the number worth reporting.
    pub fn total_ppm(&self) -> f64 {
        if self.nominal_hz == 0 {
            return 0.0;
        }
        self.correction_hz / self.nominal_hz as f64 * 1e6
    }

    /// The same total in Hz.
    pub fn total_hz(&self) -> f64 {
        self.correction_hz
    }

    /// The site started (or restarted) hunting at `now`.
    pub fn on_hunting_started(&mut self, now: Instant) {
        self.hunting_since = Some(now);
        self.last_locked_hz = None;
    }

    /// The site acquired its control channel.
    pub fn on_acquired(&mut self) {
        self.hunting_since = None;
    }

    /// Fold in one stats window and say whether the tuner should move.
    pub fn observe(&mut self, obs: Observation, now: Instant) -> Option<Decision> {
        if self.nominal_hz == 0 || self.limit_hz <= 0.0 {
            return None;
        }
        if obs.hunting {
            self.observe_hunting(obs, now)
        } else {
            self.observe_locked(obs, now)
        }
    }

    fn observe_hunting(&mut self, obs: Observation, now: Instant) -> Option<Decision> {
        // A locked run of readings does not survive a loss of lock: the site
        // is a different measurement now.
        self.last_locked_hz = None;
        let since = self.hunting_since?;
        if now.saturating_duration_since(since) < HUNT_SETTLE {
            return None;
        }
        let offset = obs.hunt_offset_hz?;
        if offset.abs() < HUNT_MIN_OFFSET_HZ || obs.hunt_snr_db < HUNT_MIN_SNR_DB {
            return None;
        }
        if self.cooling_down(now, HUNT_COOLDOWN) {
            return None;
        }
        self.apply(offset, now)
    }

    fn observe_locked(&mut self, obs: Observation, now: Instant) -> Option<Decision> {
        let Some(offset) = obs.control_offset_hz else {
            self.last_locked_hz = None;
            return None;
        };
        if offset.abs() < LOCK_MIN_OFFSET_HZ {
            // Not just "no step": a quiet window breaks the run, so two
            // corrections are never built from readings either side of a gap.
            self.last_locked_hz = None;
            return None;
        }
        // Store this reading first — even when the pair is rejected below, it
        // is the one the *next* observation is compared against.
        let previous = self.last_locked_hz.replace(offset)?;
        if previous.signum() != offset.signum() {
            return None;
        }
        let (a, b) = (previous.abs(), offset.abs());
        if (a - b).abs() > LOCK_AGREE_TOLERANCE * a.max(b) {
            return None;
        }
        if self.cooling_down(now, LOCK_COOLDOWN) {
            return None;
        }
        // The pair is spent: the next correction needs two fresh readings.
        self.last_locked_hz = None;
        self.apply(0.5 * (previous + offset), now)
    }

    fn cooling_down(&self, now: Instant, cooldown: Duration) -> bool {
        self.last_applied
            .is_some_and(|last| now.saturating_duration_since(last) < cooldown)
    }

    /// Fold `step` into the cumulative correction, clamped to the limit, and
    /// describe the retune it implies. `None` when the limit leaves no room.
    fn apply(&mut self, step: f64, now: Instant) -> Option<Decision> {
        let wanted = self.correction_hz + step;
        let clamped = wanted.abs() > self.limit_hz;
        let total = wanted.clamp(-self.limit_hz, self.limit_hz);
        let applied = total - self.correction_hz;
        if applied == 0.0 {
            // Already sitting on the limit in the direction asked for. Say
            // nothing rather than retuning to the frequency already in use.
            return None;
        }
        self.correction_hz = total;
        self.last_applied = Some(now);
        Some(Decision {
            // Nominal, and net of what the backend's own ppm already applies:
            // the two compose, so asking for the whole total here would apply
            // a seeded correction a second time.
            nominal_request_hz: (self.nominal_hz as f64 + total - self.baseline_hz)
                .round()
                .max(0.0) as u64,
            step_hz: applied,
            total_hz: total,
            total_ppm: total / self.nominal_hz as f64 * 1e6,
            clamped,
        })
    }
}

/// What the DSP thread's event router leaves for the correction loop.
///
/// The router runs inside the engine's event closure, where neither the tuner
/// stream nor the engines are reachable, so it records rather than acts: one
/// `Cell` each for the current hunt state, the fact that it just changed, and
/// the newest stats window. The DSP loop drains this once per block, after the
/// borrows are done. Single-threaded by construction — it never leaves the DSP
/// thread — so `Cell` is the whole synchronization story.
pub struct AutoPpmInbox {
    /// The site whose events drive this device's correction; every other
    /// site's are ignored.
    site: usize,
    hunting: std::cell::Cell<bool>,
    changed: std::cell::Cell<bool>,
    latest: std::cell::Cell<Option<Observation>>,
}

impl AutoPpmInbox {
    /// An inbox listening to `site`, which starts out hunting (that is what an
    /// engine does before it has acquired anything).
    pub fn new(site: usize) -> Self {
        AutoPpmInbox {
            site,
            hunting: std::cell::Cell::new(true),
            changed: std::cell::Cell::new(true),
            latest: std::cell::Cell::new(None),
        }
    }

    /// Record that `site` began hunting (or lost control, which is the same
    /// thing one event later).
    pub fn note_hunting(&self, site: usize) {
        if site == self.site && !self.hunting.replace(true) {
            self.changed.set(true);
        }
    }

    /// Record that `site` acquired its control channel.
    pub fn note_acquired(&self, site: usize) {
        if site == self.site && self.hunting.replace(false) {
            self.changed.set(true);
        }
    }

    /// Record one stats window, replacing any window not yet consumed.
    pub fn note_stats(
        &self,
        site: usize,
        hunt_offset_hz: Option<f64>,
        hunt_snr_db: f64,
        control_offset_hz: Option<f64>,
    ) {
        if site != self.site {
            return;
        }
        self.latest.set(Some(Observation {
            hunting: self.hunting.get(),
            hunt_offset_hz,
            hunt_snr_db,
            control_offset_hz,
        }));
    }

    /// Take the hunt/lock transition, if one happened since the last call.
    /// `Some(true)` means hunting started.
    pub fn take_transition(&self) -> Option<bool> {
        self.changed.replace(false).then(|| self.hunting.get())
    }

    /// Take the newest stats window, if one arrived since the last call.
    pub fn take_observation(&self) -> Option<Observation> {
        self.latest.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CENTER: u64 = 855_500_000;

    fn hunting(offset: Option<f64>, snr_db: f64) -> Observation {
        Observation {
            hunting: true,
            hunt_offset_hz: offset,
            hunt_snr_db: snr_db,
            control_offset_hz: None,
        }
    }

    fn locked(offset: Option<f64>) -> Observation {
        Observation {
            hunting: false,
            hunt_offset_hz: None,
            hunt_snr_db: 0.0,
            control_offset_hz: offset,
        }
    }

    fn started(now: Instant) -> AutoPpm {
        let mut a = AutoPpm::new(CENTER, 5.0, 0.0);
        a.on_hunting_started(now);
        a
    }

    #[test]
    fn hunting_waits_for_the_settle_time_then_corrects_upward() {
        let t0 = Instant::now();
        let mut auto = started(t0);
        assert_eq!(auto.observe(hunting(Some(901.0), 20.0), t0), None);
        assert_eq!(
            auto.observe(hunting(Some(901.0), 20.0), t0 + Duration::from_secs(2)),
            None,
            "two seconds of hunting is not yet enough"
        );
        let d = auto
            .observe(hunting(Some(901.0), 20.0), t0 + HUNT_SETTLE)
            .expect("three seconds in, a 901 Hz offset is actionable");
        // A carrier 901 Hz high means the board must be asked for 901 Hz more.
        assert_eq!(d.nominal_request_hz, CENTER + 901);
        assert!((d.step_hz - 901.0).abs() < 1e-9);
        assert!((d.total_hz - 901.0).abs() < 1e-9);
        assert!((d.total_ppm - 1.0532).abs() < 1e-3, "{d:?}");
        assert!(!d.clamped);
    }

    #[test]
    fn a_carrier_below_nominal_pulls_the_tuner_down() {
        let t0 = Instant::now();
        let mut auto = started(t0);
        let d = auto
            .observe(hunting(Some(-855.0), 12.0), t0 + HUNT_SETTLE)
            .expect("actionable");
        assert_eq!(d.nominal_request_hz, CENTER - 855);
        assert!(d.step_hz < 0.0);
        assert!((d.total_ppm + 1.0).abs() < 1e-3, "{d:?}");
    }

    #[test]
    fn a_small_offset_or_a_flat_band_is_left_alone() {
        let t0 = Instant::now();
        let at = t0 + HUNT_SETTLE;
        // Under the offset threshold: the demodulator copes on its own.
        assert_eq!(started(t0).observe(hunting(Some(299.0), 20.0), at), None);
        // Plenty of offset, but nothing stands out of the noise floor.
        assert_eq!(started(t0).observe(hunting(Some(901.0), 5.9), at), None);
        // No measurement at all.
        assert_eq!(started(t0).observe(hunting(None, 20.0), at), None);
    }

    #[test]
    fn a_second_correction_waits_out_the_cooldown() {
        let t0 = Instant::now();
        let mut auto = started(t0);
        let at = t0 + HUNT_SETTLE;
        assert!(auto.observe(hunting(Some(901.0), 20.0), at).is_some());
        assert_eq!(
            auto.observe(hunting(Some(400.0), 20.0), at + Duration::from_secs(1)),
            None
        );
        let d = auto
            .observe(hunting(Some(400.0), 20.0), at + HUNT_COOLDOWN)
            .expect("the cooldown has expired");
        // Corrections accumulate rather than replacing one another.
        assert!((d.total_hz - 1301.0).abs() < 1e-9, "{d:?}");
        assert_eq!(d.nominal_request_hz, CENTER + 1301);
    }

    #[test]
    fn the_total_correction_stops_at_the_configured_limit() {
        let t0 = Instant::now();
        // 1 ppm of 855.5 MHz is 855.5 Hz.
        let mut auto = AutoPpm::new(CENTER, 1.0, 0.0);
        auto.on_hunting_started(t0);
        let d = auto
            .observe(hunting(Some(2000.0), 20.0), t0 + HUNT_SETTLE)
            .expect("actionable");
        assert!(d.clamped);
        assert!((d.total_hz - 855.5).abs() < 1e-6, "{d:?}");
        assert!((d.total_ppm - 1.0).abs() < 1e-6, "{d:?}");
        // Nothing further in the same direction: the limit is a hard stop.
        assert_eq!(
            auto.observe(
                hunting(Some(2000.0), 20.0),
                t0 + HUNT_SETTLE + HUNT_COOLDOWN
            ),
            None
        );
    }

    #[test]
    fn a_locked_channel_needs_two_agreeing_readings_and_a_long_cooldown() {
        let t0 = Instant::now();
        let mut auto = AutoPpm::new(CENTER, 5.0, 0.0);
        auto.on_acquired();
        // One reading is never enough.
        assert_eq!(auto.observe(locked(Some(200.0)), t0), None);
        // A reading that disagrees in sign breaks the pair.
        assert_eq!(
            auto.observe(locked(Some(-200.0)), t0 + Duration::from_secs(5)),
            None
        );
        // …and one that disagrees by more than half in magnitude.
        assert_eq!(
            auto.observe(locked(Some(-1000.0)), t0 + Duration::from_secs(10)),
            None
        );
        // Below the threshold, nothing happens and the run resets.
        assert_eq!(
            auto.observe(locked(Some(149.0)), t0 + Duration::from_secs(15)),
            None
        );
        // Two agreeing readings: 200 and 240 are within 50 %.
        assert_eq!(
            auto.observe(locked(Some(200.0)), t0 + Duration::from_secs(20)),
            None
        );
        let d = auto
            .observe(locked(Some(240.0)), t0 + Duration::from_secs(25))
            .expect("two agreeing readings past the threshold");
        assert!((d.step_hz - 220.0).abs() < 1e-9, "{d:?}");
        assert_eq!(d.nominal_request_hz, CENTER + 220);

        // The next correction needs a fresh pair *and* the 10 s cooldown.
        let t = t0 + Duration::from_secs(25);
        assert_eq!(auto.observe(locked(Some(300.0)), t), None);
        assert_eq!(
            auto.observe(locked(Some(300.0)), t + Duration::from_secs(1)),
            None,
            "the pair agrees but the cooldown has not expired"
        );
        assert!(
            auto.observe(locked(Some(300.0)), t + LOCK_COOLDOWN)
                .is_some(),
            "a fresh agreeing pair once the cooldown has expired"
        );
    }

    #[test]
    fn a_device_with_no_center_frequency_never_corrects() {
        let t0 = Instant::now();
        let mut auto = AutoPpm::new(0, 5.0, 0.0);
        auto.on_hunting_started(t0);
        assert_eq!(
            auto.observe(hunting(Some(901.0), 20.0), t0 + HUNT_SETTLE),
            None
        );
    }

    #[test]
    fn a_seeded_loop_refines_the_remembered_correction_rather_than_repeating_it() {
        let t0 = Instant::now();
        // The board was opened on a remembered +1 ppm (855.5 Hz at this
        // center), so the hardware is already 855.5 Hz off nominal.
        let mut auto = AutoPpm::new(CENTER, 5.0, 1.0);
        auto.on_hunting_started(t0);
        assert!(
            (auto.total_ppm() - 1.0).abs() < 1e-9,
            "the seed is the total"
        );
        assert!((auto.total_hz() - 855.5).abs() < 1e-6);

        // What is left of the error after the remembered correction: 400 Hz.
        let d = auto
            .observe(hunting(Some(400.0), 20.0), t0 + HUNT_SETTLE)
            .expect("actionable");
        // The step is the 400 Hz measured, and the total is the seed plus it...
        assert!((d.step_hz - 400.0).abs() < 1e-9, "{d:?}");
        assert!((d.total_hz - 1255.5).abs() < 1e-6, "{d:?}");
        assert!((d.total_ppm - 1.4675).abs() < 1e-3, "{d:?}");
        // ...but the *request* is net of what the backend's ppm already does,
        // so the board moves by the 400 Hz it is actually still out by.
        assert_eq!(d.nominal_request_hz, CENTER + 400);
        assert!(!d.clamped);
        assert!((auto.total_ppm() - 1.4675).abs() < 1e-3);
    }

    #[test]
    fn a_seed_counts_against_the_limit_like_any_other_correction() {
        let t0 = Instant::now();
        // Limit 1 ppm = 855.5 Hz, and 0.9 ppm of it is already spent.
        let mut auto = AutoPpm::new(CENTER, 1.0, 0.9);
        auto.on_hunting_started(t0);
        let d = auto
            .observe(hunting(Some(2000.0), 20.0), t0 + HUNT_SETTLE)
            .expect("actionable");
        assert!(d.clamped, "the seed left only 0.1 ppm of room");
        assert!((d.total_ppm - 1.0).abs() < 1e-6, "{d:?}");
        assert!(
            (d.step_hz - 85.55).abs() < 1e-3,
            "the remainder only: {d:?}"
        );
        assert_eq!(d.nominal_request_hz, CENTER + 86);
        // And there is nothing left in that direction.
        assert_eq!(
            auto.observe(
                hunting(Some(2000.0), 20.0),
                t0 + HUNT_SETTLE + HUNT_COOLDOWN
            ),
            None
        );
    }

    #[test]
    fn a_seed_beyond_the_limit_is_capped_at_it() {
        // 9 ppm remembered, 1 ppm allowed: the loop starts on the limit rather
        // than on a total it could never have reached, so its accounting (and
        // anything it writes back to memory) stays inside the operator's cap.
        let auto = AutoPpm::new(CENTER, 1.0, 9.0);
        assert!(
            (auto.total_ppm() - 1.0).abs() < 1e-9,
            "{}",
            auto.total_ppm()
        );
        let auto = AutoPpm::new(CENTER, 1.0, -9.0);
        assert!(
            (auto.total_ppm() + 1.0).abs() < 1e-9,
            "{}",
            auto.total_ppm()
        );
        // An unseeded loop is the seeded one with a zero seed.
        assert_eq!(AutoPpm::new(CENTER, 5.0, 0.0).total_hz(), 0.0);
    }

    #[test]
    fn a_seeded_correction_can_be_walked_back_toward_nominal() {
        // The remembered value was too big — the board drifted, or it was
        // measured on a different band. The loop pulls it back, and the
        // request goes *below* nominal because the backend ppm is still
        // holding the whole seed.
        let t0 = Instant::now();
        let mut auto = AutoPpm::new(CENTER, 5.0, 1.0);
        auto.on_hunting_started(t0);
        let d = auto
            .observe(hunting(Some(-500.0), 20.0), t0 + HUNT_SETTLE)
            .expect("actionable");
        assert!((d.total_hz - 355.5).abs() < 1e-6, "{d:?}");
        assert_eq!(d.nominal_request_hz, CENTER - 500);
    }

    #[test]
    fn the_inbox_records_transitions_and_the_newest_window_only() {
        let inbox = AutoPpmInbox::new(2);
        // It starts hunting, and says so once.
        assert_eq!(inbox.take_transition(), Some(true));
        assert_eq!(inbox.take_transition(), None);
        // Another site's events are ignored.
        inbox.note_acquired(1);
        assert_eq!(inbox.take_transition(), None);
        inbox.note_acquired(2);
        assert_eq!(inbox.take_transition(), Some(false));

        inbox.note_stats(1, Some(1.0), 1.0, None);
        assert_eq!(inbox.take_observation(), None);
        inbox.note_stats(2, Some(100.0), 8.0, Some(10.0));
        inbox.note_stats(2, Some(200.0), 9.0, Some(20.0));
        let obs = inbox.take_observation().expect("a window is waiting");
        assert!(!obs.hunting);
        assert_eq!(obs.hunt_offset_hz, Some(200.0));
        assert_eq!(obs.control_offset_hz, Some(20.0));
        assert_eq!(inbox.take_observation(), None);
    }
}
