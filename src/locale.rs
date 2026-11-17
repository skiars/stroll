use serde::{Deserialize, Serialize};

/// Look up a desktop startup message using the active interface language.
pub fn text(key: &'static str) -> std::borrow::Cow<'static, str> {
    crate::_rust_i18n_translate(&rust_i18n::locale(), key)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum Language {
    #[serde(rename = "zh-CN")]
    Chinese,
    #[serde(rename = "en")]
    English,
    #[default]
    #[serde(rename = "system", other)]
    System,
}

impl Language {
    pub(crate) fn from_config(value: &str) -> Self {
        match value {
            "zh-CN" => Self::Chinese,
            "en" => Self::English,
            _ => Self::System,
        }
    }

    pub(crate) fn config_value(self) -> &'static str {
        match self {
            Self::Chinese => "zh-CN",
            Self::English => "en",
            Self::System => "system",
        }
    }

    pub(crate) fn activate(self) -> bool {
        let effective = self.resolve(sys_locale::get_locales());
        if &*gpui_component::locale() == effective.config_value() {
            return false;
        }
        // GPUI Component and the application share rust-i18n's process locale.
        gpui_component::set_locale(effective.config_value());
        true
    }

    fn resolve(self, preferred: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        if self != Self::System {
            return self;
        }
        preferred
            .into_iter()
            .find_map(|locale| {
                let language = locale.as_ref().split(['-', '_', '.', '@']).next()?;
                if language.eq_ignore_ascii_case("zh") {
                    Some(Self::Chinese)
                } else if language.eq_ignore_ascii_case("en") {
                    Some(Self::English)
                } else {
                    None
                }
            })
            .unwrap_or(Self::English)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use regex::Regex;
    use rust_i18n::t;

    use super::Language;

    #[test]
    fn follows_the_first_supported_system_language() {
        use Language::{Chinese, English, System};
        assert_eq!(System.resolve(["fr-FR", "zh-Hant-TW", "en-US"]), Chinese);
        assert_eq!(System.resolve(["EN-gb", "zh_CN.UTF-8"]), English);
        assert_eq!(System.resolve(["zh_CN.UTF-8"]), Chinese);
        assert_eq!(System.resolve(["ja-JP", "C.UTF-8"]), English);
        assert_eq!(System.resolve([] as [&str; 0]), English);
        assert_eq!(Chinese.resolve(["en-US"]), Chinese);
        assert_eq!(English.resolve(["zh-CN"]), English);
    }

    #[test]
    fn both_catalogs_load_with_matching_keys_and_arguments() {
        let en: BTreeMap<String, String> =
            serde_json::from_str(include_str!("../locales/en.json")).unwrap();
        let zh: BTreeMap<String, String> =
            serde_json::from_str(include_str!("../locales/zh-CN.json")).unwrap();
        assert!(en.keys().eq(zh.keys()));

        let pattern = Regex::new(r"%\{([a-z_][a-z_0-9]*)\}").unwrap();
        let arguments = |text: &str| {
            pattern
                .captures_iter(text)
                .map(|m| m[1].to_owned())
                .collect::<BTreeSet<_>>()
        };
        let backend = crate::_rust_i18n_backend();
        for (key, english) in &en {
            let chinese = &zh[key];
            assert_eq!(arguments(english), arguments(chinese), "{key}");
            assert_eq!(
                backend.translate("en", key).as_deref(),
                Some(english.as_str()),
                "{key}"
            );
            assert_eq!(
                backend.translate("zh-CN", key).as_deref(),
                Some(chinese.as_str()),
                "{key}"
            );
        }

        let path = "项目/%{path}.md";
        assert_eq!(
            t!("repository.open_path", locale = "en", path = path),
            "Open 项目/%{path}.md"
        );
        assert_eq!(
            t!("repository.open_path", locale = "zh-CN", path = path),
            "打开 项目/%{path}.md"
        );
        assert_eq!(t!("repository.open", locale = "fr"), "Open repository");
    }
}
