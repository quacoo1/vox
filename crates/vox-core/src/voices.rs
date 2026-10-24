//! ElevenLabs catalogue types (contracts.md §3.3) and the Voices-page view helpers (ui-settings-widget.md §4.3).

use serde::Serialize;

use crate::js::{capitalize_first, js_trim};
use crate::json::js_opt_f64;

/// A voice. `labels` keeps the API's key order (it matters for tag order).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Voice {
    pub voice_id: String,
    pub name: String,
    #[serde(serialize_with = "serialize_labels")]
    pub labels: Vec<(String, String)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview_url: Option<String>,
}

fn serialize_labels<S: serde::Serializer>(v: &[(String, String)], s: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut m = s.serialize_map(Some(v.len()))?;
    for (k, val) in v {
        m.serialize_entry(k, val)?;
    }
    m.end()
}

impl Voice {
    /// A label value (raw, untrimmed), `None` when absent.
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
    /// `(labels[key] ?? '').trim()`.
    pub fn label_trimmed(&self, key: &str) -> &str {
        self.label(key).map(js_trim).unwrap_or("")
    }
}

/// One language of a model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelLanguage {
    pub id: String,
    pub name: String,
}

/// A TTS-capable model.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TtsModel {
    pub model_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub languages: Vec<ModelLanguage>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "js_opt_f64")]
    pub max_chars: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "js_opt_f64")]
    pub cost_factor: Option<f64>,
}

/// `GET /v1/user/subscription` summary.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    pub tier: String,
    /// Credits used this period.
    pub character_count: f64,
    pub character_limit: f64,
    /// Seconds since the epoch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_reset_unix: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// Result of validating and storing an API key (`settings:set-api-key`, contracts.md §5.3). The new settings are
/// published separately; `account` is `None` when unknown, on failure or for a restricted key.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ApiKeyResult {
    pub ok: bool,
    pub error: Option<String>,
    pub warning: Option<String>,
    pub account: Option<AccountInfo>,
}

/// `creditSummary()` (ui-settings-widget.md §2.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CreditSummary {
    pub used: f64,
    pub limit: f64,
    pub remaining: f64,
    /// 0..1 share still available (0 when the limit is unknown).
    pub fraction_left: f64,
    pub reset_in_days: Option<i64>,
}

/// Derives the numbers every credits view shows.
pub fn credit_summary(a: &AccountInfo, now_ms: f64) -> CreditSummary {
    let limit = a.character_limit.max(0.0);
    let used = a.character_count.max(0.0);
    let remaining = (limit - used).max(0.0);
    let fraction_left = if limit > 0.0 { remaining / limit } else { 0.0 };
    let reset_in_days = a
        .next_reset_unix
        .filter(|u| *u != 0.0 && u.is_finite())
        .map(|u| (((u * 1000.0 - now_ms) / crate::constants::DAY_MS).ceil()).max(0.0) as i64);
    CreditSummary { used, limit, remaining, fraction_left, reset_in_days }
}

// ───────────────────────────── Voices page helpers ─────────────────────────────

fn norm(s: &str) -> String {
    js_trim(s).to_lowercase()
}

/// `'creator' → 'Creator'`, `'starter_plus' → 'Starter plus'`: `[_-]+` → space, trimmed, first char upper-cased.
pub fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_sep = false;
    for c in s.chars() {
        if c == '_' || c == '-' {
            if !in_sep {
                out.push(' ');
            }
            in_sep = true;
        } else {
            out.push(c);
            in_sep = false;
        }
    }
    capitalize_first(js_trim(&out))
}

fn rank(v: &Voice) -> f64 {
    match norm(v.category.as_deref().unwrap_or("")).as_str() {
        "cloned" => 0.0,
        "professional" => 1.0,
        "generated" => 2.0,
        "premade" => 3.0,
        _ => 2.5,
    }
}

/// Voices-page order: own voices first (cloned, professional, generated, other, premade), then by name
/// (case-insensitive; decision: plain lower-case compare instead of ICU `localeCompare`).
pub fn sort_voices_for_page(voices: &[Voice]) -> Vec<Voice> {
    let mut v = voices.to_vec();
    v.sort_by(|a, b| {
        rank(a)
            .partial_cmp(&rank(b))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    v
}

/// Voices-page filter (`''` = any).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoiceFilter {
    pub query: String,
    pub gender: String,
    pub accent: String,
    pub category: String,
}

impl VoiceFilter {
    pub fn is_active(&self) -> bool {
        !self.query.trim().is_empty() || !self.gender.is_empty() || !self.accent.is_empty() || !self.category.is_empty()
    }
}

/// Filters by gender/accent/category (normalised equality) and search terms (every term a substring of the name,
/// description, category and all label values).
pub fn filter_voices(voices: &[Voice], f: &VoiceFilter) -> Vec<Voice> {
    let q = norm(&f.query);
    let terms: Vec<&str> = q.split(crate::js::is_js_whitespace).filter(|t| !t.is_empty()).collect();
    voices
        .iter()
        .filter(|v| {
            if !f.gender.is_empty() && norm(v.label_trimmed("gender")) != norm(&f.gender) {
                return false;
            }
            if !f.accent.is_empty() && norm(v.label_trimmed("accent")) != norm(&f.accent) {
                return false;
            }
            if !f.category.is_empty() && norm(v.category.as_deref().unwrap_or("")) != norm(&f.category) {
                return false;
            }
            if terms.is_empty() {
                return true;
            }
            let mut hay = vec![v.name.clone(), v.description.clone().unwrap_or_default(), v.category.clone().unwrap_or_default()];
            hay.extend(v.labels.iter().map(|(_, val)| val.clone()));
            let hay = norm(&hay.join(" "));
            terms.iter().all(|t| hay.contains(t))
        })
        .cloned()
        .collect()
}

/// Distinct filter values (first-seen spelling), sorted case-insensitively.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoiceFacets {
    pub genders: Vec<String>,
    pub accents: Vec<String>,
    pub categories: Vec<String>,
}

pub fn voice_facets(voices: &[Voice]) -> VoiceFacets {
    fn collect(voices: &[Voice], pick: impl Fn(&Voice) -> String) -> Vec<String> {
        let mut seen: Vec<(String, String)> = Vec::new();
        for v in voices {
            let raw = pick(v);
            if !raw.is_empty() && !seen.iter().any(|(k, _)| *k == norm(&raw)) {
                seen.push((norm(&raw), raw));
            }
        }
        let mut out: Vec<String> = seen.into_iter().map(|(_, r)| r).collect();
        out.sort_by_key(|a| a.to_lowercase());
        out
    }
    VoiceFacets {
        genders: collect(voices, |v| v.label_trimmed("gender").to_string()),
        accents: collect(voices, |v| v.label_trimmed("accent").to_string()),
        categories: collect(voices, |v| js_trim(v.category.as_deref().unwrap_or("")).to_string()),
    }
}

/// Label keys the caption already shows.
pub const CAPTION_LABEL_KEYS: [&str; 2] = ["use_case", "use case"];

/// Short descriptive tags, most useful first, de-duplicated case-insensitively, at most `max`.
pub fn voice_tags(v: &Voice, max: usize, skip: &[&str]) -> Vec<String> {
    let order: Vec<&str> = ["accent", "age", "gender", "use_case", "use case", "descriptive", "description"]
        .into_iter()
        .filter(|k| !skip.contains(k))
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let t = title_case(js_trim(s));
        if !t.is_empty() && !out.iter().any(|o| norm(o) == norm(&t)) {
            out.push(t);
        }
    };
    for k in &order {
        if let Some(val) = v.label(k).filter(|s| !s.is_empty()) {
            push(val);
        }
    }
    for (k, val) in &v.labels {
        if !order.contains(&k.as_str()) && !skip.contains(&k.as_str()) && !val.is_empty() {
            push(val);
        }
    }
    out.truncate(max);
    out
}

/// `'Cloned'`, `'Designed'` (generated), `'Professional'`, or `''` for stock voices.
pub fn voice_kind(v: &Voice) -> String {
    let c = norm(v.category.as_deref().unwrap_or(""));
    if c.is_empty() || c == "premade" {
        String::new()
    } else if c == "generated" {
        "Designed".into()
    } else {
        title_case(&c)
    }
}

/// `"Cloned · Narration"`: kind and use case, empty parts dropped.
pub fn voice_caption(v: &Voice) -> String {
    let uc = {
        let a = v.label_trimmed("use_case");
        if a.is_empty() {
            v.label_trimmed("use case")
        } else {
            a
        }
    };
    [voice_kind(v), title_case(uc)].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" \u{00B7} ")
}

/// New-design row subtitle (DESIGN.md Voices: "Male · American · Narration"): gender, accent, use case title-cased;
/// falls back to the kind, the category, then `"Voice"`.
pub fn voice_subtitle(v: &Voice) -> String {
    let uc = {
        let a = v.label_trimmed("use_case");
        if a.is_empty() {
            v.label_trimmed("use case")
        } else {
            a
        }
    };
    let parts: Vec<String> = [v.label_trimmed("gender"), v.label_trimmed("accent"), uc]
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(title_case)
        .collect();
    if !parts.is_empty() {
        return parts.join(" \u{00B7} ");
    }
    let kind = voice_kind(v);
    if !kind.is_empty() {
        return kind;
    }
    match v.category.as_deref().map(js_trim).filter(|c| !c.is_empty()) {
        Some(c) => title_case(c),
        None => "Voice".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice(id: &str, name: &str, cat: Option<&str>, labels: &[(&str, &str)]) -> Voice {
        Voice {
            voice_id: id.into(),
            name: name.into(),
            labels: labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            category: cat.map(Into::into),
            description: None,
            preview_url: None,
        }
    }

    #[test]
    fn titles_and_tags() {
        assert_eq!(title_case("starter_plus"), "Starter plus");
        assert_eq!(title_case("creator"), "Creator");
        let g = voice("v", "George", Some("premade"), &[("accent", "british"), ("age", "middle aged"), ("gender", "male"), ("use_case", "narration"), ("x", "british")]);
        assert_eq!(voice_tags(&g, 4, &[]), vec!["British", "Middle aged", "Male", "Narration"]);
        assert_eq!(voice_tags(&g, 3, &CAPTION_LABEL_KEYS), vec!["British", "Middle aged", "Male"]);
        assert_eq!(voice_caption(&g), "Narration");
        assert_eq!(voice_subtitle(&g), "Male \u{b7} British \u{b7} Narration");
        let c = voice("c", "Mine", Some("generated"), &[]);
        assert_eq!(voice_kind(&c), "Designed");
        assert_eq!(voice_subtitle(&c), "Designed");
    }

    #[test]
    fn sort_filter_facets() {
        let vs = vec![
            voice("1", "zed", Some("premade"), &[("gender", "male")]),
            voice("2", "Anna", Some("cloned"), &[("gender", "Female"), ("accent", "Ghanaian")]),
            voice("3", "bob", None, &[("gender", "female")]),
        ];
        let sorted: Vec<String> = sort_voices_for_page(&vs).into_iter().map(|v| v.name).collect();
        assert_eq!(sorted, vec!["Anna", "bob", "zed"]);
        let f = VoiceFilter { gender: "female".into(), ..Default::default() };
        assert_eq!(filter_voices(&vs, &f).len(), 2);
        let q = VoiceFilter { query: "ghan ann".into(), ..Default::default() };
        assert_eq!(filter_voices(&vs, &q).len(), 1);
        let facets = voice_facets(&vs);
        assert_eq!(facets.genders, vec!["Female", "male"]);
        assert_eq!(facets.categories, vec!["cloned", "premade"]);
    }

    #[test]
    fn credits() {
        let a = AccountInfo { tier: "creator".into(), character_count: 61_234.0, character_limit: 100_000.0, next_reset_unix: Some(1_000_000.0), status: None };
        let s = credit_summary(&a, 1_000_000_000.0 - 12.0 * 86_400_000.0 + 1.0);
        assert_eq!(s.remaining, 38_766.0);
        assert!((s.fraction_left - 0.38766).abs() < 1e-12);
        assert_eq!(s.reset_in_days, Some(12));
    }
}
