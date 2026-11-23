use std::{path::Path, sync::Once};

use gpui_component::{
    highlighter::{GrammarConfig, LanguageRegistry},
    input::{
        AutoClosingPair, BracketPair, IndentationRules, SyntaxContext,
        language_config::LanguageConfig,
    },
};

struct Language {
    id: &'static str,
    extensions: &'static [&'static str],
    editing: fn() -> LanguageConfig,
}

const LANGUAGES: &[Language] = &[Language {
    id: "vue",
    extensions: &["vue"],
    editing: vue_editing,
}];

pub(super) fn for_path(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?;
    LANGUAGES
        .iter()
        .find(|language| language.extensions.contains(&extension))
        .map(|language| language.id)
}

pub(super) fn register_grammars() {
    static GRAMMARS: Once = Once::new();
    GRAMMARS.call_once(|| {
        let registry = LanguageRegistry::singleton();
        registry.register(
            "vue",
            &GrammarConfig::new(
                "vue",
                tree_sitter_vue_next::LANGUAGE.into(),
                ["javascript", "typescript", "tsx", "css", "scss", "less"]
                    .map(Into::into)
                    .to_vec(),
                include_str!("vue_highlights.scm"),
                include_str!("vue_injections.scm"),
                "",
            ),
        );
        registry.register(
            "scss",
            &GrammarConfig::new(
                "scss",
                arborium_scss::language().into(),
                vec![],
                &arborium_scss::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
        );
        registry.register(
            "less",
            &GrammarConfig::new(
                "less",
                tree_sitter_less::language(),
                vec![],
                tree_sitter_less::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
        );
    });
}

pub(super) fn install(cx: &mut gpui::App) {
    register_grammars();
    for language in LANGUAGES {
        gpui_component::input::set_language_config(language.id, (language.editing)(), cx);
    }
}

fn vue_editing() -> LanguageConfig {
    let mut config = LanguageConfig::default();
    config.brackets.push(BracketPair::new(">", "</"));
    config.auto_closing_pairs.as_mut().unwrap().extend([
        AutoClosingPair::new("`", "`").not_in([SyntaxContext::String, SyntaxContext::Comment]),
        AutoClosingPair::new("<!--", "-->").not_in([SyntaxContext::String, SyntaxContext::Comment]),
    ]);
    config.indentation_rules(IndentationRules::new(
        regex::Regex::new(r#"(?:[({\[]|<[A-Za-z][A-Za-z0-9_.:-]*(?:\s+[^<>]*[^/<>])?>)\s*$"#)
            .unwrap(),
        regex::Regex::new(r"^\s*(?:[)}\]]|</)").unwrap(),
    ))
}
