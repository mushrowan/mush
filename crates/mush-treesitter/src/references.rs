use std::collections::HashSet;

pub fn extract_references(source: &[u8], tree: &tree_sitter::Tree) -> HashSet<String> {
    let mut ids = HashSet::new();
    let mut cursor = tree.walk();
    collect_identifiers(&mut cursor, source, &mut ids);
    ids
}

fn collect_identifiers(
    cursor: &mut tree_sitter::TreeCursor,
    source: &[u8],
    ids: &mut HashSet<String>,
) {
    loop {
        let node = cursor.node();
        let kind = node.kind();

        let is_ident = kind.contains("identifier") || kind == "name";
        if is_ident
            && let Ok(text) = node.utf8_text(source)
            && text.len() > 1
        {
            ids.insert(text.to_string());
        }

        if cursor.goto_first_child() {
            collect_identifiers(cursor, source, ids);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Language;

    #[test]
    fn rust_identifiers() {
        let source = r#"
use std::collections::HashMap;

fn process(items: Vec<String>) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    for item in items {
        map.insert(item.clone(), item.len());
    }
    map
}
"#;
        let mut parser = crate::parser::new_parser(&Language::Rust.tree_sitter_language().unwrap());
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let refs = extract_references(source.as_bytes(), &tree);

        assert!(refs.contains("HashMap"), "missing HashMap: {refs:?}");
        assert!(refs.contains("process"), "missing process: {refs:?}");
        assert!(refs.contains("items"), "missing items: {refs:?}");
        assert!(refs.contains("String"), "missing String: {refs:?}");
        assert!(refs.contains("Vec"), "missing Vec: {refs:?}");
    }

    #[test]
    fn python_identifiers() {
        let source = r#"
from collections import Counter

def analyse(words):
    counts = Counter(words)
    return counts.most_common(10)
"#;
        let mut parser =
            crate::parser::new_parser(&Language::Python.tree_sitter_language().unwrap());
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let refs = extract_references(source.as_bytes(), &tree);

        assert!(refs.contains("Counter"), "missing Counter: {refs:?}");
        assert!(refs.contains("analyse"), "missing analyse: {refs:?}");
        assert!(refs.contains("words"), "missing words: {refs:?}");
    }

    #[test]
    fn single_char_filtered_out() {
        let source = "fn f(x: MyType) -> MyType { x }";
        let mut parser = crate::parser::new_parser(&Language::Rust.tree_sitter_language().unwrap());
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let refs = extract_references(source.as_bytes(), &tree);

        assert!(!refs.contains("x"), "single-char x should be filtered");
        assert!(!refs.contains("f"), "single-char f should be filtered");
        assert!(refs.contains("MyType"), "missing MyType: {refs:?}");
    }
}
