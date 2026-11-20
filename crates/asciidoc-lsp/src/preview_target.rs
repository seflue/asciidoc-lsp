//! What the preview shows for the active buffer: the whole document it
//! belongs to, or the buffer on its own.
//!
//! Pure decisions only; `main.rs` feeds them from the workspace and sends the
//! resulting conversion request.

use std::path::{Path, PathBuf};

use tower_lsp_server::ls_types::Uri;

/// Which document the preview converts for the active buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreviewMode {
    /// The top-level document that includes the buffer, with every include
    /// resolved: the page shows the document as Asciidoctor builds it. A
    /// buffer that no document includes is shown on its own.
    #[default]
    Document,
    /// The buffer alone, as a document of its own.
    Part,
}

impl PreviewMode {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "document" => Some(Self::Document),
            "part" => Some(Self::Part),
            _ => None,
        }
    }
}

/// The arguments of the open command: the document URI, then optionally
/// `{"mode": "document" | "part"}`. No options means the default mode.
///
/// # Errors
/// A message for the client if the arguments have another shape.
pub fn parse_open_arguments(
    mut arguments: Vec<serde_json::Value>,
    command: &str,
) -> Result<(Uri, PreviewMode), String> {
    if arguments.is_empty() || arguments.len() > 2 {
        return Err(format!(
            "{command} expects the document URI and optionally {{\"mode\": \"document\" | \"part\"}}"
        ));
    }
    let options = if arguments.len() == 2 {
        arguments.pop()
    } else {
        None
    };
    let uri_argument = arguments.remove(0);
    let uri: Uri = serde_json::from_value(uri_argument)
        .map_err(|error| format!("{command}: invalid URI: {error}"))?;
    let mode = match options {
        None | Some(serde_json::Value::Null) => PreviewMode::default(),
        Some(serde_json::Value::Object(options)) => match options.get("mode") {
            None | Some(serde_json::Value::Null) => PreviewMode::default(),
            Some(serde_json::Value::String(name)) => PreviewMode::parse(name).ok_or_else(|| {
                format!("{command}: unknown mode {name:?}, use \"document\" or \"part\"")
            })?,
            Some(_) => return Err(format!("{command}: \"mode\" must be a string")),
        },
        Some(_) => return Err(format!("{command}: the options must be an object")),
    };
    Ok((uri, mode))
}

/// The top-level document to convert for a buffer that several include.
///
/// `candidates` are in path order. The document the page shows already stays
/// if it is one of them, so moving between files that several documents
/// share does not flip the page; otherwise the first in path order wins, so
/// the choice never depends on timing. `None` when nothing includes the
/// buffer.
#[must_use]
pub fn choose_top_level(candidates: &[PathBuf], shown: Option<&Path>) -> Option<PathBuf> {
    shown
        .and_then(|shown| {
            candidates
                .iter()
                .find(|candidate| candidate.as_path() == shown)
        })
        .or_else(|| candidates.first())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const COMMAND: &str = "asciidoc.preview.open";

    fn parse(arguments: Vec<serde_json::Value>) -> Result<PreviewMode, String> {
        parse_open_arguments(arguments, COMMAND).map(|(_, mode)| mode)
    }

    #[test]
    fn test_the_default_mode_is_the_whole_document() {
        assert_eq!(PreviewMode::default(), PreviewMode::Document);
        assert_eq!(
            parse(vec![json!("file:///a.adoc")]),
            Ok(PreviewMode::Document)
        );
        assert_eq!(
            parse(vec![json!("file:///a.adoc"), json!({})]),
            Ok(PreviewMode::Document)
        );
        assert_eq!(
            parse(vec![json!("file:///a.adoc"), json!(null)]),
            Ok(PreviewMode::Document)
        );
    }

    #[test]
    fn test_mode_option_selects_document_or_part() {
        let with = |mode: &str| parse(vec![json!("file:///a.adoc"), json!({ "mode": mode })]);
        assert_eq!(with("document"), Ok(PreviewMode::Document));
        assert_eq!(with("part"), Ok(PreviewMode::Part));
    }

    #[test]
    fn test_bad_arguments_are_refused() {
        assert!(parse(vec![]).is_err());
        assert!(parse(vec![json!(5)]).is_err());
        assert!(parse(vec![json!("file:///a.adoc"), json!({}), json!(1)]).is_err());
        assert!(parse(vec![json!("file:///a.adoc"), json!("part")]).is_err());
        assert!(parse(vec![json!("file:///a.adoc"), json!({ "mode": "all" })]).is_err());
        assert!(parse(vec![json!("file:///a.adoc"), json!({ "mode": 1 })]).is_err());
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn test_first_candidate_in_path_order_is_chosen() {
        let tops = paths(&["/w/a.adoc", "/w/b.adoc"]);
        assert_eq!(
            choose_top_level(&tops, None),
            Some(PathBuf::from("/w/a.adoc"))
        );
        // A shown document that is no candidate does not matter.
        assert_eq!(
            choose_top_level(&tops, Some(Path::new("/w/other.adoc"))),
            Some(PathBuf::from("/w/a.adoc"))
        );
    }

    #[test]
    fn test_shown_candidate_stays() {
        let tops = paths(&["/w/a.adoc", "/w/b.adoc"]);
        assert_eq!(
            choose_top_level(&tops, Some(Path::new("/w/b.adoc"))),
            Some(PathBuf::from("/w/b.adoc"))
        );
    }

    #[test]
    fn test_no_candidate_means_no_top_level_document() {
        assert_eq!(choose_top_level(&[], Some(Path::new("/w/a.adoc"))), None);
    }
}
