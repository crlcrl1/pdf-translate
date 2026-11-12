/// Whether a character belongs to a CJK script or full-width punctuation block.
pub fn is_cjk(ch: char) -> bool {
    unicode_blocks::is_cjk(ch)
}

/// Accents can be separate spacing glyphs instead of Unicode combining marks.
pub fn is_accent(ch: char) -> bool {
    matches!(ch, '\u{00af}' | '\u{203e}' | '\u{02b0}'..='\u{02ff}' | '\u{0300}'..='\u{036f}')
}

/// A separated list symbol at the start of a line, not a prose/math operator.
pub fn bullet_marker(text: &str) -> Option<char> {
    let mut chars = text.trim_start().chars();
    let marker = chars.next()?;
    (matches!(marker, '•' | '▪' | '◦' | '‣' | '·' | '-' | '*')
        && chars.next().is_some_and(char::is_whitespace))
    .then_some(marker)
}

/// A numeric or lettered section marker, with optional levels and final dot.
pub fn is_section_number(text: &str) -> bool {
    if text.is_empty() || text.len() > 12 {
        return false;
    }
    let mut levels = text.strip_suffix('.').unwrap_or(text).split('.');
    let lead = levels.next().unwrap_or_default();
    !lead.is_empty()
        && (lead.chars().all(|ch| ch.is_ascii_digit())
            || (lead.len() == 1 && lead.chars().all(|ch| ch.is_ascii_uppercase())))
        && levels.all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

pub fn collapse_whitespace(text: &str) -> String {
    let mut result = String::new();
    let mut previous_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !previous_space {
                result.push(' ');
            }
            previous_space = true;
        } else {
            result.push(ch);
            previous_space = false;
        }
    }
    result
}

/// Expand typographic Latin ligatures for the model without normalizing math
/// symbols, superscripts, or the original glyphs used to compose the PDF.
pub fn expand_latin_ligatures(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\u{fb00}' => result.push_str("ff"),
            '\u{fb01}' => result.push_str("fi"),
            '\u{fb02}' => result.push_str("fl"),
            '\u{fb03}' => result.push_str("ffi"),
            '\u{fb04}' => result.push_str("ffl"),
            '\u{fb05}' | '\u{fb06}' => result.push_str("st"),
            _ => result.push(ch),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn bullet_markers_require_a_separated_line_prefix() {
        for marker in ['•', '▪', '◦', '‣', '·', '-', '*'] {
            assert_eq!(
                super::bullet_marker(&format!("  {marker} Item")),
                Some(marker)
            );
        }
        for text in ["x · y", "-1.25", "*ptr", "•", "1. Item", "word • word"] {
            assert_eq!(super::bullet_marker(text), None, "{text}");
        }
    }

    #[test]
    fn ligatures_expand_without_folding_mathematical_characters_or_markup() {
        assert_eq!(
            super::expand_latin_ligatures("ﬁnite diﬀerent ﬂow oﬃce shuﬄe ﬅ ﬆ <i>𝑓²</i> <f1>"),
            "finite different flow office shuffle st st <i>𝑓²</i> <f1>"
        );
    }
}
