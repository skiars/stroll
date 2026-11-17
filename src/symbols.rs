use gpui::{App, AppContext, Task, Window};
use gpui_component::input::{HoverProvider, Rope, RopeExt};
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Range};
use regex::Regex;
use rust_i18n::t;

#[derive(Clone, Copy)]
pub struct LocalSymbols(pub &'static str);

impl HoverProvider for LocalSymbols {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Option<Hover>>> {
        let text = text.clone();
        let symbols = *self;
        cx.background_spawn(async move { symbols.lookup_hover(&text, offset) })
    }
}

impl LocalSymbols {
    pub(crate) fn lookup_hover(&self, text: &Rope, offset: usize) -> anyhow::Result<Option<Hover>> {
        let Some(range) = text.word_range(offset) else {
            return Ok(None);
        };
        let word = text.slice(range.clone()).to_string();
        if !word
            .chars()
            .next()
            .is_some_and(|ch| ch.is_alphabetic() || ch == '_' || ch == '$')
        {
            return Ok(None);
        }
        let Some((row, declaration, doc)) = lookup(&text.to_string(), &word)? else {
            return Ok(None);
        };
        let range = Range::new(
            text.offset_to_position(range.start),
            text.offset_to_position(range.end),
        );
        let language = self.0;
        let line = t!("symbols.local_line", line = row + 1);
        let value = format!(
            "````{language}\n{declaration}\n````\n\n{doc}{}{line}",
            if doc.is_empty() { "" } else { "\n\n" },
        );
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: Some(range),
        }))
    }
}

fn lookup(source: &str, word: &str) -> anyhow::Result<Option<(usize, String, String)>> {
    let pattern = Regex::new(&format!(
        r"^\s*(?:(?:export|default|declare|pub(?:\([^)]*\))?|async|unsafe|static|mut)\s+)*(?:fn|function|def|class|interface|type|struct|enum|trait|const|let|var|mod)\s+{}(?:\W|$)",
        regex::escape(word)
    ))?;
    let mut docs = Vec::new();
    for (row, line) in source.lines().enumerate() {
        if let Some(doc) = line
            .trim()
            .strip_prefix("///")
            .or_else(|| line.trim().strip_prefix("* "))
        {
            docs.push(doc.trim());
            continue;
        }
        if pattern.is_match(line) {
            let line = line.trim();
            let signature = line
                .split_once(" {")
                .filter(|(prefix, _)| !prefix.contains('='))
                .map_or(line, |(prefix, _)| prefix);
            let declaration = signature.chars().take(220).collect();
            return Ok(Some((row, declaration, docs.join("\n"))));
        }
        if line.trim() != "*/" {
            docs.clear();
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_declaration_with_documentation() {
        let source = "/// 返回问候语\npub async fn greet(name: &str) -> String { todo!() }\nfn greet_again() {}";
        let (row, signature, docs) = lookup(source, "greet").unwrap().unwrap();
        assert_eq!(row, 1);
        assert_eq!(signature, "pub async fn greet(name: &str) -> String");
        assert_eq!(docs, "返回问候语");
        assert!(lookup(source, "gree").unwrap().is_none());
    }
}
