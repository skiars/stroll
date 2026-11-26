use std::{path::Path, sync::Once};

use gpui_component::{
    highlighter::{GrammarConfig, LanguageRegistry},
    input::{
        AutoClosingPair, BracketPair, IndentationRules, SyntaxContext,
        language_config::LanguageConfig,
    },
};

pub(super) fn for_path(path: &Path) -> Option<&'static str> {
    let filename = path.file_name()?.to_str()?.to_ascii_lowercase();
    if matches!(
        filename.as_str(),
        ".classpath"
            | ".cproject"
            | ".factorypath"
            | ".project"
            | "app.config"
            | "nuget.config"
            | "web.config"
    ) {
        return Some("xml");
    }
    match filename.rsplit_once('.')?.1 {
        "dtd" | "ent" => Some("dtd"),
        "ant" | "application" | "appxmanifest" | "ascx" | "atom" | "axml" | "bpmn" | "collada"
        | "cpt" | "csl" | "csproj" | "dae" | "dita" | "ditamap" | "djxml" | "dtml" | "fb2"
        | "fodg" | "fodp" | "fods" | "fodt" | "fsproj" | "fxml" | "glade" | "gpx" | "iml"
        | "isml" | "jats" | "jmx" | "kml" | "launch" | "manifest" | "mathml" | "menu" | "mxml"
        | "nuspec" | "opf" | "opml" | "osm" | "owl" | "plist" | "proj" | "props" | "pt"
        | "pubxml" | "qrc" | "rbxlx" | "rbxmx" | "rdf" | "resx" | "rng" | "rss" | "sch"
        | "shproj" | "sitemap" | "slnx" | "storyboard" | "svg" | "targets" | "tld" | "tmx"
        | "ui" | "vbproj" | "vcxproj" | "wixproj" | "wsdl" | "wxi" | "wxl" | "wxs" | "xaml"
        | "xbl" | "xht" | "xhtml" | "xib" | "xlf" | "xliff" | "xml" | "xoml" | "xpage" | "xpdl"
        | "xsd" | "xsl" | "xslt" | "xspf" | "xul" => Some("xml"),
        "config" if filename.ends_with(".exe.config") || filename.ends_with(".dll.config") => {
            Some("xml")
        }
        "filters" if filename.ends_with(".vcxproj.filters") => Some("xml"),
        "user" if filename.ends_with(".csproj.user") || filename.ends_with(".pubxml.user") => {
            Some("xml")
        }
        "vue" if path.extension().is_some_and(|extension| extension == "vue") => Some("vue"),
        _ => None,
    }
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
        for (name, language, highlights) in [
            (
                "xml",
                tree_sitter_xml::LANGUAGE_XML,
                tree_sitter_xml::XML_HIGHLIGHT_QUERY,
            ),
            (
                "dtd",
                tree_sitter_xml::LANGUAGE_DTD,
                tree_sitter_xml::DTD_HIGHLIGHT_QUERY,
            ),
        ] {
            registry.register(
                name,
                &GrammarConfig::new(name, language.into(), vec![], highlights, "", ""),
            );
        }
    });
}

pub(super) fn install(cx: &mut gpui::App) {
    register_grammars();
    gpui_component::input::set_language_config("vue", vue_editing(), cx);
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
