// SPDX-License-Identifier: GPL-3.0-or-later
//! Talkgroups this recorder does not record at all.
//!
//! An operator who has turned Record, Transcribe and every export off for a
//! talkgroup has said, as plainly as the UI allows, that they do not want it.
//! Keyup collects those talkgroups into one list and pushes it as
//! `recorder.skip_talkgroups`; the recorder holds it here and refuses the grant
//! before a tuner is claimed, so a skipped talkgroup costs no traffic slot, no
//! audio, no upload — and, on a busy site, leaves the pool for the talkgroups
//! the operator does want.
//!
//! The list has two halves, unioned:
//!
//! * the **local** half, `[recorder] skip_talkgroups` in the TOML, which is the
//!   operator's own and which keyup can never take away; and
//! * the **keyup** half, replaced whole whenever the pushed `revision` differs
//!   from the one held, and dropped when keyup pushes `null`.
//!
//! Read from the DSP threads (once per grant, which is a few times a second at
//! worst) and written from the control thread, so it is an `RwLock` around a
//! small set rather than anything cleverer.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

/// What keyup pushed: the talkgroups to skip and the revision that identifies
/// this version of the list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkipSpec {
    /// Talkgroups to skip. Deduplicated on apply.
    pub tgids: Vec<u32>,
    /// Keyup's identifier for this version of the list. A push whose revision
    /// equals the one in force costs nothing.
    pub revision: String,
}

#[derive(Debug, Default)]
struct Inner {
    /// `[recorder] skip_talkgroups`, fixed for the run.
    local: BTreeSet<u32>,
    /// The last list keyup pushed.
    keyup: BTreeSet<u32>,
    /// The revision that list carried, `None` when keyup is not managing one.
    revision: Option<String>,
    /// `local ∪ keyup`, precomputed: this is what every grant is tested
    /// against, and a grant must not pay for a union.
    effective: BTreeSet<u32>,
}

impl Inner {
    fn recompute(&mut self) {
        self.effective = self.local.union(&self.keyup).copied().collect();
    }
}

/// The talkgroups this recorder refuses to record, shared between the control
/// thread that applies keyup's pushes and the DSP threads that test grants.
#[derive(Debug, Default)]
pub struct SkipList {
    inner: RwLock<Inner>,
}

impl SkipList {
    /// A list holding only the local (TOML) half.
    pub fn new<I: IntoIterator<Item = u32>>(local: I) -> Arc<SkipList> {
        let mut inner = Inner {
            local: local.into_iter().filter(|t| *t != 0).collect(),
            ..Inner::default()
        };
        inner.recompute();
        Arc::new(SkipList {
            inner: RwLock::new(inner),
        })
    }

    /// An empty list: nothing is skipped, and nothing ever will be unless keyup
    /// pushes one. For tests and for the fixtures that need a state block.
    pub fn empty() -> Arc<SkipList> {
        SkipList::new(std::iter::empty())
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|p| p.into_inner())
    }

    /// Whether `tgid` is skipped. The hot path: one read lock and a set probe
    /// per grant.
    pub fn contains(&self, tgid: u32) -> bool {
        let inner = self.read();
        !inner.effective.is_empty() && inner.effective.contains(&tgid)
    }

    /// Whether nothing at all is skipped — the answer for almost every
    /// recorder, and worth checking before anything more expensive.
    pub fn is_empty(&self) -> bool {
        self.read().effective.is_empty()
    }

    /// How many talkgroups are skipped in total (local ∪ keyup).
    pub fn len(&self) -> usize {
        self.read().effective.len()
    }

    /// The effective list, ascending.
    pub fn tgids(&self) -> Vec<u32> {
        self.read().effective.iter().copied().collect()
    }

    /// The local (TOML) half, ascending. Reported separately so an operator can
    /// see which entries keyup cannot take away.
    pub fn local(&self) -> Vec<u32> {
        self.read().local.iter().copied().collect()
    }

    /// The revision of the keyup half in force, or `None` when keyup is not
    /// managing a list.
    pub fn revision(&self) -> Option<String> {
        self.read().revision.clone()
    }

    /// Apply what keyup pushed: `None` drops whatever keyup half is held (the
    /// local half survives), `Some` replaces it when the revision differs.
    /// Returns whether anything moved.
    ///
    /// Revision-gated exactly as the broadcastify block is: a redelivery of the
    /// revision in force costs nothing, and an empty revision is treated as
    /// "always new" so a keyup that forgets to send one still works.
    pub fn apply(&self, next: Option<SkipSpec>) -> bool {
        let mut inner = self.inner.write().unwrap_or_else(|p| p.into_inner());
        match next {
            None => {
                if inner.revision.is_none() && inner.keyup.is_empty() {
                    return false;
                }
                inner.revision = None;
                inner.keyup.clear();
                inner.recompute();
                true
            }
            Some(spec) => {
                let same = inner
                    .revision
                    .as_deref()
                    .is_some_and(|held| held == spec.revision && !spec.revision.is_empty());
                if same {
                    return false;
                }
                inner.keyup = spec.tgids.iter().copied().filter(|t| *t != 0).collect();
                inner.revision = Some(spec.revision);
                inner.recompute();
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_half_is_skipped_with_no_keyup_at_all() {
        let s = SkipList::new([1786, 1787]);
        assert!(s.contains(1786));
        assert!(!s.contains(1788));
        assert_eq!(s.len(), 2);
        assert_eq!(s.revision(), None);
        assert_eq!(s.local(), vec![1786, 1787]);
    }

    #[test]
    fn keyups_list_is_unioned_with_the_local_one_and_never_replaces_it() {
        let s = SkipList::new([1001]);
        assert!(s.apply(Some(SkipSpec {
            tgids: vec![1786],
            revision: "a1".to_owned(),
        })));
        assert!(s.contains(1001), "the local half survives a push");
        assert!(s.contains(1786));
        assert_eq!(s.tgids(), vec![1001, 1786]);

        // Same revision: nothing moves, however different the list looks.
        assert!(!s.apply(Some(SkipSpec {
            tgids: vec![9],
            revision: "a1".to_owned(),
        })));
        assert!(!s.contains(9));

        // A new revision replaces the keyup half whole.
        assert!(s.apply(Some(SkipSpec {
            tgids: vec![9],
            revision: "b2".to_owned(),
        })));
        assert!(s.contains(9));
        assert!(!s.contains(1786));
        assert!(s.contains(1001));

        // `null`: keyup's half goes, the operator's stays.
        assert!(s.apply(None));
        assert!(!s.contains(9));
        assert!(s.contains(1001));
        assert_eq!(s.revision(), None);
        // A second drop is a no-op.
        assert!(!s.apply(None));
    }

    #[test]
    fn an_empty_list_skips_nothing_and_says_so_cheaply() {
        let s = SkipList::empty();
        assert!(s.is_empty());
        assert!(!s.contains(0));
        assert!(!s.contains(1786));
        // Talkgroup 0 is unit-to-unit / telephone and is never a skip entry.
        s.apply(Some(SkipSpec {
            tgids: vec![0],
            revision: "r".to_owned(),
        }));
        assert!(s.is_empty());
    }
}
