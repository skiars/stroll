use markdown::mdast::Node;

use super::parse_hover_markdown;

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Inline {
    Code(String),
    Math(String),
    Link(String, Vec<Inline>),
    Strong(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Strike(Vec<Inline>),
}

pub(super) fn same_markdown(source: &str, translated: &str) -> bool {
    inline_structure(translated).is_some_and(|output| inline_structure(source) == Some(output))
}

fn inline_structure(markdown: &str) -> Option<Vec<Inline>> {
    let Node::Root(root) = parse_hover_markdown(markdown, true)? else {
        return None;
    };
    let [Node::Paragraph(paragraph)] = root.children.as_slice() else {
        return None;
    };
    inline_nodes(&paragraph.children)
}

fn inline_nodes(nodes: &[Node]) -> Option<Vec<Inline>> {
    let mut structure = Vec::new();
    for node in nodes {
        structure.push(match node {
            Node::Text(_) | Node::Break(_) => continue,
            Node::InlineCode(code) => Inline::Code(code.value.clone()),
            Node::InlineMath(math) => Inline::Math(math.value.clone()),
            Node::Link(link) => Inline::Link(link.url.clone(), inline_nodes(&link.children)?),
            Node::Strong(strong) => Inline::Strong(inline_nodes(&strong.children)?),
            Node::Emphasis(emphasis) => Inline::Emphasis(inline_nodes(&emphasis.children)?),
            Node::Delete(delete) => Inline::Strike(inline_nodes(&delete.children)?),
            _ => return None,
        });
    }
    structure.sort_unstable();
    Some(structure)
}
