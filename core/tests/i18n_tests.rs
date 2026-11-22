// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Public-contract tests for the zero-dependency Megu language engine.

use std::collections::BTreeSet;

use mc_spine_core::i18n::{
    detect_os_locale_from_env, parse_locale_code, BuiltinLocale as L, I18nCatalog, I18nError,
    LocaleId, UiTextKey, ALL_BUILTIN_LOCALES, ALL_UI_KEYS, MAX_LANG_JSON_BYTES, MAX_LANG_KEYS,
};

#[test]
fn all_sixteen_locales_have_stable_metadata() {
    assert_eq!(ALL_BUILTIN_LOCALES.len(), 16);
    let mut codes = BTreeSet::new();
    let mut names = BTreeSet::new();
    for &locale in ALL_BUILTIN_LOCALES {
        let info = locale.info();
        assert!(codes.insert(info.code));
        assert!(names.insert(info.native_name));
        assert!(!info.english_name.is_empty());
        assert_eq!(
            LocaleId::parse(info.code)
                .map(|id| id.as_str().to_owned())
                .as_deref(),
            Ok(info.code)
        );
        assert_eq!(parse_locale_code(info.code), Some(locale));
        assert!(ALL_BUILTIN_LOCALES.contains(&info.regional_fallback));
    }
    assert_eq!(L::EnUs.code(), "en_us");
    assert_eq!(L::JaJp.native_name(), "日本語 (日本)");
    assert_eq!(L::ZhTw.regional_fallback(), L::ZhCn);
}

#[test]
fn every_ui_key_is_unique_and_translated_for_every_builtin_locale() {
    assert!(ALL_UI_KEYS.len() >= 60);
    let mut keys = BTreeSet::new();
    for &key in ALL_UI_KEYS {
        assert!(keys.insert(key.code()), "duplicate key: {}", key.code());
        assert_eq!(UiTextKey::from_code(key.code()), Some(key));
    }
    for &locale in ALL_BUILTIN_LOCALES {
        let mut translated = BTreeSet::new();
        let catalog = I18nCatalog::new(locale.code()).expect("builtin locale is valid");
        for &key in ALL_UI_KEYS {
            let value = catalog.resolve_key(key);
            assert!(!value.trim().is_empty(), "{locale:?} {}", key.code());
            assert!(!value.contains("TODO"), "{locale:?} {}", key.code());
            assert!(
                translated.insert(value),
                "{locale:?}: repeated translation {value}"
            );
            assert_eq!(catalog.resolve(key.code()), value);
            assert_eq!(key.builtin_text(locale), value);
        }
        assert_eq!(translated.len(), ALL_UI_KEYS.len(), "{locale:?}");
    }
}

#[test]
fn locale_parser_accepts_posix_tags_and_regional_defaults() {
    for (code, expected) in [
        ("ja_JP.UTF-8", Some(L::JaJp)),
        ("ja-JP", Some(L::JaJp)),
        ("ja", Some(L::JaJp)),
        ("zh_TW.UTF-8", Some(L::ZhTw)),
        ("zh_Hant", Some(L::ZhTw)),
        ("zh_HK", Some(L::ZhTw)),
        ("zh_CN", Some(L::ZhCn)),
        ("zh_Hans", Some(L::ZhCn)),
        ("ko_KR", Some(L::KoKr)),
        ("pt_BR", Some(L::PtBr)),
        ("pt-PT", Some(L::PtBr)),
        ("es_MX", Some(L::EsEs)),
        ("en_GB", Some(L::EnUs)),
        ("uk_UA", Some(L::UkUa)),
        ("id_ID", Some(L::IdId)),
        ("C", Some(L::EnUs)),
        ("POSIX", Some(L::EnUs)),
        ("garbage", None),
        ("", None),
        ("th_TH", None),
    ] {
        assert_eq!(parse_locale_code(code), expected, "input: {code:?}");
    }
}

#[test]
fn env_detection_honors_posix_priority_and_safe_defaults() {
    assert_eq!(
        detect_os_locale_from_env(Some("ja_JP.UTF-8"), None, None),
        L::JaJp
    );
    assert_eq!(
        detect_os_locale_from_env(Some("ja_JP"), Some("es_MX"), Some("zh_TW")),
        L::EsEs
    );
    assert_eq!(
        detect_os_locale_from_env(Some("ja_JP"), Some(""), Some("zh_TW")),
        L::ZhTw
    );
    assert_eq!(detect_os_locale_from_env(None, None, None), L::EnUs);
    assert_eq!(
        detect_os_locale_from_env(Some("garbage"), None, None),
        L::EnUs
    );
    assert_eq!(
        detect_os_locale_from_env(Some("fr_FR"), Some("C"), None),
        L::EnUs
    );
}

#[test]
fn locale_ids_normalize_minecraft_and_bcp47_codes() {
    for (raw, expected) in [
        ("th_TH", "th_th"),
        ("ar-SA", "ar_sa"),
        ("nl_NL", "nl_nl"),
        ("sv_SE", "sv_se"),
        ("en_GB", "en_gb"),
        ("zh-Hant", "zh_hant"),
        ("ast_es", "ast_es"),
        ("pt-BR", "pt_br"),
    ] {
        assert_eq!(
            LocaleId::parse(raw).expect("valid locale").as_str(),
            expected
        );
    }
}

#[test]
fn external_thai_pack_and_missing_english_fallback() {
    let mut catalog = I18nCatalog::new("th_th").expect("Thai locale");
    assert_eq!(
        catalog
            .load_minecraft_lang_json("th_TH", r#"{"play":"เล่น","settings":"การตั้งค่า"}"#.as_bytes()),
        Ok(2)
    );
    assert_eq!(catalog.resolve_key(UiTextKey::Play), "เล่น");
    assert_eq!(catalog.resolve_key(UiTextKey::Settings), "การตั้งค่า");
    assert_eq!(catalog.resolve_key(UiTextKey::Quit), "Quit");
    assert_eq!(
        catalog.resolve("mod.community.something"),
        "mod.community.something"
    );
    let report = catalog.missing_keys_report("th_th");
    assert_eq!(report.len(), ALL_UI_KEYS.len() - 2);
    assert!(!report.contains(&"play"));
    assert!(report.contains(&"quit"));
}

#[test]
fn external_japanese_override_preserves_other_native_translations() {
    let mut catalog = I18nCatalog::new("ja_jp").expect("Japanese locale");
    assert_eq!(catalog.resolve_key(UiTextKey::Play), "プレイ");
    catalog
        .load_minecraft_lang_json("ja_jp", r#"{"play":"あそぶ"}"#.as_bytes())
        .expect("valid pack");
    assert_eq!(catalog.resolve_key(UiTextKey::Play), "あそぶ");
    assert_eq!(catalog.resolve_key(UiTextKey::Settings), "設定");
}

#[test]
fn lookup_order_exact_then_canonical_external_then_builtin() {
    let mut catalog = I18nCatalog::new("es_mx").expect("Mexican Spanish");
    assert_eq!(catalog.resolve_key(UiTextKey::Play), "Jugar");
    catalog
        .load_minecraft_lang_json(
            "es_es",
            r#"{"play":"Jugar en España","settings":"Configuración regional"}"#.as_bytes(),
        )
        .expect("base pack");
    assert_eq!(catalog.resolve_key(UiTextKey::Play), "Jugar en España");
    catalog
        .load_minecraft_lang_json("es_mx", r#"{"play":"Jugar en México"}"#.as_bytes())
        .expect("specific pack");
    assert_eq!(catalog.resolve_key(UiTextKey::Play), "Jugar en México");
    assert_eq!(
        catalog.resolve_key(UiTextKey::Settings),
        "Configuración regional"
    );
    assert_eq!(catalog.resolve_key(UiTextKey::Quit), "Salir");
    catalog.set_locale("en_us").expect("English");
    assert_eq!(catalog.resolve_key(UiTextKey::Play), "Play");
}

#[test]
fn community_language_uses_a_deterministic_same_language_pack() {
    let mut catalog = I18nCatalog::new("ar_eg").expect("Egyptian Arabic");
    catalog
        .load_minecraft_lang_json("ar_sa", r#"{"play":"العب"}"#.as_bytes())
        .expect("Arabic pack");
    assert_eq!(catalog.resolve_key(UiTextKey::Play), "العب");
    assert_eq!(catalog.resolve_key(UiTextKey::Settings), "Settings");
}

#[test]
fn changing_active_locale_validates_without_changing_previous_choice() {
    let mut catalog = I18nCatalog::default();
    assert_eq!(catalog.locale_code(), "en_us");
    catalog.set_locale("vi_VN").expect("Vietnamese");
    assert_eq!(catalog.locale_code(), "vi_vn");
    assert_eq!(
        catalog.set_locale("../evil"),
        Err(I18nError::InvalidLocaleCode)
    );
    assert_eq!(catalog.locale_code(), "vi_vn");
}

#[test]
fn escapes_unicode_and_surrogate_pairs_decode_correctly() {
    let mut catalog = I18nCatalog::default();
    let json = br#"{"sakura":"\u685c","emoji":"\uD83D\uDE80","escapes":"quote:\" slash:\/ backslash:\\ tab:\t newline:\n backspace:\b formfeed:\f return:\r"}"#;
    assert_eq!(catalog.load_minecraft_lang_json("en_us", json), Ok(3));
    assert_eq!(catalog.resolve("sakura"), "桜");
    assert_eq!(catalog.resolve("emoji"), "🚀");
    assert_eq!(
        catalog.resolve("escapes"),
        "quote:\" slash:/ backslash:\\ tab:\t newline:\n backspace:\u{0008} formfeed:\u{000C} return:\r"
    );
}

#[test]
fn malformed_json_and_non_string_values_are_rejected_without_panicking() {
    for bad in [
        &b"[]"[..],
        &b"null"[..],
        &b"true"[..],
        &b"42"[..],
        &b""[..],
        &b"{"[..],
        &b"{\"play\":"[..],
        &b"{\"play\":1}"[..],
        &b"{\"play\":false}"[..],
        &b"{\"play\":null}"[..],
        &b"{\"play\":{}}"[..],
        &b"{\"play\":[]}"[..],
        &b"{\"play\":\"x\",}"[..],
        &b"{\"play\":\"x\" \"quit\":\"y\"}"[..],
        &b"{\"play\":\"x\"} trailing"[..],
        &b"{\"play\":\"x\"}\""[..],
        &b"{\"play\":\"bad\\q\"}"[..],
        &b"{\"play\":\"bad\nnewline\"}"[..],
        &b"{\"play\":\"one\",\"play\":\"two\"}"[..],
        &b"{\"play\":\"\"}"[..],
    ] {
        let mut catalog = I18nCatalog::default();
        assert!(
            catalog.load_minecraft_lang_json("en_us", bad).is_err(),
            "{bad:?}"
        );
    }
}

#[test]
fn unicode_failures_and_invalid_utf8_fail_closed() {
    for bad in [
        &br#"{"a":"\uD83D"}"#[..],
        &br#"{"a":"\uDE80"}"#[..],
        &br#"{"a":"\uD83D\u0041"}"#[..],
        &br#"{"a":"\uD83D\uD83D"}"#[..],
        &br#"{"a":"\uZZZZ"}"#[..],
        &br#"{"a":"\u123"}"#[..],
        &br#"{"a":"\uDE80\uD83D"}"#[..],
    ] {
        let mut catalog = I18nCatalog::default();
        assert_eq!(
            catalog.load_minecraft_lang_json("en_us", bad),
            Err(I18nError::InvalidJson)
        );
    }
    let mut catalog = I18nCatalog::default();
    assert_eq!(
        catalog.load_minecraft_lang_json("en_us", b"{\"a\":\"\xff\"}"),
        Err(I18nError::InvalidUtf8)
    );
}

#[test]
fn invalid_locale_codes_are_rejected_as_pack_names() {
    for bad in [
        "../evil",
        "en_us/../../evil",
        "ja..jp",
        "",
        "C",
        "_en",
        "en_",
        "en__us",
        "a_us",
        "123_45",
        "中文",
        "en us",
        "en.us",
        "en_us\0",
    ] {
        let mut catalog = I18nCatalog::default();
        assert_eq!(
            LocaleId::parse(bad),
            Err(I18nError::InvalidLocaleCode),
            "{bad:?}"
        );
        assert_eq!(
            catalog.load_minecraft_lang_json(bad, b"{}"),
            Err(I18nError::InvalidLocaleCode)
        );
    }
}

#[test]
fn json_input_limits_are_enforced_without_partial_updates() {
    let mut catalog = I18nCatalog::default();
    catalog
        .load_minecraft_lang_json("en_us", br#"{"play":"original"}"#)
        .expect("initial pack");
    let too_large = vec![b' '; MAX_LANG_JSON_BYTES + 1];
    assert_eq!(
        catalog.load_minecraft_lang_json("en_us", &too_large),
        Err(I18nError::OversizedJson)
    );
    let mut many = String::from("{");
    for i in 0..=MAX_LANG_KEYS {
        if i > 0 {
            many.push(',');
        }
        many.push_str(&format!("\"key{i}\":\"value{i}\""));
    }
    many.push('}');
    assert!(many.len() < MAX_LANG_JSON_BYTES);
    assert_eq!(
        catalog.load_minecraft_lang_json("en_us", many.as_bytes()),
        Err(I18nError::TooManyKeys)
    );
    assert_eq!(catalog.resolve_key(UiTextKey::Play), "original");
}

#[test]
fn exactly_two_megabytes_of_json_is_accepted() {
    let mut payload = Vec::with_capacity(MAX_LANG_JSON_BYTES);
    payload.push(b'{');
    payload.extend(std::iter::repeat_n(b' ', MAX_LANG_JSON_BYTES - 2));
    payload.push(b'}');
    let mut catalog = I18nCatalog::default();
    assert_eq!(catalog.load_minecraft_lang_json("en_us", &payload), Ok(0));
}

#[test]
fn malformed_key_and_duplicate_escaped_key_fail_without_overwriting() {
    let mut catalog = I18nCatalog::default();
    catalog
        .load_minecraft_lang_json("en_us", br#"{"play":"kept"}"#)
        .expect("existing");
    assert_eq!(
        catalog.load_minecraft_lang_json("en_us", br#"{"play":"a","pl\u0061y":"b"}"#),
        Err(I18nError::DuplicateKey)
    );
    assert_eq!(
        catalog.load_minecraft_lang_json("en_us", br#"{"":"empty key"}"#),
        Err(I18nError::InvalidKey)
    );
    assert_eq!(
        catalog.load_minecraft_lang_json("en_us", br#"{"\u0000":"control key"}"#),
        Err(I18nError::InvalidKey)
    );
    let long_key = "x".repeat(513);
    let json = format!("{{\"{long_key}\":\"value\"}}");
    assert_eq!(
        catalog.load_minecraft_lang_json("en_us", json.as_bytes()),
        Err(I18nError::InvalidKey)
    );
    assert_eq!(catalog.resolve_key(UiTextKey::Play), "kept");
}

#[test]
fn empty_object_and_whitespace_are_accepted_and_reports_are_deterministic() {
    let mut catalog = I18nCatalog::default();
    assert_eq!(
        catalog.load_minecraft_lang_json("en_us", b"\r\n{\t}\n"),
        Ok(0)
    );
    assert_eq!(
        catalog.missing_keys_report("en_us").len(),
        ALL_UI_KEYS.len()
    );
    assert_eq!(
        catalog.missing_keys_report("invalid/locale").len(),
        ALL_UI_KEYS.len()
    );
    assert_eq!(catalog.resolve_key(UiTextKey::Settings), "Settings");
}
