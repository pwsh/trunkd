// SPDX-License-Identifier: GPL-3.0-or-later
//! The `multipart/form-data` body Keyup's `POST /api/call-upload` expects.
//!
//! Hand-built rather than pulled from a crate: the body is a dozen short text
//! parts and one file part, the field set is a contract we want to read at a
//! glance, and building it by hand keeps the dependency list (and the number of
//! copies of the audio) down.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use trunkd_call::{AudioFormat, CallKind, FinishedCall};

/// Build the body for one call.
///
/// Returns the `Content-Type` header value (which carries the boundary) and the
/// body bytes.
///
/// `audio` is the encoded call audio and `format` describes it; pass `None` for
/// a metadata-only (encrypted) call, which sends `audioName`/`audioType` and
/// `encrypted=1` but **no** `audio` part.
///
/// The field set, in the order it goes on the wire:
///
/// | field | value |
/// |---|---|
/// | `key` | the API token |
/// | `system` | the configured numeric system id (Keyup ignores it) |
/// | `systemLabel` | [`FinishedCall::system`] — what Keyup actually files under |
/// | `dateTime` | call start, unix **seconds** |
/// | `startMs` | call start, unix **milliseconds** — what reconciliation matches on |
/// | `endMs` | call end, unix milliseconds |
/// | `talkgroup` | talkgroup id — **group calls only** |
/// | `talkgroupLabel` | when known — group calls only |
/// | `call_kind` | `unit`, on a unit-to-unit call only; absent means `group` |
/// | `target_radio_id` | the called radio, decimal — unit-to-unit calls only |
/// | `frequency` | hertz |
/// | `source` | first radio id, when known |
/// | `sources` | JSON `[{"pos":…,"src":…,"emergency":…,"tag":…}]` |
/// | `frequencies` | JSON `[{"freq":…,"pos":…,"len":…,"error_count":…,"spike_count":…}]` |
/// | `patches` | JSON bare member list, only when the call is a patch |
/// | `emergency` / `encrypted` | `0` or `1` |
/// | `talkerAlias` | when received over the air |
/// | `sourceWacn`, `sourceSystem` | when a Source ID Extension qualified the talker |
/// | `audioName` | the filename, also used as the file part's `filename=` |
/// | `audioType` | `audio/mpeg` or `audio/wav` — Keyup stores by this |
/// | `recorder` | `trunkd` |
/// | `site` | site label |
/// | `nac`, `algorithm`, `keyId` | when known |
/// | `sourceQuality` | JSON `{"instance":…,"voice_frames":…,"bad_frames":…,"rssi_dbfs":…}`, when this recorder has something to say |
/// | `broadcastifyOutcome` | `uploaded` \| `duplicate` \| `failed` \| `late` \| `claim_denied` \| `skipped` — only when direct-to-Broadcastify decided something |
/// | `broadcastifyCode` | the stage-1 response code, when one was parsed |
/// | `broadcastifyResponse` | the stage-1 response line (or the local reason) |
/// | `broadcastifyRemoteId` | Broadcastify's call id — `uploaded` only |
/// | `broadcastifyHolder` | the instance holding the claim — `claim_denied` only |
/// | `speechS` | seconds of audio above −40 dBFS, two decimals |
/// | `durationMs` | call length in milliseconds |
/// | `audio` | the file part, omitted for an encrypted call |
///
/// `sources`/`frequencies` are also the flag that tells Keyup it is talking to
/// a trunk-recorder-dialect uploader, which is what makes `patches` a bare
/// member list rather than sdrtrunk's supergroup-first list.
pub fn build_multipart(
    call: &FinishedCall,
    key: &str,
    system_id: u32,
    audio: Option<&[u8]>,
    format: AudioFormat,
) -> (String, Vec<u8>) {
    build_multipart_with_priority(call, key, system_id, audio, format, "live")
}

/// [`build_multipart`] with an explicit ingestion `priority` class
/// (`urgent` | `live` | `backlog` | `bulk` or an integer). Keyup queues the
/// call's transcription by it and applies flow control to the class.
pub fn build_multipart_with_priority(
    call: &FinishedCall,
    key: &str,
    system_id: u32,
    audio: Option<&[u8]>,
    format: AudioFormat,
    priority: &str,
) -> (String, Vec<u8>) {
    let boundary = new_boundary();
    let audio_name = call.audio_name(format);
    let duration_ms = (call.duration_s() * 1000.0).round() as i64;

    let sources = json!(call
        .sources
        .iter()
        .map(|s| {
            let mut o = serde_json::Map::new();
            o.insert("pos".into(), json!(round3(s.pos_s)));
            o.insert("src".into(), json!(s.src));
            o.insert("emergency".into(), json!(s.emergency));
            if let Some(tag) = &s.tag {
                o.insert("tag".into(), json!(tag));
            }
            serde_json::Value::Object(o)
        })
        .collect::<Vec<_>>());

    let frequencies = json!(call
        .frequencies
        .iter()
        .map(|f| json!({
            "freq": f.freq_hz,
            "pos": round3(f.pos_s),
            "len": round3(f.len_s),
            "error_count": f.error_count,
            "spike_count": f.spike_count,
        }))
        .collect::<Vec<_>>());

    let mut body: Vec<u8> = Vec::with_capacity(1024 + audio.map_or(0, <[u8]>::len));
    let mut field = |name: &str, value: &str| text_part(&mut body, &boundary, name, value);

    field("key", key);
    field("system", &system_id.to_string());
    field("systemLabel", &call.system);
    field("dateTime", &call.started_at_ms.div_euclid(1000).to_string());
    // The same instant at full resolution, and the call's end. `dateTime` is
    // whole seconds, so two recorders' copies of a sub-second call can land in
    // different seconds and never reconcile; these give Keyup the millisecond
    // edges to match on. Additive — `dateTime` stays exactly as it was, since
    // older Keyup builds and the Broadcastify `ts` still derive from it.
    field("startMs", &call.started_at_ms.to_string());
    field("endMs", &call.ended_at_ms.to_string());
    // A unit-to-unit call has no talkgroup and must not invent one: it is filed
    // under the pair of radios. `call_kind` + `target_radio_id` is the contract
    // Keyup grew for it; a Keyup that predates them ignores both, which is why
    // they are additive rather than a new route. A group call's wire format is
    // untouched, so `call_kind`'s absence still means "group".
    match call.call_kind {
        CallKind::Group => {
            field("talkgroup", &call.talkgroup.to_string());
            if let Some(label) = &call.talkgroup_label {
                field("talkgroupLabel", label);
            }
        }
        CallKind::Unit => {
            field("call_kind", CallKind::Unit.as_str());
            if let Some(target) = call.target_radio {
                field("target_radio_id", &target.to_string());
            }
        }
    }
    field("frequency", &call.frequency_hz.to_string());
    if let Some(src) = call.source {
        field("source", &src.to_string());
    }
    field("sources", &sources.to_string());
    field("frequencies", &frequencies.to_string());
    if !call.patches.is_empty() {
        field("patches", &json!(call.patches).to_string());
    }
    field("emergency", flag(call.emergency));
    field("encrypted", flag(call.encrypted));
    if let Some(alias) = &call.talker_alias {
        field("talkerAlias", alias);
    }
    // The fully qualified SUID of the talker, when the air carried one. Keyup's
    // `radios` table has `wacn` / `home_system_id` columns waiting for these;
    // they are sent only for a roaming radio, so their absence means "local".
    if let Some(wacn) = call.source_wacn {
        field("sourceWacn", &wacn.to_string());
    }
    if let Some(system) = call.source_system {
        field("sourceSystem", &system.to_string());
    }
    field("audioName", &audio_name);
    field("audioType", format.mime());
    // Extras Keyup keeps verbatim in `raw_meta`.
    field("recorder", "trunkd");
    field("priority", priority);
    field("site", &call.site);
    if let Some(nac) = call.nac {
        field("nac", &nac.to_string());
    }
    if let Some(alg) = &call.algorithm {
        field("algorithm", alg);
    }
    if let Some(key_id) = call.key_id {
        field("keyId", &key_id.to_string());
    }
    // Which recorder heard this, and how well (see `SourceQuality`). Additive
    // and optional in both directions: a receiver that does not know the field
    // ignores it, and a recorder with nothing to report omits it, so neither
    // side has to be upgraded before the other.
    if let Some(q) = call.source_quality.as_ref().filter(|q| !q.is_empty()) {
        if let Ok(json) = serde_json::to_string(q) {
            field("sourceQuality", &json);
        }
    }
    // What direct-to-Broadcastify delivery did with this call, decided on this
    // thread a moment ago (or read back from the spool sidecar). Every field is
    // omitted when the feature never looked at the call, so a Keyup that does
    // not know them sees exactly the body it always saw.
    if let Some(b) = &call.broadcastify {
        field("broadcastifyOutcome", &b.outcome);
        if let Some(code) = b.code {
            field("broadcastifyCode", &code.to_string());
        }
        if let Some(resp) = &b.response {
            field("broadcastifyResponse", resp);
        }
        if let Some(id) = &b.remote_id {
            field("broadcastifyRemoteId", id);
        }
        if let Some(holder) = &b.holder {
            field("broadcastifyHolder", holder);
        }
    }
    // Seconds of audio above -40 dBFS (`speech_seconds`), measured when the
    // call was assembled. Keyup classifies the call itself and may use this as
    // a cross-check; a Keyup that does not know the field ignores it.
    field("speechS", &format!("{:.2}", call.speech_s));
    field("durationMs", &duration_ms.to_string());

    if let Some(bytes) = audio {
        file_part(
            &mut body,
            &boundary,
            "audio",
            &audio_name,
            format.mime(),
            bytes,
        );
    }

    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"--\r\n");

    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// Build a general `multipart/form-data` body: text fields in the order given,
/// then at most one file part.
///
/// The same wire format [`build_multipart`] produces, with the call-specific
/// field set lifted out — so a second uploader (the event-log archive) shares
/// one boundary generator, one escaping rule and one set of CRLF details
/// rather than growing a near-copy of them.
///
/// Returns the `Content-Type` header value (which carries the boundary) and the
/// body bytes. `file` is `(field name, filename, mime, bytes)`.
pub fn build_form(
    fields: &[(&str, &str)],
    file: Option<(&str, &str, &str, &[u8])>,
) -> (String, Vec<u8>) {
    let boundary = new_boundary();
    let mut body: Vec<u8> = Vec::with_capacity(512 + file.map_or(0, |f| f.3.len()));
    for (name, value) in fields {
        text_part(&mut body, &boundary, name, value);
    }
    if let Some((field, filename, mime, bytes)) = file {
        file_part(&mut body, &boundary, field, filename, mime, bytes);
    }
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"--\r\n");
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn flag(v: bool) -> &'static str {
    if v {
        "1"
    } else {
        "0"
    }
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn text_part(body: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
    body.extend_from_slice(escape(name).as_bytes());
    body.extend_from_slice(b"\"\r\n\r\n");
    body.extend_from_slice(value.as_bytes());
    body.extend_from_slice(b"\r\n");
}

fn file_part(
    body: &mut Vec<u8>,
    boundary: &str,
    field: &str,
    filename: &str,
    mime: &str,
    bytes: &[u8],
) {
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"");
    body.extend_from_slice(escape(field).as_bytes());
    body.extend_from_slice(b"\"; filename=\"");
    body.extend_from_slice(escape(filename).as_bytes());
    body.extend_from_slice(b"\"\r\nContent-Type: ");
    body.extend_from_slice(mime.as_bytes());
    body.extend_from_slice(b"\r\n\r\n");
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
}

/// Quotes and CR/LF cannot appear inside a `Content-Disposition` parameter.
///
/// Call names are already sanitized, but a talkgroup label comes from config,
/// so nothing that reaches a header goes unescaped.
fn escape(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '\r' && *c != '\n')
        .map(|c| if c == '"' { '\'' } else { c })
        .collect()
}

/// A boundary no body can contain by accident: a fixed prefix, the wall clock,
/// and a process-wide counter.
fn new_boundary() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("----trunkd{nanos:016x}{n:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use trunkd_call::CallBuilder;

    fn call() -> FinishedCall {
        let mut b = CallBuilder::new(
            "Example Network",
            "Site 1",
            1_788_012_000_123,
            852_137_500,
            52101,
        );
        b.set_talkgroup_label(Some("PD DISPATCH".into()));
        b.set_patches(vec![19282, 50051]);
        b.set_nac(Some(659));
        b.set_source_suid(Some(781_824), Some(57));
        b.set_talker_alias(Some("ENGINE 3".into()));
        b.talker(9_000_011, 1_788_012_000_123, false, Some("UNIT_313".into()));
        // Half a second of talk, then silence: 0.50 s of speech in a 1 s call.
        b.push_pcm(
            &(0..4000)
                .map(|i| if i % 2 == 0 { 4000 } else { -4000 })
                .collect::<Vec<i16>>(),
        );
        b.push_pcm(&[0i16; 4000]);
        b.finish(1_788_012_001_123)
    }

    /// The text fields of a body, as `(name, value)` pairs.
    type Fields = Vec<(String, String)>;
    /// The audio part of a body, as `(filename, byte length)`.
    type FilePart = Option<(String, usize)>;

    /// Split a body into its text parts and its audio part.
    fn parse(ct: &str, body: &[u8]) -> (Fields, FilePart) {
        let boundary = ct.split("boundary=").nth(1).expect("boundary").to_owned();
        let sep = format!("--{boundary}");
        let text = String::from_utf8_lossy(body).into_owned();
        let mut fields = Vec::new();
        let mut file = None;
        for chunk in text.split(&sep) {
            let chunk = chunk.trim_start_matches("\r\n");
            if chunk.is_empty() || chunk.starts_with("--") {
                continue;
            }
            let (head, value) = chunk.split_once("\r\n\r\n").expect("part head");
            let value = value.strip_suffix("\r\n").unwrap_or(value);
            let name = head
                .split("name=\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
                .expect("name")
                .to_owned();
            if let Some(fname) = head
                .split("filename=\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
            {
                file = Some((fname.to_owned(), value.len()));
            } else {
                fields.push((name, value.to_owned()));
            }
        }
        (fields, file)
    }

    fn get<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
        fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn sends_every_contract_field() {
        let call = call();
        let (ct, body) = build_multipart(
            &call,
            "kup_test",
            7,
            Some(b"AUDIOBYTES"),
            AudioFormat::default(),
        );
        assert!(ct.starts_with("multipart/form-data; boundary=----trunkd"));
        let (fields, file) = parse(&ct, &body);

        assert_eq!(get(&fields, "key"), Some("kup_test"));
        assert_eq!(get(&fields, "system"), Some("7"));
        assert_eq!(get(&fields, "systemLabel"), Some("Example Network"));
        assert_eq!(get(&fields, "dateTime"), Some("1788012000"));
        // Millisecond edges alongside the truncated second: same instant, full
        // resolution, so a sub-second call cannot straddle a second boundary
        // between two recorders.
        assert_eq!(get(&fields, "startMs"), Some("1788012000123"));
        assert_eq!(get(&fields, "endMs"), Some("1788012001123"));
        assert_eq!(get(&fields, "talkgroup"), Some("52101"));
        assert_eq!(get(&fields, "talkgroupLabel"), Some("PD DISPATCH"));
        assert_eq!(get(&fields, "frequency"), Some("852137500"));
        assert_eq!(get(&fields, "source"), Some("9000011"));
        assert_eq!(
            get(&fields, "sources"),
            Some(r#"[{"emergency":false,"pos":0.0,"src":9000011,"tag":"UNIT_313"}]"#)
        );
        assert_eq!(
            get(&fields, "frequencies"),
            Some(r#"[{"error_count":0,"freq":852137500,"len":1.0,"pos":0.0,"spike_count":0}]"#)
        );
        assert_eq!(get(&fields, "patches"), Some("[19282,50051]"));
        assert_eq!(get(&fields, "emergency"), Some("0"));
        assert_eq!(get(&fields, "encrypted"), Some("0"));
        assert_eq!(get(&fields, "talkerAlias"), Some("ENGINE 3"));
        assert_eq!(
            get(&fields, "audioName"),
            Some("Example-Network_52101_1788012000123.mp3")
        );
        assert_eq!(get(&fields, "audioType"), Some("audio/mpeg"));
        assert_eq!(get(&fields, "recorder"), Some("trunkd"));
        assert_eq!(get(&fields, "priority"), Some("live"));
        assert_eq!(get(&fields, "site"), Some("Site 1"));
        assert_eq!(get(&fields, "nac"), Some("659"));
        assert_eq!(get(&fields, "sourceWacn"), Some("781824"));
        assert_eq!(get(&fields, "sourceSystem"), Some("57"));
        assert_eq!(get(&fields, "durationMs"), Some("1000"));
        // Two decimals, always sent: what the recorder measured of this call
        // (0.5 s of the 1 s above the -40 dBFS floor), for Keyup to cross-check
        // its own classification against.
        assert_eq!(get(&fields, "speechS"), Some("0.50"));
        assert_eq!(
            file,
            Some(("Example-Network_52101_1788012000123.mp3".to_owned(), 10))
        );
    }

    /// The whole contract for a private call: `call_kind=unit` and a decimal
    /// `target_radio_id`, **no** `talkgroup` and no `talkgroupLabel`, the
    /// talker in `source`, and a filename that names the pair instead of a
    /// group id. Everything else is the group contract, unchanged.
    #[test]
    fn a_call_with_nothing_but_a_click_still_reports_its_speech_seconds() {
        let mut b = CallBuilder::new(
            "Example Network",
            "Site 1",
            1_788_012_000_123,
            852_137_500,
            52101,
        );
        b.talker(9_000_011, 1_788_012_000_123, false, None);
        let mut pcm = vec![0i16; 4000];
        for (i, s) in pcm[..240].iter_mut().enumerate() {
            *s = if i % 2 == 0 { 4000 } else { -4000 };
        }
        b.push_pcm(&pcm);
        let call = b.finish(1_788_012_000_623);
        let (ct, body) = build_multipart(&call, "k", 1, Some(b"MP3"), AudioFormat::default());
        let (fields, _) = parse(&ct, &body);
        assert_eq!(get(&fields, "speechS"), Some("0.04"));
    }

    #[test]
    fn a_unit_call_sends_the_pair_and_no_talkgroup() {
        let mut b = CallBuilder::new(
            "Example Network",
            "Site 1",
            1_788_012_000_123,
            851_125_000,
            0,
        );
        b.set_unit_call(9_000_007);
        b.talker(9_000_001, 1_788_012_000_123, false, None);
        b.push_pcm(&[0i16; 8000]);
        let call = b.finish(1_788_012_001_123);

        let (ct, body) = build_multipart(&call, "k", 1, Some(b"MP3"), AudioFormat::default());
        let (fields, file) = parse(&ct, &body);
        assert_eq!(get(&fields, "call_kind"), Some("unit"));
        assert_eq!(get(&fields, "target_radio_id"), Some("9000007"));
        assert_eq!(get(&fields, "source"), Some("9000001"));
        assert_eq!(
            get(&fields, "talkgroup"),
            None,
            "a private call has no talkgroup and must not invent one"
        );
        assert_eq!(get(&fields, "talkgroupLabel"), None);
        let name = "Example-Network_UNIT_9000001_TO_9000007_1788012000123.mp3";
        assert_eq!(get(&fields, "audioName"), Some(name));
        assert_eq!(file.expect("file part").0, name);
        // The rest of the contract is untouched.
        assert_eq!(get(&fields, "systemLabel"), Some("Example Network"));
        assert_eq!(get(&fields, "frequency"), Some("851125000"));
        assert_eq!(get(&fields, "durationMs"), Some("1000"));
        assert_eq!(get(&fields, "recorder"), Some("trunkd"));
    }

    /// The Broadcastify verdict rides along on the same call push, and every
    /// field of it is absent when the feature decided nothing.
    #[test]
    fn the_keyup_push_carries_the_broadcastify_outcome() {
        let mut call = call();
        let (ct, body) = build_multipart(&call, "k", 1, Some(b"MP3"), AudioFormat::default());
        assert_eq!(
            get(&parse(&ct, &body).0, "broadcastifyOutcome"),
            None,
            "a call the feature never looked at carries nothing"
        );

        call.broadcastify = Some(trunkd_call::BroadcastifyOutcome {
            outcome: "uploaded".into(),
            code: Some(0),
            response: None,
            remote_id: Some("abc123".into()),
            holder: None,
        });
        let (ct, body) = build_multipart(&call, "k", 1, Some(b"MP3"), AudioFormat::default());
        let (fields, _) = parse(&ct, &body);
        assert_eq!(get(&fields, "broadcastifyOutcome"), Some("uploaded"));
        assert_eq!(get(&fields, "broadcastifyCode"), Some("0"));
        assert_eq!(get(&fields, "broadcastifyRemoteId"), Some("abc123"));
        assert_eq!(get(&fields, "broadcastifyResponse"), None);
        assert_eq!(get(&fields, "broadcastifyHolder"), None);

        call.broadcastify = Some(trunkd_call::BroadcastifyOutcome {
            outcome: "claim_denied".into(),
            code: None,
            response: Some("claim held by rec-south-02".into()),
            remote_id: None,
            holder: Some("rec-south-02".into()),
        });
        let (ct, body) = build_multipart(&call, "k", 1, Some(b"MP3"), AudioFormat::default());
        let (fields, _) = parse(&ct, &body);
        assert_eq!(get(&fields, "broadcastifyOutcome"), Some("claim_denied"));
        assert_eq!(get(&fields, "broadcastifyCode"), None);
        assert_eq!(
            get(&fields, "broadcastifyResponse"),
            Some("claim held by rec-south-02")
        );
        assert_eq!(get(&fields, "broadcastifyHolder"), Some("rec-south-02"));
    }

    /// A group call's body is exactly what it always was: no `call_kind`, no
    /// `target_radio_id`. Absence is what tells an older Keyup nothing changed.
    #[test]
    fn a_group_call_carries_neither_new_field() {
        let (ct, body) = build_multipart(&call(), "k", 1, Some(b"MP3"), AudioFormat::default());
        let (fields, _) = parse(&ct, &body);
        assert_eq!(get(&fields, "talkgroup"), Some("52101"));
        assert_eq!(get(&fields, "call_kind"), None);
        assert_eq!(get(&fields, "target_radio_id"), None);
    }

    #[test]
    fn wav_format_changes_the_name_and_type() {
        let (ct, body) = build_multipart(&call(), "k", 1, Some(b"RIFF"), AudioFormat::Wav);
        let (fields, file) = parse(&ct, &body);
        assert_eq!(get(&fields, "audioType"), Some("audio/wav"));
        assert_eq!(
            get(&fields, "audioName"),
            Some("Example-Network_52101_1788012000123.wav")
        );
        assert_eq!(
            file.expect("file part").0,
            "Example-Network_52101_1788012000123.wav"
        );
    }

    #[test]
    fn encrypted_call_sends_no_audio_part() {
        let mut b = CallBuilder::new("S", "T", 1_788_012_000_000, 851_000_000, 7);
        b.set_encryption(Some("AES-256".into()), Some(42));
        let call = b.finish(1_788_012_002_000);
        let (ct, body) = build_multipart(&call, "k", 1, None, AudioFormat::default());
        let (fields, file) = parse(&ct, &body);
        assert!(file.is_none(), "encrypted calls carry no audio part");
        assert_eq!(get(&fields, "encrypted"), Some("1"));
        assert_eq!(get(&fields, "audioName"), Some("S_7_1788012000000.mp3"));
        assert_eq!(get(&fields, "audioType"), Some("audio/mpeg"));
        assert_eq!(get(&fields, "algorithm"), Some("AES-256"));
        assert_eq!(get(&fields, "keyId"), Some("42"));
        assert_eq!(get(&fields, "durationMs"), Some("2000"));
    }

    #[test]
    fn absent_optionals_are_not_sent() {
        let call = CallBuilder::new("S", "T", 1_788_012_000_000, 1, 2).finish(1_788_012_001_000);
        let (ct, body) = build_multipart(&call, "k", 1, Some(b""), AudioFormat::default());
        let (fields, _) = parse(&ct, &body);
        for name in [
            "talkgroupLabel",
            "source",
            "patches",
            "talkerAlias",
            "sourceWacn",
            "sourceSystem",
            "nac",
            "algorithm",
            "keyId",
            "sourceQuality",
        ] {
            assert!(get(&fields, name).is_none(), "{name} should not be sent");
        }
    }

    #[test]
    fn source_quality_is_sent_as_json_when_the_recorder_has_something_to_say() {
        // The contract keyup's best-source negotiation reads: which recorder,
        // how many frames it decoded, how many the vocoder gave up on, and the
        // receiver's wideband level for context.
        let mut b = CallBuilder::new("S", "T", 1_788_012_000_000, 851_000_000, 7);
        b.set_instance(Some("rec-north-01".into()));
        b.set_rssi_dbfs(Some(-38.4));
        for i in 0..1234 {
            b.push_voice_frame(&[0i16; 160], i % 200 == 0);
        }
        let call = b.finish(1_788_012_030_000);
        let (ct, body) = build_multipart(&call, "k", 1, Some(b"MP3"), AudioFormat::default());
        let (fields, _) = parse(&ct, &body);
        assert_eq!(
            get(&fields, "sourceQuality"),
            Some(
                r#"{"instance":"rec-north-01","voice_frames":1234,"bad_frames":7,"rssi_dbfs":-38.4}"#
            )
        );
    }

    #[test]
    fn source_quality_omits_the_level_the_device_never_measured() {
        // Absent, not zero: 0 dBFS is a real and very loud reading.
        let mut b = CallBuilder::new("S", "T", 1_788_012_000_000, 851_000_000, 7);
        b.set_instance(Some("rec-north-01".into()));
        b.push_voice_frame(&[0i16; 160], false);
        let call = b.finish(1_788_012_001_000);
        let (ct, body) = build_multipart(&call, "k", 1, Some(b"MP3"), AudioFormat::default());
        let (fields, _) = parse(&ct, &body);
        assert_eq!(
            get(&fields, "sourceQuality"),
            Some(r#"{"instance":"rec-north-01","voice_frames":1,"bad_frames":0}"#)
        );
    }

    #[test]
    fn body_is_well_formed_and_terminated() {
        let (ct, body) = build_multipart(
            &call(),
            "k",
            1,
            Some(&[0xff, 0xfb, 0x00]),
            AudioFormat::default(),
        );
        let boundary = ct.split("boundary=").nth(1).unwrap();
        assert!(body.starts_with(format!("--{boundary}\r\n").as_bytes()));
        assert!(body.ends_with(format!("--{boundary}--\r\n").as_bytes()));
    }

    #[test]
    fn boundaries_are_unique() {
        let c = call();
        let (a, _) = build_multipart(&c, "k", 1, None, AudioFormat::default());
        let (b, _) = build_multipart(&c, "k", 1, None, AudioFormat::default());
        assert_ne!(a, b);
    }

    #[test]
    fn header_parameters_cannot_be_broken_out_of() {
        let mut b = CallBuilder::new("S", "T", 1_788_012_000_000, 1, 2);
        b.set_talkgroup_label(Some("bad\"\r\nname".into()));
        let call = b.finish(1_788_012_001_000);
        let (ct, body) = build_multipart(&call, "k", 1, None, AudioFormat::default());
        let (fields, _) = parse(&ct, &body);
        // The value itself is a body, not a header, so it is sent as-is; what
        // matters is that names and filenames stay quotable.
        assert_eq!(get(&fields, "talkgroupLabel"), Some("bad\"\r\nname"));
        assert_eq!(escape("bad\"\r\nname"), "bad'name");
    }
}
