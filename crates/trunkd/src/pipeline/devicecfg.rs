// SPDX-License-Identifier: GPL-3.0-or-later
//! Editing `trunkd.toml` in place: the `[[device]]` tables and the site
//! membership that decides which of them actually run.
//!
//! # Why `toml_edit` and not `toml`
//!
//! A config file an operator hand-wrote is mostly *not* config: it is the
//! comments explaining why this Airspy is on the 800 MHz mast, the commented-out
//! site they turn on during storm season, and the ordering they chose. Round
//! tripping through `toml::to_string(&Config)` would produce a semantically
//! equal file and destroy every one of those, which is a thing you get to do to
//! an operator exactly once. [`toml_edit`] parses to a document that remembers
//! its own formatting, so an edit here touches the bytes of the tables it names
//! and nothing else — in particular `[uplink] api_key` and `[server] tokens`
//! are never re-serialized, so a panel write cannot mangle, reformat or leak a
//! secret it had no business reading. There is a test that asserts exactly that
//! on the whole rest of the document.
//!
//! # Why writing the file means a reload, not a hot-add
//!
//! The running pipeline sizes its per-device arrays once: `DesiredState` is
//! seeded from `cfg.device` at boot, the supervisor's unit list, the
//! coordinator's route table and the grant fan-out are all `Vec`s indexed by
//! device number. There is no hot-add path and inventing one would mean making
//! every one of those structures resizable under a live DSP thread. Adding or
//! removing a `[[device]]` is therefore a *file* edit that takes effect on a
//! clean in-process reload (`POST /api/config/reload`), which tears the
//! pipeline down the same way a stop does and builds it again from the new
//! file. Parameters that keyup's desired-state can already push (gain, ppm,
//! bands) stay live; the shape of the device table does not.
//!
//! # Shape of an edit
//!
//! Every entry point takes a parsed [`DocumentMut`] and mutates it, so a route
//! can parse once, apply several edits, prove the result still loads
//! ([`validate_document`]) and only then touch the disk ([`write_atomic`]).
//! Nothing here writes a file it has not first parsed back through the same
//! path [`Config::load`] uses.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value as Json;
use toml_edit::{Array, DocumentMut, Formatted, Item, Table, Value as Toml};

use crate::config::Config;

/// The `[[device]]` keys this module will write, i.e. the serde field names of
/// [`crate::config::Device`].
///
/// Spelled out rather than derived because it is also the *allow list* for the
/// panel's PUT route: serde ignores unknown fields, so without this a typo like
/// `sample_rte = 10000000` would be written to the file, silently dropped at
/// load, and leave the operator staring at a device running at the wrong rate.
/// `device_fields_match_the_struct` keeps it honest against `Device` itself.
pub(crate) const DEVICE_FIELDS: &[&str] = &[
    "id",
    "backend",
    "soapy",
    "driver",
    "gains",
    "agc",
    "bandwidth_hz",
    "rx_port",
    "settings",
    "path",
    "sample_rate",
    "gain",
    "ppm",
    "auto_ppm",
    "auto_ppm_limit",
    "ring_seconds",
    "center_frequency",
    "lna_db",
    "vga_db",
    "amp",
    "serial",
    "portapack_port",
    "restore_portapack_on_exit",
    "pacing",
    "role",
    "receiver",
    "antenna",
    "cable",
    "amplifier",
    "height_m",
    "notes",
    "bands",
    "site",
    "remote_grants",
];

/// The two `[[device]]` keys that are maps, and are written as their own
/// sub-tables (`[device.gains]`) rather than inline.
const SUB_TABLE_FIELDS: &[&str] = &["gains", "settings"];

/// One requested device edit: the id, and the fields the caller named.
///
/// Merge semantics, deliberately: a field the caller did not mention keeps
/// whatever the file says, and a field set to JSON `null` is *removed*. A panel
/// form that only knows about gain must not be able to wipe the `notes` an
/// operator typed into the file by hand, and "unset this key" has to be
/// expressible as something other than "omit it".
#[derive(Debug, Clone, Default)]
pub(crate) struct DeviceSpec {
    /// The `id` key. Always written, and the key the table is found by.
    pub id: String,
    /// Supplied fields by their TOML names; `Json::Null` removes the key.
    pub fields: BTreeMap<String, Json>,
}

impl DeviceSpec {
    /// Build a spec from a JSON object, rejecting any key that is not a
    /// [`crate::config::Device`] field. `id` in the object is ignored: the
    /// route takes it from the path.
    pub fn from_json(id: &str, body: &serde_json::Map<String, Json>) -> Result<DeviceSpec, String> {
        let mut fields = BTreeMap::new();
        for (k, v) in body {
            // The route's own keys, handled by the caller.
            if k == "id" || k == "sites" {
                continue;
            }
            if !DEVICE_FIELDS.contains(&k.as_str()) {
                return Err(format!("unknown device field {k:?}"));
            }
            fields.insert(k.clone(), v.clone());
        }
        Ok(DeviceSpec {
            id: id.to_owned(),
            fields,
        })
    }
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

/// Add or update the `[[device]]` whose `id` is `spec.id`.
///
/// An existing table is edited in place — its comments, its key order and the
/// keys the caller did not name all survive. A new one is appended after the
/// last `[[device]]`, which is before the first `[[site]]` in any file trunkd
/// itself has ever written, so the document keeps reading devices-then-sites.
///
/// Returns whether a new table was created.
pub(crate) fn upsert_device(doc: &mut DocumentMut, spec: &DeviceSpec) -> Result<bool, String> {
    for (k, v) in &spec.fields {
        if !DEVICE_FIELDS.contains(&k.as_str()) {
            return Err(format!("unknown device field {k:?}"));
        }
        if SUB_TABLE_FIELDS.contains(&k.as_str()) && !(v.is_null() || v.is_object()) {
            return Err(format!("device field {k:?} must be a table of values"));
        }
    }

    // Taken before the array is borrowed mutably: it is a property of the whole
    // document, and a new table needs it the moment it is pushed.
    let position = next_position(doc);
    let devices = array_of_tables(doc, "device")?;
    let existing = (0..devices.len()).find(|&i| {
        devices
            .get(i)
            .and_then(|t| t.get("id"))
            .and_then(Item::as_str)
            == Some(spec.id.as_str())
    });

    let created = existing.is_none();
    let idx = match existing {
        Some(i) => i,
        None => {
            let mut t = Table::new();
            t.set_position(Some(position));
            devices.push(t);
            devices.len() - 1
        }
    };
    let table = devices
        .get_mut(idx)
        .expect("the index was just found or just pushed");

    // `id` is not optional and never removable: it is what the table is.
    set_item(table, "id", Item::Value(Toml::from(spec.id.as_str())));

    for (key, value) in &spec.fields {
        apply_field(table, key, value)?;
    }
    Ok(created)
}

/// Write (or remove) one key on a device table.
fn apply_field(table: &mut Table, key: &str, value: &Json) -> Result<(), String> {
    if value.is_null() {
        table.remove(key);
        return Ok(());
    }
    if SUB_TABLE_FIELDS.contains(&key) {
        let map = value
            .as_object()
            .expect("checked by the caller before any table was touched");
        let mut sub = Table::new();
        // An implicit sub-table would print no header of its own; this one is
        // the operator's `[device.gains]` and has to look like it.
        sub.set_implicit(false);
        for (k, v) in map {
            sub.insert(k, Item::Value(json_to_toml(v, &format!("{key}.{k}"))?));
        }
        set_item(table, key, Item::Table(sub));
        return Ok(());
    }
    set_item(table, key, Item::Value(json_to_toml(value, key)?));
    Ok(())
}

/// Set a key, keeping the decoration the existing line already had.
///
/// A comment written *above* a key is stored on that key's leading decor, so a
/// plain `Table::insert` — which builds a fresh `Key` — silently deletes the
/// operator's note the first time the value beside it is edited. Overwriting the
/// item and putting the old key's decor back is what makes "change the ppm"
/// change only the ppm.
fn set_item(table: &mut Table, key: &str, item: Item) {
    let previous = table.key(key).cloned();
    table.insert(key, item);
    if let (Some(old), Some(mut now)) = (previous, table.key_mut(key)) {
        *now.leaf_decor_mut() = old.leaf_decor().clone();
        *now.dotted_decor_mut() = old.dotted_decor().clone();
    }
}

/// Remove the `[[device]]` named `id`.
///
/// Refuses while anything still points at it: a site whose `devices` names a
/// device that does not exist fails validation at load, and taking the recorder
/// down at the next restart is a strictly worse outcome than refusing the
/// delete now. The error names the referrers so the panel can say what to fix.
pub(crate) fn remove_device(doc: &mut DocumentMut, id: &str) -> Result<(), String> {
    let refs = references_to(doc, id);
    if !refs.is_empty() {
        return Err(format!(
            "device '{id}' is still used by {}",
            refs.join(", ")
        ));
    }
    let devices = array_of_tables(doc, "device")?;
    let idx = (0..devices.len()).find(|&i| {
        devices
            .get(i)
            .and_then(|t| t.get("id"))
            .and_then(Item::as_str)
            == Some(id)
    });
    match idx {
        Some(i) => {
            devices.remove(i);
            Ok(())
        }
        None => Err(format!("no device '{id}'")),
    }
}

/// Every `[[site]]` and `[[channel]]` that names `id`, by display name.
pub(crate) fn references_to(doc: &DocumentMut, id: &str) -> Vec<String> {
    let mut out = Vec::new();
    for t in tables(doc, "site") {
        if site_device_ids(t).iter().any(|d| d == id) {
            out.push(format!("site '{}'", site_name(t)));
        }
    }
    for t in tables(doc, "channel") {
        if t.get("device").and_then(Item::as_str) == Some(id) {
            let name = t
                .get("name")
                .and_then(Item::as_str)
                .unwrap_or("(unnamed)")
                .to_owned();
            out.push(format!("channel '{name}'"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Site membership
// ---------------------------------------------------------------------------

/// Make `sites` the exact set of sites `id` serves.
///
/// `sites` are `"System/Site"` names as [`site_name`] spells them. A site that
/// still uses the historical scalar `device = "x"` is migrated to
/// `devices = ["x"]` before anything is added, because the two cannot both be
/// authoritative and `Site::device_ids` ignores `device` the moment `devices`
/// is non-empty — silently dropping the old tuner otherwise.
///
/// An unknown site name is an error rather than a no-op: the panel would
/// otherwise report success for a membership change that did not happen.
pub(crate) fn set_device_sites(
    doc: &mut DocumentMut,
    id: &str,
    sites: &[String],
) -> Result<(), String> {
    let known: Vec<String> = tables(doc, "site").iter().map(|t| site_name(t)).collect();
    for want in sites {
        if !known.iter().any(|k| k == want) {
            return Err(format!("no site '{want}'"));
        }
    }

    let array = match doc.get_mut("site") {
        Some(Item::ArrayOfTables(a)) => a,
        None => return Ok(()),
        Some(_) => return Err("`site` is not an array of tables".to_owned()),
    };
    for t in array.iter_mut() {
        let name = site_name(t);
        let member = sites.iter().any(|s| s == &name);
        migrate_legacy_device(t);
        let mut ids: Vec<String> = t
            .get("devices")
            .and_then(Item::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let present = ids.iter().any(|d| d == id);
        if member && !present {
            ids.push(id.to_owned());
        } else if !member && present {
            ids.retain(|d| d != id);
        } else {
            continue;
        }
        t.insert("devices", Item::Value(Toml::Array(string_array(&ids))));
    }
    Ok(())
}

/// Turn `device = "x"` into `devices = ["x"]` on a site that still uses the
/// scalar form, leaving a site that already has `devices` alone.
fn migrate_legacy_device(t: &mut Table) {
    let has_devices = t
        .get("devices")
        .and_then(Item::as_array)
        .is_some_and(|a| !a.is_empty());
    let legacy = t
        .get("device")
        .and_then(Item::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty());
    match (has_devices, legacy) {
        (false, Some(d)) => {
            t.remove("device");
            t.insert("devices", Item::Value(Toml::Array(string_array(&[d]))));
        }
        // Both set: `devices` already wins at load, so the scalar is dead
        // weight that would only mislead the next reader of the file.
        (true, Some(_)) => {
            t.remove("device");
        }
        _ => {}
    }
}

/// The `"System/Site"` display name of a `[[site]]` table.
pub(crate) fn site_name(t: &Table) -> String {
    let get = |k: &str| t.get(k).and_then(Item::as_str).unwrap_or("").to_owned();
    format!("{}/{}", get("system"), get("site"))
}

/// The device ids a `[[site]]` table names, in `Site::device_ids` order:
/// `devices` when it has anything in it, else the scalar `device`.
fn site_device_ids(t: &Table) -> Vec<String> {
    let listed: Vec<String> = t
        .get("devices")
        .and_then(Item::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if !listed.is_empty() {
        return listed;
    }
    match t.get("device").and_then(Item::as_str) {
        Some(d) if !d.is_empty() => vec![d.to_owned()],
        _ => Vec::new(),
    }
}

/// The `"System/Site"` names of every site that serves `id`, read straight off
/// the document.
///
/// Test-only: the panel reports membership from the loaded [`Config`], which is
/// the same answer arrived at through the code the recorder itself runs. This
/// reads it back from the *document* instead, so a membership test cannot pass
/// by agreeing with the writer's own bug.
#[cfg(test)]
pub(crate) fn sites_of(doc: &DocumentMut, id: &str) -> Vec<String> {
    tables(doc, "site")
        .iter()
        .filter(|t| site_device_ids(t).iter().any(|d| d == id))
        .map(|t| site_name(t))
        .collect()
}

// ---------------------------------------------------------------------------
// Validation and writing
// ---------------------------------------------------------------------------

/// Prove an edited document is still a config trunkd would start on.
///
/// Serializes and re-loads through [`Config::from_toml_str`], which is
/// [`Config::load`] minus the file read, so the answer is the same one the next
/// boot would give — including the semantic checks (`sample_rate` granularity,
/// a control channel outside the capture, a site naming a device that is not
/// there). The message is returned verbatim: the panel has nothing better to
/// say than what startup would have said.
pub(crate) fn validate_document(doc: &DocumentMut) -> Result<Config, String> {
    Config::from_toml_str(&doc.to_string()).map_err(|e| e.to_string())
}

/// Replace `path`'s contents, keeping the previous version as `<path>.bak`.
///
/// The new text goes to `<path>.tmp` first; the current file is *copied* to
/// `<path>.bak`; then the temp is renamed over `path`. There is a config file
/// on disk at every instant, so no ordering here loses both versions.
///
/// The file holds secrets (`uplink.api_key`, `[link] key`, panel tokens), so
/// on Unix the temp is created with the current file's permission bits — or
/// `0600` when there is no current file — before any text is written, and the
/// `.bak` gets the same: a save from the panel must never turn a `0600` config
/// into a world-readable `0644` one. Windows has no mode bits to carry; the
/// file inherits its directory's ACL there, as it did before.
pub(crate) fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = with_extra_extension(path, "tmp");
    let bak = with_extra_extension(path, "bak");
    write_private(&tmp, text.as_bytes(), existing_mode(path))?;

    // The backup is a copy, not a rename: there must be a config file on disk
    // at every instant, so a crash here costs the backup rather than the
    // config. `std::fs::rename` replaces an existing target on both Unix and
    // Windows (MoveFileEx with REPLACE_EXISTING), so the swap is one step.
    if path.exists() {
        // `copy` carries the source's permission bits on Unix.
        std::fs::copy(path, &bak)?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        // Windows can still refuse the rename while another process holds the
        // file open without sharing; the copy is the fallback rather than the
        // failure, because the text is already safely in `tmp`.
        Err(e) => {
            std::fs::copy(&tmp, path).map_err(|_| e)?;
            let _ = std::fs::remove_file(&tmp);
            Ok(())
        }
    }
}

/// The permission bits of the file being replaced, when there is one.
#[cfg(unix)]
fn existing_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn existing_mode(_path: &Path) -> Option<u32> {
    None
}

/// Write `bytes` to a fresh `path`, created with `mode` (default `0600`) on
/// Unix so the text is never readable by anyone the final file would not be.
#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mode = mode.unwrap_or(0o600);
    // A stale temp from a crashed save would keep its own mode through
    // `O_CREAT`; start from nothing.
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    // `mode` at open is filtered by the umask; set it exactly.
    f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    f.write_all(bytes)?;
    f.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8], _mode: Option<u32>) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// `foo.toml` + `bak` -> `foo.toml.bak`, so the backup sorts next to the file
/// and cannot collide with another config in the same directory.
fn with_extra_extension(path: &Path, extra: &str) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(extra);
    path.with_file_name(name)
}

// ---------------------------------------------------------------------------
// Per-driver starting points
// ---------------------------------------------------------------------------

/// A sensible first `[[device]]` for a board that was just discovered.
///
/// Not defaults in the config sense — nothing here is applied to a device the
/// operator did not create. It is the prefill the panel puts in the "add this
/// board" form, so the common case is one click rather than a trip to the
/// driver's documentation. Each row is the setting trunkd is actually run with
/// on that hardware: an Airspy R2 at 10 Msps with the three-element gain split
/// that keeps a P25 control channel out of the noise, an RSP at 8 MHz with its
/// analog filter opened up (an RSP otherwise filters the edges off a wide
/// capture) and the RF notch off, and a HackRF at 20 Msps with the amp on.
pub(crate) struct DriverDefaults {
    /// `backend` for the generated `[[device]]`.
    pub backend: &'static str,
    /// Complex sample rate in samples/sec.
    pub sample_rate: u32,
    /// `bandwidth_hz`, when the driver is one that needs it said out loud.
    pub bandwidth_hz: Option<u32>,
    /// A single `gain` in dB, for drivers with only one knob.
    pub gain: Option<f64>,
    /// Per-element gains, by the driver's own element names.
    pub gains: &'static [(&'static str, f64)],
    /// Hardware AGC, when it is worth pinning.
    pub agc: Option<bool>,
    /// `writeSetting` pairs.
    pub settings: &'static [(&'static str, &'static str)],
    /// `ring_seconds`, when the rate is high enough to want more than default.
    pub ring_seconds: Option<f64>,
    /// HackRF LNA gain in dB.
    pub lna_db: Option<u32>,
    /// HackRF VGA gain in dB.
    pub vga_db: Option<u32>,
    /// HackRF front-end amp.
    pub amp: Option<bool>,
}

const DEFAULTS_BASE: DriverDefaults = DriverDefaults {
    backend: "soapy",
    sample_rate: 2_400_000,
    bandwidth_hz: None,
    gain: None,
    gains: &[],
    agc: None,
    settings: &[],
    ring_seconds: None,
    lna_db: None,
    vga_db: None,
    amp: None,
};

/// The suggestion table, by Soapy driver name (plus `hackrf`, which is its own
/// backend). One place, so the panel and any future `trunkd add-device` cannot
/// suggest two different things for the same board.
pub(crate) fn driver_defaults(driver: &str) -> Option<DriverDefaults> {
    let d = match driver.trim().to_ascii_lowercase().as_str() {
        "airspy" => DriverDefaults {
            sample_rate: 10_000_000,
            gains: &[("LNA", 12.0), ("MIX", 10.0), ("VGA", 12.0)],
            ring_seconds: Some(2.0),
            ..DEFAULTS_BASE
        },
        "sdrplay" => DriverDefaults {
            sample_rate: 8_000_000,
            bandwidth_hz: Some(8_000_000),
            gains: &[("IFGR", 40.0), ("RFGR", 3.0)],
            agc: Some(false),
            settings: &[("rfnotch_ctrl", "false")],
            ..DEFAULTS_BASE
        },
        "rtlsdr" => DriverDefaults {
            sample_rate: 2_400_000,
            gain: Some(40.0),
            ..DEFAULTS_BASE
        },
        "hackrf" => DriverDefaults {
            backend: "hackrf",
            sample_rate: 20_000_000,
            lna_db: Some(40),
            vga_db: Some(40),
            amp: Some(true),
            ..DEFAULTS_BASE
        },
        _ => return None,
    };
    Some(d)
}

/// The suggestion for `driver` as the JSON object the discover route hands the
/// UI to prefill its form with — exactly the body a PUT would take back.
pub(crate) fn suggested_json(driver: &str, serial: Option<&str>) -> Json {
    let mut out = serde_json::Map::new();
    let Some(d) = driver_defaults(driver) else {
        // An unknown board still gets the two fields that name it; the operator
        // supplies the rate the driver wants.
        out.insert("backend".into(), Json::from("soapy"));
        out.insert("driver".into(), Json::from(driver));
        if let Some(s) = serial {
            out.insert("serial".into(), Json::from(s));
        }
        return Json::Object(out);
    };
    out.insert("backend".into(), Json::from(d.backend));
    if d.backend == "soapy" {
        out.insert("driver".into(), Json::from(driver));
    }
    if let Some(s) = serial {
        out.insert("serial".into(), Json::from(s));
    }
    out.insert("sample_rate".into(), Json::from(d.sample_rate));
    if let Some(b) = d.bandwidth_hz {
        out.insert("bandwidth_hz".into(), Json::from(b));
    }
    if let Some(g) = d.gain {
        out.insert("gain".into(), Json::from(g));
    }
    if !d.gains.is_empty() {
        let m: serde_json::Map<String, Json> = d
            .gains
            .iter()
            .map(|(k, v)| ((*k).to_owned(), Json::from(*v)))
            .collect();
        out.insert("gains".into(), Json::Object(m));
    }
    if let Some(a) = d.agc {
        out.insert("agc".into(), Json::from(a));
    }
    if !d.settings.is_empty() {
        let m: serde_json::Map<String, Json> = d
            .settings
            .iter()
            .map(|(k, v)| ((*k).to_owned(), Json::from(*v)))
            .collect();
        out.insert("settings".into(), Json::Object(m));
    }
    if let Some(r) = d.ring_seconds {
        out.insert("ring_seconds".into(), Json::from(r));
    }
    if let Some(v) = d.lna_db {
        out.insert("lna_db".into(), Json::from(v));
    }
    if let Some(v) = d.vga_db {
        out.insert("vga_db".into(), Json::from(v));
    }
    if let Some(v) = d.amp {
        out.insert("amp".into(), Json::from(v));
    }
    Json::Object(out)
}

// ---------------------------------------------------------------------------
// Document helpers
// ---------------------------------------------------------------------------

/// The `[[key]]` array of tables, created empty when the document has none.
fn array_of_tables<'a>(
    doc: &'a mut DocumentMut,
    key: &str,
) -> Result<&'a mut toml_edit::ArrayOfTables, String> {
    if doc.get(key).is_none() {
        doc.insert(key, Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    }
    match doc.get_mut(key) {
        Some(Item::ArrayOfTables(a)) => Ok(a),
        _ => Err(format!("`{key}` is not an array of tables")),
    }
}

/// Read-only view of the `[[key]]` tables; empty when there are none.
fn tables<'a>(doc: &'a DocumentMut, key: &str) -> Vec<&'a Table> {
    match doc.get(key) {
        Some(Item::ArrayOfTables(a)) => a.iter().collect(),
        _ => Vec::new(),
    }
}

/// A render position past everything the document already has, so a new
/// `[[device]]` lands after the existing ones rather than being hoisted above a
/// table that was written before it.
fn next_position(doc: &DocumentMut) -> isize {
    let mut max = 0;
    for (_, item) in doc.iter() {
        match item {
            Item::Table(t) => max = max.max(t.position().unwrap_or(0)),
            Item::ArrayOfTables(a) => {
                for t in a.iter() {
                    max = max.max(t.position().unwrap_or(0));
                }
            }
            _ => {}
        }
    }
    max + 1
}

/// A TOML array of strings.
fn string_array(ids: &[String]) -> Array {
    let mut a = Array::new();
    for id in ids {
        a.push(id.as_str());
    }
    a
}

/// Convert one JSON value to TOML, refusing what TOML has no room for.
///
/// Integers stay integers: a `sample_rate` that arrived as `10000000` must not
/// be written as `10000000.0`, which figment would then refuse to read back
/// into a `u32`. `serde_json` keeps that distinction, so this only has to
/// respect it.
fn json_to_toml(v: &Json, what: &str) -> Result<Toml, String> {
    Ok(match v {
        Json::Bool(b) => Toml::Boolean(Formatted::new(*b)),
        Json::String(s) => Toml::String(Formatted::new(s.clone())),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Toml::Integer(Formatted::new(i))
            } else if let Some(u) = n.as_u64() {
                // Above i64::MAX there is no TOML integer; nothing trunkd
                // configures is that large, so this is an honest refusal.
                i64::try_from(u)
                    .map(|i| Toml::Integer(Formatted::new(i)))
                    .map_err(|_| format!("{what}: {u} is too large for a TOML integer"))?
            } else if let Some(f) = n.as_f64() {
                Toml::Float(Formatted::new(f))
            } else {
                return Err(format!("{what}: not a number TOML can hold"));
            }
        }
        Json::Array(items) => {
            let mut a = Array::new();
            for (i, item) in items.iter().enumerate() {
                a.push(json_to_toml(item, &format!("{what}[{i}]"))?);
            }
            Toml::Array(a)
        }
        Json::Object(_) => {
            return Err(format!(
                "{what}: only `gains` and `settings` may be a table of values"
            ))
        }
        Json::Null => return Err(format!("{what}: null is not a value")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    /// A config with everything a write must not disturb: comments above and
    /// beside keys, a secret, a blank line, and tables either side of the ones
    /// the writer edits.
    const SAMPLE: &str = r#"# trunkd configuration — hand written, and it shows.
[server]
bind = "127.0.0.1:8080"

[uplink]
# Do not touch this. Seriously.
url = "https://keyup.example/api/call-upload"
api_key   =    'kup_live_do_not_reformat_me'

[[device]]
# The Airspy on the 800 MHz mast.
id = "airspy0"
backend = "soapy"
driver = "airspy"
serial = "00000000deadbeef"
sample_rate = 10000000
center_frequency = 855500000

[device.gains]
LNA = 12.0
MIX = 10.0
VGA = 12.0

[[site]]
system = "Example Network"
site = "Central"
devices = ["airspy0"]
control_frequencies = [851050000]

# Storm season only.
#[[site]]
#system = "Example Network"
#site = "Eastside"

[health]
max_drop_rate = 0.01
"#;

    fn doc() -> DocumentMut {
        SAMPLE.parse::<DocumentMut>().expect("the sample parses")
    }

    /// Everything in `text` that is not inside a `[[device]]` or `[[site]]`
    /// table, so a test can assert the writer left it alone.
    fn outside_device_and_site(text: &str) -> String {
        let mut out = String::new();
        let mut skipping = false;
        for line in text.lines() {
            let t = line.trim_start();
            if t.starts_with('[') {
                skipping = t.starts_with("[[device]]")
                    || t.starts_with("[[site]]")
                    || t.starts_with("[device.");
            }
            if !skipping {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    #[test]
    fn a_parse_and_print_round_trip_changes_nothing_at_all() {
        assert_eq!(doc().to_string(), SAMPLE);
    }

    #[test]
    fn the_field_list_is_exactly_the_device_structs_own() {
        // Every optional field set, so nothing is skipped at serialization.
        let toml = r#"
id = "d"
backend = "soapy"
soapy = "driver=airspy"
driver = "airspy"
agc = true
bandwidth_hz = 8000000
rx_port = "Antenna B"
path = "cap.iq"
sample_rate = 10000000
gain = 20.0
ppm = 1.5
auto_ppm = true
auto_ppm_limit = 5.0
ring_seconds = 2.0
center_frequency = 855500000
lna_db = 40
vga_db = 40
amp = true
serial = "abc"
portapack_port = "COM3"
restore_portapack_on_exit = true
pacing = "realtime"
role = "auto"
receiver = "Airspy R2"
antenna = "X50"
cable = "LMR-400"
amplifier = "LaNA"
height_m = 8.0
notes = "n"
bands = ["800"]
site = "Central"
remote_grants = false
[gains]
LNA = 12.0
[settings]
biastee = "false"
"#;
        let d: crate::config::Device = toml::from_str(toml).expect("a valid device");
        let json = serde_json::to_value(&d).expect("a device serializes");
        let keys: Vec<&str> = json
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        for k in &keys {
            assert!(
                DEVICE_FIELDS.contains(k),
                "Device gained a field the writer does not know: {k}"
            );
        }
        for k in DEVICE_FIELDS {
            assert!(
                keys.contains(k),
                "the writer knows a field Device does not have: {k}"
            );
        }
    }

    #[test]
    fn editing_a_device_leaves_the_rest_of_the_document_byte_for_byte() {
        let mut d = doc();
        let spec = DeviceSpec {
            id: "airspy0".into(),
            fields: [("ppm".to_owned(), json!(1.5))].into_iter().collect(),
        };
        assert!(!upsert_device(&mut d, &spec).expect("an edit"));
        let after = d.to_string();

        assert_eq!(
            outside_device_and_site(&after),
            outside_device_and_site(SAMPLE),
            "a device edit rewrote something outside the device tables"
        );
        // The secret in particular: same bytes, same spacing, same quote style.
        assert!(
            after.contains("api_key   =    'kup_live_do_not_reformat_me'"),
            "the uplink key was reformatted:\n{after}"
        );
        assert!(after.contains("# Do not touch this. Seriously."));
        assert!(after.contains("# The Airspy on the 800 MHz mast."));
        assert!(after.contains("# Storm season only."));
        assert!(after.contains("ppm = 1.5"));
    }

    #[test]
    fn an_edit_keeps_the_keys_it_was_not_told_about() {
        let mut d = doc();
        let spec = DeviceSpec {
            id: "airspy0".into(),
            fields: [("gain".to_owned(), json!(20.0))].into_iter().collect(),
        };
        upsert_device(&mut d, &spec).expect("an edit");
        let after = d.to_string();
        assert!(after.contains("serial = \"00000000deadbeef\""));
        assert!(after.contains("center_frequency = 855500000"));
        assert!(after.contains("gain = 20.0"));
    }

    #[test]
    fn null_removes_a_key() {
        let mut d = doc();
        let spec = DeviceSpec {
            id: "airspy0".into(),
            fields: [("serial".to_owned(), Json::Null)].into_iter().collect(),
        };
        upsert_device(&mut d, &spec).expect("an edit");
        let after = d.to_string();
        assert!(!after.contains("serial = "), "serial survived:\n{after}");
        assert!(
            after.contains("driver = \"airspy\""),
            "and nothing else went"
        );
        assert!(validate_document(&d).is_ok());
    }

    #[test]
    fn a_new_device_is_appended_and_keeps_gains_as_a_sub_table() {
        let mut d = doc();
        let spec = DeviceSpec {
            id: "rsp1".into(),
            fields: [
                ("backend".to_owned(), json!("soapy")),
                ("driver".to_owned(), json!("sdrplay")),
                ("sample_rate".to_owned(), json!(8_000_000)),
                ("center_frequency".to_owned(), json!(855_500_000)),
                ("gains".to_owned(), json!({ "IFGR": 40, "RFGR": 3 })),
                ("settings".to_owned(), json!({ "rfnotch_ctrl": "false" })),
            ]
            .into_iter()
            .collect(),
        };
        assert!(upsert_device(&mut d, &spec).expect("a new device"));
        let after = d.to_string();

        assert!(after.contains("[device.gains]"), "gains inline:\n{after}");
        assert!(after.contains("[device.settings]"));
        assert!(after.contains("IFGR = 40"));
        assert!(after.contains("rfnotch_ctrl = \"false\""));
        // Two devices now, and the original still first.
        let cfg = validate_document(&d).expect("still a valid config");
        assert_eq!(cfg.device.len(), 2);
        assert_eq!(cfg.device[0].id, "airspy0");
        assert_eq!(cfg.device[1].id, "rsp1");
        assert_eq!(cfg.device[1].gains.get("IFGR"), Some(&40.0));
        assert_eq!(
            cfg.device[1]
                .settings
                .get("rfnotch_ctrl")
                .map(String::as_str),
            Some("false")
        );
        // And the rest of the file is untouched.
        assert!(after.contains("api_key   =    'kup_live_do_not_reformat_me'"));
        assert!(after.contains("[health]"));
    }

    #[test]
    fn an_unknown_field_is_refused_before_anything_is_written() {
        let mut d = doc();
        let spec = DeviceSpec {
            id: "airspy0".into(),
            fields: [("sample_rte".to_owned(), json!(10_000_000))]
                .into_iter()
                .collect(),
        };
        let err = upsert_device(&mut d, &spec).expect_err("a typo is refused");
        assert!(err.contains("sample_rte"), "{err}");
        assert_eq!(d.to_string(), SAMPLE, "and the document is untouched");
    }

    #[test]
    fn removing_a_device_is_refused_while_a_site_uses_it() {
        let mut d = doc();
        let err = remove_device(&mut d, "airspy0").expect_err("still referenced");
        assert!(err.contains("Example Network/Central"), "{err}");
        assert_eq!(d.to_string(), SAMPLE);

        // Drop the membership and it goes.
        set_device_sites(&mut d, "airspy0", &[]).expect("membership");
        remove_device(&mut d, "airspy0").expect("now unreferenced");
        assert!(!d.to_string().contains("id = \"airspy0\""));
        assert!(d
            .to_string()
            .contains("api_key   =    'kup_live_do_not_reformat_me'"));
    }

    #[test]
    fn removing_a_device_that_is_not_there_is_an_error() {
        let mut d = doc();
        assert!(remove_device(&mut d, "nope").is_err());
    }

    #[test]
    fn site_membership_is_added_and_removed() {
        let mut d = doc();
        let spec = DeviceSpec {
            id: "rsp1".into(),
            fields: [
                ("driver".to_owned(), json!("sdrplay")),
                ("sample_rate".to_owned(), json!(8_000_000)),
                ("center_frequency".to_owned(), json!(855_500_000)),
            ]
            .into_iter()
            .collect(),
        };
        upsert_device(&mut d, &spec).expect("a new device");
        set_device_sites(&mut d, "rsp1", &["Example Network/Central".to_owned()]).expect("added");
        assert_eq!(
            sites_of(&d, "rsp1"),
            vec!["Example Network/Central".to_owned()]
        );
        assert!(d.to_string().contains(r#"devices = ["airspy0", "rsp1"]"#));

        set_device_sites(&mut d, "rsp1", &[]).expect("removed");
        assert!(sites_of(&d, "rsp1").is_empty());
        assert!(d.to_string().contains(r#"devices = ["airspy0"]"#));
    }

    #[test]
    fn an_unknown_site_name_is_an_error_rather_than_a_silent_no_op() {
        let mut d = doc();
        let err = set_device_sites(&mut d, "airspy0", &["Nowhere/Nothing".to_owned()])
            .expect_err("no such site");
        assert!(err.contains("Nowhere/Nothing"), "{err}");
        assert_eq!(d.to_string(), SAMPLE);
    }

    #[test]
    fn a_legacy_scalar_device_is_migrated_before_a_second_tuner_is_added() {
        let text = r#"[[device]]
id = "a"
backend = "iqfile"
path = "x.iq"
sample_rate = 2400000
center_frequency = 851050000

[[device]]
id = "b"
backend = "iqfile"
path = "y.iq"
sample_rate = 2400000
center_frequency = 851050000

[[site]]
system = "Sys"
site = "One"
device = "a"
control_frequencies = [851050000]
"#;
        let mut d = text.parse::<DocumentMut>().expect("parses");
        set_device_sites(&mut d, "b", &["Sys/One".to_owned()]).expect("added");
        let after = d.to_string();
        assert!(
            !after.contains("device = \"a\""),
            "scalar survived:\n{after}"
        );
        assert!(after.contains(r#"devices = ["a", "b"]"#), "{after}");
        // Both tuners still serve the site — the migration did not drop `a`.
        let cfg = validate_document(&d).expect("valid");
        assert_eq!(cfg.site[0].device_ids(), vec!["a", "b"]);
    }

    #[test]
    fn a_legacy_scalar_is_migrated_even_when_the_site_is_only_being_left_alone() {
        // Removing an unrelated device from a legacy site still normalises it,
        // which is the only way `devices` and `device` cannot end up disagreeing.
        let text = r#"[[device]]
id = "a"
backend = "iqfile"
path = "x.iq"
sample_rate = 2400000
center_frequency = 851050000

[[site]]
system = "Sys"
site = "One"
device = "a"
control_frequencies = [851050000]
"#;
        let mut d = text.parse::<DocumentMut>().expect("parses");
        set_device_sites(&mut d, "a", &["Sys/One".to_owned()]).expect("already a member");
        let after = d.to_string();
        assert!(after.contains(r#"devices = ["a"]"#), "{after}");
        assert_eq!(sites_of(&d, "a"), vec!["Sys/One".to_owned()]);
    }

    #[test]
    fn validation_rejects_what_startup_would_reject_and_says_why() {
        // A sample rate no capture can have.
        let mut d = doc();
        let spec = DeviceSpec {
            id: "airspy0".into(),
            fields: [("sample_rate".to_owned(), json!(0))].into_iter().collect(),
        };
        upsert_device(&mut d, &spec).expect("an edit");
        let err = validate_document(&d).expect_err("an impossible rate");
        assert!(
            err.contains("airspy0") && err.contains("sample_rate"),
            "the message is not the one startup gives: {err}"
        );

        // A control channel outside what the tuner can hear — the mistake an
        // operator actually makes, and the one only `validate()` catches.
        let mut d = doc();
        let spec = DeviceSpec {
            id: "airspy0".into(),
            fields: [("center_frequency".to_owned(), json!(155_000_000))]
                .into_iter()
                .collect(),
        };
        upsert_device(&mut d, &spec).expect("an edit");
        let err = validate_document(&d).expect_err("out of span");
        assert!(
            err.contains("851050000") && err.contains("Example Network/Central"),
            "the message does not name the frequency and site: {err}"
        );
    }

    #[test]
    fn a_write_replaces_the_file_and_keeps_one_backup() {
        let dir = std::env::temp_dir().join(format!(
            "trunkd-devicecfg-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("a scratch dir");
        let path = dir.join("trunkd.toml");
        let bak = dir.join("trunkd.toml.bak");

        // First write: nothing to back up.
        write_atomic(&path, "one\n").expect("the first write");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "one\n");
        assert!(!bak.exists(), "nothing to back up yet");

        // Second: the previous content moves aside. This is the Windows case —
        // `rename` over an existing target fails there, so the order matters.
        write_atomic(&path, "two\n").expect("the second write");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "two\n");
        assert_eq!(std::fs::read_to_string(&bak).expect("read"), "one\n");

        // Third: exactly one backup, the immediately previous version.
        write_atomic(&path, "three\n").expect("the third write");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "three\n");
        assert_eq!(std::fs::read_to_string(&bak).expect("read"), "two\n");
        assert!(
            !dir.join("trunkd.toml.tmp").exists(),
            "the temp file was left behind"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_write_keeps_the_files_permissions_and_a_new_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| {
            std::fs::metadata(p)
                .expect("the file exists")
                .permissions()
                .mode()
                & 0o777
        };
        let dir = std::env::temp_dir().join(format!(
            "trunkd-devicecfg-mode-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch dir");
        let path = dir.join("trunkd.toml");
        let bak = dir.join("trunkd.toml.bak");

        // No file yet: the config holds secrets, so it starts owner-only.
        write_atomic(&path, "one\n").expect("the first write");
        assert_eq!(mode(&path), 0o600);

        // An operator's 0600 file stays 0600 across a save (the bug: the temp
        // was created 0644 by the umask and renamed over it), and so does its
        // backup.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        write_atomic(&path, "two\n").expect("the second write");
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&bak), 0o600);

        // And a mode the operator chose on purpose is kept, not tightened.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod");
        write_atomic(&path, "three\n").expect("the third write");
        assert_eq!(mode(&path), 0o640);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "three\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_suggestions_are_the_documented_ones() {
        let airspy = suggested_json("airspy", Some("00000000deadbeef"));
        assert_eq!(airspy["backend"], "soapy");
        assert_eq!(airspy["driver"], "airspy");
        assert_eq!(airspy["serial"], "00000000deadbeef");
        assert_eq!(airspy["sample_rate"], 10_000_000);
        assert_eq!(airspy["ring_seconds"], 2.0);
        assert_eq!(
            airspy["gains"],
            json!({ "LNA": 12.0, "MIX": 10.0, "VGA": 12.0 })
        );

        let rsp = suggested_json("sdrplay", None);
        assert_eq!(rsp["sample_rate"], 8_000_000);
        assert_eq!(rsp["bandwidth_hz"], 8_000_000);
        assert_eq!(rsp["agc"], false);
        assert_eq!(rsp["gains"], json!({ "IFGR": 40.0, "RFGR": 3.0 }));
        assert_eq!(rsp["settings"], json!({ "rfnotch_ctrl": "false" }));

        let rtl = suggested_json("rtlsdr", None);
        assert_eq!(rtl["sample_rate"], 2_400_000);
        assert_eq!(rtl["gain"], 40.0);

        let hackrf = suggested_json("hackrf", Some("abc"));
        assert_eq!(hackrf["backend"], "hackrf");
        assert!(
            hackrf.get("driver").is_none(),
            "hackrf is not a Soapy driver"
        );
        assert_eq!(hackrf["sample_rate"], 20_000_000);
        assert_eq!(hackrf["lna_db"], 40);
        assert_eq!(hackrf["vga_db"], 40);
        assert_eq!(hackrf["amp"], true);

        // An unknown board still names itself, and suggests nothing it cannot
        // know.
        let other = suggested_json("bladerf", None);
        assert_eq!(other, json!({ "backend": "soapy", "driver": "bladerf" }));
    }

    #[test]
    fn every_suggestion_is_a_body_the_writer_accepts() {
        for driver in ["airspy", "sdrplay", "rtlsdr", "hackrf"] {
            let mut d = doc();
            let body = suggested_json(driver, Some("s0"));
            let mut spec =
                DeviceSpec::from_json("new0", body.as_object().expect("suggestions are objects"))
                    .expect("every suggested field is a real one");
            spec.fields
                .insert("center_frequency".to_owned(), json!(855_500_000));
            upsert_device(&mut d, &spec).expect("the writer takes it");
            validate_document(&d)
                .unwrap_or_else(|e| panic!("the {driver} suggestion does not validate: {e}"));
        }
    }

    #[test]
    fn a_json_body_is_screened_against_the_field_list() {
        let body = json!({ "id": "ignored", "sites": ["A/B"], "ppm": 1.0 });
        let spec = DeviceSpec::from_json("real", body.as_object().expect("an object"))
            .expect("id and sites are the route's own");
        assert_eq!(spec.id, "real");
        assert_eq!(spec.fields.len(), 1);

        let bad = json!({ "nonsense": 1 });
        assert!(DeviceSpec::from_json("x", bad.as_object().expect("an object")).is_err());
    }
}
