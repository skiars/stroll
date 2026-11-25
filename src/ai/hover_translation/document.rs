use super::*;
use crate::ai::context::allowed_local_document;

pub(crate) type DocumentTranslations = HashMap<String, SharedString>;

pub(crate) fn document_prose_keys(root: &Node, source: &str) -> HashMap<usize, String> {
    document_prose(root, source)
        .into_iter()
        .map(|paragraph| (paragraph.offset, paragraph.original()))
        .collect()
}

fn document_prose(root: &Node, source: &str) -> Vec<ProseParagraph> {
    fn collect(
        node: &Node,
        source: &str,
        definitions: &HashMap<&str, &str>,
        paragraphs: &mut Vec<ProseParagraph>,
    ) {
        match node {
            Node::Paragraph(_)
            | Node::Heading(_)
            | Node::TableCell(_)
            | Node::FootnoteDefinition(_) => {
                paragraphs.extend(ProseParagraph::parse(node, source, definitions, true));
            }
            Node::Root(_)
            | Node::Blockquote(_)
            | Node::List(_)
            | Node::ListItem(_)
            | Node::Table(_)
            | Node::TableRow(_) => {
                for child in node.children().unwrap() {
                    collect(child, source, definitions, paragraphs);
                }
            }
            _ => {}
        }
    }

    let mut definitions = HashMap::new();
    collect_definitions(root, &mut definitions);
    let mut paragraphs = Vec::new();
    collect(root, source, &definitions, &mut paragraphs);
    paragraphs
}

impl Snapshot {
    pub(crate) async fn translate_document(
        self,
        repo: Option<Arc<Repository>>,
        path: PathBuf,
        source: impl FnOnce() -> String + Send + 'static,
        mut report: impl FnMut(DocumentTranslations, &AtomicBool) -> bool + Send + 'static,
    ) -> Result<(), ()> {
        let cancellation = Cancellation::default();
        let cancelled = cancellation.flag();
        blocking::unblock(move || {
            if !self.is_current() || cancelled.load(Ordering::Relaxed) {
                return Err(());
            }
            let allowed = if path.is_absolute() {
                allowed_local_document(&path)
            } else {
                repo.is_some_and(|repo| repo.allows_ai_hover(&path))
            };
            if !allowed || !self.is_current() || cancelled.load(Ordering::Relaxed) {
                return Err(());
            }
            let source = source();
            let root = crate::docs::parse_markdown(&source).map_err(|_| ())?;
            let mut paragraphs = document_prose(&root, &source);
            let keys: Vec<_> = paragraphs.iter().map(ProseParagraph::original).collect();

            // Long runs are split only for the request. Their complete source
            // key still identifies the original rendered paragraph.
            for paragraph in &mut paragraphs {
                paragraph.runs = paragraph.runs.drain(..).flat_map(split_long_run).collect();
            }

            let mut fragments = Vec::new();
            let mut dependents: HashMap<&str, Vec<usize>> = HashMap::new();
            let mut pending = Vec::with_capacity(paragraphs.len());
            for (index, paragraph) in paragraphs.iter().enumerate() {
                let mut seen = HashSet::new();
                for text in paragraph.candidates().filter(|text| seen.insert(*text)) {
                    let dependents = dependents.entry(text).or_default();
                    if dependents.is_empty() {
                        fragments.push(text);
                    }
                    dependents.push(index);
                }
                pending.push(seen.len());
            }
            if fragments.is_empty() {
                return Ok(());
            }

            if !self.is_current() {
                return Err(());
            }
            let provider =
                Provider::open(&self.settings, &self.service.credential_scope).map_err(|_| ())?;
            let mut translations = Translations::new();
            translate_batches(
                &fragments,
                &self.service.cache,
                &provider,
                &cancelled,
                true,
                |batch| {
                    if !self.is_current() {
                        return false;
                    }
                    let mut complete = Vec::new();
                    for text in batch.keys() {
                        for &index in &dependents[text.as_str()] {
                            pending[index] -= 1;
                            if pending[index] == 0 {
                                complete.push(index);
                            }
                        }
                    }
                    translations.extend(batch);
                    report(
                        complete
                            .into_iter()
                            .map(|index| {
                                (
                                    keys[index].clone(),
                                    paragraphs[index].translation(&translations),
                                )
                            })
                            .collect(),
                        &cancelled,
                    )
                },
            )
            .map_err(|_| ())
        })
        .await
    }
}

fn split_long_run(run: Run) -> Vec<Run> {
    let Run::Text(text) = run else {
        return vec![run];
    };
    if text.len() <= BATCH_LIMIT {
        return vec![Run::Text(text)];
    }
    let mut parts = Vec::new();
    let mut remaining = text.as_str();
    loop {
        let mut size = 0;
        let Some(limit) = remaining.char_indices().find_map(|(offset, ch)| {
            size += ch.len_utf16();
            (size > BATCH_LIMIT).then_some(offset)
        }) else {
            break;
        };
        let split = remaining[..limit]
            .rfind(char::is_whitespace)
            .filter(|offset| *offset >= limit / 2)
            .unwrap_or(limit);
        let (part, tail) = remaining.split_at(split);
        parts.push(Run::Text(part.to_owned()));
        remaining = tail.trim_start_matches(char::is_whitespace);
        if tail.len() != remaining.len() {
            parts.push(Run::Markup(" ".into()));
        }
    }
    if !remaining.is_empty() {
        parts.push(Run::Text(remaining.to_owned()));
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_translation_extracts_prose_without_repeating_images_or_math() {
        let source = "# Document heading\n\n> Quoted sentence.\n\n- List item.\n\n| Header text |\n| --- |\n| Cell text |\n\nA [reference][ref] with `code()`, $a^2$, and ![image](./p.png).\n\n[ref]: ./guide.md\n\n[^note]: Footnote text.\n\n```rust\nfn sample() {}\n```\n";
        let ast = crate::docs::parse_markdown(source).unwrap();
        let paragraphs = document_prose(&ast, source);
        let fragments: Vec<_> = paragraphs
            .iter()
            .flat_map(ProseParagraph::candidates)
            .collect();
        for prose in [
            "Document heading",
            "Quoted sentence.",
            "List item.",
            "Header text",
            "Cell text",
            "Footnote text.",
        ] {
            assert!(fragments.contains(&prose), "{prose}: {fragments:?}");
        }
        for markup in ["sample", "code", "guide.md", "p.png", "a^2"] {
            assert!(fragments.iter().all(|fragment| !fragment.contains(markup)));
        }
        let reference = paragraphs
            .iter()
            .find(|p| p.original().contains("guide.md"))
            .unwrap();
        let translated =
            reference.translation(&Translations::from([("reference".into(), "参考".into())]));
        assert!(translated.contains("[参考](<./guide.md>)"));
        assert!(translated.contains("`code()`"));
        assert!(!translated.contains("p.png"));
        assert!(!translated.contains("a^2"));
    }
}
