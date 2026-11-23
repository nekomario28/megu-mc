// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lightweight UI-facing labels for dynamically registered mod content.

use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct UiModStrings {
    strings: BTreeMap<(String, String), String>,
}

impl UiModStrings {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, locale: &str, id: &str, label: impl Into<String>) {
        self.strings
            .insert((normalize(locale), id.to_owned()), label.into());
    }

    pub fn resolve(&self, id: &str, locale: &str) -> String {
        let locale = normalize(locale);
        self.strings
            .get(&(locale, id.to_owned()))
            .or_else(|| self.strings.get(&("en_us".to_owned(), id.to_owned())))
            .cloned()
            .unwrap_or_else(|| humanize(id))
    }
}

pub fn format_item_tooltip(
    id: &str,
    custom_name: Option<&str>,
    mod_strings: &UiModStrings,
    locale: &str,
) -> (String, String) {
    let namespace = id.split_once(':').map(|(ns, _)| ns).unwrap_or("minecraft");
    let attribution = if namespace == "minecraft" {
        String::new()
    } else {
        namespace_label(namespace)
    };
    (
        custom_name
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                mod_strings.resolve(&format!("item.{}", id.replace(':', ".")), locale)
            }),
        attribution,
    )
}

fn normalize(locale: &str) -> String {
    locale.trim().replace('-', "_").to_ascii_lowercase()
}

fn humanize(id: &str) -> String {
    let name = id
        .split_once(':')
        .map_or(id, |(_, tail)| tail)
        .replace(['_', '-'], " ");
    let mut result = String::new();
    let mut capitalize = true;
    for c in name.chars() {
        if c.is_whitespace() {
            capitalize = true;
            result.push(c);
        } else if capitalize {
            result.extend(c.to_uppercase());
            capitalize = false;
        } else {
            result.push(c);
        }
    }
    result
}

fn namespace_label(namespace: &str) -> String {
    match namespace {
        "create" => "Create".into(),
        "farmersdelight" | "farmers_delight" => "Farmer's Delight".into(),
        other => humanize(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localized_tooltip_has_namespace_attribution() {
        let mut strings = UiModStrings::new();
        strings.insert("ja_jp", "item.create.wrench", "レンチ");
        strings.insert("en_us", "item.create.wrench", "Wrench");
        assert_eq!(
            format_item_tooltip("create:wrench", None, &strings, "ja-JP"),
            ("レンチ".into(), "Create".into())
        );
        assert_eq!(
            format_item_tooltip("farmersdelight:skillet", Some("Old Pan"), &strings, "en_us"),
            ("Old Pan".into(), "Farmer's Delight".into())
        );
        assert_eq!(
            strings.resolve("item.custom:moon_stone", "fr_fr"),
            "Moon Stone"
        );
    }
}
