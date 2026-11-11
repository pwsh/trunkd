// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end [`SiteEngine`] replay of a real wideband capture of the reference
//! system's site (control 851.05 MHz, NAC 0x034), centred at 855.5 MHz and
//! sampled at 20 Msps.
//!
//! It runs in every `cargo t`: the workspace's test profile optimizes the
//! signal-processing crates, so 60 s of 20 Msps replays in about half a
//! minute with debug assertions still on. The capture is 2.4 GB and
//! gitignored; where it is absent the build script sets
//! `trunkd_fixture_missing` and the test is reported **ignored**, with the
//! fixture it needs — never a silent pass. Point `TRUNKD_FIXTURES` at the
//! corpus if it does not sit in the workspace's `fixtures/` (how to capture
//! one: `fixtures/README.md`).
//!
//! ```text
//! cargo t -p trunkd-p25 --test site_fixture -- --nocapture
//! ```
//!
//! Besides the calls, the replay is graded on what the control channel
//! decoded and on the rules that keep a corrupt block out of the recorder's
//! state: the band plan grants resolve through and
//! the one the inventory publishes are exactly the site's two bands, every
//! traffic frequency is one the capture really carried, and no call is
//! attributed to a radio that keyed a previous call on its frequency.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::PathBuf;

use num_complex::Complex;
use trunkd_p25::call::CallRecord;
use trunkd_p25::event::P25Event;
use trunkd_p25::iden::IdenBand;
use trunkd_p25::inventory::SiteInventory;
use trunkd_p25::message::TsbkMessage;
use trunkd_p25::site::{CallId, SiteConfig, SiteEngine, SiteEvent};

/// The 60 s capture, less the system prefix its file name may carry
/// (`<system>_wb855500k_20M_int8_run2.iq`).
const FIXTURE: &str = "wb855500k_20M_int8_run2.iq";
const RATE: u32 = 20_000_000;
const CENTER_HZ: f64 = 855_500_000.0;
const CONTROL_HZ: u64 = 851_050_000;
/// NAC 0x034.
const NAC: u16 = 52;

/// Every control candidate of this site. 859.9875 MHz was carrying a voice
/// call during the capture: the hunt must not be fooled by it.
const CANDIDATES: [u64; 11] = [
    851_050_000,
    851_787_500,
    852_137_500,
    852_537_500,
    852_825_000,
    853_112_500,
    854_237_500,
    854_437_500,
    856_712_500,
    858_987_500,
    859_987_500,
];

/// Locate the gitignored capture: in `$TRUNKD_FIXTURES`, then the workspace's
/// `fixtures/`, the file `$TRUNKD_FIXTURE_NAME` if set, else the first file
/// named [`FIXTURE`] or `<system prefix>_`[`FIXTURE`] — the same rule the build
/// script applies.
fn fixture_path() -> Option<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut roots = Vec::new();
    if let Ok(dir) = std::env::var("TRUNKD_FIXTURES") {
        roots.push(PathBuf::from(dir));
    }
    // Workspace root (crates/trunkd-p25/../../fixtures).
    roots.push(manifest.join("../../fixtures"));
    roots.iter().find_map(|r| find_fixture(r))
}

/// The capture in one corpus directory (see [`fixture_path`]).
fn find_fixture(root: &std::path::Path) -> Option<PathBuf> {
    if let Ok(name) = std::env::var("TRUNKD_FIXTURE_NAME") {
        let p = root.join(name);
        return p.is_file().then_some(p);
    }
    let suffix = format!("_{FIXTURE}");
    let mut hits: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name == FIXTURE || name.ends_with(&suffix)
        })
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    hits.sort();
    hits.into_iter().next()
}

/// 64-bit FNV-1a, to pin decoded strings that should not appear in source.
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// An identifier-update's fields: (id, base, spacing, offset, bandwidth, TDMA).
type BandKey = (u8, u64, u32, i64, u32, bool);

fn band_key(b: &IdenBand) -> BandKey {
    (
        b.id,
        b.base_hz,
        b.spacing_hz,
        b.offset_hz,
        b.bandwidth_hz,
        b.tdma,
    )
}

/// What the replay is graded on.
#[derive(Default)]
struct Outcome {
    control: Option<(u64, u16)>,
    control_lost: u32,
    voice_frames: u64,
    /// Call id → frequency, for per-frequency voice totals.
    freq_of: HashMap<CallId, u64>,
    voice_by_freq: HashMap<u64, u64>,
    /// Every `CallStarted`, in order: (id, frequency, talkgroup, source).
    started: Vec<(CallId, u64, u16, u32)>,
    ended: Vec<CallRecord>,
    ended_ids: Vec<CallId>,
    /// Stream time of the block being fed, for the inventory's clock.
    now_ms: u64,
    // -- the raw control channel (`SiteConfig::raw_control`) -----------------
    tsbks: u64,
    tsbks_failed: u64,
    /// Explicit grants (`update == false`) and grant updates.
    grants: u64,
    grant_updates: u64,
    grants_unresolvable: u64,
    /// Stream time of the last grant that named a band with no plan yet.
    last_unresolvable_ms: u64,
    /// Every radio a non-update grant named on a (frequency, talkgroup).
    granted_sources: HashSet<(u64, u16, u32)>,
    /// Every identifier-update decoded, field for field, with its count.
    iden_decodes: HashMap<BandKey, u64>,
    /// The site inventory, fed exactly as the recorder feeds it: every raw
    /// control event, the `Tsbk` and the tracker's `IdenUpdate` echo both.
    inventory: Option<SiteInventory>,
}

impl Outcome {
    fn observe(&mut self, ev: &SiteEvent) {
        match ev {
            SiteEvent::ControlAcquired { frequency_hz, nac } => {
                self.control = Some((*frequency_hz, *nac))
            }
            SiteEvent::ControlLost { .. } => self.control_lost += 1,
            SiteEvent::CallStarted {
                id,
                frequency_hz,
                talkgroup,
                source,
                ..
            } => {
                self.freq_of.insert(*id, *frequency_hz);
                self.started.push((*id, *frequency_hz, *talkgroup, *source));
            }
            SiteEvent::CallVoice { id, .. } => {
                self.voice_frames += 1;
                if let Some(f) = self.freq_of.get(id) {
                    *self.voice_by_freq.entry(*f).or_default() += 1;
                }
            }
            SiteEvent::CallEnded { id, record, .. } => {
                self.ended.push(record.clone());
                self.ended_ids.push(*id);
            }
            SiteEvent::Control(e) => self.observe_control(e),
            _ => {}
        }
    }

    fn observe_control(&mut self, e: &P25Event) {
        match e {
            P25Event::Tsbk { tsbk, .. } => {
                self.tsbks += 1;
                if let TsbkMessage::IdenUpdate(band) = tsbk.message {
                    *self.iden_decodes.entry(band_key(&band)).or_default() += 1;
                }
            }
            P25Event::TsbkFailed => self.tsbks_failed += 1,
            P25Event::Grant {
                frequency_hz,
                talkgroup,
                source,
                update,
                ..
            } => {
                if *update {
                    self.grant_updates += 1;
                } else {
                    self.grants += 1;
                    self.granted_sources
                        .insert((*frequency_hz, *talkgroup, *source));
                }
            }
            P25Event::GrantUnresolvable { .. } => {
                self.grants_unresolvable += 1;
                self.last_unresolvable_ms = self.now_ms;
            }
            _ => {}
        }
        let control_hz = self.control.map_or(0, |c| c.0);
        let now_ms = self.now_ms;
        self.inventory
            .get_or_insert_with(|| SiteInventory::new(CANDIDATES.to_vec()))
            .observe(control_hz, e, now_ms);
    }
}

fn replay(path: &PathBuf) -> (Outcome, Vec<IdenBand>) {
    let mut engine = SiteEngine::new(SiteConfig {
        sample_rate: RATE,
        center_hz: CENTER_HZ,
        control_frequencies: CANDIDATES.to_vec(),
        // Pass the control channel's own events through as well, so the
        // replay can be graded on what was decoded, not only on the calls.
        raw_control: true,
        ..SiteConfig::default()
    })
    .expect("engine builds for a 20 Msps capture");
    assert_eq!(
        engine.candidates().len(),
        CANDIDATES.len(),
        "every candidate is inside the 20 MHz span"
    );

    let file = File::open(path).expect("fixture opens");
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut raw = vec![0u8; 1 << 18];
    let mut wb: Vec<Complex<f32>> = Vec::with_capacity(1 << 17);
    let mut outcome = Outcome::default();
    let mut leftover = 0usize;
    let mut samples = 0u64;
    loop {
        let fresh = reader.read(&mut raw[leftover..]).expect("fixture reads");
        if fresh == 0 {
            break;
        }
        let n = leftover + fresh;
        let usable = n - n % 2;
        wb.clear();
        for pair in raw[..usable].as_chunks::<2>().0 {
            wb.push(Complex::new(
                f32::from(pair[0] as i8) / 128.0,
                f32::from(pair[1] as i8) / 128.0,
            ));
        }
        outcome.now_ms = samples * 1000 / u64::from(RATE);
        samples += wb.len() as u64;
        engine.process_block(&wb, &mut |ev| outcome.observe(&ev));
        raw.copy_within(usable..n, 0);
        leftover = n - usable;
    }
    engine.shutdown(&mut |ev| outcome.observe(&ev));
    (outcome, engine.channel_plan())
}

/// The site's band plan, as its identifier updates announce it (47 decodes
/// of each in the capture, every one identical).
const SITE_BANDS: [BandKey; 2] = [
    // 700 MHz: 769.00625 MHz + 12.5 kHz × n, uplink +30 MHz.
    (0, 769_006_250, 12_500, 30_000_000, 12_500, false),
    // 800 MHz: 851.0125 MHz + 12.5 kHz × n, uplink −45 MHz.
    (1, 851_012_500, 12_500, -45_000_000, 12_500, false),
];

/// Every frequency the capture carries voice on (and so every one the
/// control channel granted).
const TRAFFIC: [u64; 9] = [
    851_787_500,
    852_137_500,
    852_825_000,
    853_112_500,
    854_237_500,
    854_437_500,
    856_712_500,
    858_987_500,
    859_987_500,
];

#[test]
#[cfg_attr(
    trunkd_fixture_missing,
    ignore = "needs fixture *_wb855500k_20M_int8_run2.iq (2.4 GB, gitignored): \
              put it in fixtures/ or set TRUNKD_FIXTURES (and TRUNKD_FIXTURE_NAME), then rebuild — see fixtures/README.md"
)]
fn decodes_the_reference_site_from_a_wideband_capture() {
    let path = fixture_path().unwrap_or_else(|| {
        panic!(
            "fixture {FIXTURE} not found although it was present at build time \
             (set TRUNKD_FIXTURES, see fixtures/README.md)"
        )
    });

    let started_at = std::time::Instant::now();
    let (outcome, plan) = replay(&path);
    let inv = outcome
        .inventory
        .as_ref()
        .expect("the control channel's events reached the inventory")
        .snapshot(outcome.now_ms);
    eprintln!(
        "replayed in {:.1} s: tsbks={} failed={} grants={} updates={} unresolvable={} \
         calls started={} ended={} voice_frames={}",
        started_at.elapsed().as_secs_f64(),
        outcome.tsbks,
        outcome.tsbks_failed,
        outcome.grants,
        outcome.grant_updates,
        outcome.grants_unresolvable,
        outcome.started.len(),
        outcome.ended.len(),
        outcome.voice_frames,
    );

    // Control acquisition: 851.05 MHz, NAC 0x034 — never one of the traffic
    // candidates, even the one carrying a call at capture start.
    assert_eq!(
        outcome.control,
        Some((CONTROL_HZ, NAC)),
        "control channel acquired on 851.05 MHz with NAC 0x034"
    );
    assert_eq!(
        outcome.control_lost, 0,
        "control holds for the whole capture"
    );

    // -- what the control channel decoded -----------------------------------
    // Measured 2026-09-23: 1,579 CRC-valid TSBKs and 10 failed blocks, 28
    // explicit grants and 414 grant updates. The windows leave room for a
    // better demodulator, not for a decode that has broken or double-counts.
    assert!(
        (1_500..=1_700).contains(&outcome.tsbks),
        "CRC-valid TSBKs: {} (1,579 measured)",
        outcome.tsbks
    );
    assert!(
        outcome.tsbks_failed <= 30,
        "failed TSBK blocks: {} (10 measured)",
        outcome.tsbks_failed
    );
    assert!(
        (25..=35).contains(&outcome.grants),
        "explicit grants: {} (28 measured)",
        outcome.grants
    );
    assert!(
        (380..=450).contains(&outcome.grant_updates),
        "grant updates: {} (414 measured)",
        outcome.grant_updates
    );

    // -- the band plan: exactly the site's two bands, and no ghost ----------
    // Every identifier update decoded is one of the two real bands...
    let mut decoded: Vec<BandKey> = outcome.iden_decodes.keys().copied().collect();
    decoded.sort();
    assert_eq!(
        decoded,
        SITE_BANDS.to_vec(),
        "every IDEN decoded is one of the site's two bands: {:?}",
        outcome.iden_decodes
    );
    assert!(
        outcome.iden_decodes.values().all(|&n| n >= 40),
        "each band is re-announced throughout the capture: {:?}",
        outcome.iden_decodes
    );
    // ...the plan grants resolve through is exactly those two (the trunking
    // tracker's plausibility gate and two-decode replacement)...
    let mut resolved: Vec<BandKey> = plan.iter().map(band_key).collect();
    resolved.sort();
    assert_eq!(
        resolved,
        SITE_BANDS.to_vec(),
        "the trunking band plan is the site's"
    );
    // ...and so is the one the inventory publishes, having counted each block
    // once although the recorder feeds it both the `Tsbk` and the tracker's
    // `IdenUpdate` echo.
    let published: Vec<BandKey> = inv
        .bands
        .iter()
        .map(|b| {
            (
                b.id,
                b.base_hz,
                b.spacing_hz,
                b.offset_hz,
                b.bandwidth_hz,
                b.tdma,
            )
        })
        .collect();
    assert_eq!(
        published,
        SITE_BANDS.to_vec(),
        "the inventory publishes the site's band plan and nothing else"
    );
    let plan_known_ms = inv.bands.iter().map(|b| b.first_seen_ms).max();
    // A grant can name a band before its first identifier update (6 do, in
    // the first two seconds); one naming an unknown band afterwards would
    // be a corrupt block the tracker let through.
    assert!(
        outcome.grants_unresolvable == 0
            || plan_known_ms.is_some_and(|t| outcome.last_unresolvable_ms <= t + 1_000),
        "{} unresolvable grants, the last at {} ms, after the plan was known at {:?} ms",
        outcome.grants_unresolvable,
        outcome.last_unresolvable_ms,
        plan_known_ms
    );

    // -- the inventory's identity and traffic tables ------------------------
    assert_eq!(inv.nac, Some(NAC));
    assert_eq!(inv.wacn, Some(284_004), "WACN 0x45564");
    assert_eq!(inv.system_id, Some(0x034));
    assert_eq!((inv.rfss, inv.site), (Some(10), Some(1)));
    let traffic: Vec<u64> = inv.traffic.iter().map(|t| t.hz).collect();
    assert_eq!(
        traffic,
        TRAFFIC.to_vec(),
        "the traffic table is the capture's voice frequencies, no more"
    );
    assert!(
        inv.traffic.iter().all(|t| t.channel.is_some()),
        "every traffic frequency maps back onto the published band plan: {:?}",
        inv.traffic
    );

    // -- calls ---------------------------------------------------------------
    // The capture carries 15 calls, every one closed by a terminator, and
    // none is left open at the end of the file.
    assert_eq!(outcome.started.len(), 15, "calls started");
    assert_eq!(outcome.ended.len(), 15, "calls ended");
    let started_ids: HashSet<CallId> = outcome.started.iter().map(|c| c.0).collect();
    let ended_ids: HashSet<CallId> = outcome.ended_ids.iter().copied().collect();
    assert_eq!(started_ids.len(), 15, "call ids are unique");
    assert_eq!(started_ids, ended_ids, "every call started is ended once");
    assert!(
        outcome.ended.iter().all(|r| r.terminated),
        "every call in the capture ends with a terminator"
    );
    assert!(
        outcome.started.iter().all(|c| TRAFFIC.contains(&c.1)),
        "every call rides one of the capture's voice frequencies"
    );

    // Caller attribution. A call starts with the radio its grant
    // named, or with nobody when only a grant update (which names no radio)
    // was heard — never with the previous caller on that frequency. The
    // capture has the case: 852.825 MHz carries a talkgroup-57151
    // call (radio 5715173), then a talkgroup-52103 call heard first as an
    // update, which used to start attributed to 5715173.
    for &(id, freq, talkgroup, source) in &outcome.started {
        assert!(
            source == 0 || outcome.granted_sources.contains(&(freq, talkgroup, source)),
            "{id:?} on {freq} Hz (talkgroup {talkgroup}) starts with radio {source}, \
             which no grant for that call named"
        );
    }
    let on_852825: Vec<(u16, u32)> = outcome
        .started
        .iter()
        .filter(|c| c.1 == 852_825_000)
        .map(|c| (c.2, c.3))
        .collect();
    assert_eq!(
        on_852825,
        vec![(57151, 5_715_173), (52103, 0)],
        "the second call on 852.825 MHz does not inherit the first one's radio"
    );
    assert!(
        outcome.ended.iter().all(|r| r.talkgroup.is_some()),
        "every ended call carries a talkgroup"
    );
    assert!(
        outcome
            .ended
            .iter()
            .all(|r| r.source.is_some() && r.talkers.len() == 1),
        "every call names exactly the one radio that keyed it (from its link control)"
    );
    assert!(
        outcome.voice_frames > 1_500,
        "expected the capture's voice, got {} frames",
        outcome.voice_frames
    );

    // Clear and encrypted calls are both present and both fully described:
    // P25 encrypts only the IMBE payload, never the signaling.
    let clear: Vec<_> = outcome.ended.iter().filter(|r| !r.encrypted).collect();
    let encrypted: Vec<_> = outcome.ended.iter().filter(|r| r.encrypted).collect();
    assert_eq!(clear.len(), 5, "clear calls (all talkgroup 52103)");
    assert_eq!(encrypted.len(), 10, "encrypted calls");
    assert!(
        clear.iter().all(|r| matches!(r.algid, None | Some(0x80))),
        "a clear call reports ALGID 0x80 (UNENCRYPTED), or none at all when it \
         carried no HDU/ES"
    );
    assert!(
        encrypted.iter().all(|r| r.algorithm() == "AES-256"),
        "the site's encrypted traffic is AES-256"
    );
    assert!(
        outcome.ended.iter().any(|r| r.voice_frames >= 200),
        "at least one long call (the capture's longest is 405 frames)"
    );
    assert!(
        clear.iter().any(|r| r.voice_frames >= 100),
        "at least one substantial clear call (144 frames on 856.7125 MHz)"
    );

    // Talker aliases (L3Harris LC 50–53). Published only once the 14-octet
    // field is known complete: a NUL in the first half, or both
    // halves. Seven complete aliases: six end inside their first half (one of
    // them twice, from the same radio), one arrives with both. The capture's
    // aliases are real unit names, so the source pins the sorted list by
    // digest rather than spelling it out; a mismatch prints what was decoded.
    let mut aliases: Vec<&str> = outcome
        .ended
        .iter()
        .filter_map(|r| r.talker_alias.as_deref())
        .collect();
    aliases.sort_unstable();
    assert_eq!(
        aliases.len(),
        7,
        "the complete aliases, each on its own call"
    );
    assert_eq!(
        fnv1a64(aliases.join("\n").as_bytes()),
        0xd27c_c63a_72f9_3c13,
        "the complete aliases, each on its own call: decoded {aliases:?}"
    );
    // The clear talkgroup-52103 call on 852.825 MHz (ALGID 0x80) carries only
    // the first half of its alias — seven octets with no NUL — so the field
    // may continue and nothing is published. This assertion used to expect
    // that half-alias; it went unchecked while the test skipped in debug
    // builds, and the decoder was later changed on purpose to stop publishing
    // half a field.
    //
    // The engine's per-radio alias memory names such a head when the
    // same radio was heard earlier with the complete alias — but this 60 s
    // capture never carries this radio's complete alias, so there is nothing
    // to recall and the call stays unnamed (the recall itself is covered by
    // `site::tests::a_head_alone_is_completed_from_the_radios_known_alias`).
    let head_only = outcome
        .ended_ids
        .iter()
        .zip(&outcome.ended)
        .filter(|(id, _)| outcome.freq_of.get(id) == Some(&852_825_000))
        .map(|(_, r)| r)
        .find(|r| !r.encrypted)
        .expect("the clear call on 852.825 MHz");
    assert_eq!(head_only.talkgroup, Some(52103));
    assert_eq!(head_only.algid, Some(0x80));
    assert_eq!(
        head_only.talker_alias, None,
        "half an alias field is never published"
    );
    assert!(
        outcome
            .ended
            .iter()
            .all(|r| r.source != head_only.source || r.talker_alias.is_none()),
        "the capture never names this radio completely, so memory has nothing to recall"
    );

    // Voice-frame parity against `channelizer_decode` on the same frequencies:
    // the engine subscribes on a grant, so the only loss is the head of a call
    // already in progress when the control channel was acquired (854.4375 MHz:
    // 468 of 495 frames, i.e. the first 540 ms).
    let expected: [(u64, u64); 9] = [
        (851_787_500, 54),
        (852_137_500, 162),
        (852_825_000, 342),
        (853_112_500, 207),
        (854_237_500, 108),
        (854_437_500, 495),
        (856_712_500, 225),
        (858_987_500, 81),
        (859_987_500, 90),
    ];
    for (freq, reference) in expected {
        let got = outcome.voice_by_freq.get(&freq).copied().unwrap_or(0);
        assert!(
            got * 100 >= reference * 90,
            "{freq} Hz: {got} voice frames vs {reference} from channelizer_decode"
        );
        assert!(
            got <= reference,
            "{freq} Hz: {got} voice frames exceeds the {reference} the DDC path sees"
        );
    }
}
