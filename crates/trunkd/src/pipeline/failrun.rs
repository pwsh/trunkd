// SPDX-License-Identifier: GPL-3.0-or-later
//! Logging a request that keeps failing without logging every attempt.
//!
//! The control and observation loops talk to Keyup on fixed cadences (the
//! desired-state GET every 10 s, tuner status every 15 s, observations and
//! radio events per site). While Keyup is down each attempt used to warn:
//! ~14 000 lines a day from the first two alone, burying everything else
//! (review 2026-09-22 §3.5). A [`FailureRun`] logs the first failure of a
//! run, then a reminder at most every [`FailureRun::REPEAT`] carrying the
//! count, and one line when the request works again.

use std::time::{Duration, Instant};

/// One request's current run of consecutive failures.
#[derive(Debug, Default)]
pub(crate) struct FailureRun {
    failures: u64,
    since: Option<Instant>,
    last_logged: Option<Instant>,
}

impl FailureRun {
    /// Longest gap between two warnings about the same run.
    pub(crate) const REPEAT: Duration = Duration::from_secs(600);

    /// Count one failure. Returns the run's length when this one should be
    /// logged: the first of a run, then once per [`Self::REPEAT`].
    pub(crate) fn failed(&mut self, now: Instant) -> Option<u64> {
        self.failures += 1;
        self.since.get_or_insert(now);
        match self.last_logged {
            Some(at) if now.duration_since(at) < Self::REPEAT => None,
            _ => {
                self.last_logged = Some(now);
                Some(self.failures)
            }
        }
    }

    /// Count a success. Returns how many attempts failed and for how long when
    /// it ends a run, so the recovery can be logged once.
    pub(crate) fn succeeded(&mut self, now: Instant) -> Option<(u64, Duration)> {
        let since = self.since.take()?;
        let failures = std::mem::take(&mut self.failures);
        self.last_logged = None;
        Some((failures, now.duration_since(since)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_day_of_failures_every_ten_seconds_logs_a_few_hundred_lines_not_thousands() {
        let mut run = FailureRun::default();
        let start = Instant::now();
        let attempts = 24 * 60 * 60 / 10;
        let logged = (0..attempts)
            .filter(|i| run.failed(start + Duration::from_secs(10 * i)).is_some())
            .count();
        // 8 640 attempts: the first, then one per ten minutes.
        assert_eq!(logged, 144);
        let (failures, lasted) = run
            .succeeded(start + Duration::from_secs(10 * attempts))
            .expect("the run ended");
        assert_eq!(failures, attempts);
        assert_eq!(lasted, Duration::from_secs(10 * attempts));
    }

    #[test]
    fn the_first_failure_of_every_run_is_logged_and_a_success_alone_is_not() {
        let mut run = FailureRun::default();
        let t = Instant::now();
        assert_eq!(run.succeeded(t), None, "nothing had failed");
        assert_eq!(run.failed(t), Some(1));
        assert_eq!(run.failed(t + Duration::from_secs(10)), None);
        assert_eq!(run.failed(t + Duration::from_secs(20)), None);
        assert_eq!(
            run.succeeded(t + Duration::from_secs(30)),
            Some((3, Duration::from_secs(30)))
        );
        // A new outage a minute later is news again.
        assert_eq!(run.failed(t + Duration::from_secs(90)), Some(1));
        assert_eq!(
            run.failed(t + Duration::from_secs(90) + FailureRun::REPEAT),
            Some(2),
            "and still failing is repeated with the count"
        );
    }
}
