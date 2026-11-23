//! Module: glassbox-inspect JSON structural profiling.
//! File: json_profile.rs.
//! Responsibility: Stream schema names and scalar telemetry without constructing a JSON DOM.
use crate::{InspectOptions, InspectReport, binary::Source, xml};
use serde::{
    Serialize,
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    hash::{Hash, Hasher},
    path::Path,
};

#[derive(Debug, Serialize)]
pub struct JsonProfile {
    pub total_leaves: usize,
    pub max_depth: usize,
    pub leaf_stats: BTreeMap<String, LeafStats>,
    pub loops: Vec<Loop>,
    #[serde(skip)]
    arrays: BTreeMap<String, usize>,
    #[serde(skip)]
    nodes: usize,
    #[serde(skip)]
    distinct: usize,
    /// Cumulative retained full-path bytes charged before retention.
    #[serde(skip)]
    path_bytes: usize,
    /// Cumulative retained-path byte cap; production pins the design constant.
    #[serde(skip)]
    path_byte_cap: usize,
}

/// Production construction pins every limit at its design constant.
impl Default for JsonProfile {
    fn default() -> Self {
        Self {
            total_leaves: 0,
            max_depth: 0,
            leaf_stats: BTreeMap::new(),
            loops: Vec::new(),
            arrays: BTreeMap::new(),
            nodes: 0,
            distinct: 0,
            path_bytes: 0,
            path_byte_cap: RETAINED_PATH_BYTES_CAP,
        }
    }
}

impl JsonProfile {
    /// Charges one retained full-path string against the cumulative budget
    /// before it is stored, so retained collections cannot grow past it.
    ///
    /// Accepts: the byte length of the path about to be retained.
    /// Returns: `Ok(())` when the charge fits the cumulative budget.
    /// Errors: static "JSON retained-path byte cap exceeded" when it does not.
    fn charge_path_bytes(&mut self, bytes: usize) -> Result<(), &'static str> {
        self.path_bytes = self
            .path_bytes
            .checked_add(bytes)
            .ok_or("JSON retained-path byte cap exceeded")?;
        if self.path_bytes > self.path_byte_cap {
            return Err("JSON retained-path byte cap exceeded");
        }
        Ok(())
    }
}

#[derive(Debug, Default, Serialize)]
pub struct LeafStats {
    pub count: usize,
    pub value_type: String,
    pub total_bytes: usize,
    pub distinct_count: usize,
    pub sample_values: Vec<String>,
    #[serde(skip)]
    hashes: BTreeSet<u64>,
}

#[derive(Debug, Serialize)]
pub struct Loop {
    pub path: String,
    pub count: usize,
    pub fields: Vec<String>,
    pub fields_truncated: bool,
}

/// Total `Loop.fields` entries emitted across all array depths.
const LOOP_FIELDS_CAP: usize = 1_024;
/// Total `Loop.fields` string bytes emitted across all array depths.
const LOOP_FIELD_BYTES_CAP: usize = 64 << 10;
/// Cumulative byte cap on retained full-path strings across all retained
/// JSON path collections. Leaf-stat keys and array keys are charged before
/// retention; loop paths reuse the same charged strings, so every retained
/// full-path collection inherits this single budget.
const RETAINED_PATH_BYTES_CAP: usize = 1 << 20;

struct Seed<'a> {
    profile: &'a mut JsonProfile,
    path: String,
    depth: usize,
    metadata_only: bool,
}
impl<'de> DeserializeSeed<'de> for Seed<'_> {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        self.profile.nodes += 1;
        self.profile.max_depth = self.profile.max_depth.max(self.depth);
        if self.depth > 64 || self.path.len() > 8192 || self.profile.nodes > 2_000_000 {
            return Err(de::Error::custom("JSON depth/path/node cap exceeded"));
        }
        deserializer.deserialize_any(self)
    }
}

impl Seed<'_> {
    /// Records one scalar's path and type, optionally suppressing all value work.
    ///
    /// Accepts the scalar's transient display value and type name. Returns unit
    /// after updating bounded path statistics. Metadata-only mode skips value
    /// bytes, hashes, distinct samples, and retained strings; legacy mode keeps
    /// its existing bounded telemetry. New full-path keys are charged against
    /// the cumulative retained-path byte budget before retention. Path/distinct
    /// caps return deserializer errors.
    fn scalar<E: de::Error>(self, value: &str, kind: &str) -> Result<(), E> {
        let tracked = self.profile.leaf_stats.contains_key(&self.path);
        if !tracked && self.profile.leaf_stats.len() >= 16_384 {
            return Err(E::custom("JSON tracked-path cap exceeded"));
        }
        if !tracked {
            self.profile
                .charge_path_bytes(self.path.len())
                .map_err(E::custom)?;
        }
        let stat = self.profile.leaf_stats.entry(self.path).or_default();
        stat.count += 1;
        stat.total_bytes += value.len();
        if stat.value_type.is_empty() {
            stat.value_type = kind.into();
        } else if stat.value_type != kind {
            stat.value_type = "mixed".into();
        }
        if self.metadata_only {
            self.profile.total_leaves += 1;
            return Ok(());
        }
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        kind.hash(&mut hash);
        value.hash(&mut hash);
        let hash = hash.finish();
        if !stat.hashes.contains(&hash) {
            if self.profile.distinct >= 524_288 {
                return Err(E::custom("JSON distinct-value cap exceeded"));
            }
            stat.hashes.insert(hash);
            self.profile.distinct += 1;
            stat.distinct_count += 1;
            if stat.sample_values.len() < 3 {
                stat.sample_values.push(value.chars().take(128).collect());
            }
        }
        self.profile.total_leaves += 1;
        Ok(())
    }
}

impl<'de> Visitor<'de> for Seed<'_> {
    type Value = ();
    fn expecting(&self, out: &mut fmt::Formatter) -> fmt::Result {
        out.write_str("a JSON value")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<(), E> {
        self.scalar(if v { "true" } else { "false" }, "bool")
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<(), E> {
        self.scalar(&v.to_string(), "number")
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<(), E> {
        self.scalar(&v.to_string(), "number")
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<(), E> {
        self.scalar(&v.to_string(), "number")
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<(), E> {
        self.scalar(v, "string")
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        self.scalar("null", "null")
    }
    fn visit_none<E: de::Error>(self) -> Result<(), E> {
        self.scalar("null", "null")
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
        while let Some(key) = map.next_key::<String>()? {
            let simple = !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || c == '_');
            let path = if simple {
                if self.path.is_empty() {
                    key
                } else {
                    format!("{}.{}", self.path, key)
                }
            } else {
                format!(
                    "{}[{}]",
                    self.path,
                    serde_json::to_string(&key).map_err(de::Error::custom)?
                )
            };
            map.next_value_seed(Seed {
                profile: self.profile,
                path,
                depth: self.depth + 1,
                metadata_only: self.metadata_only,
            })?;
        }
        Ok(())
    }
    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<(), S::Error> {
        let path = format!("{}[]", self.path);
        let tracked = self.profile.arrays.contains_key(&path);
        if !tracked && self.profile.arrays.len() >= 16_384 {
            return Err(de::Error::custom("JSON array-path cap exceeded"));
        }
        if !tracked {
            self.profile
                .charge_path_bytes(path.len())
                .map_err(de::Error::custom)?;
        }
        self.profile.arrays.entry(path.clone()).or_default();
        while seq
            .next_element_seed(Seed {
                profile: self.profile,
                path: path.clone(),
                depth: self.depth + 1,
                metadata_only: self.metadata_only,
            })?
            .is_some()
        {
            *self.profile.arrays.get_mut(&path).unwrap() += 1;
        }
        Ok(())
    }
}

pub(crate) fn inspect(path: &Path, report: &mut InspectReport, options: &InspectOptions) {
    let result = (|| -> xml::Result<()> {
        let mut source = Source::open(path)?;
        let bytes = source.range(0, source.len as usize)?;
        let mut profile = JsonProfile::default();
        let mut decoder = serde_json::Deserializer::from_slice(&bytes);
        Seed {
            profile: &mut profile,
            path: String::new(),
            depth: 0,
            metadata_only: false,
        }
        .deserialize(&mut decoder)?;
        decoder.end()?;
        let mut field_entries = 0usize;
        let mut field_bytes = 0usize;
        // Loop paths are clones of already-charged array keys, so loop
        // retention inherits the cumulative retained-path byte budget; the
        // per-array field lists keep their own shared entry/byte caps.
        for (path, count) in &profile.arrays {
            if *count < 2 {
                continue;
            }
            let prefix = format!("{path}.");
            let mut fields = Vec::new();
            let mut fields_truncated = false;
            for key in profile.leaf_stats.keys() {
                let field = if key == path {
                    "<value>"
                } else if let Some(suffix) = key.strip_prefix(&prefix) {
                    suffix
                } else {
                    continue;
                };
                if field_entries >= LOOP_FIELDS_CAP
                    || field_bytes + field.len() > LOOP_FIELD_BYTES_CAP
                {
                    fields_truncated = true;
                    break;
                }
                field_entries += 1;
                field_bytes += field.len();
                fields.push(field.to_string());
            }
            profile.loops.push(Loop {
                path: path.clone(),
                count: *count,
                fields,
                fields_truncated,
            });
        }
        let mut markdown = format!(
            "# JSON structure\n\n- Scalar leaves: {}\n- Maximum depth: {}\n\n| Leaf path | Type | Occurrences | Distinct |\n| --- | --- | --- | --- |\n",
            profile.total_leaves, profile.max_depth
        );
        for (path, stat) in &profile.leaf_stats {
            markdown.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                crate::markdown::escape(path).replace('|', "\\|"),
                stat.value_type,
                stat.count,
                stat.distinct_count
            ));
            if markdown.len() > options.max_chars.saturating_mul(4) {
                break;
            }
        }
        crate::markdown::append(report, &markdown, options, "native_structure");
        report.append_text(std::str::from_utf8(&bytes)?, options.max_chars);
        report.json = Some(profile);
        Ok(())
    })();
    if let Err(error) = result {
        report.error("json", error);
    }
}

/// Profiles JSON schema identifiers without retaining or hashing scalar values.
///
/// Accepts a JSON file path. Returns the bounded profile plus whether the
/// cumulative retained-path byte cap stopped the stream. When it did, the
/// retained schema names are the valid observed prefix and the scalar count
/// is a lower bound; callers publish that prefix with a structured cap
/// diagnostic and partial status. Source, UTF-8, syntax, and other cap
/// failures are returned as errors with no profile; no body string or sample
/// ever enters the public v2 report.
pub(crate) fn metadata(path: &Path) -> xml::Result<(JsonProfile, bool)> {
    let mut source = Source::open(path)?;
    let bytes = source.range(0, source.len as usize)?;
    profile_metadata(&bytes, JsonProfile::default())
}

/// Shared metadata-only streaming core; split from `metadata` so reduced-cap
/// unit tests share the exact profile path. The retained-path byte-cap error
/// carries the single static message "JSON retained-path byte cap exceeded";
/// here it is converted to an explicit truncation flag with the valid schema
/// prefix retained. Every other failure keeps its error semantics unchanged.
fn profile_metadata(bytes: &[u8], mut profile: JsonProfile) -> xml::Result<(JsonProfile, bool)> {
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let result = Seed {
        profile: &mut profile,
        path: String::new(),
        depth: 0,
        metadata_only: true,
    }
    .deserialize(&mut decoder);
    match result {
        Ok(()) => {
            decoder.end()?;
            Ok((profile, false))
        }
        Err(error)
            if error
                .to_string()
                .contains("JSON retained-path byte cap exceeded") =>
        {
            Ok((profile, true))
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reduced-seam harness: profiles in-memory JSON with a chosen byte cap.
    fn profile_of(
        bytes: &[u8],
        metadata_only: bool,
        path_byte_cap: usize,
    ) -> Result<JsonProfile, serde_json::Error> {
        let mut profile = JsonProfile {
            path_byte_cap,
            ..JsonProfile::default()
        };
        let mut decoder = serde_json::Deserializer::from_slice(bytes);
        Seed {
            profile: &mut profile,
            path: String::new(),
            depth: 0,
            metadata_only,
        }
        .deserialize(&mut decoder)?;
        decoder.end()?;
        Ok(profile)
    }

    /// Ordinary documents keep the exact deterministic leaf/array key output;
    /// the production byte cap never engages at ordinary scale.
    #[test]
    fn ordinary_json_keeps_deterministic_leaf_and_array_paths() {
        let profile = profile_of(br#"{"a": 1, "b": [1, 2]}"#, false, RETAINED_PATH_BYTES_CAP)
            .expect("ordinary JSON must profile");
        assert_eq!(profile.total_leaves, 3);
        assert_eq!(profile.max_depth, 2);
        let keys: Vec<&str> = profile.leaf_stats.keys().map(String::as_str).collect();
        assert_eq!(keys, ["a", "b[]"]);
        assert_eq!(profile.arrays.get("b[]"), Some(&2));
    }

    /// A single retained full path over the cumulative budget fails with the
    /// static cap message; the reduced seam proves the guard itself.
    #[test]
    fn retained_path_byte_cap_fires_under_reduced_seam() {
        let error = profile_of(br#"{"aaa": 1}"#, false, 2)
            .expect_err("a 3-byte key must not fit a 2-byte budget");
        assert!(
            error
                .to_string()
                .contains("JSON retained-path byte cap exceeded"),
            "{error}"
        );
    }

    /// The retained-path budget is cumulative across collections: earlier
    /// charges reduce the room available to later leaf keys.
    #[test]
    fn retained_path_budget_is_cumulative_across_keys() {
        let profile =
            profile_of(br#"{"aa": 1, "bb": 2}"#, false, 4).expect("two 2-byte keys fit exactly");
        let keys: Vec<&str> = profile.leaf_stats.keys().map(String::as_str).collect();
        assert_eq!(keys, ["aa", "bb"]);
        let error = profile_of(br#"{"aa": 1, "bb": 2}"#, false, 3)
            .expect_err("the second 2-byte key must exhaust a 3-byte budget");
        assert!(
            error
                .to_string()
                .contains("JSON retained-path byte cap exceeded"),
            "{error}"
        );
    }

    /// Array paths share the same cumulative budget as leaf paths.
    #[test]
    fn array_paths_share_the_retained_path_budget() {
        // The array key "a[]" is 3 bytes; cap 2 rejects it before retention.
        let error = profile_of(br#"{"a": [1]}"#, false, 2)
            .expect_err("the 3-byte array path must not fit a 2-byte budget");
        assert!(
            error
                .to_string()
                .contains("JSON retained-path byte cap exceeded"),
            "{error}"
        );
    }

    /// Metadata-only mode keeps schema paths and counts while retaining no
    /// sample values or distinct telemetry.
    #[test]
    fn metadata_only_mode_keeps_paths_and_skips_value_telemetry() {
        let profile = profile_of(br#"{"x": "value"}"#, true, RETAINED_PATH_BYTES_CAP)
            .expect("ordinary JSON must profile");
        assert_eq!(profile.total_leaves, 1);
        let stat = &profile.leaf_stats["x"];
        assert_eq!(stat.value_type, "string");
        assert_eq!(stat.count, 1);
        assert_eq!(stat.distinct_count, 0);
        assert!(stat.sample_values.is_empty());
    }

    /// Ordinary metadata profiles complete without the truncation flag.
    #[test]
    fn metadata_core_completes_without_truncation_at_ordinary_scale() {
        let (profile, truncated) =
            profile_metadata(br#"{"a": 1, "b": [1, 2]}"#, JsonProfile::default())
                .expect("ordinary JSON must profile");
        assert!(!truncated);
        assert_eq!(profile.total_leaves, 3);
        let keys: Vec<&str> = profile.leaf_stats.keys().map(String::as_str).collect();
        assert_eq!(keys, ["a", "b[]"]);
    }

    /// When the cumulative retained-path byte cap stops the stream, the valid
    /// observed schema prefix is retained with an explicit truncation flag.
    #[test]
    fn metadata_core_retains_valid_prefix_when_path_byte_cap_fires() {
        let profile = JsonProfile {
            path_byte_cap: 5,
            ..JsonProfile::default()
        };
        let (profile, truncated) =
            profile_metadata(br#"{"aaa": 1, "bbb": 2}"#, profile).expect("prefix must be kept");
        assert!(truncated);
        let keys: Vec<&str> = profile.leaf_stats.keys().map(String::as_str).collect();
        assert_eq!(keys, ["aaa"]);
    }

    /// Non-cap failures keep their error semantics: no profile, no flag.
    #[test]
    fn metadata_core_propagates_non_cap_errors() {
        let error = profile_metadata(br#"{"a": "#, JsonProfile::default())
            .expect_err("malformed JSON must fail");
        assert!(!error.to_string().contains("cap exceeded"), "{error}");
    }
}
