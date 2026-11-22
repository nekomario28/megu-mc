// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Deterministic UI translations and Minecraft-format `assets/megu/lang/*.json` packs.
//!
//! All built-in translations are static. External packs are loaded atomically
//! after validating a bounded, flat UTF-8 JSON object of string pairs.

use std::collections::BTreeMap;
use std::env;
use std::fmt;

/// Maximum accepted uncompressed JSON payload, in bytes.
pub const MAX_LANG_JSON_BYTES: usize = 2 * 1024 * 1024;
/// Maximum number of translations per external locale.
pub const MAX_LANG_KEYS: usize = 16_384;

/// Tier-1 languages, in their stable catalog-column order.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(usize)]
pub enum BuiltinLocale {
    EnUs,
    JaJp,
    ZhCn,
    ZhTw,
    KoKr,
    EsEs,
    PtBr,
    FrFr,
    DeDe,
    RuRu,
    ItIt,
    PlPl,
    TrTr,
    IdId,
    ViVn,
    UkUa,
}

pub const ALL_BUILTIN_LOCALES: &[BuiltinLocale] = &[
    BuiltinLocale::EnUs,
    BuiltinLocale::JaJp,
    BuiltinLocale::ZhCn,
    BuiltinLocale::ZhTw,
    BuiltinLocale::KoKr,
    BuiltinLocale::EsEs,
    BuiltinLocale::PtBr,
    BuiltinLocale::FrFr,
    BuiltinLocale::DeDe,
    BuiltinLocale::RuRu,
    BuiltinLocale::ItIt,
    BuiltinLocale::PlPl,
    BuiltinLocale::TrTr,
    BuiltinLocale::IdId,
    BuiltinLocale::ViVn,
    BuiltinLocale::UkUa,
];

/// Display metadata, including the next built-in fallback for a locale.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocaleInfo {
    pub code: &'static str,
    pub native_name: &'static str,
    pub english_name: &'static str,
    pub regional_fallback: BuiltinLocale,
}

impl BuiltinLocale {
    pub const fn info(self) -> LocaleInfo {
        use BuiltinLocale as L;
        match self {
            L::EnUs => LocaleInfo {
                code: "en_us",
                native_name: "English (US)",
                english_name: "English (US)",
                regional_fallback: L::EnUs,
            },
            L::JaJp => LocaleInfo {
                code: "ja_jp",
                native_name: "日本語 (日本)",
                english_name: "Japanese (Japan)",
                regional_fallback: L::EnUs,
            },
            L::ZhCn => LocaleInfo {
                code: "zh_cn",
                native_name: "简体中文 (中国)",
                english_name: "Chinese (Simplified, China)",
                regional_fallback: L::EnUs,
            },
            L::ZhTw => LocaleInfo {
                code: "zh_tw",
                native_name: "繁體中文 (台灣)",
                english_name: "Chinese (Traditional, Taiwan)",
                regional_fallback: L::ZhCn,
            },
            L::KoKr => LocaleInfo {
                code: "ko_kr",
                native_name: "한국어 (대한민국)",
                english_name: "Korean (South Korea)",
                regional_fallback: L::EnUs,
            },
            L::EsEs => LocaleInfo {
                code: "es_es",
                native_name: "Español (España)",
                english_name: "Spanish (Spain)",
                regional_fallback: L::EnUs,
            },
            L::PtBr => LocaleInfo {
                code: "pt_br",
                native_name: "Português (Brasil)",
                english_name: "Portuguese (Brazil)",
                regional_fallback: L::EnUs,
            },
            L::FrFr => LocaleInfo {
                code: "fr_fr",
                native_name: "Français (France)",
                english_name: "French (France)",
                regional_fallback: L::EnUs,
            },
            L::DeDe => LocaleInfo {
                code: "de_de",
                native_name: "Deutsch (Deutschland)",
                english_name: "German (Germany)",
                regional_fallback: L::EnUs,
            },
            L::RuRu => LocaleInfo {
                code: "ru_ru",
                native_name: "Русский (Россия)",
                english_name: "Russian (Russia)",
                regional_fallback: L::EnUs,
            },
            L::ItIt => LocaleInfo {
                code: "it_it",
                native_name: "Italiano (Italia)",
                english_name: "Italian (Italy)",
                regional_fallback: L::EnUs,
            },
            L::PlPl => LocaleInfo {
                code: "pl_pl",
                native_name: "Polski (Polska)",
                english_name: "Polish (Poland)",
                regional_fallback: L::EnUs,
            },
            L::TrTr => LocaleInfo {
                code: "tr_tr",
                native_name: "Türkçe (Türkiye)",
                english_name: "Turkish (Türkiye)",
                regional_fallback: L::EnUs,
            },
            L::IdId => LocaleInfo {
                code: "id_id",
                native_name: "Bahasa Indonesia",
                english_name: "Indonesian (Indonesia)",
                regional_fallback: L::EnUs,
            },
            L::ViVn => LocaleInfo {
                code: "vi_vn",
                native_name: "Tiếng Việt",
                english_name: "Vietnamese (Vietnam)",
                regional_fallback: L::EnUs,
            },
            L::UkUa => LocaleInfo {
                code: "uk_ua",
                native_name: "Українська (Україна)",
                english_name: "Ukrainian (Ukraine)",
                regional_fallback: L::EnUs,
            },
        }
    }

    pub const fn code(self) -> &'static str {
        self.info().code
    }
    pub const fn native_name(self) -> &'static str {
        self.info().native_name
    }
    pub const fn english_name(self) -> &'static str {
        self.info().english_name
    }
    pub const fn regional_fallback(self) -> Self {
        self.info().regional_fallback
    }
}

/// Map POSIX, BCP-47, and Minecraft variants to the nearest Tier-1 locale.
pub fn parse_locale_code(raw: &str) -> Option<BuiltinLocale> {
    use BuiltinLocale as L;
    let base = raw
        .trim()
        .split(['.', '@'])
        .next()?
        .to_ascii_lowercase()
        .replace('-', "_");
    let (language, region) = base.split_once('_').unwrap_or((base.as_str(), ""));
    match language {
        "c" | "posix" if region.is_empty() => Some(L::EnUs),
        "en" => Some(L::EnUs),
        "ja" => Some(L::JaJp),
        "zh" => match region {
            "tw" | "hk" | "mo" | "hant" => Some(L::ZhTw),
            _ => Some(L::ZhCn),
        },
        "ko" => Some(L::KoKr),
        "es" => Some(L::EsEs),
        "pt" => Some(L::PtBr),
        "fr" => Some(L::FrFr),
        "de" => Some(L::DeDe),
        "ru" => Some(L::RuRu),
        "it" => Some(L::ItIt),
        "pl" => Some(L::PlPl),
        "tr" => Some(L::TrTr),
        "id" | "in" => Some(L::IdId),
        "vi" => Some(L::ViVn),
        "uk" => Some(L::UkUa),
        _ => None,
    }
}

/// POSIX precedence: LC_ALL, LC_MESSAGES, LANG, then English.
pub fn detect_os_locale_from_env(
    lang: Option<&str>,
    lc_all: Option<&str>,
    lc_messages: Option<&str>,
) -> BuiltinLocale {
    [lc_all, lc_messages, lang]
        .into_iter()
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .find_map(parse_locale_code)
        .unwrap_or(BuiltinLocale::EnUs)
}

/// Read only the process's standard locale environment variables.
pub fn detect_os_locale() -> BuiltinLocale {
    let lang = env::var("LANG").ok();
    let lc_all = env::var("LC_ALL").ok();
    let lc_messages = env::var("LC_MESSAGES").ok();
    detect_os_locale_from_env(lang.as_deref(), lc_all.as_deref(), lc_messages.as_deref())
}

/// Validated, canonical lowercase Minecraft locale identifier.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct LocaleId(String);

impl LocaleId {
    /// Accept language tags of two to eight letters and up to two subtags
    /// of two to eight ASCII alphanumerics. Both `_` and `-` separators work.
    pub fn parse(raw: &str) -> Result<Self, I18nError> {
        if raw.len() > 32 || !raw.is_ascii() {
            return Err(I18nError::InvalidLocaleCode);
        }
        let code = raw.to_ascii_lowercase().replace('-', "_");
        let mut segments = code.split('_');
        let language = segments.next().ok_or(I18nError::InvalidLocaleCode)?;
        if !(2..=8).contains(&language.len()) || !language.bytes().all(|c| c.is_ascii_lowercase()) {
            return Err(I18nError::InvalidLocaleCode);
        }
        let mut subtag_count = 0;
        for subtag in segments {
            subtag_count += 1;
            if subtag_count > 2
                || !(2..=8).contains(&subtag.len())
                || !subtag
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            {
                return Err(I18nError::InvalidLocaleCode);
            }
        }
        Ok(Self(code))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum I18nError {
    InvalidLocaleCode,
    OversizedJson,
    InvalidUtf8,
    InvalidJson,
    TooManyKeys,
    DuplicateKey,
    InvalidKey,
    EmptyTranslation,
}

impl fmt::Display for I18nError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::InvalidLocaleCode => "invalid locale code",
                Self::OversizedJson => "language JSON exceeds 2 MiB",
                Self::InvalidUtf8 => "language JSON is not UTF-8",
                Self::InvalidJson => "expected a flat JSON object of string pairs",
                Self::TooManyKeys => "language JSON has more than 16,384 keys",
                Self::DuplicateKey => "language JSON contains duplicate keys",
                Self::InvalidKey => "language JSON contains an invalid key",
                Self::EmptyTranslation => "language JSON contains an empty translation",
            }
        )
    }
}

impl std::error::Error for I18nError {}

/// Active locale and fully validated Minecraft-format language packs.
#[derive(Clone, Debug)]
pub struct I18nCatalog {
    active: LocaleId,
    packs: BTreeMap<String, BTreeMap<String, String>>,
}

impl Default for I18nCatalog {
    fn default() -> Self {
        Self {
            active: LocaleId("en_us".into()),
            packs: BTreeMap::new(),
        }
    }
}

impl I18nCatalog {
    pub fn new(locale_code: &str) -> Result<Self, I18nError> {
        Ok(Self {
            active: LocaleId::parse(locale_code)?,
            ..Self::default()
        })
    }

    pub fn locale_code(&self) -> &str {
        self.active.as_str()
    }

    pub fn set_locale(&mut self, locale_code: &str) -> Result<(), I18nError> {
        self.active = LocaleId::parse(locale_code)?;
        Ok(())
    }

    /// Load a flat JSON object transactionally. Any error leaves prior data intact.
    pub fn load_minecraft_lang_json(
        &mut self,
        locale_code: &str,
        json_bytes: &[u8],
    ) -> Result<usize, I18nError> {
        let locale = LocaleId::parse(locale_code)?;
        if json_bytes.len() > MAX_LANG_JSON_BYTES {
            return Err(I18nError::OversizedJson);
        }
        std::str::from_utf8(json_bytes).map_err(|_| I18nError::InvalidUtf8)?;
        let translations = JsonParser::new(json_bytes).parse_object()?;
        let count = translations.len();
        self.packs.insert(locale.0, translations);
        Ok(count)
    }

    /// Exact external pack → regional/base external pack → built-in locale →
    /// built-in English → raw key. Unknown keys are safe to pass through.
    pub fn resolve<'a>(&'a self, key: &'a str) -> &'a str {
        if let Some(value) = self
            .packs
            .get(self.active.as_str())
            .and_then(|pack| pack.get(key))
        {
            return value;
        }
        let language = self.active.as_str().split('_').next().unwrap_or("");
        let builtin = parse_locale_code(self.active.as_str());
        // Prefer the canonical regional pack, then the first same-language
        // pack in BTreeMap order for community-only languages.
        let canonical = builtin.map(BuiltinLocale::code);
        if let Some(code) = canonical.filter(|code| *code != self.active.as_str()) {
            if let Some(value) = self.packs.get(code).and_then(|pack| pack.get(key)) {
                return value;
            }
        }
        for (code, pack) in &self.packs {
            if code != self.active.as_str()
                && code.split('_').next() == Some(language)
                && Some(code.as_str()) != canonical
            {
                if let Some(value) = pack.get(key) {
                    return value;
                }
            }
        }
        if let Some(ui_key) = UiTextKey::from_code(key) {
            return ui_key.builtin_text(builtin.unwrap_or(BuiltinLocale::EnUs));
        }
        key
    }

    pub fn resolve_key(&self, key: UiTextKey) -> &str {
        self.resolve(key.code())
    }

    /// Canonical keys absent from the *exact* external pack, for translators.
    /// A missing pack (or invalid code) reports every canonical key.
    pub fn missing_keys_report(&self, locale_code: &str) -> Vec<&'static str> {
        let pack = LocaleId::parse(locale_code)
            .ok()
            .and_then(|locale| self.packs.get(locale.as_str()));
        ALL_UI_KEYS
            .iter()
            .map(|key| key.code())
            .filter(|key| pack.is_none_or(|entries| !entries.contains_key(*key)))
            .collect()
    }
}

/// A bounded, strict parser for vanilla's flat `assets/<namespace>/lang/*.json`.
struct JsonParser<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn whitespace(&mut self) {
        while matches!(self.data.get(self.pos), Some(b' ' | b'\r' | b'\n' | b'\t')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), I18nError> {
        if self.data.get(self.pos) == Some(&expected) {
            self.pos += 1;
            Ok(())
        } else {
            Err(I18nError::InvalidJson)
        }
    }

    fn parse_object(mut self) -> Result<BTreeMap<String, String>, I18nError> {
        self.whitespace();
        self.expect(b'{')?;
        self.whitespace();
        let mut result = BTreeMap::new();
        if self.data.get(self.pos) == Some(&b'}') {
            self.pos += 1;
        } else {
            loop {
                let key = self.string()?;
                if key.is_empty() || key.len() > 512 || key.chars().any(char::is_control) {
                    return Err(I18nError::InvalidKey);
                }
                self.whitespace();
                self.expect(b':')?;
                self.whitespace();
                let value = self.string()?;
                if value.is_empty() {
                    return Err(I18nError::EmptyTranslation);
                }
                if result.contains_key(&key) {
                    return Err(I18nError::DuplicateKey);
                }
                if result.len() >= MAX_LANG_KEYS {
                    return Err(I18nError::TooManyKeys);
                }
                result.insert(key, value);
                self.whitespace();
                match self.data.get(self.pos) {
                    Some(b',') => {
                        self.pos += 1;
                        self.whitespace();
                    }
                    Some(b'}') => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Err(I18nError::InvalidJson),
                }
            }
        }
        self.whitespace();
        if self.pos == self.data.len() {
            Ok(result)
        } else {
            Err(I18nError::InvalidJson)
        }
    }

    fn string(&mut self) -> Result<String, I18nError> {
        self.expect(b'"')?;
        let mut result = String::new();
        let mut start = self.pos;
        while let Some(&byte) = self.data.get(self.pos) {
            match byte {
                b'"' => {
                    result.push_str(
                        std::str::from_utf8(&self.data[start..self.pos])
                            .map_err(|_| I18nError::InvalidUtf8)?,
                    );
                    self.pos += 1;
                    return Ok(result);
                }
                b'\\' => {
                    result.push_str(
                        std::str::from_utf8(&self.data[start..self.pos])
                            .map_err(|_| I18nError::InvalidUtf8)?,
                    );
                    self.pos += 1;
                    let escaped = *self.data.get(self.pos).ok_or(I18nError::InvalidJson)?;
                    self.pos += 1;
                    match escaped {
                        b'"' => result.push('"'),
                        b'\\' => result.push('\\'),
                        b'/' => result.push('/'),
                        b'b' => result.push('\u{0008}'),
                        b'f' => result.push('\u{000C}'),
                        b'n' => result.push('\n'),
                        b'r' => result.push('\r'),
                        b't' => result.push('\t'),
                        b'u' => result.push(self.unicode_escape()?),
                        _ => return Err(I18nError::InvalidJson),
                    }
                    start = self.pos;
                }
                0..=31 => return Err(I18nError::InvalidJson),
                _ => self.pos += 1,
            }
        }
        Err(I18nError::InvalidJson)
    }

    fn unicode_escape(&mut self) -> Result<char, I18nError> {
        let high = self.hex4()?;
        let codepoint = if (0xD800..=0xDBFF).contains(&high) {
            self.expect(b'\\')?;
            self.expect(b'u')?;
            let low = self.hex4()?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(I18nError::InvalidJson);
            }
            0x10000 + ((u32::from(high) - 0xD800) << 10) + u32::from(low) - 0xDC00
        } else {
            u32::from(high)
        };
        char::from_u32(codepoint).ok_or(I18nError::InvalidJson)
    }

    fn hex4(&mut self) -> Result<u16, I18nError> {
        let mut code = 0_u16;
        for _ in 0..4 {
            let byte = *self.data.get(self.pos).ok_or(I18nError::InvalidJson)?;
            self.pos += 1;
            let digit = char::from(byte)
                .to_digit(16)
                .ok_or(I18nError::InvalidJson)?;
            code = (code << 4) | digit as u16;
        }
        Ok(code)
    }
}

macro_rules! ui_catalog {
    ($( $name:ident, $code:literal => [$($text:expr),+ $(,)?]; )+) => {
        /// Stable, typed identifiers for built-in Megu UI text.
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
        #[repr(usize)]
        pub enum UiTextKey { $($name),+ }

        pub const ALL_UI_KEYS: &[UiTextKey] = &[$(UiTextKey::$name),+];

        // The array dimension enforces exactly sixteen translations per key.
        const BUILTIN_TEXT: &[[&str; 16]] = &[$([$($text),+]),+];

        impl UiTextKey {
            pub const fn code(self) -> &'static str {
                match self { $( Self::$name => $code, )+ }
            }

            pub fn from_code(raw: &str) -> Option<Self> {
                match raw { $( $code => Some(Self::$name), )+ _ => None }
            }

            pub fn builtin_text(self, locale: BuiltinLocale) -> &'static str {
                BUILTIN_TEXT.get(self as usize)
                    .and_then(|row| row.get(locale as usize))
                    .copied()
                    .unwrap_or(self.code())
            }
        }
    };
}

ui_catalog! {
    Play, "play" => ["Play", "プレイ", "开始游戏", "開始遊戲", "플레이", "Jugar", "Jogar", "Jouer", "Spielen", "Играть", "Gioca", "Graj", "Oyna", "Main", "Chơi", "Грати"];
    Singleplayer, "singleplayer" => ["Singleplayer", "シングルプレイ", "单人游戏", "單人遊戲", "싱글플레이", "Un jugador", "Um jogador", "Solo", "Einzelspieler", "Одиночная игра", "Giocatore singolo", "Jeden gracz", "Tek oyunculu", "Pemain tunggal", "Chơi đơn", "Одиночна гра"];
    Multiplayer, "multiplayer" => ["Multiplayer", "マルチプレイ", "多人游戏", "多人遊戲", "멀티플레이", "Multijugador", "Multijogador", "Multijoueur", "Mehrspieler", "Сетевая игра", "Multigiocatore", "Wieloosobowa", "Çok oyunculu", "Multipemain", "Nhiều người chơi", "Мережева гра"];
    Friends, "friends" => ["Play with Friends", "フレンドと遊ぶ", "与好友一起玩", "與好友同樂", "친구와 함께 플레이", "Jugar con amigos", "Jogar com amigos", "Jouer avec des amis", "Mit Freunden spielen", "Играть с друзьями", "Gioca con gli amici", "Graj ze znajomymi", "Arkadaşlarla oyna", "Main bersama teman", "Chơi cùng bạn bè", "Грати з друзями"];
    Mods, "mods" => ["Mods & Packs", "Modとパック", "模组与资源包", "模組與資源包", "모드 및 리소스 팩", "Mods y paquetes", "Mods e pacotes", "Mods et packs", "Mods und Pakete", "Моды и наборы", "Mod e pacchetti", "Mody i paczki", "Modlar ve paketler", "Mod dan paket", "Mod và gói tài nguyên", "Моди й набори"];
    Settings, "settings" => ["Settings", "設定", "设置", "設定", "설정", "Ajustes", "Configurações", "Paramètres", "Einstellungen", "Настройки", "Impostazioni", "Ustawienia", "Ayarlar", "Pengaturan", "Cài đặt", "Налаштування"];
    About, "about" => ["About", "情報", "关于", "關於", "정보", "Acerca de", "Sobre", "À propos", "Über", "О программе", "Informazioni", "Informacje", "Hakkında", "Tentang", "Giới thiệu", "Про програму"];
    Quit, "quit" => ["Quit", "終了", "退出", "離開", "종료", "Salir", "Sair", "Quitter", "Beenden", "Выход", "Esci", "Wyjdź", "Çıkış", "Keluar", "Thoát", "Вийти"];
    Support, "support" => ["Support", "支援する", "支持项目", "支持專案", "후원하기", "Apoyar", "Apoiar", "Soutenir", "Unterstützen", "Поддержать", "Sostieni", "Wesprzyj", "Destekle", "Dukung", "Ủng hộ", "Підтримати"];
    Graphics, "graphics" => ["Graphics", "グラフィックス", "图形", "圖形", "그래픽", "Gráficos", "Gráficos", "Graphismes", "Grafik", "Графика", "Grafica", "Grafika", "Grafikler", "Grafis", "Đồ họa", "Графіка"];
    Shaders, "shaders" => ["Shaders", "シェーダー", "光影", "光影效果", "셰이더", "Sombreadores", "Shaders", "Shaders", "Shader", "Шейдеры", "Shader", "Shadery", "Gölgelendiriciler", "Shader", "Bộ đổ bóng", "Шейдери"];
    NatureAndEffects, "nature_and_effects" => ["Nature & Effects", "自然とエフェクト", "自然与特效", "自然與特效", "자연 및 효과", "Naturaleza y efectos", "Natureza e efeitos", "Nature et effets", "Natur und Effekte", "Природа и эффекты", "Natura ed effetti", "Przyroda i efekty", "Doğa ve efektler", "Alam dan efek", "Thiên nhiên và hiệu ứng", "Природа й ефекти"];
    Audio, "audio" => ["Audio", "オーディオ", "音频", "音效", "오디오", "Sonido", "Áudio", "Audio", "Audio", "Звук", "Audio", "Dźwięk", "Ses", "Audio", "Âm thanh", "Звук"];
    Controls, "controls" => ["Controls", "操作設定", "控制", "操作設定", "조작", "Controles", "Controles", "Commandes", "Steuerung", "Управление", "Comandi", "Sterowanie", "Kontroller", "Kontrol", "Điều khiển", "Керування"];
    Accessibility, "accessibility" => ["Accessibility", "アクセシビリティ", "辅助功能", "無障礙設定", "접근성", "Accesibilidad", "Acessibilidade", "Accessibilité", "Barrierefreiheit", "Специальные возможности", "Accessibilità", "Ułatwienia dostępu", "Erişilebilirlik", "Aksesibilitas", "Trợ năng", "Доступність"];
    Minihud, "minihud" => ["MiniHUD", "ミニHUD", "迷你信息栏", "迷你資訊欄", "미니 HUD", "MiniHUD", "MiniHUD", "MiniHUD", "MiniHUD", "Мини-индикаторы", "MiniHUD", "MiniHUD", "Mini HUD", "MiniHUD", "HUD thu nhỏ", "Мініпанель"];
    Language, "language" => ["Language", "言語", "语言", "語言", "언어", "Idioma", "Idioma", "Langue", "Sprache", "Язык", "Lingua", "Język", "Dil", "Bahasa", "Ngôn ngữ", "Мова"];
    Qol, "qol" => ["Quality of Life", "便利機能", "便捷功能", "便利功能", "편의 기능", "Comodidad", "Conveniência", "Confort de jeu", "Komfortfunktionen", "Удобство игры", "Comodità", "Udogodnienia", "Yaşam kalitesi", "Kenyamanan bermain", "Tiện ích", "Зручність гри"];
    FairPlay, "fair_play" => ["Fair Play", "フェアプレイ", "公平游戏", "公平遊戲", "공정한 플레이", "Juego limpio", "Jogo justo", "Jeu équitable", "Fairplay", "Честная игра", "Gioco leale", "Uczciwa gra", "Adil oyun", "Permainan adil", "Chơi công bằng", "Чесна гра"];
    RenderDistance, "render_distance" => ["Render Distance", "描画距離", "渲染距离", "算繪距離", "렌더 거리", "Distancia de renderizado", "Distância de renderização", "Distance d'affichage", "Sichtweite", "Дальность прорисовки", "Distanza di rendering", "Odległość renderowania", "Görüş mesafesi", "Jarak render", "Khoảng cách hiển thị", "Дальність промальовування"];
    FpsCap, "fps_cap" => ["FPS Cap", "FPS上限", "帧率上限", "FPS 上限", "FPS 제한", "Límite de FPS", "Limite de FPS", "Limite d'IPS", "FPS-Begrenzung", "Ограничение FPS", "Limite FPS", "Limit FPS", "FPS sınırı", "Batas FPS", "Giới hạn FPS", "Обмеження FPS"];
    Vsync, "vsync" => ["VSync", "垂直同期", "垂直同步", "垂直同步", "수직 동기화", "Sincronización vertical", "Sincronização vertical", "Synchronisation verticale", "Vertikale Synchronisation", "Вертикальная синхронизация", "Sincronizzazione verticale", "Synchronizacja pionowa", "Dikey eşitleme", "Sinkronisasi vertikal", "Đồng bộ dọc", "Вертикальна синхронізація"];
    Fullscreen, "fullscreen" => ["Fullscreen", "フルスクリーン", "全屏", "全螢幕", "전체 화면", "Pantalla completa", "Tela cheia", "Plein écran", "Vollbild", "Полный экран", "Schermo intero", "Pełny ekran", "Tam ekran", "Layar penuh", "Toàn màn hình", "Повний екран"];
    GuiScale, "gui_scale" => ["GUI Scale", "GUIの大きさ", "界面缩放", "介面縮放", "GUI 크기", "Escala de interfaz", "Escala da interface", "Échelle de l'interface", "GUI-Skalierung", "Масштаб интерфейса", "Scala interfaccia", "Skala interfejsu", "Arayüz ölçeği", "Skala antarmuka", "Tỷ lệ giao diện", "Масштаб інтерфейсу"];
    ShaderPreset, "shader_preset" => ["Shader Preset", "シェーダープリセット", "光影预设", "光影預設", "셰이더 프리셋", "Ajuste de sombreado", "Predefinição de shaders", "Préréglage de shaders", "Shader-Voreinstellung", "Набор настроек шейдеров", "Preimpostazione shader", "Profil shaderów", "Gölgelendirici ön ayarı", "Preset shader", "Cấu hình đổ bóng", "Набір налаштувань шейдерів"];
    PresetVanilla, "preset_vanilla" => ["Vanilla", "バニラ", "原版风格", "原版風格", "바닐라", "Original", "Padrão", "Classique", "Vanilla", "Оригинальный стиль", "Originale", "Klasyczny", "Varsayılan", "Bawaan", "Nguyên bản", "Оригінальний стиль"];
    PresetIneiWabisabi, "preset_inei_wabisabi" => ["Inei Wabi-Sabi", "陰翳・侘び寂び", "光影侘寂", "陰翳侘寂", "음영과 와비사비", "Luces y sombras wabi-sabi", "Luz e sombra wabi-sabi", "Ombres et wabi-sabi", "Licht und Wabi-Sabi", "Светотень и ваби-саби", "Ombre e wabi-sabi", "Światłocień wabi-sabi", "Işık ve wabi-sabi", "Cahaya dan wabi-sabi", "Ánh sáng và wabi-sabi", "Світлотінь і вабі-сабі"];
    PresetFeatherLowSpec, "preset_feather_low_spec" => ["Feather (Low-Spec)", "フェザー（軽量）", "羽量级（低配置）", "羽量級（低配備）", "페더 (저사양)", "Pluma (bajo consumo)", "Leve (hardware básico)", "Plume (petite configuration)", "Federleicht (sparsam)", "Лёгкий (слабый ПК)", "Piuma (bassi requisiti)", "Lekki (słaby sprzęt)", "Hafif (düşük donanım)", "Ringan (perangkat sederhana)", "Siêu nhẹ (máy yếu)", "Легкий (слабкий ПК)"];
    BackgroundFps, "background_fps" => ["Background FPS", "バックグラウンドFPS", "后台帧率", "背景 FPS", "백그라운드 FPS", "FPS en segundo plano", "FPS em segundo plano", "IPS en arrière-plan", "Hintergrund-FPS", "FPS в фоне", "FPS in background", "FPS w tle", "Arka plan FPS", "FPS latar belakang", "FPS nền", "FPS у фоні"];
    VanillaPlusNature, "vanilla_plus_nature" => ["Vanilla+ Nature", "バニラ＋自然表現", "原版增强自然效果", "原版增強自然效果", "바닐라+ 자연 효과", "Naturaleza Vanilla+", "Natureza Vanilla+", "Nature Vanilla+", "Vanilla+-Natur", "Природа Vanilla+", "Natura Vanilla+", "Przyroda Vanilla+", "Vanilla+ doğa", "Alam Vanilla+", "Thiên nhiên Vanilla+", "Природа Vanilla+"];
    TactileEffects, "tactile_effects" => ["Tactile Effects", "手触りのある演出", "触感反馈效果", "觸覺回饋效果", "촉각 효과", "Efectos táctiles", "Efeitos táteis", "Effets tactiles", "Haptische Effekte", "Тактильные эффекты", "Effetti tattili", "Efekty dotykowe", "Dokunsal efektler", "Efek taktil", "Hiệu ứng xúc giác", "Тактильні ефекти"];
    PresetOff, "preset_off" => ["Effects Off", "演出なし", "关闭特效", "關閉特效", "효과 끔", "Sin efectos", "Sem efeitos", "Effets désactivés", "Effekte aus", "Эффекты выключены", "Effetti disattivati", "Efekty wyłączone", "Efektler kapalı", "Efek mati", "Tắt hiệu ứng", "Ефекти вимкнено"];
    PresetSubtle, "preset_subtle" => ["Subtle", "控えめ", "轻微", "細膩", "은은하게", "Sutil", "Sutil", "Discret", "Dezent", "Ненавязчиво", "Discreto", "Subtelne", "Hafif", "Halus", "Tinh tế", "Ледь помітно"];
    PresetFancy, "preset_fancy" => ["Fancy", "華やか", "精美", "華麗", "화려하게", "Detallado", "Sofisticado", "Détaillé", "Aufwendig", "Красочно", "Elaborato", "Efektowne", "Gösterişli", "Mewah", "Lộng lẫy", "Барвисто"];
    WaterAndLavaEffects, "water_and_lava_effects" => ["Water & Lava Effects", "水と溶岩の演出", "水与熔岩特效", "水與熔岩特效", "물과 용암 효과", "Efectos de agua y lava", "Efeitos de água e lava", "Effets d'eau et de lave", "Wasser- und Lavaeffekte", "Эффекты воды и лавы", "Effetti acqua e lava", "Efekty wody i lawy", "Su ve lav efektleri", "Efek air dan lava", "Hiệu ứng nước và dung nham", "Ефекти води й лави"];
    TreeShakeAndLeaves, "tree_shake_and_leaves" => ["Tree Sway & Falling Leaves", "木々の揺れと落ち葉", "树木摇曳与落叶", "樹木搖曳與落葉", "나무 흔들림과 낙엽", "Árboles y hojas que caen", "Árvores e folhas ao vento", "Arbres et feuilles au vent", "Baumbewegung und Laubfall", "Колыхание деревьев и листопад", "Alberi mossi e foglie cadenti", "Kołysanie drzew i opadanie liści", "Sallanan ağaçlar ve yapraklar", "Pohon bergoyang dan daun gugur", "Cây đung đưa và lá rơi", "Коливання дерев і листопад"];
    ColdBreathAndEmbers, "cold_breath_and_embers" => ["Cold Breath & Embers", "白い息と火の粉", "寒气与余烬", "白霧吐息與餘燼", "입김과 불씨", "Vaho y brasas", "Bafo frio e brasas", "Souffle froid et braises", "Atemwolken und Funken", "Пар изо рта и искры", "Fiato freddo e braci", "Para z ust i iskry", "Soğuk nefes ve közler", "Napas dingin dan bara", "Hơi thở lạnh và tàn lửa", "Пара з рота й жарини"];
    ShootingStarsAndAurora, "shooting_stars_and_aurora" => ["Shooting Stars & Aurora", "流れ星とオーロラ", "流星与极光", "流星與極光", "별똥별과 오로라", "Estrellas fugaces y auroras", "Estrelas cadentes e auroras", "Étoiles filantes et aurores", "Sternschnuppen und Polarlichter", "Падающие звёзды и сияние", "Stelle cadenti e aurore", "Spadające gwiazdy i zorze", "Kayan yıldızlar ve kutup ışıkları", "Bintang jatuh dan aurora", "Sao băng và cực quang", "Метеори й полярне сяйво"];
    CameraMicroMotion, "camera_micro_motion" => ["Subtle Camera Motion", "カメラの微細な揺れ", "镜头细微晃动", "鏡頭細微晃動", "미세 카메라 움직임", "Movimiento sutil de cámara", "Movimento sutil da câmera", "Léger mouvement de caméra", "Dezente Kamerabewegung", "Лёгкое движение камеры", "Movimento lieve della visuale", "Delikatny ruch kamery", "Hafif kamera hareketi", "Gerakan kamera halus", "Chuyển động máy quay nhẹ", "Легкий рух камери"];
    SeasonModSync, "season_mod_sync" => ["Season Mod Sync", "季節Modとの連動", "季节模组同步", "季節模組同步", "계절 모드 연동", "Sincronizar mod de estaciones", "Sincronizar mod de estações", "Synchronisation des saisons", "Saison-Mod-Synchronisierung", "Синхронизация с модом сезонов", "Sincronizza mod stagioni", "Synchronizacja moda pór roku", "Mevsim modu eşitleme", "Sinkronisasi mod musim", "Đồng bộ mod mùa", "Синхронізація мода сезонів"];
    SmoothLighting, "smooth_lighting" => ["Smooth Lighting", "スムースライティング", "平滑光照", "平滑光照", "부드러운 조명", "Iluminación suave", "Iluminação suave", "Éclairage adouci", "Weiche Beleuchtung", "Плавное освещение", "Illuminazione uniforme", "Płynne oświetlenie", "Yumuşak aydınlatma", "Pencahayaan halus", "Ánh sáng mượt", "Плавне освітлення"];
    ModMenu, "mod_menu" => ["Mod Manager", "Mod管理", "模组管理", "模組管理", "모드 관리자", "Gestor de mods", "Gerenciador de mods", "Gestionnaire de mods", "Mod-Verwaltung", "Управление модами", "Gestione mod", "Menedżer modów", "Mod yöneticisi", "Pengelola mod", "Quản lý mod", "Керування модами"];
    RecipeBrowser, "recipe_browser" => ["Recipe Browser", "レシピブラウザ", "配方浏览器", "配方瀏覽器", "제작법 탐색기", "Explorador de recetas", "Explorador de receitas", "Explorateur de recettes", "Rezeptbrowser", "Просмотр рецептов", "Ricettario", "Przeglądarka receptur", "Tarif tarayıcısı", "Penjelajah resep", "Tra cứu công thức", "Перегляд рецептів"];
    ModSyncConsent, "mod_sync_consent" => ["Server Mod Sync Permission", "サーバーMod同期の許可", "服务器模组同步授权", "伺服器模組同步授權", "서버 모드 동기화 동의", "Permiso para sincronizar mods", "Permissão para sincronizar mods", "Autoriser la synchronisation des mods", "Mod-Synchronisierung erlauben", "Разрешение синхронизации модов", "Autorizza sincronizzazione mod", "Zgoda na synchronizację modów", "Mod eşitleme izni", "Izin sinkronisasi mod server", "Cho phép đồng bộ mod máy chủ", "Дозвіл на синхронізацію модів"];
    ModSecurityQuarantined, "mod_security_quarantined" => ["Mod Quarantined", "Modを隔離しました", "模组已隔离", "模組已隔離", "모드 격리됨", "Mod en cuarentena", "Mod em quarentena", "Mod mis en quarantaine", "Mod unter Quarantäne", "Мод помещён в карантин", "Mod in quarantena", "Mod poddany kwarantannie", "Mod karantinaya alındı", "Mod dikarantina", "Mod đã bị cách ly", "Мод переміщено до карантину"];
    ModAutoRoutedWgpu, "mod_auto_routed_wgpu" => ["Mod Routed to wgpu", "Modをwgpuへ自動振り分け", "模组已转交 wgpu", "模組已轉交 wgpu", "모드가 wgpu로 자동 연결됨", "Mod derivado a wgpu", "Mod direcionado para wgpu", "Mod redirigé vers wgpu", "Mod an wgpu weitergeleitet", "Мод перенаправлен в wgpu", "Mod instradato a wgpu", "Mod skierowany do wgpu", "Mod wgpu'ya yönlendirildi", "Mod dialihkan ke wgpu", "Mod được chuyển sang wgpu", "Мод перенаправлено до wgpu"];
    CrashRecoveryTitle, "crash_recovery_title" => ["Crash Recovery", "クラッシュからの復旧", "崩溃恢复", "當機復原", "충돌 복구", "Recuperación tras cierre inesperado", "Recuperação de falha", "Récupération après plantage", "Wiederherstellung nach Absturz", "Восстановление после сбоя", "Ripristino dopo arresto anomalo", "Odzyskiwanie po awarii", "Çökme kurtarma", "Pemulihan setelah crash", "Khôi phục sau sự cố", "Відновлення після збою"];
    Back, "back" => ["Back", "戻る", "返回", "返回上頁", "뒤로", "Atrás", "Voltar", "Retour", "Zurück", "Назад", "Indietro", "Wstecz", "Geri", "Kembali", "Quay lại", "Назад"];
    Done, "done" => ["Done", "完了", "完成", "完成設定", "완료", "Hecho", "Concluído", "Terminé", "Fertig", "Готово", "Fine", "Gotowe", "Bitti", "Selesai", "Xong", "Готово"];
    Cancel, "cancel" => ["Cancel", "キャンセル", "取消", "取消操作", "취소", "Cancelar", "Cancelar", "Annuler", "Abbrechen", "Отмена", "Annulla", "Anuluj", "İptal", "Batal", "Hủy", "Скасувати"];
    ResetDefaults, "reset_defaults" => ["Reset to Defaults", "初期設定に戻す", "恢复默认设置", "還原預設設定", "기본값으로 초기화", "Restablecer valores", "Restaurar padrões", "Rétablir les valeurs par défaut", "Standardwerte wiederherstellen", "Сбросить настройки", "Ripristina predefiniti", "Przywróć domyślne", "Varsayılanlara sıfırla", "Atur ulang ke bawaan", "Đặt lại mặc định", "Скинути до типових"];
    SearchOptions, "search_options" => ["Search Options", "設定を検索", "搜索选项", "搜尋選項", "옵션 검색", "Buscar opciones", "Buscar opções", "Rechercher des options", "Optionen suchen", "Поиск параметров", "Cerca opzioni", "Szukaj opcji", "Seçenekleri ara", "Cari opsi", "Tìm tùy chọn", "Пошук параметрів"];
    On, "on" => ["On", "オン", "开", "開啟", "켜짐", "Sí", "Ligado", "Activé", "An", "Вкл.", "Acceso", "Wł.", "Açık", "Nyala", "Bật", "Увімк."];
    Off, "off" => ["Off", "オフ", "关", "關閉", "꺼짐", "No", "Desligado", "Désactivé", "Aus", "Выкл.", "Spento", "Wył.", "Kapalı", "Mati", "Tắt", "Вимк."];
    Enabled, "enabled" => ["Enabled", "有効", "已启用", "已啟用", "활성화됨", "Activado", "Ativado", "En service", "Aktiviert", "Включено", "Abilitato", "Włączone", "Etkin", "Diaktifkan", "Đã bật", "Увімкнено"];
    Disabled, "disabled" => ["Disabled", "無効", "已禁用", "已停用", "비활성화됨", "Desactivado", "Desativado", "Hors service", "Deaktiviert", "Отключено", "Disabilitato", "Wyłączone", "Devre dışı", "Dinonaktifkan", "Đã tắt", "Вимкнено"];
    PauseTitle, "pause_title" => ["Game Paused", "一時停止中", "游戏已暂停", "遊戲已暫停", "게임 일시 정지", "Juego en pausa", "Jogo pausado", "Jeu en pause", "Spiel pausiert", "Пауза", "Gioco in pausa", "Gra wstrzymana", "Oyun duraklatıldı", "Permainan dijeda", "Trò chơi tạm dừng", "Гру призупинено"];
    ResumeGame, "resume_game" => ["Back to Game", "ゲームに戻る", "返回游戏", "返回遊戲", "게임으로 돌아가기", "Volver al juego", "Voltar ao jogo", "Reprendre la partie", "Zurück zum Spiel", "Вернуться в игру", "Torna al gioco", "Wróć do gry", "Oyuna dön", "Kembali ke permainan", "Quay lại trò chơi", "Повернутися до гри"];
    SaveAndQuit, "save_and_quit" => ["Save & Quit to Title", "保存してタイトルへ", "保存并返回标题", "儲存並返回標題", "저장 후 타이틀로", "Guardar y salir al menú", "Salvar e sair para o menu", "Sauvegarder et quitter", "Speichern und zum Menü", "Сохранить и выйти в меню", "Salva e torna al titolo", "Zapisz i wróć do menu", "Kaydet ve menüye dön", "Simpan dan kembali ke menu", "Lưu và về màn hình chính", "Зберегти й вийти до меню"];
    AcceptAndInstall, "accept_and_install" => ["Accept & Install", "承認してインストール", "接受并安装", "同意並安裝", "동의하고 설치", "Aceptar e instalar", "Aceitar e instalar", "Accepter et installer", "Annehmen und installieren", "Принять и установить", "Accetta e installa", "Zaakceptuj i zainstaluj", "Kabul et ve yükle", "Setujui dan pasang", "Chấp nhận và cài đặt", "Прийняти й установити"];
    Decline, "decline" => ["Decline", "拒否", "拒绝", "拒絕", "거부", "Rechazar", "Recusar", "Refuser", "Ablehnen", "Отклонить", "Rifiuta", "Odrzuć", "Reddet", "Tolak", "Từ chối", "Відхилити"];
    Modpacks, "modpacks" => ["Modpacks", "Modパック", "整合包", "模組包", "모드팩", "Paquetes de mods", "Pacotes de mods", "Packs de mods", "Modpakete", "Сборки модов", "Pacchetti di mod", "Paczki modów", "Mod paketleri", "Paket mod", "Gói mod", "Збірки модів"];
    ReturnToTitle, "return_to_title" => ["Return to Title", "タイトルへ戻る", "返回主菜单", "返回主選單", "타이틀로 돌아가기", "Volver al título", "Voltar à tela inicial", "Retour au menu principal", "Zum Hauptmenü", "В главное меню", "Torna al titolo", "Wróć do menu głównego", "Ana menüye dön", "Kembali ke menu utama", "Về màn hình tiêu đề", "До головного меню"];
    SendReport, "send_report" => ["Send Crash Report", "クラッシュレポートを送信", "发送崩溃报告", "傳送當機報告", "충돌 보고서 보내기", "Enviar informe de error", "Enviar relatório de falha", "Envoyer un rapport de plantage", "Absturzbericht senden", "Отправить отчёт о сбое", "Invia rapporto di arresto", "Wyślij raport o awarii", "Çökme raporu gönder", "Kirim laporan crash", "Gửi báo cáo lỗi", "Надіслати звіт про збій"];
    CrashReportMode, "crash_report_mode" => ["Crash Report Mode", "クラッシュレポート", "崩溃报告模式", "當機報告模式", "충돌 보고 모드", "Modo de informes de errores", "Modo de relatório de falhas", "Mode de rapport d'incident", "Absturzberichtsmodus", "Режим отчётов о сбоях", "Modalità segnalazione errori", "Tryb raportowania awarii", "Çökme raporu modu", "Mode laporan crash", "Chế độ báo cáo sự cố", "Режим звітування про збої"];
    HudFps, "hud_fps" => ["Frame Rate", "フレームレート", "帧速率", "影格率", "프레임 속도", "Fotogramas por segundo", "Taxa de quadros", "Images par seconde", "Bildrate", "Частота кадров", "Frequenza fotogrammi", "Liczba klatek na sekundę", "Kare hızı", "Laju bingkai", "Tốc độ khung hình", "Частота кадрів"];
    HudCoordinates, "hud_coordinates" => ["Coordinates", "座標", "坐标", "座標位置", "좌표", "Coordenadas", "Coordenadas", "Coordonnées", "Koordinaten", "Координаты", "Coordinate", "Współrzędne", "Koordinatlar", "Koordinat", "Tọa độ", "Координати"];
    HudBiome, "hud_biome" => ["Biome", "バイオーム", "生物群系", "生態域", "생물 군계", "Bioma", "Bioma", "Biome", "Biom", "Биом", "Bioma", "Biom", "Biyom", "Bioma", "Quần xã sinh vật", "Біом"];
    HudPing, "hud_ping" => ["Network Latency", "通信遅延", "网络延迟", "網路延遲", "네트워크 지연", "Latencia de red", "Latência da rede", "Latence réseau", "Netzwerklatenz", "Задержка сети", "Latenza di rete", "Opóźnienie sieci", "Ağ gecikmesi", "Latensi jaringan", "Độ trễ mạng", "Затримка мережі"];
    HudTps, "hud_tps" => ["Server TPS", "サーバーTPS", "服务器 TPS", "伺服器 TPS", "서버 TPS", "TPS del servidor", "TPS do servidor", "TPS du serveur", "Server-TPS", "TPS сервера", "TPS del server", "TPS serwera", "Sunucu TPS", "TPS server", "TPS máy chủ", "TPS сервера"];
    HudRam, "hud_ram" => ["RAM Usage", "メモリ使用量", "内存占用", "記憶體用量", "메모리 사용량", "Uso de RAM", "Uso de RAM", "Utilisation mémoire", "RAM-Auslastung", "Использование ОЗУ", "Utilizzo RAM", "Użycie pamięci RAM", "RAM kullanımı", "Penggunaan RAM", "Mức dùng RAM", "Використання пам'яті"];
    HudGpu, "hud_gpu" => ["GPU Usage", "GPU使用率", "GPU 占用率", "GPU 使用率", "GPU 사용률", "Uso de GPU", "Uso de GPU", "Utilisation du GPU", "GPU-Auslastung", "Загрузка видеокарты", "Utilizzo GPU", "Użycie GPU", "GPU kullanımı", "Penggunaan GPU", "Mức dùng GPU", "Завантаження відеокарти"];
    InviteFriend, "invite_friend" => ["Invite Friend", "フレンドを招待", "邀请好友", "邀請好友", "친구 초대", "Invitar a un amigo", "Convidar amigo", "Inviter un ami", "Freund einladen", "Пригласить друга", "Invita un amico", "Zaproś znajomego", "Arkadaş davet et", "Undang teman", "Mời bạn bè", "Запросити друга"];
    GenerateCode, "generate_code" => ["Generate Invite Code", "招待コードを生成", "生成邀请码", "產生邀請碼", "초대 코드 생성", "Generar código de invitación", "Gerar código de convite", "Générer un code d'invitation", "Einladungscode erstellen", "Создать код приглашения", "Genera codice d'invito", "Wygeneruj kod zaproszenia", "Davet kodu oluştur", "Buat kode undangan", "Tạo mã mời", "Створити код запрошення"];
    CopyCode, "copy_code" => ["Copy Invite Code", "招待コードをコピー", "复制邀请码", "複製邀請碼", "초대 코드 복사", "Copiar código de invitación", "Copiar código de convite", "Copier le code d'invitation", "Einladungscode kopieren", "Копировать код приглашения", "Copia codice d'invito", "Kopiuj kod zaproszenia", "Davet kodunu kopyala", "Salin kode undangan", "Sao chép mã mời", "Копіювати код запрошення"];
    RevokeCode, "revoke_code" => ["Revoke Invite Code", "招待コードを無効化", "撤销邀请码", "撤銷邀請碼", "초대 코드 취소", "Revocar código de invitación", "Revogar código de convite", "Révoquer le code d'invitation", "Einladungscode widerrufen", "Отозвать код приглашения", "Revoca codice d'invito", "Unieważnij kod zaproszenia", "Davet kodunu iptal et", "Cabut kode undangan", "Thu hồi mã mời", "Відкликати код запрошення"];
    JoinWorld, "join_world" => ["Join Friend's World", "フレンドのワールドに参加", "加入好友的世界", "加入好友的世界", "친구의 월드 참가", "Unirse al mundo de un amigo", "Entrar no mundo de um amigo", "Rejoindre le monde d'un ami", "Welt eines Freundes beitreten", "Войти в мир друга", "Entra nel mondo di un amico", "Dołącz do świata znajomego", "Arkadaşının dünyasına katıl", "Gabung ke dunia teman", "Vào thế giới của bạn bè", "Приєднатися до світу друга"];
    Minimap, "minimap" => ["Minimap", "ミニマップ", "小地图", "小地圖", "미니맵", "Minimapa", "Minimapa", "Minicarte", "Minikarte", "Мини-карта", "Minimappa", "Minimapa", "Mini harita", "Peta mini", "Bản đồ nhỏ", "Мінімапа"];
    Subtitles, "subtitles" => ["Subtitles", "字幕", "字幕显示", "字幕顯示", "자막", "Subtítulos", "Legendas", "Sous-titres", "Untertitel", "Субтитры", "Sottotitoli", "Napisy", "Altyazılar", "Takarir", "Phụ đề", "Субтитри"];
    VideoSettings, "video_settings" => ["Video Settings", "ビデオ設定", "视频设置", "影像設定", "비디오 설정", "Ajustes de vídeo", "Configurações de vídeo", "Paramètres vidéo", "Grafikeinstellungen", "Настройки видео", "Impostazioni video", "Ustawienia obrazu", "Görüntü ayarları", "Pengaturan video", "Cài đặt hình ảnh", "Налаштування відео"];
    AboutMegu, "about_megu" => ["About Megu", "Meguについて", "关于 Megu", "關於 Megu", "Megu 정보", "Acerca de Megu", "Sobre o Megu", "À propos de Megu", "Über Megu", "О Megu", "Informazioni su Megu", "Informacje o Megu", "Megu hakkında", "Tentang Megu", "Giới thiệu Megu", "Про Megu"];
    Status, "status" => ["Status", "状態", "状态", "狀態", "상태", "Estado", "Estado", "État", "Status", "Состояние", "Stato", "Stan", "Durum", "Status", "Trạng thái", "Стан"];
    ChatHistory, "chat_history" => ["Chat History", "チャット履歴", "聊天记录", "聊天紀錄", "채팅 기록", "Historial de chat", "Histórico de chat", "Historique du chat", "Chatverlauf", "История чата", "Cronologia chat", "Historia czatu", "Sohbet geçmişi", "Riwayat obrolan", "Lịch sử trò chuyện", "Історія чату"];
    ToolBreakGuard, "tool_break_guard" => ["Tool Break Guard", "ツール破壊ガード", "工具损坏保护", "工具損壞保護", "도구 파손 방지", "Protección de herramientas", "Proteção contra quebra de ferramentas", "Protection des outils", "Werkzeugbruchschutz", "Защита инструментов", "Protezione degli strumenti", "Ochrona narzędzi", "Alet kırılma koruması", "Pelindung alat", "Bảo vệ dụng cụ", "Захист інструментів"];
    SmoothZoom, "smooth_zoom" => ["Smooth Zoom", "スムーズズーム", "平滑缩放", "平滑縮放", "부드러운 확대", "Zoom suave", "Zoom suave", "Zoom fluide", "Sanfter Zoom", "Плавное приближение", "Zoom fluido", "Płynne przybliżanie", "Yumuşak yakınlaştırma", "Zoom halus", "Thu phóng mượt", "Плавне наближення"];
    LanguageAuto, "language_auto" => ["Auto-Detect Language", "言語を自動判定", "自动检测语言", "自動偵測語言", "언어 자동 감지", "Detectar idioma automáticamente", "Detectar idioma automaticamente", "Détecter la langue", "Sprache automatisch erkennen", "Определять язык автоматически", "Rileva lingua automaticamente", "Wykrywaj język automatycznie", "Dili otomatik algıla", "Deteksi bahasa otomatis", "Tự nhận diện ngôn ngữ", "Визначати мову автоматично"];
    Yes, "yes" => ["Yes", "はい", "是", "是的", "예", "Confirmar", "Sim", "Oui", "Ja", "Да", "Sì", "Tak", "Evet", "Ya", "Có", "Так"];
    No, "no" => ["No", "いいえ", "否", "否定", "아니요", "Negativo", "Não", "Non", "Nein", "Нет", "No", "Nie", "Hayır", "Tidak", "Không", "Ні"];
    Statistics, "statistics" => ["Statistics", "統計", "统计信息", "統計資料", "통계", "Estadísticas", "Estatísticas", "Statistiques", "Statistik", "Статистика", "Statistiche", "Statystyki", "İstatistikler", "Statistik", "Thống kê", "Статистика"];
    MinihudAdvanced, "minihud_advanced" => ["Advanced MiniHUD", "詳細ミニHUD", "高级迷你信息栏", "進階迷你資訊欄", "고급 미니 HUD", "MiniHUD avanzado", "MiniHUD avançado", "MiniHUD avancé", "Erweitertes MiniHUD", "Расширенные мини-индикаторы", "MiniHUD avanzato", "Zaawansowany MiniHUD", "Gelişmiş Mini HUD", "MiniHUD lanjutan", "HUD thu nhỏ nâng cao", "Розширена мініпанель"];
    MinihudBasic, "minihud_basic" => ["Basic MiniHUD", "基本ミニHUD", "基础迷你信息栏", "基本迷你資訊欄", "기본 미니 HUD", "MiniHUD básico", "MiniHUD básico", "MiniHUD simple", "Einfaches MiniHUD", "Базовые мини-индикаторы", "MiniHUD di base", "Podstawowy MiniHUD", "Temel Mini HUD", "MiniHUD dasar", "HUD thu nhỏ cơ bản", "Базова мініпанель"];
    ViewDistance, "view_distance" => ["View Distance", "表示距離", "视距", "視野距離", "시야 거리", "Distancia de visión", "Distância de visão", "Distance de vue", "Anzeigedistanz", "Дальность видимости", "Distanza di visuale", "Zasięg widzenia", "Görüntüleme mesafesi", "Jarak pandang", "Tầm nhìn", "Відстань видимості"];
    GpuBackend, "gpu_backend" => ["GPU Backend", "GPUバックエンド", "GPU 渲染后端", "GPU 算繪後端", "GPU 백엔드", "Motor de GPU", "Backend de GPU", "Moteur GPU", "GPU-Backend", "Графический движок", "Backend GPU", "Moduł GPU", "GPU arka ucu", "Backend GPU", "Bộ xử lý GPU", "Графічний рушій"];
    CrashId, "crash_id" => ["Crash ID", "クラッシュID", "崩溃编号", "當機識別碼", "충돌 ID", "Identificador de error", "ID da falha", "Identifiant du plantage", "Absturz-ID", "Идентификатор сбоя", "ID arresto anomalo", "Identyfikator awarii", "Çökme kimliği", "ID crash", "Mã sự cố", "Ідентифікатор збою"];
    RecipeBrowserSearch, "recipe_browser_search" => ["Search Recipes & Resources", "レシピと資源を検索", "搜索配方与资源", "搜尋配方與資源", "제작법과 자원 검색", "Buscar recetas y recursos", "Buscar receitas e recursos", "Rechercher recettes et ressources", "Rezepte und Ressourcen suchen", "Поиск рецептов и ресурсов", "Cerca ricette e risorse", "Szukaj receptur i zasobów", "Tarif ve kaynak ara", "Cari resep dan sumber daya", "Tìm công thức và tài nguyên", "Пошук рецептів і ресурсів"];
    ModSyncChanged, "mod_sync_changed" => ["Server Mod Changes", "サーバーModの変更", "服务器模组变更", "伺服器模組變更", "서버 모드 변경 사항", "Cambios de mods del servidor", "Alterações nos mods do servidor", "Modifications des mods du serveur", "Server-Mod-Änderungen", "Изменения модов сервера", "Modifiche ai mod del server", "Zmiany modów serwera", "Sunucu mod değişiklikleri", "Perubahan mod server", "Thay đổi mod máy chủ", "Зміни модів сервера"];
    MasterVolume, "master_volume" => ["Master Volume", "マスター音量", "主音量", "主音量設定", "전체 음량", "Volumen general", "Volume principal", "Volume principal", "Gesamtlautstärke", "Общая громкость", "Volume generale", "Głośność główna", "Ana ses düzeyi", "Volume utama", "Âm lượng tổng", "Загальна гучність"];
    MusicVolume, "music_volume" => ["Music Volume", "音楽の音量", "音乐音量", "音樂音量", "음악 음량", "Volumen de música", "Volume da música", "Volume de la musique", "Musiklautstärke", "Громкость музыки", "Volume musica", "Głośność muzyki", "Müzik ses düzeyi", "Volume musik", "Âm lượng nhạc", "Гучність музики"];
    SearchMods, "search_mods" => ["Search Mods", "Modを検索", "搜索模组", "搜尋模組", "모드 검색", "Buscar mods", "Buscar mods", "Rechercher des mods", "Mods suchen", "Поиск модов", "Cerca mod", "Szukaj modów", "Mod ara", "Cari mod", "Tìm mod", "Пошук модів"];
    AllowMobRadar, "allow_mob_radar" => ["Allow Mob Radar", "モブレーダー許可", "允许生物雷达", "允許生物雷達", "몹 레이더 허용", "Permitir radar de criaturas", "Permitir radar de criaturas", "Autoriser le radar de créatures", "Kreaturenradar erlauben", "Разрешить радар мобов", "Consenti radar dei mob", "Zezwól na radar mobów", "Yaratık radarına izin ver", "Izinkan radar mob", "Cho phép radar quái vật", "Дозволити радар мобів"];
    AllowCaveMap, "allow_cave_map" => ["Allow Cave Map", "洞窟マップ許可", "允许洞穴地图", "允許洞穴地圖", "동굴 지도 허용", "Permitir mapa de cuevas", "Permitir mapa de cavernas", "Autoriser la carte des grottes", "Höhlenkarte erlauben", "Разрешить карту пещер", "Consenti mappa delle grotte", "Zezwól na mapę jaskiń", "Mağara haritasına izin ver", "Izinkan peta gua", "Cho phép bản đồ hang động", "Дозволити мапу печер"];
    PlayerRadarScope, "player_radar_scope" => ["Player Radar Scope", "プレイヤーレーダー", "玩家雷达范围", "玩家雷達範圍", "플레이어 레이더 범위", "Alcance del radar de jugadores", "Alcance do radar de jogadores", "Portée du radar des joueurs", "Spielerradar-Reichweite", "Дальность радара игроков", "Portata radar giocatori", "Zasięg radaru graczy", "Oyuncu radarı menzili", "Jangkauan radar pemain", "Tầm radar người chơi", "Дальність радара гравців"];
    WaypointTeleport, "waypoint_teleport" => ["Waypoint Teleport", "ウェイポイントTP", "航点传送", "路標傳送", "웨이포인트 순간이동", "Teletransporte a marcadores", "Teleporte para pontos marcados", "Téléportation aux repères", "Wegpunkt-Teleportation", "Телепортация к меткам", "Teletrasporto ai segnaposti", "Teleportacja do punktów", "İşaret noktasına ışınlanma", "Teleportasi ke penanda", "Dịch chuyển tới điểm đánh dấu", "Телепортація до міток"];
}
