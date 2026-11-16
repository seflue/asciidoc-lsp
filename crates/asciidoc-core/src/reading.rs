//! Reading-order traversal of one top-level document: the files an
//! `include::` pulls in are read where the macro stands, a file included
//! twice is read twice. Shared by everything that depends on reading order
//! (attributes, anchor IDs).

use std::path::{Path, PathBuf};

use crate::types::{DocumentModel, IncludeEdge};
use crate::workspace::Workspace;

/// Include depth at which [`read_in_order`] stops, as a guard on top of the
/// cycle check.
const MAX_WALK_DEPTH: usize = 64;

/// One line of a document in reading order: an item of the visitor or an
/// include.
enum Step<'a, I> {
    Item(I),
    Include(&'a IncludeEdge),
}

/// What a traversal does with the lines of each file it reads.
pub(crate) trait ReadingVisitor<'a> {
    type Item;

    /// The lines of `file` the visitor cares about, besides its includes.
    fn items(&self, file: &Path, doc: &'a DocumentModel) -> Vec<(usize, Self::Item)>;

    /// The item comes before the other lines it shares a line with.
    fn first_on_line(_item: &Self::Item) -> bool {
        false
    }

    /// Called in reading order. `stack` holds the files being read,
    /// outermost first: the top-level document, then each include down to
    /// `file`.
    fn item(&mut self, stack: &[PathBuf], file: &Path, item: Self::Item);

    /// Called before the target of `edge` is read.
    fn enter(&mut self, _edge: &IncludeEdge) {}

    /// Called after the target of `edge` was read.
    fn leave(&mut self, _edge: &IncludeEdge) {}
}

/// Reads the top-level document `root` and its includes in reading order.
pub(crate) fn read_in_order<'a, V: ReadingVisitor<'a>>(
    workspace: &'a Workspace,
    root: &Path,
    visitor: &mut V,
) {
    visit(workspace, visitor, &mut Vec::new(), root);
}

fn visit<'a, V: ReadingVisitor<'a>>(
    workspace: &'a Workspace,
    visitor: &mut V,
    stack: &mut Vec<PathBuf>,
    file: &Path,
) {
    let Some(doc) = workspace.documents.get(file) else {
        return;
    };
    if stack.len() >= MAX_WALK_DEPTH || stack.iter().any(|open| open == file) {
        return;
    }
    stack.push(file.to_path_buf());
    let mut steps: Vec<(usize, Step<'a, V::Item>)> = visitor
        .items(file, doc)
        .into_iter()
        .map(|(line, item)| (line, Step::Item(item)))
        .chain(
            workspace
                .include_graph
                .includes_of(file)
                .iter()
                .map(|edge| (edge.line, Step::Include(edge))),
        )
        .collect();
    steps.sort_by_key(|(line, step)| {
        let first = matches!(step, Step::Item(item) if V::first_on_line(item));
        (*line, !first)
    });
    for (_, step) in steps {
        match step {
            Step::Item(item) => visitor.item(stack, file, item),
            Step::Include(edge) => {
                visitor.enter(edge);
                visit(workspace, visitor, stack, &edge.target);
                visitor.leave(edge);
            }
        }
    }
    stack.pop();
}
