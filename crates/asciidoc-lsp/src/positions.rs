use tower_lsp_server::ls_types::Position;

/// Convert a 1-indexed line and byte column to an LSP Position (0-indexed line, UTF-16 column).
///
/// - `line`: 1-indexed line number (from our Section/Anchor types)
/// - `col`: 0-based byte offset within the line
/// - `content`: full document text (needed for UTF-16 conversion)
// LSP uses u32 for line/column numbers; real-world files won't exceed u32::MAX lines.
#[allow(clippy::cast_possible_truncation)]
pub fn to_lsp_position(line: usize, col: usize, content: &str) -> Position {
    let lsp_line = line.saturating_sub(1) as u32;

    let line_text = content.lines().nth(lsp_line as usize).unwrap_or_default();

    let utf16_col = byte_offset_to_utf16(line_text, col);

    Position {
        line: lsp_line,
        character: utf16_col,
    }
}

/// Convert an LSP Position (0-indexed line, UTF-16 column) back to 1-indexed line and byte column.
///
/// - `pos`: LSP Position
/// - `content`: full document text (needed for UTF-16 conversion)
///
/// Returns `(line_1indexed, byte_col)`.
pub fn from_lsp_position(pos: Position, content: &str) -> (usize, usize) {
    let line_1indexed = (pos.line as usize) + 1;

    let line_text = content.lines().nth(pos.line as usize).unwrap_or_default();

    let byte_col = utf16_offset_to_byte(line_text, pos.character);

    (line_1indexed, byte_col)
}

/// Convert a byte offset within a line to a UTF-16 code unit offset.
/// Only performs character-by-character counting when non-ASCII is present.
// LSP uses u32 for column offsets; real-world lines won't exceed u32::MAX code units.
#[allow(clippy::cast_possible_truncation)]
fn byte_offset_to_utf16(line: &str, byte_offset: usize) -> u32 {
    if line.is_ascii() {
        return byte_offset as u32;
    }

    let mut utf16_offset = 0u32;
    for (i, ch) in line.char_indices() {
        if i >= byte_offset {
            break;
        }
        utf16_offset += ch.len_utf16() as u32;
    }
    utf16_offset
}

/// Convert a UTF-16 code unit offset to a byte offset within a line.
/// Only performs character-by-character counting when non-ASCII is present.
// LSP uses u32 for column offsets; real-world lines won't exceed u32::MAX code units.
#[allow(clippy::cast_possible_truncation)]
fn utf16_offset_to_byte(line: &str, utf16_offset: u32) -> usize {
    if line.is_ascii() {
        return utf16_offset as usize;
    }

    let mut current_utf16 = 0u32;
    for (i, ch) in line.char_indices() {
        if current_utf16 >= utf16_offset {
            return i;
        }
        current_utf16 += ch.len_utf16() as u32;
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_lsp_position_ascii() {
        let content = "= Title\n\n== Chapter One\n";
        // Section "Chapter One" is at line 3 (1-indexed), col 0
        let pos = to_lsp_position(3, 0, content);
        assert_eq!(pos.line, 2); // 0-indexed
        assert_eq!(pos.character, 0);
    }

    #[test]
    fn test_to_lsp_position_ascii_with_col() {
        let content = "= Title\n\nSome text here\n";
        // byte col 5 in ASCII = UTF-16 col 5
        let pos = to_lsp_position(3, 5, content);
        assert_eq!(pos.line, 2);
        assert_eq!(pos.character, 5);
    }

    #[test]
    fn test_to_lsp_position_utf16_multibyte() {
        // German umlaut: "ae" = U+00E4, 2 bytes in UTF-8, 1 code unit in UTF-16
        let content = "= \u{00E4}\n";
        // Line 1 (1-indexed): "= ae"
        // byte col 0 = UTF-16 col 0 (before '=')
        let pos = to_lsp_position(1, 0, content);
        assert_eq!(pos.character, 0);

        // byte col 2 = UTF-16 col 2 (before the umlaut, after "= ")
        let pos = to_lsp_position(1, 2, content);
        assert_eq!(pos.character, 2);

        // byte col 4 = UTF-16 col 3 (after the 2-byte umlaut)
        let pos = to_lsp_position(1, 4, content);
        assert_eq!(pos.character, 3);
    }

    #[test]
    fn test_to_lsp_position_emoji_surrogate_pair() {
        // Emoji: U+1F600 (grinning face), 4 bytes in UTF-8, 2 code units in UTF-16
        let content = "A\u{1F600}B\n";
        // Line 1: "A<emoji>B"
        // byte 0 -> UTF-16 0 (A)
        let pos = to_lsp_position(1, 0, content);
        assert_eq!(pos.character, 0);

        // byte 1 -> UTF-16 1 (start of emoji)
        let pos = to_lsp_position(1, 1, content);
        assert_eq!(pos.character, 1);

        // byte 5 -> UTF-16 3 (B, after 4-byte emoji which is 2 UTF-16 units)
        let pos = to_lsp_position(1, 5, content);
        assert_eq!(pos.character, 3);
    }

    #[test]
    fn test_from_lsp_position_ascii() {
        let content = "= Title\n\n== Chapter One\n";
        let pos = Position {
            line: 2,
            character: 0,
        };
        let (line, col) = from_lsp_position(pos, content);
        assert_eq!(line, 3); // 1-indexed
        assert_eq!(col, 0);
    }

    #[test]
    fn test_from_lsp_position_utf16_multibyte() {
        // "= ae\n" where ae is U+00E4
        let content = "= \u{00E4}\n";
        // UTF-16 col 3 -> the position after the umlaut -> byte col 4
        let pos = Position {
            line: 0,
            character: 3,
        };
        let (line, col) = from_lsp_position(pos, content);
        assert_eq!(line, 1);
        assert_eq!(col, 4);
    }

    #[test]
    fn test_roundtrip_ascii() {
        let content = "= Title\n\n== Section\n\nParagraph text.\n";
        let original_line = 3;
        let original_col = 3;

        let lsp_pos = to_lsp_position(original_line, original_col, content);
        let (rt_line, rt_col) = from_lsp_position(lsp_pos, content);

        assert_eq!(rt_line, original_line);
        assert_eq!(rt_col, original_col);
    }

    #[test]
    fn test_roundtrip_utf16() {
        // Line with mixed ASCII and multi-byte chars
        let content = "Hello \u{00E4}\u{00F6}\u{00FC} world\n";
        let original_line = 1;
        let original_col = 6; // byte offset of first umlaut

        let lsp_pos = to_lsp_position(original_line, original_col, content);
        let (rt_line, rt_col) = from_lsp_position(lsp_pos, content);

        assert_eq!(rt_line, original_line);
        assert_eq!(rt_col, original_col);
    }

    #[test]
    fn test_roundtrip_emoji() {
        let content = "A\u{1F600}B\n";
        let original_line = 1;
        let original_col = 5; // byte offset of 'B'

        let lsp_pos = to_lsp_position(original_line, original_col, content);
        let (rt_line, rt_col) = from_lsp_position(lsp_pos, content);

        assert_eq!(rt_line, original_line);
        assert_eq!(rt_col, original_col);
    }

    #[test]
    fn test_to_lsp_position_line_zero_safe() {
        // line 0 should not panic (saturating_sub)
        let content = "= Title\n";
        let pos = to_lsp_position(0, 0, content);
        assert_eq!(pos.line, 0);
    }
}
