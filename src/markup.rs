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

/// PDF line breaks and bound formulas do not end a prose emphasis run.
/// Formula glyphs retain their original style independently of these tags.
pub fn coalesce_styles(text: &str) -> String {
    let mut text = text.to_owned();
    for style in [
        TextStyle {
            bold: true,
            italic: true,
        },
        TextStyle {
            bold: true,
            italic: false,
        },
        TextStyle {
            bold: false,
            italic: true,
        },
    ] {
        let (open, close) = (style_open(style), style_close(style));
        let mut rest = text.as_str();
        let mut joined = String::new();
        while let Some(end) = rest.find(close) {
            joined.push_str(&rest[..end]);
            rest = &rest[end + close.len()..];
            if let Some(start) = rest.find(open)
                && pieces(&rest[..start]).all(|piece| match piece {
                    Piece::Formula(_) => true,
                    Piece::Text(text) => text.chars().all(char::is_whitespace),
                    _ => false,
                })
            {
                joined.push_str(&rest[..start]);
                rest = &rest[start + open.len()..];
            } else {
                joined.push_str(close);
            }
        }
        joined.push_str(rest);
        text = joined;
    }
    text
}

fn emphasis_counts(text: &str) -> [usize; 3] {
    let mut counts = [0; 3];
    let mut style = TextStyle::default();
    for piece in pieces(text) {
        match piece {
            Piece::Bold(on) => style.bold = on,
            Piece::Italic(on) => style.italic = on,
            Piece::Text(text) => {
                let count = text.chars().filter(|ch| ch.is_alphanumeric()).count();
                counts[0] += count;
                counts[1] += count * usize::from(style.bold);
                counts[2] += count * usize::from(style.italic);
            }
            Piece::Formula(_) => {}
        }
    }
    counts
}

/// An article can have no separate translated word when a fallback fragment
/// contains only that article and sentence punctuation.
pub fn is_article_fragment(text: &str) -> bool {
    if text.chars().any(char::is_numeric) {
        return false;
    }
    let mut words = text
        .split(|ch: char| !ch.is_alphabetic())
        .filter(|word| !word.is_empty());
    words
        .next()
        .is_some_and(|word| matches!(word.to_ascii_lowercase().as_str(), "a" | "an" | "the"))
        && words.next().is_none()
}

fn preserves_emphasis_extent(source: &str, translation: &str) -> bool {
    let a = emphasis_counts(source);
    let b = emphasis_counts(translation);
    if a[1] == 0 && a[2] == 0 && b[0] == 0 && is_article_fragment(source) {
        return true;
    }
    (1..3).all(|i| {
        // Paired tags alone allow a short emphasized phrase to swallow the
        // entire paragraph, or to become empty. Both prose styles must survive.
        if (a[i] >= 2 && b[i] == 0) || (a[0] - a[i] >= 2 && b[0] == b[i]) {
            return false;
        }
        // Translation changes relative lengths. Only reject a gross shift
        // (35 percentage points), then retry or translate bounded spans.
        a[0] < 40
            || b[0] < 12
            || (a[i] as f64 / a[0] as f64 - b[i] as f64 / b[0] as f64).abs() <= 0.35
    })
}

/// Restore unambiguously literal markers before validating formula order and styles.
pub fn validate_translation(
    source: &str,
    translation: &str,
    formulas: &[String],
) -> Option<String> {
    let mut text = translation.to_owned();
    // Protocol explanations are not translations, even when their tags have
    // been stripped by the model. Reject them and use the bounded retry path.
    for instruction in [
        "只翻译",
        "仅翻译",
        "只输出译文",
        "公式占位符",
        "标签需保持配对",
        "Translate only the text",
        "Keep each placeholder",
        "Output only the translation",
    ] {
        if text.contains(instruction) && !source.contains(instruction) {
            return None;
        }
    }
    // Formula placeholders are atomic, not HTML containers. A model-generated
    // closing tag would otherwise be rendered as visible text in the PDF.
    if text.match_indices("</f").any(|(offset, _)| {
        let rest = &text[offset + 3..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        digits > 0 && rest.as_bytes().get(digits) == Some(&b'>')
    }) {
        return None;
    }
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
        let escaped = &source[start..start + 4 + end + 4];
        if !source.contains(&marker) {
            text = text.replace(&marker, escaped);
        }
        if text.matches(escaped).count() != source.matches(escaped).count() {
            return None;
        }
    }
    // These escapes represent literal source characters, not instructions to
    // add examples of the markup protocol to the translated paragraph.
    for entity in ["&lt;", "&gt;", "&amp;"] {
        // A conjunction such as A & B may become A and B in translation.
        // Tagged literals above are strict; ordinary prose can use fewer
        // escapes, but the model must not add protocol examples from a prompt.
        if text.matches(entity).count() > source.matches(entity).count() {
            return None;
        }
    }
    // Every raw tag must belong to this protocol. Unknown tags such as
    // <f6.45 bits> are malformed model output, not ordinary translated text.
    if pieces(&text).any(|piece| matches!(piece, Piece::Text(plain) if plain.contains(['<', '>'])))
    {
        return None;
    }
    let actual = pieces(&text)
        .filter_map(|piece| match piece {
            Piece::Formula(id) => Some(id),
            _ => None,
        })
        .collect::<Vec<_>>();
    // Translation can legitimately reorder inline variables when the target
    // language changes word order (for example, "answer a for input x" becomes
    // "for input x, answer a" in Chinese). Require each bound formula exactly
    // once, but do not require the source order.
    if actual.len() != formulas.len() {
        return None;
    }
    let mut actual_sorted = actual;
    let mut expected_sorted = formulas.iter().map(String::as_str).collect::<Vec<_>>();
    actual_sorted.sort_unstable();
    expected_sorted.sort_unstable();
    if actual_sorted != expected_sorted {
        return None;
    }
    let styles = |piece: &Piece<'_>| matches!(piece, Piece::Bold(_) | Piece::Italic(_));
    if pieces(source)
        .filter(styles)
        .eq(pieces(&text).filter(styles))
        && preserves_emphasis_extent(source, &text)
    {
        Some(text)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emphasis_cannot_spread_to_unstyled_prose_or_disappear() {
        for tag in ["b", "i"] {
            let source = format!(
                "<{tag}>Main finding</{tag}>. The explanation continues in ordinary type with several details."
            );
            for output in [
                format!("<{tag}>主要发现。接下来的解释以普通字体给出若干细节。</{tag}>"),
                format!("<{tag}></{tag}>主要发现。接下来的解释以普通字体给出若干细节。"),
                format!("<{tag}>主要发现。接下来的解释以普通字体给出若干</{tag}>细节。"),
            ] {
                assert!(validate_translation(&source, &output, &[]).is_none());
            }
            assert!(
                validate_translation(
                    &source,
                    &format!("<{tag}>主要发现</{tag}>。接下来的解释以普通字体给出若干细节。"),
                    &[],
                )
                .is_some()
            );
        }
    }

    #[test]
    fn emphasis_is_coalesced_but_never_silently_discarded() {
        assert_eq!(
            coalesce_styles("<b>A long</b>\n<b>heading</b> and <i>two</i> <i>words</i>"),
            "<b>A long\nheading</b> and <i>two words</i>"
        );
        assert!(validate_translation("<b>Important</b> text", "重要文本", &[]).is_none());
        assert!(validate_translation("Normal text", "<b>普通文本</b>", &[]).is_none());
        assert!(
            validate_translation(
                "A statement.",
                "仅翻译文本，标签需保持配对。一个陈述。",
                &[]
            )
            .is_none()
        );
    }

    #[test]
    fn continuous_emphasis_crosses_bound_formulas_but_not_plain_prose() {
        for source in [
            "<i>A system of distinct </i><f1><i>subspaces </i><f2> <i>is symmetric.</i>",
            "<b><i>Using </i></b><f1>\n<b><i>and </i></b><f2><b><i>together.</i></b>",
        ] {
            let joined = coalesce_styles(source);
            assert_eq!(joined.matches("<i>").count(), 1);
            assert_eq!(emphasis_counts(source), emphasis_counts(&joined));
            assert_eq!(strip_style_tags(source), strip_style_tags(&joined));
        }
        let separate = "<i>First </i><f1> and <i>second </i><f2>.";
        assert_eq!(coalesce_styles(separate), separate);
    }

    #[test]
    fn punctuation_and_article_fragments_do_not_trigger_false_style_failures() {
        assert_eq!(
            validate_translation("). The ", "）。", &[]),
            Some("）。".into())
        );
        assert_eq!(validate_translation(". An ", "。", &[]), Some("。".into()));
        for source in ["The result.", "Some explanation.", "The 12", "<i>The</i>"] {
            assert!(
                validate_translation(source, "。", &[]).is_none(),
                "{source}"
            );
        }
    }

    #[test]
    fn literal_entities_cannot_be_invented_but_conjunctions_can_be_translated() {
        assert!(validate_translation("A statement.", "一个陈述。\n&lt; &gt; &amp;", &[]).is_none());
        assert_eq!(
            validate_translation("A &amp; B", "甲和乙", &[]),
            Some("甲和乙".to_owned())
        );
        assert_eq!(
            validate_translation("A &amp; B", "甲 &amp; 乙", &[]),
            Some("甲 &amp; 乙".to_owned())
        );
    }

    #[test]
    fn formula_closing_tags_cannot_leak_into_the_output() {
        assert!(validate_translation("A <f1> formula", "<f1>公式</f1>", &["f1".into()]).is_none());
        assert_eq!(
            validate_translation("literal &lt;/f1&gt;", "字面 &lt;/f1&gt;", &[]),
            Some("字面 &lt;/f1&gt;".into())
        );
        assert!(validate_translation("6.45 bits", "<f6.45 bits>", &[]).is_none());
        assert_eq!(
            validate_translation("literal &lt;bot&gt;", "字面 <bot>", &[]),
            Some("字面 &lt;bot&gt;".into())
        );
    }

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
            validate_translation(
                "Using <f1> for input and <f2> for output",
                "先输出 <f2>，再输入 <f1>",
                &["f1".to_string(), "f2".to_string()]
            )
            .unwrap(),
            "先输出 <f2>，再输入 <f1>"
        );
        assert!(
            validate_translation(
                "Using <f1> for input and <f2> for output",
                "只有 <f1>",
                &["f1".to_string(), "f2".to_string()]
            )
            .is_none()
        );
        assert!(
            validate_translation(
                "Using <f1> for input and <f2> for output",
                "<f1><f1><f2>",
                &["f1".to_string(), "f2".to_string()]
            )
            .is_none()
        );
        assert_eq!(
            validate_translation("literal &lt;b&gt;", "字面 <b>", &[]).unwrap(),
            "字面 &lt;b&gt;"
        );
    }
}
