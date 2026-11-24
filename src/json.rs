//! Shared asset/datapack value parser, independent of terrain generation.
//! Booleans use numeric values, preserving false separately from null/missing.

#[derive(Debug, Clone)]
pub(crate) enum J {
    Null,
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub(crate) fn num(&self) -> f64 {
        match self {
            J::Num(n) => *n,
            _ => 0.0,
        }
    }
    pub(crate) fn str(&self) -> &str {
        match self {
            J::Str(s) => s,
            _ => "",
        }
    }
    pub(crate) fn as_arr(&self) -> Option<&Vec<J>> {
        match self {
            J::Arr(a) => Some(a),
            _ => None,
        }
    }
    pub(crate) fn get(&self, k: &str) -> Option<&J> {
        match self {
            J::Obj(m) => m.iter().find(|(kk, _)| kk == k).map(|(_, v)| v),
            _ => None,
        }
    }
}

const MAX_JSON_DEPTH: usize = 128;

/// Bounds for the shared asset/datapack reader. These are operator-supplied
/// data files, not wire input, so node/byte ceilings stay off (the same
/// convention `proto::table` and `state-store::migrate` use); the depth cap
/// keeps the canonical recursive reader fail-closed. The canonical reader
/// counts the root value at depth 0, so the historical 128-container bound
/// maps to `MAX_JSON_DEPTH` levels of values.
const JSON_LIMITS: megu_core::json::JsonLimits = megu_core::json::JsonLimits {
    max_depth: MAX_JSON_DEPTH,
    max_nodes: usize::MAX,
    max_bytes: usize::MAX,
};

/// Lower the canonical [`megu_core::json::JsonValue`] tree into this module's
/// [`J`] shape. `true`/`false` surface as `Num(1.0)`/`Num(0.0)` exactly as the
/// retired cursor spelled them, so `jbool`/`jnum` callers see no change.
fn lower_json(value: megu_core::json::JsonValue) -> J {
    use megu_core::json::JsonValue as Core;
    match value {
        Core::Null => J::Null,
        Core::Bool(b) => J::Num(if b { 1.0 } else { 0.0 }),
        Core::Number(n) => J::Num(n),
        Core::String(s) => J::Str(s),
        Core::Array(items) => J::Arr(items.into_iter().map(lower_json).collect()),
        Core::Object(pairs) => J::Obj(
            pairs
                .into_iter()
                .map(|(k, v)| (k, lower_json(v)))
                .collect(),
        ),
    }
}

pub(crate) fn jparse(b: &[u8]) -> Option<J> {
    let text = std::str::from_utf8(b).ok()?;
    megu_core::json::parse_json_with_limits(text, JSON_LIMITS, false)
        .ok()
        .map(lower_json)
}

// Typed field accessors shared by the registry, providers and feature parsing.
// They move the existing lookup/defaulting contract; they do not change it.
pub(crate) fn js<'a>(j: &'a J, k: &str) -> Option<&'a J> {
    j.get(k)
}
pub(crate) fn jstr(j: &J, k: &str) -> Option<String> {
    js(j, k).map(|v| v.str().to_string())
}
pub(crate) fn jnum(j: &J, k: &str) -> Option<f64> {
    js(j, k).map(|v| v.num())
}
pub(crate) fn jint(j: &J, k: &str, d: i32) -> i32 {
    jnum(j, k).unwrap_or(d as f64) as i32
}
pub(crate) fn jf32(j: &J, k: &str, d: f32) -> f32 {
    jnum(j, k).unwrap_or(d as f64) as f32
}
pub(crate) fn jbool(j: &J, k: &str, d: bool) -> bool {
    match js(j, k) {
        Some(J::Num(n)) => *n != 0.0,
        Some(J::Str(s)) => s == "true",
        _ => d,
    }
}
pub(crate) fn jarr<'a>(j: &'a J, k: &str) -> Option<&'a Vec<J>> {
    js(j, k).and_then(|v| v.as_arr())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_false_overrides_a_true_default() {
        let value = jparse(br#"{"aquifers_enabled":false,"enabled":true,"unset":null}"#).unwrap();
        assert!(!jbool(&value, "aquifers_enabled", true));
        assert!(jbool(&value, "enabled", false));
        assert!(jbool(&value, "missing", true));
        assert!(jbool(&value, "unset", true));
        assert!(matches!(value.get("unset"), Some(J::Null)));
        assert!(!matches!(value.get("aquifers_enabled"), Some(J::Null)));
    }

    #[test]
    fn deeply_nested_json_fails_closed_without_stack_overflow() {
        let deep_arr = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
        assert!(jparse(deep_arr.as_bytes()).is_none());
        let deep_obj = format!("{}{}", "{\"a\":".repeat(10_000), "}".repeat(10_000));
        assert!(jparse(deep_obj.as_bytes()).is_none());
    }

    #[test]
    fn utf16_surrogate_pair_and_utf8_japanese_decode_cleanly() {
        let value = jparse("{\"icon\":\"\\uD83D\\uDE00\",\"ja\":\"日本語テスト\"}".as_bytes()).unwrap();
        assert_eq!(jstr(&value, "icon").as_deref(), Some("\u{1F600}"));
        assert_eq!(jstr(&value, "ja").as_deref(), Some("日本語テスト"));
        assert!(jparse(b"tr").is_none());
        assert!(jparse(b"nul").is_none());
    }
}
