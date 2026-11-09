/// Whether a character belongs to a CJK script or full-width punctuation block.
pub fn is_cjk(ch: char) -> bool {
    unicode_blocks::is_cjk(ch)
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
