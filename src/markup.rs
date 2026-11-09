//! Markup shared by PDF layout, the translation protocol and typesetting.
use crate::model::TextStyle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Piece<'a> {
    Text(&'a str),
    Formula(&'a str),
    Bold(bool),
    Italic(bool),
}

fn tag(text: &str) -> Option<(Piece<'_>, usize)> {
    for (name, piece) in [
        ("<b>", Piece::Bold(true)),
        ("</b>", Piece::Bold(false)),
        ("<i>", Piece::Italic(true)),
        ("</i>", Piece::Italic(false)),
    ] {
        if text.starts_with(name) {
            return Some((piece, name.len()));
        }
    }
    let body = text.strip_prefix("<f")?;
    let digits = body.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0 && body.as_bytes().get(digits) == Some(&b'>'))
        .then(|| (Piece::Formula(&text[1..2 + digits]), 3 + digits))
}

pub fn pieces(mut text: &str) -> impl Iterator<Item = Piece<'_>> {
    std::iter::from_fn(move || {
        if text.is_empty() {
            return None;
        }
        for (offset, _) in text.match_indices('<') {
            if let Some((piece, len)) = tag(&text[offset..]) {
                if offset == 0 {
                    text = &text[len..];
                    return Some(piece);
                }
                let (plain, rest) = text.split_at(offset);
                text = rest;
                return Some(Piece::Text(plain));
            }
        }
        Some(Piece::Text(std::mem::take(&mut text)))
    })
}

pub fn escape_literal(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn decode_literal(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

pub fn style_open(style: TextStyle) -> &'static str {
    match (style.bold, style.italic) {
        (false, false) => "",
        (true, false) => "<b>",
        (false, true) => "<i>",
        (true, true) => "<b><i>",
    }
}

pub fn style_close(style: TextStyle) -> &'static str {
    match (style.bold, style.italic) {
        (false, false) => "",
        (true, false) => "</b>",
        (false, true) => "</i>",
        (true, true) => "</i></b>",
    }
}

pub fn strip_style_tags(text: &str) -> String {
    let mut out = String::new();
    for piece in pieces(text) {
        match piece {
            Piece::Text(plain) => out.push_str(plain),
            Piece::Formula(id) => {
                out.push('<');
                out.push_str(id);
                out.push('>');
            }
            _ => {}
        }
    }
    out
}

/// Restore unambiguously literal markers before validating formula order and styles.
pub fn validate_translation(
    source: &str,
    translation: &str,
    formulas: &[String],
) -> Option<String> {
    let mut text = translation.to_owned();
    for (start, _) in source.match_indices("&lt;") {
        let after = &source[start + 4..];
        let end = after
            .bytes()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == b'/')
            .count();
        if !after[end..].starts_with("&gt;") {
            continue;
        }
        let marker = format!("<{}>", &after[..end]);
        if tag(&marker).is_none() {
            continue;
        }
        let escaped = &source[start..start + 4 + end + 4];
        if !source.contains(&marker) {
            text = text.replace(&marker, escaped);
        }
        if text.matches(escaped).count() != source.matches(escaped).count() {
            return None;
        }
    }
    let actual = pieces(&text).filter_map(|piece| match piece {
        Piece::Formula(id) => Some(id),
        _ => None,
    });
    if !actual.eq(formulas.iter().map(String::as_str)) {
        return None;
    }
    let styles = |piece: &Piece<'_>| matches!(piece, Piece::Bold(_) | Piece::Italic(_));
    if pieces(source)
        .filter(styles)
        .eq(pieces(&text).filter(styles))
    {
        Some(text)
    } else {
        Some(strip_style_tags(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literal_tags_and_bound_formulas_have_separate_identities() {
        let src = "Literal &lt;b&gt; and &lt;f1&gt; beside <b>bold</b> <f2>";
        let ids = ["f2".to_string()];
        let translated = "字面 &lt;b&gt;、<f1> 与 <b>粗体</b> <f2>";
        assert_eq!(
            validate_translation(src, translated, &ids).unwrap(),
            "字面 &lt;b&gt;、&lt;f1&gt; 与 <b>粗体</b> <f2>"
        );
        assert!(validate_translation(src, "丢了字面标记 <f2>", &ids).is_none());
        assert_eq!(
            validate_translation("literal &lt;b&gt;", "字面 <b>", &[]).unwrap(),
            "字面 &lt;b&gt;"
        );
    }
}
