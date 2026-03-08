use tree_sitter::{Parser, Tree};

use crate::error::CoreError;

/// Initialize a tree-sitter parser for the block-level `AsciiDoc` grammar.
///
/// # Errors
/// Returns an error if the tree-sitter language fails to load.
pub fn init_block_parser() -> Result<Parser, CoreError> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_asciidoc::language())?;
    Ok(parser)
}

/// Initialize a tree-sitter parser for the inline-level `AsciiDoc` grammar.
///
/// # Errors
/// Returns an error if the tree-sitter language fails to load.
pub fn init_inline_parser() -> Result<Parser, CoreError> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_asciidoc_inline::language())?;
    Ok(parser)
}

/// Parse source text with the given tree-sitter parser.
///
/// # Errors
/// Returns an error if tree-sitter fails to produce a parse tree.
pub fn parse(parser: &mut Parser, source: &str) -> Result<Tree, CoreError> {
    parser.parse(source, None).ok_or(CoreError::ParseFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_parser_sexp() {
        let mut parser = init_block_parser().unwrap();
        let source = "= Hello World\n\nThis is a paragraph.\n\ninclude::other.adoc[]\n\n[[my-anchor]]\n== Section Two\n\nxref:file.adoc#anchor[Link Text]\n\n:myattr: myvalue\n";
        let tree = parse(&mut parser, source).unwrap();
        let root = tree.root_node();
        println!("=== S-EXPRESSION ===");
        println!("{}", root.to_sexp());
        println!("=== END ===");
        assert!(!root.has_error(), "Parse tree has errors");
    }

    #[test]
    fn test_inline_parser_sexp() {
        let mut parser = init_inline_parser().unwrap();
        let source = "xref:file.adoc#anchor[Link Text]";
        let tree = parse(&mut parser, source).unwrap();
        let root = tree.root_node();
        println!("=== INLINE S-EXPRESSION ===");
        println!("{}", root.to_sexp());
        println!("=== END INLINE ===");
        assert!(!root.has_error(), "Inline parse tree has errors");
    }
}
