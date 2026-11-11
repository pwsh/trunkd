// SPDX-License-Identifier: GPL-3.0-or-later
//! The effective configuration, with every secret taken out of it.
//!
//! The About page publishes the *whole* loaded config — that is the point of
//! it: "what is this recorder actually configured with" is the question a
//! remote operator cannot otherwise answer. That is only safe if the dump
//! provably cannot carry the upload key, the link's shared secret, or a panel
//! token, so redaction happens once, on a serialized tree, by **field name**:
//!
//! * a key whose name contains `key`, `token`, `secret` or `password`
//!   (case-insensitively) has its *value* replaced;
//! * the name itself is always kept — an operator has to be able to see that
//!   `uplink.api_key` exists and is set;
//! * the replacement says whether it was set and how long it was
//!   (`<redacted: set, 32 chars>` / `<redacted: unset>`), because "is the key
//!   even configured, and is it the short one or the long one" is most of what
//!   an operator is trying to find out;
//! * it recurses through nested tables, arrays, and free-form string maps like
//!   `[device.settings]`, where a driver setting called `api_token` is just as
//!   much a secret as a field we declared.
//!
//! Matching on the name and never on the value is what keeps
//! `uplink.url = "https://keyup.example/..."` intact: "keyup" contains "key",
//! but `url` does not, and the value is never inspected.

use serde_json::Value;

use crate::config::Config;

/// Substrings that make a field name a secret. Compared lowercase.
const SECRET_MARKERS: [&str; 4] = ["key", "token", "secret", "password"];

/// Substrings that veto the markers above.
///
/// A field naming a *location* is not a credential however it is spelled, and
/// "keyup" — the name of the service trunkd uploads to — contains "key". A
/// `keyup_url` blanked out would make the page useless for the first question
/// anyone asks it ("where is this recorder pointed?"), so an address-shaped
/// name wins over a marker.
const ADDRESS_MARKERS: [&str; 4] = ["url", "uri", "endpoint", "host"];

/// Whether a field of this name holds a secret.
pub fn is_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if ADDRESS_MARKERS.iter().any(|m| lower.contains(m)) {
        return false;
    }
    SECRET_MARKERS.iter().any(|m| lower.contains(m))
}

/// What replaces a secret value.
fn placeholder(v: &Value) -> Value {
    match v {
        // A string is the only shape a secret takes in this config; its length
        // is the one fact that is safe and useful to keep.
        Value::String(s) if s.is_empty() => Value::String("<redacted: unset>".to_owned()),
        Value::String(s) => Value::String(format!("<redacted: set, {} chars>", s.chars().count())),
        Value::Null => Value::String("<redacted: unset>".to_owned()),
        // Anything else (a list of tokens, say) is redacted element-wise by the
        // caller; this arm covers a scalar of some other type.
        _ => Value::String("<redacted: set>".to_owned()),
    }
}

/// Redact `value` in place, treating `name` as the field it arrived under.
fn redact_named(name: &str, value: &mut Value) {
    if is_secret_name(name) {
        match value {
            // `server.tokens` is an array of secrets: each element is redacted
            // so the *count* still shows, which is what an operator checking
            // "did my second token get deployed" needs.
            Value::Array(items) => {
                for item in items.iter_mut() {
                    *item = placeholder(item);
                }
            }
            other => *other = placeholder(other),
        }
        return;
    }
    redact_in_place(value);
}

/// Walk a serialized tree, redacting by field name at every depth.
pub fn redact_in_place(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (name, v) in map.iter_mut() {
                redact_named(name, v);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                redact_in_place(item);
            }
        }
        _ => {}
    }
}

/// The loaded config as redacted JSON.
pub fn config_json(cfg: &Config) -> Value {
    let mut v = serde_json::to_value(cfg)
        .unwrap_or_else(|e| Value::String(format!("config could not be serialized: {e}")));
    redact_in_place(&mut v);
    v
}

/// The same tree rendered as TOML text, for the `<pre>` on the page and for
/// pasting into a bug report.
///
/// Rendered from the redacted JSON rather than from the typed `Config`, so
/// there is exactly one redaction path and no way for a future field to be
/// covered in one rendering and not the other. `null`s are dropped: TOML has no
/// null, and an unset `Option` is absent from a TOML document by definition.
pub fn config_toml(cfg: &Config) -> String {
    let json = config_json(cfg);
    match json_to_toml(&json) {
        Some(toml::Value::Table(t)) => toml::to_string_pretty(&t)
            .unwrap_or_else(|e| format!("# config could not be rendered as TOML: {e}\n")),
        _ => "# config did not serialize to a TOML table\n".to_owned(),
    }
}

/// Convert redacted JSON to a TOML value, dropping what TOML cannot hold.
///
/// `None` means "omit this": a JSON null, or a number that is not finite.
fn json_to_toml(v: &Value) -> Option<toml::Value> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else if let Some(u) = n.as_u64() {
                // Beyond i64: TOML integers are signed, so keep the exact
                // digits as a string rather than wrapping them into a lie.
                toml::Value::String(u.to_string())
            } else {
                let f = n.as_f64()?;
                if f.is_finite() {
                    toml::Value::Float(f)
                } else {
                    return None;
                }
            }
        }
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Array(items) => toml::Value::Array(items.iter().filter_map(json_to_toml).collect()),
        Value::Object(map) => {
            let mut table = toml::map::Map::new();
            for (k, v) in map {
                if let Some(v) = json_to_toml(v) {
                    table.insert(k.clone(), v);
                }
            }
            toml::Value::Table(table)
        }
    })
}

/// The dotted paths an operator actually wrote, for the "default or file?"
/// column: every key present in the config file, in document order.
///
/// Cheap and exact — it re-reads the file the recorder loaded and walks its
/// tables — and deliberately does *not* try to compare values: a file that sets
/// a field to the same thing the default would have is still a field the
/// operator wrote, and that is the useful distinction.
pub fn keys_present(toml_text: &str) -> Vec<String> {
    // A whole document parses as a `Table`; since toml 0.9 `Value: FromStr`
    // parses a single TOML *value* and would reject every config file.
    let Ok(doc) = toml_text.parse::<toml::Table>() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk_keys("", &toml::Value::Table(doc), &mut out);
    out.sort();
    out.dedup();
    out
}

/// Collect dotted key paths. Array elements collapse to one path (`device.id`
/// rather than `device.0.id`): the page shows which *fields* were written, and
/// per-element paths would be a wall of noise on a multi-tuner box.
fn walk_keys(prefix: &str, value: &toml::Value, out: &mut Vec<String>) {
    match value {
        toml::Value::Table(t) => {
            for (k, v) in t {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                if matches!(v, toml::Value::Table(_) | toml::Value::Array(_)) {
                    out.push(path.clone());
                    walk_keys(&path, v, out);
                } else {
                    out.push(path);
                }
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                walk_keys(prefix, item, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config with a secret in each of the three shapes the document has:
    /// a declared string field, a field on another table, and a free-form
    /// entry in a `[device.settings]` map.
    fn secretive() -> Config {
        let mut cfg = Config::default();
        cfg.uplink.api_key = "kup_live_abcdefgh".into();
        cfg.uplink.url = "https://keyup.example/api/call-upload".into();
        cfg.uplink.system_label = "Example".into();
        cfg.link.enabled = true;
        cfg.link.key = "link-shared-secret-value".into();
        cfg.link.bind = "0.0.0.0:7411".into();
        cfg.server.tokens = vec!["panel-token-one".into(), "panel-token-two".into()];
        cfg.server.bind = "127.0.0.1:8080".into();
        cfg.device = vec![toml::from_str(
            "id = \"airspy-0\"\nbackend = \"soapy\"\nsample_rate = 10000000\n\
             center_frequency = 855500000\n\
             [settings]\napi_token = \"driver-token-value\"\nbiastee = \"false\"\n",
        )
        .expect("a valid device")];
        cfg
    }

    /// Every secret value, and every value that must survive.
    const SECRET_VALUES: [&str; 5] = [
        "kup_live_abcdefgh",
        "link-shared-secret-value",
        "panel-token-one",
        "panel-token-two",
        "driver-token-value",
    ];
    const KEPT_VALUES: [&str; 6] = [
        "https://keyup.example/api/call-upload",
        "Example",
        "0.0.0.0:7411",
        "127.0.0.1:8080",
        "airspy-0",
        "biastee",
    ];

    #[test]
    fn no_secret_survives_either_rendering_and_everything_else_does() {
        let cfg = secretive();
        let json = config_json(&cfg).to_string();
        let text = config_toml(&cfg);

        for rendering in [&json, &text] {
            for secret in SECRET_VALUES {
                assert!(
                    !rendering.contains(secret),
                    "the secret {secret:?} survived: {rendering}"
                );
            }
            for kept in KEPT_VALUES {
                assert!(
                    rendering.contains(kept),
                    "the ordinary value {kept:?} was lost: {rendering}"
                );
            }
        }
    }

    #[test]
    fn the_field_names_are_never_redacted_only_their_values() {
        let cfg = secretive();
        let json = config_json(&cfg);
        assert_eq!(json["uplink"]["api_key"], "<redacted: set, 17 chars>");
        assert_eq!(json["link"]["key"], "<redacted: set, 24 chars>");
        assert_eq!(json["server"]["tokens"][0], "<redacted: set, 15 chars>");
        assert_eq!(json["server"]["tokens"][1], "<redacted: set, 15 chars>");
        assert_eq!(
            json["device"][0]["settings"]["api_token"], "<redacted: set, 18 chars>",
            "a free-form settings map is redacted by its own entry names"
        );
        assert_eq!(json["device"][0]["settings"]["biastee"], "false");

        // And the names themselves are all still there to be read.
        let text = config_toml(&cfg);
        for name in ["api_key", "tokens", "api_token"] {
            assert!(text.contains(name), "the field name {name:?} was dropped");
        }
    }

    #[test]
    fn an_unset_secret_says_so_rather_than_looking_configured() {
        let json = config_json(&Config::default());
        assert_eq!(json["uplink"]["api_key"], "<redacted: unset>");
        assert_eq!(json["link"]["key"], "<redacted: unset>");
        assert_eq!(
            json["server"]["tokens"],
            serde_json::json!([]),
            "an empty list stays empty; there is nothing to redact"
        );
    }

    #[test]
    fn a_url_that_merely_contains_the_word_key_is_left_alone() {
        // The rule is about field names, so nothing about a value can trigger
        // it — this is the case that would silently break the panel's own
        // "where do I upload to" line if redaction ever moved to matching
        // values.
        assert!(!is_secret_name("url"));
        assert!(
            !is_secret_name("keyup_url"),
            "an address-shaped name is never a credential"
        );
        assert!(is_secret_name("api_key"));
        assert!(is_secret_name("KEY"));
        assert!(is_secret_name("Password"));
        assert!(is_secret_name("client_secret"));
        assert!(is_secret_name("bearer_TOKEN"));

        let mut cfg = Config::default();
        cfg.uplink.url = "https://keyup.example/api/call-upload".into();
        assert_eq!(
            config_json(&cfg)["uplink"]["url"],
            "https://keyup.example/api/call-upload"
        );
    }

    #[test]
    fn the_toml_rendering_round_trips_as_toml() {
        // The page shows it in a <pre> for an operator to paste back into a
        // config file, so it has to actually parse.
        let text = config_toml(&secretive());
        let parsed: toml::Table = text.parse().unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert_eq!(parsed["uplink"]["system_label"].as_str(), Some("Example"));
        assert_eq!(parsed["device"][0]["id"].as_str(), Some("airspy-0"));
    }

    #[test]
    fn the_keys_an_operator_wrote_are_listed_as_dotted_paths() {
        let keys = keys_present(
            "[uplink]\nurl = \"x\"\n\n[[device]]\nid = \"a\"\nsample_rate = 1\n\
             [device.settings]\nbiastee = \"false\"\n",
        );
        assert!(keys.contains(&"uplink.url".to_owned()));
        assert!(keys.contains(&"device.id".to_owned()));
        assert!(
            keys.contains(&"device.settings.biastee".to_owned()),
            "nested tables inside an array element are reached: {keys:?}"
        );
        assert!(
            !keys.iter().any(|k| k.contains(".0.")),
            "array elements collapse to one path: {keys:?}"
        );
        assert!(keys_present("not { toml").is_empty());
    }
}
