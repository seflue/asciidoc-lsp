//! The reader's stack of open conditional blocks, which every consumer of
//! the directives keeps in step with the lines it reads.

use crate::types::ConditionalOperator;

/// The open `ifdef`, `ifndef` and `ifeval` blocks, each with a value of the
/// consumer's own (whether it applies, how it reads, ...).
///
/// An `endif` closes the innermost block only if it carries no target or the
/// target of that block. The reader ignores a stray or mismatching `endif`
/// with an error (`reader.rb`, `process_line`), and so does [`close`].
///
/// [`close`]: BlockStack::close
#[derive(Debug, Clone)]
pub struct BlockStack<T> {
    /// The lower case target of each block, `None` for an `ifeval`.
    blocks: Vec<(Option<String>, T)>,
}

impl<T> Default for BlockStack<T> {
    fn default() -> Self {
        Self { blocks: Vec::new() }
    }
}

impl<T> BlockStack<T> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the block of an `ifdef` or `ifndef` with these names.
    pub fn open_condition(&mut self, names: &[String], operator: ConditionalOperator, value: T) {
        let delimiter = match operator {
            ConditionalOperator::And => "+",
            _ => ",",
        };
        self.blocks.push((Some(names.join(delimiter)), value));
    }

    /// Open the block of an `ifeval`, which no `endif` target matches.
    pub fn open_ifeval(&mut self, value: T) {
        self.blocks.push((None, value));
    }

    /// Close the innermost block for an `endif` with `target` (lower case),
    /// and return its value. `None`, and nothing closed, when there is no
    /// block or the target is not the block's.
    pub fn close(&mut self, target: Option<&str>) -> Option<T> {
        let (block, _) = self.blocks.last()?;
        let matches = match target {
            None => true,
            Some(target) => block.as_deref() == Some(target),
        };
        if matches {
            self.blocks.pop().map(|(_, value)| value)
        } else {
            None
        }
    }

    /// The value of the innermost block.
    #[must_use]
    pub fn last(&self) -> Option<&T> {
        self.blocks.last().map(|(_, value)| value)
    }

    /// The values of the open blocks, outermost first.
    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.blocks.iter().map(|(_, value)| value)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    /// Asciidoctor 2.0.26 reports `mismatched preprocessor directive` and
    /// keeps the block open for another target, and for an `endif` without a
    /// block; an `ifeval` has no target to match.
    #[test]
    fn an_endif_closes_a_block_of_its_target_only() {
        let mut stack = BlockStack::new();
        assert_eq!(stack.close(None), None);
        stack.open_condition(&names(&["a", "b"]), ConditionalOperator::Or, 1);
        assert_eq!(stack.close(Some("a")), None);
        assert_eq!(stack.close(Some("a,b")), Some(1));
        stack.open_condition(&names(&["a", "b"]), ConditionalOperator::And, 2);
        assert_eq!(stack.close(Some("a,b")), None);
        assert_eq!(stack.close(None), Some(2));
        stack.open_ifeval(3);
        assert_eq!(stack.close(Some("a")), None);
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.close(None), Some(3));
        assert!(stack.is_empty());
    }
}
