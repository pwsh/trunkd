// SPDX-License-Identifier: GPL-3.0-or-later
//! HDU, TDULC and LDU1/LDU2 payload processing (spec §11, §12).

use crate::bits::BitBuf;
use crate::edac::golay::{golay18_check_and_correct, golay24_check_and_correct};
use crate::edac::hamming::hamming10_check_and_correct;
use crate::edac::lsd;
use crate::edac::rs;
use crate::event::{LduKind, P25Event};
use crate::payload::{EncryptionSync, HduHeader, LinkControl};

/// IMBE voice frame bit offsets within an LDU (spec §12).
pub const IMBE_OFFSETS: [usize; 9] = [0, 144, 328, 512, 696, 880, 1064, 1248, 1424];

/// Embedded-signaling Hamming(10,6,3) word start offsets (both LDUs).
pub const LDU_HAMMING_OFFSETS: [usize; 24] = [
    288, 298, 308, 318, 472, 482, 492, 502, 656, 666, 676, 686, 840, 850, 860, 870, 1024, 1034,
    1044, 1054, 1208, 1218, 1228, 1238,
];

/// Process an HDU payload (658-bit buffer, 648 raw): 36 Golay(18,6,8) hex
/// codewords at 18-bit stride → RS(63,47,17) → 120-bit header data.
pub fn process_hdu(buf: &BitBuf, sink: &mut dyn FnMut(P25Event)) {
    let mut corrected = 0u32;
    let mut hexes = [0u8; 36];
    for (w, hex) in hexes.iter_mut().enumerate() {
        let mut word = buf.get_u32(18 * w, 18);
        let before = word;
        // A Golay18 detection (status 2) leaves the word untouched: we keep
        // the raw received data bits and let RS(63,47,17) deal with the hex.
        // Unlike TDULC (RS(24,12) fed from Golay24, where the spec aborts the
        // frame), the HDU's outer code corrects up to 8 of the 36 hexes, so
        // aborting the whole header on a single uncorrectable hex would throw
        // away recoverable frames. Only an RS failure invalidates the HDU.
        let _status = golay18_check_and_correct(&mut word);
        corrected += (word ^ before).count_ones();
        *hex = (word >> 12) as u8 & 0x3F;
    }

    // RS(63,47,17): 20 data hexes then 16 parity hexes (spec §8.6).
    let rs_result = decode_rs_hexes::<15>(&hexes, 20, 16);

    let mut data = [0u8; 15];
    let valid = rs_result.is_some();
    if let Some((word, rs_bits)) = rs_result {
        corrected += rs_bits;
        data = word;
    }
    let header = valid.then(|| HduHeader::parse(&data));
    sink(P25Event::Hdu {
        valid,
        data,
        header,
        corrected,
    });
}

/// Process a TDULC payload (308-bit buffer, 288 raw): twelve Golay(24,12,8)
/// words → 24 hex symbols → RS(24,12,13) → 72-bit link control.
pub fn process_tdulc(buf: &BitBuf, sink: &mut dyn FnMut(P25Event)) {
    let mut corrected = 0u32;
    let mut hexes = [0u8; 24];
    for w in 0..12 {
        let mut word = buf.get_u32(24 * w, 24);
        let before = word;
        let status = golay24_check_and_correct(&mut word);
        if status >= 2 {
            // Abort the frame if any word is uncorrectable (spec §8.4d).
            sink(P25Event::TduLc {
                valid: false,
                link_control: [0; 9],
                corrected,
            });
            return;
        }
        corrected += (word ^ before).count_ones();
        hexes[2 * w] = (word >> 18) as u8 & 0x3F;
        hexes[2 * w + 1] = (word >> 12) as u8 & 0x3F;
    }

    match decode_lc_rs(&hexes) {
        Some((word, rs_bits)) => sink(P25Event::TduLc {
            valid: true,
            link_control: word,
            corrected: corrected + rs_bits,
        }),
        None => sink(P25Event::TduLc {
            valid: false,
            link_control: [0; 9],
            corrected,
        }),
    }
}

/// Decode one of the three shortened RS payload codes (spec §8.6). All share
/// the same packing, so they share one routine:
///
/// * `rs_in[0..parity_n]`   = parity hexes, reversed (`hexes[data_n + parity_n − 1 − i]`)
/// * `rs_in[parity_n..]`    = data hexes, reversed (`hexes[data_n − 1 − i]`)
/// * everything above `data_n + parity_n` stays zero (shortened region)
///
/// and the data is read back MSB-first from the top data symbol down.
/// Instantiations: LC = RS(24,12,13) (12 data, 12 parity → 72 bits),
/// ES = RS(24,16,9) (16 data, 8 parity → 96 bits), HDU = RS(63,47,17)
/// shortened to 36 (20 data, 16 parity → 120 bits).
fn decode_rs_hexes<const BYTES: usize>(
    hexes: &[u8],
    data_n: usize,
    parity_n: usize,
) -> Option<([u8; BYTES], u32)> {
    let used_len = data_n + parity_n;
    debug_assert_eq!(hexes.len(), used_len);
    debug_assert_eq!(BYTES * 8, data_n * 6);
    let mut rs_in = [0u8; 63];
    for i in 0..parity_n {
        rs_in[i] = hexes[data_n + (parity_n - 1 - i)];
    }
    for i in 0..data_n {
        rs_in[parity_n + i] = hexes[data_n - 1 - i];
    }
    let r = rs::decode(&mut rs_in, parity_n, used_len)?;
    let mut bits = BitBuf::new();
    for i in (parity_n..used_len).rev() {
        for b in (0..6).rev() {
            bits.push((rs_in[i] >> b) & 1 != 0);
        }
    }
    let mut out = [0u8; BYTES];
    bits.copy_bits_to_bytes(0, data_n * 6, &mut out);
    Some((out, r.corrected_bits))
}

/// RS(24,12,13) over 24 hex symbols (12 data then 12 parity) → 72-bit LC.
fn decode_lc_rs(hexes: &[u8; 24]) -> Option<([u8; 9], u32)> {
    decode_rs_hexes::<9>(hexes, 12, 12)
}

/// RS(24,16,9) over 24 hex symbols (16 data then 8 parity) → 96-bit
/// encryption sync word.
fn decode_es_rs(hexes: &[u8; 24]) -> Option<([u8; 12], u32)> {
    decode_rs_hexes::<12>(hexes, 16, 8)
}

/// Process an LDU1 or LDU2 (1568 bits): slice 9 raw IMBE frames, decode the
/// 24 embedded Hamming(10,6) words, then RS → link control (LDU1) or
/// encryption sync (LDU2). Also extracts the low-speed data bytes.
pub fn process_ldu(buf: &BitBuf, kind: LduKind, sink: &mut dyn FnMut(P25Event)) {
    // Voice frames — raw and untouched (vocoder boundary, spec §12.1).
    for (index, &off) in IMBE_OFFSETS.iter().enumerate() {
        let mut frame = [0u8; 18];
        buf.copy_bits_to_bytes(off, 144, &mut frame);
        sink(P25Event::ImbeFrame {
            ldu: kind,
            index: index as u8,
            frame,
        });
    }

    // Embedded signaling: 24 Hamming(10,6,3) words.
    let mut corrected = 0u32;
    let mut hexes = [0u8; 24];
    for (k, &off) in LDU_HAMMING_OFFSETS.iter().enumerate() {
        let mut word = buf.get_u32(off, 10) as u16;
        if let Some(c) = hamming10_check_and_correct(&mut word) {
            corrected += c;
        }
        // On Hamming failure keep the raw data bits; RS may still recover.
        hexes[k] = (word >> 4) as u8 & 0x3F;
    }

    match kind {
        LduKind::Ldu1 => {
            // First 12 hexes = data, last 12 = RS parity → 72-bit LC.
            if let Some((word, rs_bits)) = decode_lc_rs(&hexes) {
                sink(P25Event::LinkControl {
                    word,
                    parsed: LinkControl::parse(&word),
                    corrected: corrected + rs_bits,
                });
            }
        }
        LduKind::Ldu2 => {
            // First 16 hexes = data, last 8 = parity → 96-bit ES.
            if let Some((word, rs_bits)) = decode_es_rs(&hexes) {
                sink(P25Event::EncryptionSync {
                    word,
                    parsed: EncryptionSync::parse(&word),
                    corrected: corrected + rs_bits,
                });
            }
        }
    }

    // Low-speed data: two (16,8,5)-coded octets. Octet 0 = data 1392–1399 +
    // parity 1400–1407; octet 1 = data 1408–1415 + parity 1416–1423 (spec
    // §12.2, §8 LSD). The reference leaves parity unchecked; we correct it.
    let o0 = lsd::decode(buf.get_u32(1392, 16) as u16);
    let o1 = lsd::decode(buf.get_u32(1408, 16) as u16);
    sink(P25Event::LowSpeedData {
        data: [o0.data, o1.data],
        errors: [o0.errors, o1.errors],
        valid: [o0.valid, o1.valid],
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edac::golay::{golay18_encode, golay24_encode};
    use crate::edac::hamming::hamming10_encode;

    fn collect(events: &mut Vec<P25Event>) -> impl FnMut(P25Event) + '_ {
        move |e| events.push(e)
    }

    /// Build an RS codeword for the LC packing and return its 24 hexes
    /// (12 data then 12 parity) for a given 72-bit LC word.
    fn encode_lc(word: u128) -> [u8; 24] {
        let mut rs_in = [0u8; 63];
        // Data hexes: hex j = bits [6j, 6j+6) of the LC word (MSB-first).
        let mut data = [0u8; 12];
        for (j, d) in data.iter_mut().enumerate() {
            *d = ((word >> (66 - 6 * j)) & 0x3F) as u8;
        }
        for (i, &d) in data.iter().enumerate().take(12) {
            let _ = d;
            rs_in[12 + i] = data[11 - i];
        }
        rs::encode(&mut rs_in, 12);
        let mut hexes = [0u8; 24];
        for i in 0..12 {
            hexes[11 - i] = rs_in[12 + i]; // data
            hexes[12 + (11 - i)] = rs_in[i]; // parity
        }
        hexes
    }

    /// Build the 24 hexes (16 data then 8 parity) for a 96-bit ES word.
    fn encode_es(word: u128) -> [u8; 24] {
        let mut rs_in = [0u8; 63];
        let mut data = [0u8; 16];
        for (j, d) in data.iter_mut().enumerate() {
            *d = ((word >> (90 - 6 * j)) & 0x3F) as u8;
        }
        for i in 0..16 {
            rs_in[8 + i] = data[15 - i];
        }
        rs::encode(&mut rs_in, 8);
        let mut hexes = [0u8; 24];
        for i in 0..16 {
            hexes[15 - i] = rs_in[8 + i]; // data
        }
        for i in 0..8 {
            hexes[16 + (7 - i)] = rs_in[i]; // parity
        }
        hexes
    }

    fn lc_bytes(word: u128) -> [u8; 9] {
        let mut out = [0u8; 9];
        for (k, b) in out.iter_mut().enumerate() {
            *b = ((word >> (8 * (8 - k))) & 0xFF) as u8;
        }
        out
    }

    #[test]
    fn tdulc_roundtrip() {
        let lc: u128 = 0x1234_89AB_CD55_0123;
        let hexes = encode_lc(lc);
        let mut buf = BitBuf::new();
        for w in 0..12 {
            let data12 = (u16::from(hexes[2 * w]) << 6) | u16::from(hexes[2 * w + 1]);
            let cw = golay24_encode(data12);
            for b in (0..24).rev() {
                buf.push((cw >> b) & 1 != 0);
            }
        }
        // Inject a couple of correctable errors.
        buf.flip(5);
        buf.flip(100);
        let mut events = Vec::new();
        process_tdulc(&buf, &mut collect(&mut events));
        match &events[0] {
            P25Event::TduLc {
                valid,
                link_control,
                corrected,
            } => {
                assert!(valid);
                assert_eq!(*link_control, lc_bytes(lc));
                assert!(*corrected >= 2);
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn ldu1_link_control_and_voice_slicing() {
        let lc: u128 = 0x0300_0056_4321_ABCD;
        let hexes = encode_lc(lc);
        let mut buf = BitBuf::new();
        for _ in 0..1568 {
            buf.push(false);
        }
        // Distinctive pattern in voice frame 3.
        for k in 0..144 {
            buf.set(IMBE_OFFSETS[3] + k, k % 3 == 0);
        }
        for (k, &off) in LDU_HAMMING_OFFSETS.iter().enumerate() {
            let cw = hamming10_encode(hexes[k]);
            for b in 0..10 {
                buf.set(off + b, (cw >> (9 - b)) & 1 != 0);
            }
        }
        // One Hamming-correctable error.
        buf.flip(LDU_HAMMING_OFFSETS[0] + 2);
        let mut events = Vec::new();
        process_ldu(&buf, LduKind::Ldu1, &mut collect(&mut events));
        let frames: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, P25Event::ImbeFrame { .. }))
            .collect();
        assert_eq!(frames.len(), 9);
        if let P25Event::ImbeFrame { index, frame, .. } = frames[3] {
            assert_eq!(*index, 3);
            assert_eq!(frame[0], 0b1001_0010); // k%3==0 pattern
        }
        let lc_evt = events
            .iter()
            .find_map(|e| match e {
                P25Event::LinkControl { word, .. } => Some(*word),
                _ => None,
            })
            .expect("link control");
        assert_eq!(lc_evt, lc_bytes(lc));
    }

    #[test]
    fn ldu1_group_voice_lc_parsed() {
        // Group Voice Channel User (LCO 0): TG 52101, a source radio, decoded
        // end-to-end through the LDU1 embedded-signaling path.
        let mut lc: u128 = 0;
        lc |= 1u128 << (71 - 1); // standard flag (bit 1)
        lc |= 52101u128 << (71 - 47); // talkgroup at 32..47
        lc |= 0x0A_1B2Cu128; // source at 48..71 (LSBs)
        let hexes = encode_lc(lc);
        let mut buf = BitBuf::new();
        for _ in 0..1568 {
            buf.push(false);
        }
        for (k, &off) in LDU_HAMMING_OFFSETS.iter().enumerate() {
            let cw = hamming10_encode(hexes[k]);
            for b in 0..10 {
                buf.set(off + b, (cw >> (9 - b)) & 1 != 0);
            }
        }
        let mut events = Vec::new();
        process_ldu(&buf, LduKind::Ldu1, &mut collect(&mut events));
        let parsed = events
            .iter()
            .find_map(|e| match e {
                P25Event::LinkControl { parsed, .. } => Some(parsed.clone()),
                _ => None,
            })
            .expect("link control");
        match parsed {
            LinkControl::GroupVoiceChannelUser {
                talkgroup, source, ..
            } => {
                assert_eq!(talkgroup, 52101);
                assert_eq!(source, 0x0A_1B2C);
            }
            m => panic!("{m:?}"),
        }
    }

    #[test]
    fn ldu2_encryption_sync_parsed() {
        // ES word: MI 0x01..09, ALGID 0x84, KID 0x1234 → encrypted.
        let mut es: u128 = 0;
        for i in 0..9 {
            es |= ((i as u128) + 1) << (88 - 8 * i); // MI octets at bits 0..71
        }
        es |= 0x84u128 << (95 - 79); // ALGID at 72..79
        es |= 0x1234u128; // KID at 80..95
        let hexes = encode_es(es);
        let mut buf = BitBuf::new();
        for _ in 0..1568 {
            buf.push(false);
        }
        for (k, &off) in LDU_HAMMING_OFFSETS.iter().enumerate() {
            let cw = hamming10_encode(hexes[k]);
            for b in 0..10 {
                buf.set(off + b, (cw >> (9 - b)) & 1 != 0);
            }
        }
        let mut events = Vec::new();
        process_ldu(&buf, LduKind::Ldu2, &mut collect(&mut events));
        let parsed = events
            .iter()
            .find_map(|e| match e {
                P25Event::EncryptionSync { parsed, .. } => Some(*parsed),
                _ => None,
            })
            .expect("encryption sync");
        assert!(!parsed.is_clear());
        assert_eq!(parsed.algid, 0x84);
        assert_eq!(parsed.kid, 0x1234);
        assert_eq!(parsed.mi, [1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    /// A Golay24 word beyond the correction radius aborts the TDULC frame
    /// (spec §8.4d). This path only became reachable once Golay24 stopped
    /// miscorrecting weight-4 patterns.
    #[test]
    fn tdulc_uncorrectable_golay_word_aborts() {
        let lc: u128 = 0x1234_89AB_CD55_0123;
        let hexes = encode_lc(lc);
        let mut buf = BitBuf::new();
        for w in 0..12 {
            let data12 = (u16::from(hexes[2 * w]) << 6) | u16::from(hexes[2 * w + 1]);
            let cw = golay24_encode(data12);
            for b in (0..24).rev() {
                buf.push((cw >> b) & 1 != 0);
            }
        }
        // Four errors in one Golay24 word: detectable, not correctable.
        for b in [0, 3, 7, 19] {
            buf.flip(24 * 5 + b);
        }
        let mut events = Vec::new();
        process_tdulc(&buf, &mut collect(&mut events));
        match &events[0] {
            P25Event::TduLc { valid, .. } => assert!(!valid),
            e => panic!("{e:?}"),
        }
    }

    /// The HDU must *not* abort on a single uncorrectable Golay18 hex: the
    /// outer RS(63,47,17) corrects up to 8 of the 36 hexes, so the header is
    /// still recoverable. (TDULC aborts; the HDU deliberately does not.)
    #[test]
    fn hdu_survives_uncorrectable_golay_hex() {
        let data_hexes: [u8; 20] = [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ];
        let mut rs_in = [0u8; 63];
        for i in 0..20 {
            rs_in[16 + i] = data_hexes[19 - i];
        }
        rs::encode(&mut rs_in, 16);
        let mut hexes = [0u8; 36];
        for i in 0..20 {
            hexes[19 - i] = rs_in[16 + i];
        }
        for i in 0..16 {
            hexes[20 + (15 - i)] = rs_in[i];
        }
        let mut buf = BitBuf::new();
        for &h in hexes.iter() {
            let cw = golay18_encode(h);
            for b in (0..18).rev() {
                buf.push((cw >> b) & 1 != 0);
            }
        }
        for _ in 648..658 {
            buf.push(false);
        }
        // Four errors inside hex 2 → Golay18 reports uncorrectable; the hex is
        // passed through raw and RS has to clean it up.
        for b in [1, 4, 9, 15] {
            buf.flip(18 * 2 + b);
        }
        let mut word = buf.get_u32(18 * 2, 18);
        assert_eq!(golay18_check_and_correct(&mut word), 2, "test precondition");
        let mut events = Vec::new();
        process_hdu(&buf, &mut collect(&mut events));
        match &events[0] {
            P25Event::Hdu { valid, data, .. } => {
                assert!(valid, "RS should recover the header");
                assert_eq!(data[0] >> 2, 1);
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn hdu_roundtrip() {
        // 20 data hexes → RS(63,47) parity → Golay18 words.
        let data_hexes: [u8; 20] = [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ];
        let mut rs_in = [0u8; 63];
        for i in 0..20 {
            rs_in[16 + i] = data_hexes[19 - i];
        }
        rs::encode(&mut rs_in, 16);
        let mut hexes = [0u8; 36];
        for i in 0..20 {
            hexes[19 - i] = rs_in[16 + i];
        }
        for i in 0..16 {
            hexes[20 + (15 - i)] = rs_in[i];
        }
        let mut buf = BitBuf::new();
        for &h in hexes.iter() {
            let cw = golay18_encode(h);
            for b in (0..18).rev() {
                buf.push((cw >> b) & 1 != 0);
            }
        }
        for _ in 648..658 {
            buf.push(false);
        }
        buf.flip(20); // one correctable error
        let mut events = Vec::new();
        process_hdu(&buf, &mut collect(&mut events));
        match &events[0] {
            P25Event::Hdu {
                valid,
                data,
                header,
                ..
            } => {
                assert!(valid);
                // First hex (1) occupies the top 6 bits of the header.
                assert_eq!(data[0] >> 2, 1);
                // A valid HDU carries a parsed header (spec §18.3).
                let h = header.expect("parsed header");
                assert_eq!(h, HduHeader::parse(data));
            }
            e => panic!("{e:?}"),
        }
    }
}
