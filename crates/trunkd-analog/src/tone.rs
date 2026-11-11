// SPDX-License-Identifier: GPL-3.0-or-later
//! Sub-audible squelch tones: CTCSS tones and DCS codes.
//!
//! [`Tone`] is what a `[[channel]]` `tone = "..."` setting parses into and what
//! the detectors report. Parsing is deliberately forgiving about spelling and
//! strict about value: every spelling below is accepted, but only standard
//! table entries are (no nearest-tone rounding, no non-standard DCS codes).
//!
//! # Accepted spellings (case-insensitive, surrounding whitespace ignored)
//!
//! CTCSS (value must be one of the 50 entries of [`CTCSS_TONES`]):
//! - `"167.9"`, `"100"`, `"100.0"`, `"100.00"`: a decimal number of Hz with at
//!   most one significant decimal place;
//! - an optional `Hz` suffix: `"167.9Hz"`, `"167.9 hz"`;
//! - an optional `CTCSS`, `PL` or `C` prefix, with or without a space:
//!   `"CTCSS 167.9"`, `"PL167.9"`, `"C 67.0"`.
//!
//! DCS (code must be one of the 104 entries of [`DCS_CODES`]):
//! - `"D445N"`, `"D445I"`: canonical form, `N` = normal, `I` = inverted;
//! - `"D445"`, `"445"`, `"445N"`, `"445I"`: prefix and/or polarity omitted
//!   (omitted polarity = normal);
//! - `"DCS 445"`, `"DCS445I"`, `"DPL 445"`: `DCS` or `DPL` prefix, with or
//!   without a space; a space before the polarity letter is allowed
//!   (`"D445 I"`);
//! - `R` is accepted as a synonym for `I` (`"D445R"`, "reverse"), and the
//!   code always has exactly three octal digits (`"023"`, never `"23"`).
//!
//! A bare number is DCS when it is a three-digit DCS code and CTCSS otherwise;
//! the two tables share no bare spelling (`"100"` is CTCSS 100.0 Hz, there is
//! no DCS 100), so this is unambiguous.
//!
//! # Canonical display
//!
//! CTCSS prints as Hz with one decimal (`"67.0"`, `"167.9"`), DCS as
//! `D<3 octal digits><N|I>` (`"D023N"`, `"D445I"`). Display output always
//! parses back to the same value.
//!
//! # DCS polarity and equivalent codes
//!
//! On air a DCS code is its 23-bit Golay codeword ([`Tone::dcs_word`]),
//! repeated continuously at [`DCS_BIT_RATE`], bit 0 first. In this crate's
//! convention, normal polarity sends a `1` bit as positive frequency deviation;
//! inverted polarity sends the complement. Because the stream repeats, the
//! complement of one code's word is a rotation of another code's word, so each
//! inverted code is indistinguishable on air from one normal code
//! (`D023I` ≡ `D047N`, `D445I` ≡ `D043N`). [`Tone::dcs_equivalent`] gives that
//! partner and [`Tone::same_signal`] compares tones by what is on air.

use std::fmt;
use std::str::FromStr;

use crate::golay;

/// The 50 standard CTCSS tones, in tenths of Hz, ascending (EIA/TIA-603 set
/// plus the commonly supported extras 69.3, 159.8, 165.5, 171.3, 177.3, 183.5,
/// 189.9, 196.6, 199.5, 206.5, 229.1 and 254.1; 150.0 Hz is not included).
///
/// The closest pair is 67.0 / 69.3 Hz (2.3 Hz apart).
pub const CTCSS_TONES: [u16; 50] = [
    670, 693, 719, 744, 770, 797, 825, 854, 885, 915, 948, 974, 1000, 1035, 1072, 1109, 1148, 1188,
    1230, 1273, 1318, 1365, 1413, 1462, 1514, 1567, 1598, 1622, 1655, 1679, 1713, 1738, 1773, 1799,
    1835, 1862, 1899, 1928, 1966, 1995, 2035, 2065, 2107, 2181, 2257, 2291, 2336, 2418, 2503, 2541,
];

/// The 104 standard DCS codes, written as their three octal digits read as a
/// decimal number (`445` means octal 445), ascending.
pub const DCS_CODES: [u16; 104] = [
    23, 25, 26, 31, 32, 36, 43, 47, 51, 53, 54, 65, 71, 72, 73, 74, 114, 115, 116, 122, 125, 131,
    132, 134, 143, 145, 152, 155, 156, 162, 165, 172, 174, 205, 212, 223, 225, 226, 243, 244, 245,
    246, 251, 252, 255, 261, 263, 265, 266, 271, 274, 306, 311, 315, 325, 331, 332, 343, 346, 351,
    356, 364, 365, 371, 411, 412, 413, 423, 431, 432, 445, 446, 452, 454, 455, 462, 464, 465, 466,
    503, 506, 516, 523, 526, 532, 546, 565, 606, 612, 624, 627, 631, 632, 654, 662, 664, 703, 712,
    723, 731, 732, 734, 743, 754,
];

/// DCS bit rate on air, bits per second.
pub const DCS_BIT_RATE: f32 = 134.4;

/// Fixed data bits 9..=11 of every DCS word (`100`, i.e. bit 11 set).
pub const DCS_FIXED_BITS: u16 = 0x800;

/// A sub-audible squelch tone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tone {
    /// CTCSS tone, in tenths of Hz (`1679` = 167.9 Hz). Always an entry of
    /// [`CTCSS_TONES`] when built by parsing or [`Tone::ctcss`].
    Ctcss(u16),
    /// DCS code.
    Dcs {
        /// The three octal digits read as decimal (`445` = octal 445). Always
        /// an entry of [`DCS_CODES`] when built by parsing or [`Tone::dcs`].
        code: u16,
        /// Inverted polarity (`D445I`); `false` is normal (`D445N`).
        inverted: bool,
    },
}

/// Why a tone string did not parse.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ToneError {
    /// The string was empty or only whitespace.
    #[error("empty tone; use a CTCSS frequency like \"100.0\" or a DCS code like \"D023N\"")]
    Empty,
    /// The string is not shaped like a CTCSS frequency or a DCS code.
    #[error(
        "unrecognised tone {0:?}; use a CTCSS frequency like \"100.0\" or a DCS code like \"D023N\" / \"D023I\""
    )]
    Malformed(String),
    /// A well-formed frequency that is not one of the 50 standard CTCSS tones.
    #[error("{0:?} is not a standard CTCSS tone (67.0 .. 254.1 Hz, 50-tone table)")]
    UnknownCtcss(String),
    /// A well-formed three-digit code that is not one of the 104 standard DCS
    /// codes (or has a non-octal digit).
    #[error("{0:?} is not a standard DCS code (104-code table, three octal digits)")]
    UnknownDcs(String),
}

impl Tone {
    /// CTCSS tone from tenths of Hz, if it is a standard tone.
    pub fn ctcss(tenths_hz: u16) -> Option<Tone> {
        CTCSS_TONES
            .binary_search(&tenths_hz)
            .ok()
            .map(|_| Tone::Ctcss(tenths_hz))
    }

    /// DCS code (octal digits read as decimal), if it is a standard code.
    pub fn dcs(code: u16, inverted: bool) -> Option<Tone> {
        DCS_CODES
            .binary_search(&code)
            .ok()
            .map(|_| Tone::Dcs { code, inverted })
    }

    /// Every standard tone: 50 CTCSS, then 104 DCS normal, then 104 DCS
    /// inverted (258 in total).
    pub fn all() -> impl Iterator<Item = Tone> {
        CTCSS_TONES
            .iter()
            .map(|&t| Tone::Ctcss(t))
            .chain(DCS_CODES.iter().map(|&code| Tone::Dcs {
                code,
                inverted: false,
            }))
            .chain(DCS_CODES.iter().map(|&code| Tone::Dcs {
                code,
                inverted: true,
            }))
    }

    /// CTCSS frequency in Hz; `None` for DCS.
    pub fn ctcss_hz(&self) -> Option<f32> {
        match *self {
            Tone::Ctcss(t) => Some(t as f32 / 10.0),
            Tone::Dcs { .. } => None,
        }
    }

    /// Index into [`CTCSS_TONES`] (CTCSS) or [`DCS_CODES`] (DCS), for
    /// detectors that keep per-entry state in arrays.
    pub fn table_index(&self) -> Option<usize> {
        match *self {
            Tone::Ctcss(t) => CTCSS_TONES.binary_search(&t).ok(),
            Tone::Dcs { code, .. } => DCS_CODES.binary_search(&code).ok(),
        }
    }

    /// The 23-bit word this DCS tone puts on air, bit 0 first, in the
    /// polarity convention of the module docs (inverted = complemented). The
    /// word repeats continuously, so a receiver sees it at any rotation.
    /// `None` for CTCSS or a code with a non-octal digit.
    pub fn dcs_word(&self) -> Option<u32> {
        let Tone::Dcs { code, inverted } = *self else {
            return None;
        };
        let bits = octal_value(code)?;
        let word = golay::encode(DCS_FIXED_BITS | bits);
        Some(if inverted {
            !word & golay::CODEWORD_MASK
        } else {
            word
        })
    }

    /// The DCS tone of the opposite polarity that is identical on air (the
    /// partner's on-air word is a rotation of this one), e.g.
    /// `D023I` → `D047N`, `D047N` → `D023I`. Every entry of [`DCS_CODES`] has
    /// one. `None` for CTCSS.
    pub fn dcs_equivalent(&self) -> Option<Tone> {
        let Tone::Dcs { inverted, .. } = *self else {
            return None;
        };
        let word = self.dcs_word()?;
        // The partner (opposite polarity) sends some rotation of `word`.
        for k in 0..23 {
            let rotated = golay::rotate_right(word, k);
            // Recover the partner's normal-polarity word to read its code.
            let normal = if inverted {
                rotated
            } else {
                !rotated & golay::CODEWORD_MASK
            };
            let data = (normal & golay::DATA_MASK) as u16;
            if data & 0xE00 != DCS_FIXED_BITS || golay::encode(data) != normal {
                continue;
            }
            if let Some(t) = Tone::dcs(decimal_from_octal(data & 0x1FF), !inverted) {
                return Some(t);
            }
        }
        None
    }

    /// Whether `self` and `other` are the same signal on air: equal, or a DCS
    /// code and its [`dcs_equivalent`](Tone::dcs_equivalent).
    pub fn same_signal(&self, other: &Tone) -> bool {
        self == other || self.dcs_equivalent().as_ref() == Some(other)
    }

    /// `true` for CTCSS.
    pub fn is_ctcss(&self) -> bool {
        matches!(self, Tone::Ctcss(_))
    }

    /// `true` for DCS.
    pub fn is_dcs(&self) -> bool {
        matches!(self, Tone::Dcs { .. })
    }
}

/// Octal value of a code written as decimal digits (`445` → `0o445`); `None`
/// if any digit is 8 or 9 or the code has more than three digits.
fn octal_value(code: u16) -> Option<u16> {
    if code > 777 {
        return None;
    }
    let (a, b, c) = (code / 100, code / 10 % 10, code % 10);
    if a > 7 || b > 7 || c > 7 {
        return None;
    }
    Some(a * 64 + b * 8 + c)
}

/// Inverse of [`octal_value`] for a 9-bit value.
fn decimal_from_octal(v: u16) -> u16 {
    (v >> 6 & 7) * 100 + (v >> 3 & 7) * 10 + (v & 7)
}

impl fmt::Display for Tone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Tone::Ctcss(t) => write!(f, "{}.{}", t / 10, t % 10),
            Tone::Dcs { code, inverted } => {
                write!(f, "D{code:03}{}", if inverted { 'I' } else { 'N' })
            }
        }
    }
}

impl FromStr for Tone {
    type Err = ToneError;

    fn from_str(s: &str) -> Result<Tone, ToneError> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(ToneError::Empty);
        }
        let upper = trimmed.to_ascii_uppercase();
        let malformed = || ToneError::Malformed(trimmed.to_string());

        // Explicit DCS prefixes. `D` alone must be followed by a digit, so a
        // word like "DOG" is malformed rather than "unknown DCS".
        for prefix in ["DCS", "DPL", "D"] {
            if let Some(rest) = upper.strip_prefix(prefix) {
                let rest = rest.trim_start();
                if rest.starts_with(|c: char| c.is_ascii_digit()) {
                    return parse_dcs(rest, trimmed).ok_or_else(malformed)?;
                }
                if prefix != "D" {
                    return Err(malformed());
                }
            }
        }
        // Explicit CTCSS prefixes.
        for prefix in ["CTCSS", "PL", "C"] {
            if let Some(rest) = upper.strip_prefix(prefix) {
                let rest = rest.trim_start();
                if rest.starts_with(|c: char| c.is_ascii_digit()) {
                    return parse_ctcss(rest, trimmed).ok_or_else(malformed)?;
                }
                return Err(malformed());
            }
        }
        if !upper.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(malformed());
        }
        // Bare: three digits with an optional polarity letter is DCS when it
        // is a DCS code; everything else is a CTCSS frequency.
        let digits = upper.bytes().take_while(u8::is_ascii_digit).count();
        let has_polarity = upper[digits..].trim_start().starts_with(['N', 'I', 'R']);
        if digits == 3 && (has_polarity || upper.len() == 3) {
            let code: u16 = upper[..3].parse().map_err(|_| malformed())?;
            if has_polarity || DCS_CODES.binary_search(&code).is_ok() {
                return parse_dcs(&upper, trimmed).ok_or_else(malformed)?;
            }
        }
        parse_ctcss(&upper, trimmed).ok_or_else(malformed)?
    }
}

/// Parse `NNN[ ][N|I|R]` (already upper-cased). Outer `None` = malformed,
/// inner `Err` = well-formed but not a standard code.
fn parse_dcs(rest: &str, original: &str) -> Option<Result<Tone, ToneError>> {
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits != 3 {
        return None;
    }
    let inverted = match rest[digits..].trim_start() {
        "" | "N" => false,
        "I" | "R" => true,
        _ => return None,
    };
    let code: u16 = rest[..3].parse().ok()?;
    Some(
        Tone::dcs(code, inverted)
            .filter(|_| octal_value(code).is_some())
            .ok_or_else(|| ToneError::UnknownDcs(original.to_string())),
    )
}

/// Parse `<decimal>[ ][HZ]` (already upper-cased). Outer `None` = malformed,
/// inner `Err` = well-formed but not a standard tone.
fn parse_ctcss(rest: &str, original: &str) -> Option<Result<Tone, ToneError>> {
    let number = rest.strip_suffix("HZ").unwrap_or(rest).trim_end();
    let (int, frac) = match number.split_once('.') {
        Some((i, f)) => (i, f),
        None => (number, ""),
    };
    let all_digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if int.is_empty() || int.len() > 3 || !all_digits(int) || !all_digits(frac) {
        return None;
    }
    if number.contains('.') && frac.is_empty() {
        return None;
    }
    let unknown = || ToneError::UnknownCtcss(original.to_string());
    // At most one significant decimal: "100.00" is fine, "100.05" is not.
    let mut frac_digits = frac.bytes();
    let tenth = frac_digits.next().map_or(0, |b| (b - b'0') as u16);
    if frac_digits.any(|b| b != b'0') {
        return Some(Err(unknown()));
    }
    let tenths = int.parse::<u16>().ok()? * 10 + tenth;
    Some(Tone::ctcss(tenths).ok_or_else(unknown))
}

impl serde::Serialize for Tone {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Tone {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Tone, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Tone, ToneError> {
        s.parse()
    }

    #[test]
    fn tables_are_sorted_unique_and_sized() {
        assert!(CTCSS_TONES.windows(2).all(|w| w[0] < w[1]));
        assert!(DCS_CODES.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(Tone::all().count(), 50 + 2 * 104);
        assert_eq!(CTCSS_TONES[0], 670);
        assert_eq!(CTCSS_TONES[49], 2541);
        assert!(!CTCSS_TONES.contains(&1500));
        let min_gap = CTCSS_TONES.windows(2).map(|w| w[1] - w[0]).min().unwrap();
        assert_eq!(min_gap, 23, "67.0 / 69.3 Hz is the closest pair");
        for code in DCS_CODES {
            assert!(octal_value(code).is_some(), "{code} has a non-octal digit");
        }
    }

    #[test]
    fn display_round_trips_every_tone() {
        for t in Tone::all() {
            let s = t.to_string();
            assert_eq!(parse(&s), Ok(t), "{s}");
            assert_eq!(parse(&s.to_lowercase()), Ok(t), "{s}");
        }
    }

    #[test]
    fn every_ctcss_spelling() {
        for &tenths in &CTCSS_TONES {
            let t = Tone::Ctcss(tenths);
            let hz = format!("{}.{}", tenths / 10, tenths % 10);
            let mut spellings = vec![
                hz.clone(),
                format!("{hz}0"),
                format!("{hz}Hz"),
                format!("{hz} hz"),
                format!("CTCSS {hz}"),
                format!("ctcss{hz}"),
                format!("PL{hz}"),
                format!("pl {hz} Hz"),
                format!("C{hz}"),
                format!("c {hz}"),
                format!("  {hz}\t"),
            ];
            if tenths % 10 == 0 {
                spellings.push(format!("{}", tenths / 10));
                spellings.push(format!("{}Hz", tenths / 10));
                spellings.push(format!("PL {}", tenths / 10));
            }
            for s in spellings {
                assert_eq!(parse(&s), Ok(t), "{s:?}");
            }
            assert_eq!(t.to_string(), hz);
            assert_eq!(t.ctcss_hz(), Some(tenths as f32 / 10.0));
        }
        assert_eq!(parse("67").unwrap().to_string(), "67.0");
        assert_eq!(parse("100").unwrap(), Tone::Ctcss(1000));
    }

    #[test]
    fn every_dcs_spelling_both_polarities() {
        for &code in &DCS_CODES {
            let n = Tone::Dcs {
                code,
                inverted: false,
            };
            let i = Tone::Dcs {
                code,
                inverted: true,
            };
            let c = format!("{code:03}");
            for s in [
                format!("D{c}N"),
                format!("D{c}"),
                format!("d{c}n"),
                c.clone(),
                format!("{c}N"),
                format!("DCS {c}"),
                format!("DCS{c}"),
                format!("dcs {c} n"),
                format!("DPL {c}"),
                format!("D{c} N"),
                format!(" D{c}N "),
            ] {
                assert_eq!(parse(&s), Ok(n), "{s:?}");
            }
            for s in [
                format!("D{c}I"),
                format!("d{c}i"),
                format!("D{c}R"),
                format!("{c}I"),
                format!("{c}r"),
                format!("DCS {c}I"),
                format!("DCS{c} I"),
                format!("DPL{c}I"),
                format!("D{c} I"),
            ] {
                assert_eq!(parse(&s), Ok(i), "{s:?}");
            }
            assert_eq!(n.to_string(), format!("D{c}N"));
            assert_eq!(i.to_string(), format!("D{c}I"));
        }
        assert_eq!(
            parse("D445N"),
            Ok(Tone::Dcs {
                code: 445,
                inverted: false
            })
        );
        assert_eq!(parse("DCS 445").unwrap().to_string(), "D445N");
        assert_eq!(parse("445").unwrap().to_string(), "D445N");
        assert_eq!(parse("D445I").unwrap().to_string(), "D445I");
    }

    #[test]
    fn rejects_nonsense() {
        assert_eq!(parse(""), Err(ToneError::Empty));
        assert_eq!(parse("   "), Err(ToneError::Empty));
        for s in [
            "abc",
            "DOG",
            "Hz",
            "PL",
            "DCS",
            "D",
            "D44",
            "D4455",
            "D445X",
            "D445NN",
            "445Q",
            "1.2.3",
            "167,9",
            "167.",
            ".5",
            "-100.0",
            "+100",
            "100 Hz Hz",
            "DCS PL 100",
            "C D023",
            "1679",
            "D 445 N N",
            "CTCSS",
            "PLX",
            "0x100",
        ] {
            assert!(
                matches!(parse(s), Err(ToneError::Malformed(_))),
                "{s:?} -> {:?}",
                parse(s)
            );
        }
        for s in [
            "150.0", "150", "100.1", "167.95", "67.05", "999", "0", "12.5", "PL 150",
        ] {
            assert!(
                matches!(parse(s), Err(ToneError::UnknownCtcss(_))),
                "{s:?} -> {:?}",
                parse(s)
            );
        }
        for s in [
            "D100N", "D000", "D777I", "D089", "DCS 999", "D448", "100N", "024I",
        ] {
            assert!(
                matches!(parse(s), Err(ToneError::UnknownDcs(_))),
                "{s:?} -> {:?}",
                parse(s)
            );
        }
        // Error messages name the offending input.
        assert!(parse("D100N").unwrap_err().to_string().contains("D100N"));
    }

    #[test]
    fn constructors_check_tables() {
        assert_eq!(Tone::ctcss(1679), Some(Tone::Ctcss(1679)));
        assert_eq!(Tone::ctcss(1500), None);
        assert!(Tone::dcs(445, true).is_some());
        assert_eq!(Tone::dcs(100, false), None);
        assert_eq!(Tone::Ctcss(670).table_index(), Some(0));
        assert_eq!(Tone::dcs(754, true).unwrap().table_index(), Some(103));
        assert!(Tone::Ctcss(670).is_ctcss() && !Tone::Ctcss(670).is_dcs());
        assert_eq!(Tone::Ctcss(670).dcs_word(), None);
    }

    #[test]
    fn dcs_words_are_distinct_and_well_formed() {
        let words: Vec<u32> = DCS_CODES
            .iter()
            .map(|&code| Tone::dcs(code, false).unwrap().dcs_word().unwrap())
            .collect();
        assert_eq!(words[0], 0x76_3813, "D023N reference word");
        for (i, &w) in words.iter().enumerate() {
            assert_eq!(w & 0xE00, DCS_FIXED_BITS as u32);
            // No standard normal code is a rotation of another one.
            for (j, &v) in words.iter().enumerate() {
                if i != j {
                    for k in 0..23 {
                        assert_ne!(golay::rotate_right(w, k), v);
                    }
                }
            }
        }
        let i = Tone::dcs(23, true).unwrap().dcs_word().unwrap();
        assert_eq!(i, !0x76_3813 & golay::CODEWORD_MASK);
    }

    #[test]
    fn every_dcs_code_has_an_equivalent_partner() {
        for t in Tone::all().filter(Tone::is_dcs) {
            let p = t
                .dcs_equivalent()
                .unwrap_or_else(|| panic!("{t} has no partner"));
            let (Tone::Dcs { inverted: a, .. }, Tone::Dcs { inverted: b, .. }) = (t, p) else {
                unreachable!()
            };
            assert_ne!(a, b, "{t} -> {p} keeps polarity");
            assert_eq!(p.dcs_equivalent(), Some(t), "{t} -> {p} not symmetric");
            assert!(t.same_signal(&p) && p.same_signal(&t));
            // Same on air: the partner's word is a rotation of this word.
            let (w, v) = (t.dcs_word().unwrap(), p.dcs_word().unwrap());
            assert!((0..23).any(|k| golay::rotate_right(w, k) == v), "{t} / {p}");
        }
        let d023i = Tone::dcs(23, true).unwrap();
        assert_eq!(d023i.dcs_equivalent(), Tone::dcs(47, false));
        let d445i = Tone::dcs(445, true).unwrap();
        assert_eq!(d445i.dcs_equivalent(), Tone::dcs(43, false));
        assert!(!Tone::Ctcss(1000).same_signal(&Tone::Ctcss(1035)));
        assert_eq!(Tone::Ctcss(1000).dcs_equivalent(), None);
    }

    #[test]
    fn serde_uses_the_canonical_string() {
        // Deserialize through serde's in-memory string deserializer.
        use serde::de::value::{Error as DeError, StrDeserializer};
        use serde::de::IntoDeserializer;
        let d: StrDeserializer<'_, DeError> = "pl 167.9".into_deserializer();
        assert_eq!(
            <Tone as serde::Deserialize>::deserialize(d),
            Ok(Tone::Ctcss(1679))
        );
        let d: StrDeserializer<'_, DeError> = "D100N".into_deserializer();
        assert!(<Tone as serde::Deserialize>::deserialize(d).is_err());
    }
}
