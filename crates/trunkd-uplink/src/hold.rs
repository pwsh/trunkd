// SPDX-License-Identifier: GPL-3.0-or-later
//! The **uplink hold**: a runtime switch that stops uploads without stopping
//! the recorder.
//!
//! `[uplink] spool_only = true` used to mean "never open a socket", which took
//! the control plane down with it: a spool-only recorder never registered with
//! keyup and could not be managed from anywhere but the TOML. The hold splits
//! those two ideas apart. `[uplink] enabled` still decides whether this
//! recorder talks to keyup at all; the hold decides only whether *call and
//! event uploads* are happening right now.
//!
//! While held:
//!
//! * finished calls go straight to the offline spool instead of being POSTed,
//! * radio-event batches go to the event spool instead of being POSTed,
//! * site observations are skipped entirely — they are periodic snapshots, and
//!   a snapshot nobody received is not worth keeping,
//! * the status POST and the desired-state poll carry on, which is what lets
//!   the hold be released from somewhere other than a text editor.
//!
//! Releasing it puts the uplink worker back on its normal path, and the spool
//! drains through the reconnect logic that already existed for an unreachable
//! keyup.
//!
//! Three writers can move it — the boot config, keyup's desired state, and the
//! local panel — and the most recent writer wins. The current value and the
//! source that set it are reported on every status POST, so "why is this
//! recorder not uploading?" is answerable from keyup.
//!
//! Reads are a single relaxed atomic load: the flag is consulted on control-
//! plane threads, never on a DSP thread, and no writer can ever block one.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// Who last set the hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldSource {
    /// The boot value of `[uplink] spool_only`.
    Config,
    /// keyup's desired state pushed it.
    Keyup,
    /// The local control panel toggled it.
    Panel,
}

impl HoldSource {
    /// The wire name, as it appears in the status POST and the panel.
    pub fn as_str(self) -> &'static str {
        match self {
            HoldSource::Config => "config",
            HoldSource::Keyup => "keyup",
            HoldSource::Panel => "panel",
        }
    }

    fn tag(self) -> u8 {
        match self {
            HoldSource::Config => 0,
            HoldSource::Keyup => 1,
            HoldSource::Panel => 2,
        }
    }

    fn from_tag(t: u8) -> HoldSource {
        match t {
            1 => HoldSource::Keyup,
            2 => HoldSource::Panel,
            _ => HoldSource::Config,
        }
    }
}

/// The shared "uploads are held" switch.
///
/// Share it as an `Arc`: the uplink worker, the observations/events thread, the
/// keyup control loop and the local panel all hold one, and each reads it on
/// its own cadence.
#[derive(Debug)]
pub struct UplinkHold {
    held: AtomicBool,
    source: AtomicU8,
}

impl UplinkHold {
    /// A hold in its boot state, attributed to the config.
    pub fn new(held: bool) -> Self {
        UplinkHold {
            held: AtomicBool::new(held),
            source: AtomicU8::new(HoldSource::Config.tag()),
        }
    }

    /// Whether uploads are held right now.
    pub fn is_held(&self) -> bool {
        self.held.load(Ordering::Relaxed)
    }

    /// Who last set it.
    pub fn source(&self) -> HoldSource {
        HoldSource::from_tag(self.source.load(Ordering::Relaxed))
    }

    /// Set the hold and record who did it.
    ///
    /// Returns true when the value actually changed, so a caller can log a
    /// transition rather than a heartbeat. The source is stamped either way:
    /// "keyup last confirmed this" is worth reporting even when nothing moved.
    pub fn set(&self, held: bool, source: HoldSource) -> bool {
        // Source first: a reader that catches the two stores apart should see
        // the new owner of an old value rather than the old owner of a new one,
        // because the pair is only ever *reported*, never acted on together.
        self.source.store(source.tag(), Ordering::Relaxed);
        self.held.swap(held, Ordering::Relaxed) != held
    }
}

impl Default for UplinkHold {
    fn default() -> Self {
        UplinkHold::new(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_hold_carries_the_config_boot_value() {
        let h = UplinkHold::new(true);
        assert!(h.is_held());
        assert_eq!(h.source(), HoldSource::Config);
        assert_eq!(h.source().as_str(), "config");
    }

    #[test]
    fn the_most_recent_writer_wins_and_is_named() {
        let h = UplinkHold::new(false);
        assert!(h.set(true, HoldSource::Keyup), "false -> true is a change");
        assert!(h.is_held());
        assert_eq!(h.source(), HoldSource::Keyup);

        assert!(!h.set(true, HoldSource::Panel), "true -> true is not");
        assert_eq!(h.source(), HoldSource::Panel, "but the source still moves");

        assert!(h.set(false, HoldSource::Panel));
        assert!(!h.is_held());
    }
}
