// SPDX-License-Identifier: GPL-3.0-or-later
//! The trunk-recorder-style JSON sidecar.
//!
//! Keyup's file ingest (`app.ingest.tr_parser`) already reads this shape, and
//! it treats the sidecar as authoritative over the filename and the audio
//! header — true start/stop to the millisecond, the emergency and encrypted
//! flags, the patch members and every radio that keyed up. That is why the
//! offline spool writes one next to each recording: a spooled call that is
//! later swept up by the file path arrives with exactly the metadata the push
//! path would have carried.
//!
//! Field names follow trunk-recorder, including its two quirks Keyup depends
//! on: `talkgroup_group_tag` is the *alpha tag* (the display name), and
//! timestamps come in both seconds (`start_time`) and milliseconds
//! (`start_time_ms`), the millisecond pair winning.
//!
//! trunkd's own additions — `recorder`, `site`, `systemLabel`, `nac`,
//! `algorithm`, `keyId`, `talkerAlias`, `sourceWacn`, `sourceSystem`,
//! `sourceQuality`, `speechS`, `source` — are extra keys that
//! trunk-recorder never writes; Keyup keeps unknown keys in `raw_meta`, and
//! `trunkd-uplink` reads them back to rebuild a spooled call for retry.

use serde_json::{json, Map, Value};

use crate::call::{CallKind, FinishedCall};

/// Render a call as a trunk-recorder-style JSON sidecar (pretty-printed).
pub fn sidecar_json(call: &FinishedCall) -> String {
    let duration = call.duration_s();
    let start_s = call.started_at_ms.div_euclid(1000);
    let stop_s = call.ended_at_ms.div_euclid(1000);

    let src_list: Vec<Value> = call
        .sources
        .iter()
        .map(|s| {
            json!({
                "src": s.src,
                "time": start_s + s.pos_s as i64,
                "pos": round3(s.pos_s),
                "emergency": u8::from(s.emergency),
                "signal_system": "",
                "tag": s.tag.clone().unwrap_or_default(),
            })
        })
        .collect();

    let freq_list: Vec<Value> = call
        .frequencies
        .iter()
        .map(|f| {
            json!({
                "freq": f.freq_hz,
                "time": start_s + f.pos_s as i64,
                "pos": round3(f.pos_s),
                "len": round3(f.len_s),
                "error_count": f.error_count,
                "spike_count": f.spike_count,
            })
        })
        .collect();

    let mut root = json!({
        // --- trunk-recorder's own fields ---
        "freq": call.frequency_hz,
        "start_time": start_s,
        "stop_time": stop_s,
        "call_length": round3(duration),
        "talkgroup": call.talkgroup,
        "emergency": u8::from(call.emergency),
        "encrypted": u8::from(call.encrypted),
        "audio_type": "digital",
        "short_name": call.system,
        "patched_talkgroups": call.patches,
        "srcList": src_list,
        "freqList": freq_list,
        // --- trunkd extras, kept in Keyup's raw_meta ---
        "recorder": "trunkd",
        "site": call.site,
        "systemLabel": call.system,
        "start_time_ms": call.started_at_ms,
        "stop_time_ms": call.ended_at_ms,
        "call_length_ms": (duration * 1000.0).round() as i64,
        // How much of this call was loud enough to be speech (`speech_seconds`).
        // Written so a spooled call is retried — and pushed to Keyup — with the
        // number the live attempt measured, rather than with a zero that would
        // read as "silent" to anything cross-checking it.
        "speechS": round3(f64::from(call.speech_s)),
    });

    let obj: &mut Map<String, Value> = root.as_object_mut().expect("json! built an object");
    if let Some(label) = &call.talkgroup_label {
        // The alpha tag Keyup shows: `talkgroup_group_tag`, not `talkgroup_tag`.
        obj.insert("talkgroup_group_tag".into(), json!(label));
    }
    if let Some(src) = call.source {
        obj.insert("source".into(), json!(src));
    }
    if let Some(alias) = &call.talker_alias {
        obj.insert("talkerAlias".into(), json!(alias));
    }
    // A unit-to-unit call: written so a spooled private call is retried as the
    // private call it was rather than as a group call on talkgroup 0. Omitted
    // for a group call, whose sidecar is byte-for-byte what it always was.
    if call.call_kind == CallKind::Unit {
        obj.insert("call_kind".into(), json!(CallKind::Unit.as_str()));
        if let Some(target) = call.target_radio {
            obj.insert("target_radio_id".into(), json!(target));
        }
    }
    // Written so a spooled call keeps its roaming talker's identity across a
    // retry: `call_from_sidecar` reads both keys back.
    if let Some(wacn) = call.source_wacn {
        obj.insert("sourceWacn".into(), json!(wacn));
    }
    if let Some(system) = call.source_system {
        obj.insert("sourceSystem".into(), json!(system));
    }
    if let Some(nac) = call.nac {
        obj.insert("nac".into(), json!(nac));
    }
    if let Some(alg) = &call.algorithm {
        obj.insert("algorithm".into(), json!(alg));
    }
    if let Some(key_id) = call.key_id {
        obj.insert("keyId".into(), json!(key_id));
    }
    // The spooled copy of the upload's `sourceQuality`: a call that waits out an
    // outage on disk must be re-offered with the same quality facts it would
    // have carried live, or a later best-source negotiation would rank the
    // retry below a rival copy for no reason but the delay.
    if let Some(q) = call.source_quality.as_ref().filter(|q| !q.is_empty()) {
        if let Ok(v) = serde_json::to_value(q) {
            obj.insert("sourceQuality".into(), v);
        }
    }

    // The Broadcastify verdict for this call, written so a spooled retry is
    // pushed to Keyup with the outcome the live attempt already produced —
    // and so nothing re-registers the call with Broadcastify on the way out of
    // the spool. `call_from_sidecar` reads it straight back.
    if let Some(b) = &call.broadcastify {
        if let Ok(v) = serde_json::to_value(b) {
            obj.insert("broadcastify".into(), v);
        }
    }

    serde_json::to_string_pretty(&root).expect("a JSON object of scalars always serializes")
}

/// Round to milliseconds; sidecar positions are never finer than that.
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::CallBuilder;

    fn sample_call() -> FinishedCall {
        let mut b = CallBuilder::new(
            "Example Network",
            "Site 1",
            1_788_012_000_123,
            852_137_500,
            52101,
        );
        b.set_talkgroup_label(Some("PD DISPATCH".into()));
        b.set_nac(Some(0x293));
        b.set_patches(vec![19282, 50051]);
        b.set_talker_alias(Some("ENGINE 3".into()));
        b.set_source_suid(Some(284_004), Some(57));
        b.talker(9_000_011, 1_788_012_000_123, false, Some("UNIT_313".into()));
        b.push_pcm(&[0i16; 8000]);
        b.talker(9_000_012, 1_788_012_001_123, true, None);
        b.push_pcm(&[0i16; 8000]);
        b.add_errors(4);
        b.finish(1_788_012_002_123)
    }

    #[test]
    fn sidecar_carries_the_trunk_recorder_fields() {
        let json: Value = serde_json::from_str(&sidecar_json(&sample_call())).expect("valid json");
        assert_eq!(json["freq"], 852_137_500u64);
        assert_eq!(json["start_time"], 1_788_012_000i64);
        assert_eq!(json["stop_time"], 1_788_012_002i64);
        assert_eq!(json["start_time_ms"], 1_788_012_000_123i64);
        assert_eq!(json["stop_time_ms"], 1_788_012_002_123i64);
        assert_eq!(json["call_length"], 2.0);
        assert_eq!(json["call_length_ms"], 2000i64);
        assert_eq!(json["talkgroup"], 52101);
        assert_eq!(json["talkgroup_group_tag"], "PD DISPATCH");
        assert_eq!(json["short_name"], "Example Network");
        assert_eq!(json["audio_type"], "digital");
        assert_eq!(json["emergency"], 1); // latched by the second talker
        assert_eq!(json["encrypted"], 0);
        assert_eq!(json["patched_talkgroups"], json!([19282, 50051]));
        assert_eq!(json["recorder"], "trunkd");
        assert_eq!(json["site"], "Site 1");
        assert_eq!(json["nac"], 659);
        assert_eq!(json["talkerAlias"], "ENGINE 3");
        assert_eq!(json["source"], 9_000_011u64);
    }

    #[test]
    fn src_list_matches_keyups_parser() {
        let json: Value = serde_json::from_str(&sidecar_json(&sample_call())).expect("valid json");
        let srcs = json["srcList"].as_array().expect("srcList");
        assert_eq!(srcs.len(), 2);
        assert_eq!(srcs[0]["src"], 9_000_011u64);
        assert_eq!(srcs[0]["pos"], 0.0);
        assert_eq!(srcs[0]["time"], 1_788_012_000i64);
        assert_eq!(srcs[0]["tag"], "UNIT_313");
        assert_eq!(srcs[0]["emergency"], 0);
        assert_eq!(srcs[1]["pos"], 1.0);
        assert_eq!(srcs[1]["time"], 1_788_012_001i64);
        assert_eq!(srcs[1]["emergency"], 1);
        assert_eq!(srcs[1]["tag"], "");
    }

    #[test]
    fn freq_list_carries_the_error_counters() {
        let json: Value = serde_json::from_str(&sidecar_json(&sample_call())).expect("valid json");
        let freqs = json["freqList"].as_array().expect("freqList");
        assert_eq!(freqs.len(), 1);
        assert_eq!(freqs[0]["freq"], 852_137_500u64);
        assert_eq!(freqs[0]["pos"], 0.0);
        assert_eq!(freqs[0]["len"], 2.0);
        assert_eq!(freqs[0]["error_count"], 4);
        assert_eq!(freqs[0]["spike_count"], 0);
    }

    #[test]
    fn a_roaming_talkers_suid_is_written() {
        let json: Value = serde_json::from_str(&sidecar_json(&sample_call())).expect("valid json");
        assert_eq!(json["sourceWacn"], 284_004);
        assert_eq!(json["sourceSystem"], 57);
    }

    #[test]
    fn a_local_talker_carries_no_suid() {
        // `set_source_suid(None, _)` must not clear a value already latched,
        // and must not invent one where the air carried none.
        let mut b = CallBuilder::new("S", "T", 1_788_012_000_000, 1, 2);
        b.set_source_suid(Some(284_004), Some(57));
        b.set_source_suid(None, None);
        let call = b.finish(1_788_012_001_000);
        assert_eq!(call.source_wacn, Some(284_004));
        assert_eq!(call.source_system, Some(57));
    }

    #[test]
    fn absent_optionals_are_omitted_not_null() {
        let call = CallBuilder::new("S", "T", 1_788_012_000_000, 1, 2).finish(1_788_012_001_000);
        let json: Value = serde_json::from_str(&sidecar_json(&call)).expect("valid json");
        let obj = json.as_object().expect("object");
        for key in [
            "talkgroup_group_tag",
            "source",
            "talkerAlias",
            "sourceWacn",
            "sourceSystem",
            "nac",
            "algorithm",
            "keyId",
            "sourceQuality",
        ] {
            assert!(!obj.contains_key(key), "{key} should be omitted");
        }
    }

    #[test]
    fn source_quality_rides_the_sidecar_so_a_retry_keeps_it() {
        let mut b = CallBuilder::new("S", "T", 1_788_012_000_000, 1, 2);
        b.set_instance(Some("rec-north-01".into()));
        b.set_rssi_dbfs(Some(-38.4));
        for i in 0..50 {
            b.push_voice_frame(&[0i16; 160], i % 10 == 0);
        }
        let json: Value =
            serde_json::from_str(&sidecar_json(&b.finish(1_788_012_001_000))).expect("valid json");
        assert_eq!(json["sourceQuality"]["instance"], "rec-north-01");
        assert_eq!(json["sourceQuality"]["voice_frames"], 50);
        assert_eq!(json["sourceQuality"]["bad_frames"], 5);
        assert_eq!(json["sourceQuality"]["rssi_dbfs"], -38.4);
    }

    #[test]
    fn encrypted_call_reports_its_algorithm() {
        let mut b = CallBuilder::new("S", "T", 1_788_012_000_000, 851_000_000, 7);
        b.set_encryption(Some("AES-256".into()), Some(42));
        let json: Value =
            serde_json::from_str(&sidecar_json(&b.finish(1_788_012_003_000))).expect("valid json");
        assert_eq!(json["encrypted"], 1);
        assert_eq!(json["algorithm"], "AES-256");
        assert_eq!(json["keyId"], 42);
        assert_eq!(json["call_length"], 3.0);
    }
}
