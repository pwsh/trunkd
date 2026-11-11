// SPDX-License-Identifier: GPL-3.0-or-later
//! `SoapySDRKwargs` helpers: the `k=v,k=v` args strings SoapySDR speaks.
//!
//! Everything here is either pure string work (testable without the library) or
//! a small `unsafe` reader that copies a C kwargs struct into an owned
//! [`BTreeMap`] so no borrowed C memory escapes.

use std::collections::BTreeMap;
use std::ffi::{c_char, CStr, CString};

use super::ffi::{Api, SoapySDRKwargs};

pub use crate::device::soapy_args as compose_args;

/// An owned, ordered copy of a `SoapySDRKwargs`.
pub type Kwargs = BTreeMap<String, String>;

/// Parse a `k=v,k=v` args string. Whitespace around keys and values is
/// trimmed; a bare token becomes an empty-valued key, matching SoapySDR.
pub fn parse_args(s: &str) -> Kwargs {
    let mut map = Kwargs::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('=') {
            Some((k, v)) => {
                let k = k.trim();
                if !k.is_empty() {
                    map.insert(k.to_owned(), v.trim().to_owned());
                }
            }
            None => {
                map.insert(part.to_owned(), String::new());
            }
        }
    }
    map
}

/// Render kwargs back to the `k=v,k=v` form SoapySDR accepts.
pub fn format_args(map: &Kwargs) -> String {
    map.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// A `CString` for a Rust string, with interior NULs stripped rather than
/// erroring: these are config values, and a NUL in one is not worth a failure.
pub fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

/// Copy a C `SoapySDRKwargs` into an owned map.
///
/// # Safety
/// `k` must point at a kwargs struct the library filled in, whose `keys`/`vals`
/// arrays each hold `size` NUL-terminated pointers.
pub unsafe fn read_kwargs(k: &SoapySDRKwargs) -> Kwargs {
    let mut map = Kwargs::new();
    if k.keys.is_null() || k.vals.is_null() {
        return map;
    }
    for i in 0..k.size {
        let kp = unsafe { *k.keys.add(i) };
        let vp = unsafe { *k.vals.add(i) };
        if kp.is_null() {
            continue;
        }
        let key = unsafe { CStr::from_ptr(kp) }.to_string_lossy().into_owned();
        let val = if vp.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(vp) }.to_string_lossy().into_owned()
        };
        map.insert(key, val);
    }
    map
}

/// Copy a `char**` string list into owned strings and free it.
///
/// # Safety
/// `ptr`/`len` must be exactly what a SoapySDR `list*` call returned; the array
/// is released with `SoapySDRStrings_clear`, which takes a pointer to the array
/// and nulls it, so the caller must not reuse it.
pub unsafe fn take_strings(api: &Api, mut ptr: *mut *mut c_char, len: usize) -> Vec<String> {
    if ptr.is_null() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let p = unsafe { *ptr.add(i) };
        if !p.is_null() {
            out.push(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned());
        }
    }
    unsafe { (api.strings_clear)(&mut ptr, len) };
    out
}

/// Copy a `char*` the library allocated into an owned string and free it.
///
/// # Safety
/// `ptr` must be a `SoapySDR_free`-able NUL-terminated string (what
/// `getDriverKey`/`getHardwareKey` return), or null.
pub unsafe fn take_string(api: &Api, ptr: *mut c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { (api.free)(ptr.cast()) };
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_round_trip_through_a_map() {
        let s = "driver=airspy,serial=deadbeef,label=Airspy R2";
        let map = parse_args(s);
        assert_eq!(map.get("driver").map(String::as_str), Some("airspy"));
        assert_eq!(map.get("serial").map(String::as_str), Some("deadbeef"));
        assert_eq!(map.get("label").map(String::as_str), Some("Airspy R2"));
        // BTreeMap orders by key, so the round trip is canonical, not literal.
        assert_eq!(
            format_args(&map),
            "driver=airspy,label=Airspy R2,serial=deadbeef"
        );
        assert_eq!(parse_args(&format_args(&map)), map);
    }

    #[test]
    fn args_parsing_tolerates_spaces_blanks_and_bare_keys() {
        let map = parse_args(" driver = rtlsdr , , biastee, rtl = 0 ,");
        assert_eq!(map.get("driver").map(String::as_str), Some("rtlsdr"));
        assert_eq!(map.get("biastee").map(String::as_str), Some(""));
        assert_eq!(map.get("rtl").map(String::as_str), Some("0"));
        assert_eq!(map.len(), 3);
        assert!(parse_args("").is_empty());
    }
}
