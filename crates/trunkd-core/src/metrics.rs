// SPDX-License-Identifier: GPL-3.0-or-later
//! A tiny, dependency-light metrics registry.
//!
//! Named counters and gauges backed by [`AtomicU64`]. The control-plane API
//! (ARCHITECTURE §5, `GET /metrics`) will later render [`Metrics::snapshot`] as
//! Prometheus text; nothing here pulls in a Prometheus dependency.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Whether a metric counts monotonically (counter) or fluctuates (gauge).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// Monotonically increasing value (e.g. overflow count).
    Counter,
    /// Arbitrary instantaneous value (e.g. current sample rate).
    Gauge,
}

/// A cheap, cloneable handle to one atomic metric cell.
///
/// Gauges are stored as raw `u64` bits; use [`Metric::set_f64`] / [`Metric::get_f64`]
/// for fractional gauges.
#[derive(Debug, Clone)]
pub struct Metric {
    cell: Arc<AtomicU64>,
}

impl Metric {
    /// Add `n` to the value, returning the previous value.
    pub fn add(&self, n: u64) -> u64 {
        self.cell.fetch_add(n, Ordering::Relaxed)
    }

    /// Increment by one.
    pub fn inc(&self) {
        self.add(1);
    }

    /// Overwrite the value.
    pub fn set(&self, n: u64) {
        self.cell.store(n, Ordering::Relaxed);
    }

    /// Read the current value.
    pub fn get(&self) -> u64 {
        self.cell.load(Ordering::Relaxed)
    }

    /// Store an `f64` gauge value (bit-encoded).
    pub fn set_f64(&self, v: f64) {
        self.cell.store(v.to_bits(), Ordering::Relaxed);
    }

    /// Read an `f64` gauge value (bit-decoded).
    pub fn get_f64(&self) -> f64 {
        f64::from_bits(self.cell.load(Ordering::Relaxed))
    }
}

/// A point-in-time reading of one registered metric.
#[derive(Debug, Clone)]
pub struct MetricSnapshot {
    /// The metric's registered name.
    pub name: String,
    /// Counter or gauge.
    pub kind: MetricKind,
    /// Raw value (for gauges set via `set_f64`, decode with [`f64::from_bits`]).
    pub value: u64,
}

#[derive(Debug)]
struct Entry {
    kind: MetricKind,
    cell: Arc<AtomicU64>,
}

/// A registry of named counters and gauges.
///
/// Cloneable and shareable across threads; clones observe the same registry.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    inner: Arc<Mutex<BTreeMap<String, Entry>>>,
}

impl Metrics {
    /// Create an empty registry.
    pub fn new() -> Self {
        Metrics::default()
    }

    fn handle(&self, name: &str, kind: MetricKind) -> Metric {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Looking up a metric that exists is the common case (status lines,
        // panel snapshots, routing): it must not allocate the key again.
        if let Some(entry) = map.get(name) {
            return Metric {
                cell: entry.cell.clone(),
            };
        }
        let entry = map.entry(name.to_owned()).or_insert_with(|| Entry {
            kind,
            cell: Arc::new(AtomicU64::new(0)),
        });
        Metric {
            cell: entry.cell.clone(),
        }
    }

    /// Get (or create) a counter by name.
    pub fn counter(&self, name: &str) -> Metric {
        self.handle(name, MetricKind::Counter)
    }

    /// Get (or create) a gauge by name.
    pub fn gauge(&self, name: &str) -> Metric {
        self.handle(name, MetricKind::Gauge)
    }

    /// Snapshot every registered metric, sorted by name.
    pub fn snapshot(&self) -> Vec<MetricSnapshot> {
        let map = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.iter()
            .map(|(name, e)| MetricSnapshot {
                name: name.clone(),
                kind: e.kind,
                value: e.cell.load(Ordering::Relaxed),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_share_backing_cell_by_name() {
        let m = Metrics::new();
        let a = m.counter("sdr.overflows");
        let b = m.counter("sdr.overflows");
        a.add(5);
        b.inc();
        assert_eq!(a.get(), 6);
        assert_eq!(b.get(), 6);
    }

    #[test]
    fn gauge_f64_round_trips() {
        let m = Metrics::new();
        let g = m.gauge("sdr.sample_rate");
        g.set_f64(2.4e6);
        assert_eq!(g.get_f64(), 2.4e6);
    }

    #[test]
    fn snapshot_lists_all_metrics_sorted() {
        let m = Metrics::new();
        m.counter("b.count").inc();
        m.gauge("a.value").set(42);
        let snap = m.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].name, "a.value");
        assert_eq!(snap[0].kind, MetricKind::Gauge);
        assert_eq!(snap[1].name, "b.count");
        assert_eq!(snap[1].kind, MetricKind::Counter);
    }
}
