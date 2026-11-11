// SPDX-License-Identifier: GPL-3.0-or-later
//! Timestamp wrapper over [`time::OffsetDateTime`] with millisecond helpers.
//!
//! Calls carry true start/stop timestamps in milliseconds (ARCHITECTURE §4), so
//! millisecond conversion is first-class here.

use time::OffsetDateTime;

/// A UTC instant with millisecond helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(OffsetDateTime);

impl Timestamp {
    /// Current UTC instant.
    pub fn now() -> Self {
        Timestamp(OffsetDateTime::now_utc())
    }

    /// Build from Unix milliseconds since the epoch.
    ///
    /// Returns [`None`] if the value is outside the representable range.
    pub fn from_unix_millis(ms: i64) -> Option<Self> {
        let nanos = (ms as i128).checked_mul(1_000_000)?;
        OffsetDateTime::from_unix_timestamp_nanos(nanos)
            .ok()
            .map(Timestamp)
    }

    /// Unix milliseconds since the epoch (truncated toward the epoch).
    pub fn unix_millis(&self) -> i64 {
        (self.0.unix_timestamp_nanos() / 1_000_000) as i64
    }

    /// The wrapped [`OffsetDateTime`].
    pub fn inner(&self) -> OffsetDateTime {
        self.0
    }

    /// This instant advanced by a [`std::time::Duration`].
    pub fn saturating_add(&self, d: std::time::Duration) -> Self {
        Timestamp(self.0 + d)
    }
}

impl From<OffsetDateTime> for Timestamp {
    fn from(v: OffsetDateTime) -> Self {
        Timestamp(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_unix_millis() {
        let ms = 1_700_000_000_123;
        let ts = Timestamp::from_unix_millis(ms).expect("valid");
        assert_eq!(ts.unix_millis(), ms);
    }

    #[test]
    fn now_is_after_2020() {
        // 2020-01-01T00:00:00Z in ms.
        assert!(Timestamp::now().unix_millis() > 1_577_836_800_000);
    }
}
