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

    /// Whether the target of `edge` is read at all; asked where the include
    /// line stands, after the items of that line. An include in a branch the
    /// reader skips is not read.
    fn follow(&mut self, _edge: &IncludeEdge) -> bool {
        true
    }

    /// Whether the traversal is over: asked before each line, it stops the
    /// whole walk, includes included.
    fn done(&self) -> bool {
        false
    }

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
        if visitor.done() {
            break;
        }
        match step {
            Step::Item(item) => visitor.item(stack, file, item),
            Step::Include(edge) => {
                if !visitor.follow(edge) {
                    continue;
                }
                visitor.enter(edge);
                visit(workspace, visitor, stack, &edge.target);
                visitor.leave(edge);
            }
        }
    }
    stack.pop();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records the file of each line it reads, and what it is told to skip
    /// or to stop at.
    struct Recorder {
        read: Vec<String>,
        skip: Option<&'static str>,
        stop_at: Option<&'static str>,
    }

    fn name(path: &Path) -> String {
        path.file_name().unwrap().to_string_lossy().into_owned()
    }

    impl<'a> ReadingVisitor<'a> for Recorder {
        type Item = ();

        fn items(&self, _file: &Path, doc: &'a DocumentModel) -> Vec<(usize, ())> {
            (1..=doc.line_count).map(|line| (line, ())).collect()
        }

        fn item(&mut self, _stack: &[PathBuf], file: &Path, _item: ()) {
            self.read.push(name(file));
        }

        fn follow(&mut self, edge: &IncludeEdge) -> bool {
            self.skip.map_or(true, |skip| skip != name(&edge.target))
        }

        fn done(&self) -> bool {
            self.stop_at
                .is_some_and(|stop| self.read.last().is_some_and(|last| last == stop))
        }
    }

    fn read(skip: Option<&'static str>, stop_at: Option<&'static str>) -> Vec<String> {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let files = [
            ("top.adoc", "one\ninclude::a.adoc[]\ninclude::b.adoc[]\n"),
            ("a.adoc", "a1\na2\n"),
            ("b.adoc", "b1\n"),
        ];
        for (file, text) in files {
            std::fs::write(root.join(file), text).unwrap();
        }
        let mut ws = Workspace::new(&root);
        ws.scan_workspace();
        let mut recorder = Recorder {
            read: Vec::new(),
            skip,
            stop_at,
        };
        read_in_order(&ws, &root.join("top.adoc"), &mut recorder);
        recorder.read
    }

    #[test]
    fn includes_are_read_where_they_stand() {
        assert_eq!(
            read(None, None),
            ["top", "top", "a", "a", "top", "b"].map(|name| format!("{name}.adoc"))
        );
    }

    #[test]
    fn an_include_the_visitor_does_not_follow_is_not_read() {
        assert_eq!(
            read(Some("a.adoc"), None),
            ["top", "top", "top", "b"].map(|name| format!("{name}.adoc"))
        );
    }

    #[test]
    fn a_finished_visitor_stops_the_walk_inside_an_include() {
        assert_eq!(
            read(None, Some("a.adoc")),
            ["top", "top", "a"].map(|name| format!("{name}.adoc"))
        );
    }
}
