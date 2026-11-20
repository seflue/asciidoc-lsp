//! How `AsciiDoc` source text enters the server. Asciidoctor drops a UTF-8
//! byte order mark at the very start of the source it reads; every read
//! of an `AsciiDoc` file and every text received from a client goes through
//! here so the BOM never hides the first line (a header attribute entry
//! right after it, say).

use std::io;
use std::path::Path;

/// The byte order mark, as a character.
const BOM: char = '\u{FEFF}';

/// `text` without a BOM at its very start; a BOM anywhere else stays. No
/// copy when there is no BOM.
///
/// Trade-off: a client that keeps the BOM in its buffer sees line-0
/// positions shifted by one UTF-16 unit against the stripped text. Neovim
/// and VS Code do not keep it.
#[must_use]
pub fn strip_bom_owned(mut text: String) -> String {
    if text.starts_with(BOM) {
        text.drain(..BOM.len_utf8());
    }
    text
}

/// Read the `AsciiDoc` file at `path`, without a leading BOM.
///
/// # Errors
/// Whatever [`std::fs::read_to_string`] returns.
pub fn read_adoc_file(path: impl AsRef<Path>) -> io::Result<String> {
    std::fs::read_to_string(path).map(strip_bom_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_a_leading_bom_only_once() {
        assert_eq!(strip_bom_owned("\u{FEFF}= T".to_string()), "= T");
        assert_eq!(
            strip_bom_owned("\u{FEFF}\u{FEFF}= T".to_string()),
            "\u{FEFF}= T"
        );
    }

    #[test]
    fn keeps_text_without_a_leading_bom() {
        assert_eq!(strip_bom_owned("= T\u{FEFF}".to_string()), "= T\u{FEFF}");
        assert_eq!(strip_bom_owned(String::new()), "");
        assert_eq!(strip_bom_owned("a\u{FEFF}b".to_string()), "a\u{FEFF}b");
    }

    #[test]
    fn read_adoc_file_strips_the_bom() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.adoc");
        std::fs::write(&file, b"\xEF\xBB\xBF:a: 1\n").unwrap();
        assert_eq!(read_adoc_file(&file).unwrap(), ":a: 1\n");
    }
}
