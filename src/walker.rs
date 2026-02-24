use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::types::IncludeTree;

pub fn substitute_attributes(path: &str, attrs: &HashMap<String, String>) -> String {
    let mut result = path.to_string();
    for (key, value) in attrs {
        result = result.replace(&format!("{{{}}}", key), value);
    }
    result
}

pub fn walk_includes(
    start: &Path,
    attributes: &HashMap<String, String>,
    visited: &mut HashSet<PathBuf>,
) -> IncludeTree {
    let canonical = match start.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            return IncludeTree {
                file: start.to_path_buf(),
                includes: vec![],
                sections: vec![],
                xrefs: vec![],
                anchors: vec![],
                error: Some(format!("Cannot resolve path: {}", e)),
            };
        }
    };

    if !visited.insert(canonical.clone()) {
        return IncludeTree {
            file: start.to_path_buf(),
            includes: vec![],
            sections: vec![],
            xrefs: vec![],
            anchors: vec![],
            error: Some("Circular include detected".to_string()),
        };
    }

    // Read file
    let source = match std::fs::read_to_string(&canonical) {
        Ok(s) => s,
        Err(e) => {
            visited.remove(&canonical);
            return IncludeTree {
                file: start.to_path_buf(),
                includes: vec![],
                sections: vec![],
                xrefs: vec![],
                anchors: vec![],
                error: Some(format!("Cannot read file: {}", e)),
            };
        }
    };

    // Parse
    let mut parser = match crate::parser::init_block_parser() {
        Ok(p) => p,
        Err(e) => {
            visited.remove(&canonical);
            return IncludeTree {
                file: start.to_path_buf(),
                includes: vec![],
                sections: vec![],
                xrefs: vec![],
                anchors: vec![],
                error: Some(format!("Parser init failed: {}", e)),
            };
        }
    };

    let tree = match crate::parser::parse(&mut parser, &source) {
        Ok(t) => t,
        Err(e) => {
            visited.remove(&canonical);
            return IncludeTree {
                file: start.to_path_buf(),
                includes: vec![],
                sections: vec![],
                xrefs: vec![],
                anchors: vec![],
                error: Some(format!("Parse failed: {}", e)),
            };
        }
    };

    let root = tree.root_node();
    let sections = crate::extractors::extract_sections(root, source.as_bytes());
    let includes_raw = crate::extractors::extract_includes(root, source.as_bytes());

    // Collect attributes from this file and merge with passed-in attributes
    let mut local_attrs = attributes.clone();
    for attr in crate::extractors::extract_attributes(root, source.as_bytes()) {
        local_attrs.insert(attr.name, attr.value);
    }

    // Resolve includes
    let base_dir = canonical.parent().unwrap_or(Path::new("."));
    let mut child_trees = Vec::new();
    for inc in &includes_raw {
        let resolved_path = substitute_attributes(&inc.path, &local_attrs);
        let full_path = base_dir.join(&resolved_path);
        let child = walk_includes(&full_path, &local_attrs, visited);
        child_trees.push(child);
    }

    // Dual-grammar: run inline parser for xrefs and anchors
    let inline_ranges = crate::extractors::collect_inline_ranges(root);
    let (xrefs, anchors) = if !inline_ranges.is_empty() {
        match crate::parser::init_inline_parser() {
            Ok(mut inline_parser) => {
                if inline_parser.set_included_ranges(&inline_ranges).is_ok() {
                    match crate::parser::parse(&mut inline_parser, &source) {
                        Ok(inline_tree) => {
                            let inline_root = inline_tree.root_node();
                            (
                                crate::extractors::extract_xrefs(inline_root, source.as_bytes()),
                                crate::extractors::extract_anchors(inline_root, source.as_bytes()),
                            )
                        }
                        Err(_) => (vec![], vec![]),
                    }
                } else {
                    (vec![], vec![])
                }
            }
            Err(_) => (vec![], vec![]),
        }
    } else {
        (vec![], vec![])
    };

    visited.remove(&canonical);

    IncludeTree {
        file: start.to_path_buf(),
        includes: child_trees,
        sections,
        xrefs,
        anchors,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_substitute_attributes() {
        let mut attrs = HashMap::new();
        attrs.insert("chapdir".to_string(), "chapters".to_string());
        attrs.insert("ext".to_string(), "adoc".to_string());

        let result = substitute_attributes("{chapdir}/intro.{ext}", &attrs);
        assert_eq!(result, "chapters/intro.adoc");
    }

    #[test]
    fn test_substitute_no_match() {
        let attrs = HashMap::new();
        let result = substitute_attributes("plain/path.adoc", &attrs);
        assert_eq!(result, "plain/path.adoc");
    }
}
