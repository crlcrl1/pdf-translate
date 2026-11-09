use crate::markup::{escape_literal, style_close, style_open};
use crate::model::{BBox, FormulaRef, GlyphBox, Paragraph, TextLine, TextSpan, TextStyle};
use crate::text::{collapse_whitespace, is_cjk};
use std::cmp::Ordering;

pub(super) fn attach_inline_formulas(paragraph: &mut Paragraph, lines: &[TextLine]) {
    let mut formulas = Vec::new();
    let mut prompt_lines = Vec::new();
    let mut next_id = 1;
    let base = paragraph.base_style;
    for line in paragraph.lines(lines) {
        prompt_lines.push(if line.formula {
            escape_literal(&line.text)
        } else {
            prompt_line(line, base, &paragraph.text, &mut next_id, &mut formulas)
        });
    }
    let has_style = prompt_lines
        .iter()
        .any(|line| line.contains("<i>") || line.contains("<b>"));
    if formulas.is_empty() && !has_style {
        let escaped = escape_literal(&paragraph.text);
        paragraph.prompt_text = (escaped != paragraph.text).then_some(escaped);
        return;
    }
    let mut prompt = String::new();
    for (index, line) in prompt_lines.iter().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if index > 0 && !prompt.is_empty() {
            if prompt.ends_with('-') && line.chars().next().is_some_and(|ch| ch.is_lowercase()) {
                prompt.pop();
            } else if !prompt.ends_with(char::is_whitespace)
                && !(prompt.ends_with(is_cjk) && line.starts_with(is_cjk))
            {
                prompt.push(' ');
            }
        }
        prompt.push_str(line);
    }
    paragraph.prompt_text = Some(prompt);
    paragraph.formulas = formulas;
}

struct InlineGroup {
    start: usize,
    end: usize,
    bbox: BBox,
    text: String,
}

fn prompt_line(
    line: &TextLine,
    base: TextStyle,
    original: &str,
    next_id: &mut usize,
    formulas: &mut Vec<FormulaRef>,
) -> String {
    let glyphs: Vec<_> = line
        .glyphs
        .iter()
        .filter(|g| !g.text.trim().is_empty())
        .collect();
    let body_font = dominant_font(&glyphs);
    let urls = find_url_groups(line, &glyphs);
    let mut groups = Vec::new();
    for (start, end) in find_formula_groups(line, &glyphs, body_font.as_deref()) {
        if urls.iter().any(|url| start <= url.end && url.start <= end) {
            continue;
        }
        let group = &glyphs[start..=end];
        let bbox = group
            .iter()
            .fold(group[0].bbox, |bbox, glyph| bbox.union(glyph.bbox));
        let text = group.iter().map(|g| g.text.as_str()).collect();
        groups.push(InlineGroup {
            start,
            end,
            bbox,
            text,
        });
    }
    groups.extend(urls);
    groups.sort_by_key(|g| g.start);
    let mut groups = groups.into_iter().peekable();
    let mut prompt = String::new();
    let mut previous_x1 = None;
    let mut style = TextStyle::default();
    let mut i = 0;
    while let Some(glyph) = glyphs.get(i) {
        while groups.peek().is_some_and(|group| group.start < i) {
            groups.next();
        }
        let group = groups.next_if(|group| group.start == i);
        let x0 = group.as_ref().map_or(glyph.bbox.x0, |group| group.bbox.x0);
        if previous_x1.is_some_and(|x1| x0 - x1 > glyph.font_size.max(6.0) * 0.22)
            && !prompt.ends_with(char::is_whitespace)
            && (group.is_some() || !prompt.ends_with('_'))
        {
            prompt.push(' ');
        }
        if let Some(group) = group {
            prompt.push_str(style_close(style));
            style = TextStyle::default();
            let id = loop {
                let id = format!("f{}", *next_id);
                *next_id += 1;
                if !original.contains(&format!("<{id}>")) {
                    break id;
                }
            };
            prompt.push_str(&format!("<{id}>"));
            formulas.push(FormulaRef {
                id,
                bbox: group.bbox,
                text: group.text,
                baseline: line.baseline,
                sources: glyphs[i..=group.end]
                    .iter()
                    .flat_map(|g| g.sources.iter().copied())
                    .collect(),
            });
            previous_x1 = Some(group.bbox.x1);
            i = group.end + 1;
        } else {
            let next = TextStyle {
                bold: glyph.bold && !base.bold,
                italic: glyph.italic && !base.italic,
            };
            if next != style {
                prompt.push_str(style_close(style));
                prompt.push_str(style_open(next));
                style = next;
            }
            prompt.push_str(&escape_literal(&glyph.text));
            previous_x1 = Some(glyph.bbox.x1.max(glyph.bbox.x0 + glyph.advance * 0.5));
            i += 1;
        }
    }
    prompt.push_str(style_close(style));
    collapse_whitespace(&prompt)
}

fn dominant_font(glyphs: &[&GlyphBox]) -> Option<String> {
    // BTreeMap keeps tie-breaking deterministic across runs; HashMap iteration
    // order is randomized and changes which font wins on equal counts.
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for glyph in glyphs {
        if let Some(name) = &glyph.font_name {
            *counts.entry(name.clone()).or_default() += glyph.text.chars().count().max(1);
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(name, _)| name)
}

pub(super) fn is_url_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("http://")
        || lower.contains("https://")
        || lower.contains("www.")
        || lower.contains("doi.org/")
        || lower.contains("ftp://")
}

fn is_url_like_span(span: &TextSpan) -> bool {
    is_url_text(&span.text)
        || (span.mono
            && (span.text.contains(".php")
                || span.text.contains('?')
                || span.text.contains('&')
                || span.text.contains('=')
                || span.text.contains('#')))
}

fn find_url_groups(line: &TextLine, glyphs: &[&GlyphBox]) -> Vec<InlineGroup> {
    let has_url_span = line.spans.iter().any(is_url_like_span);
    let mut spans: Vec<&TextSpan> = line
        .spans
        .iter()
        .filter(|span| is_url_like_span(span) || (has_url_span && span.mono))
        .collect();
    spans.sort_by(|a, b| a.bbox.x0.partial_cmp(&b.bbox.x0).unwrap_or(Ordering::Equal));

    let mut span_groups: Vec<(BBox, String)> = Vec::new();
    for span in spans {
        if let Some((bbox, text)) = span_groups.last_mut()
            && span.bbox.x0 - bbox.x1 < 3.0
        {
            *bbox = bbox.union(span.bbox);
            text.push_str(&span.text);
            continue;
        }
        span_groups.push((span.bbox, span.text.clone()));
    }

    let mut groups = Vec::new();
    for (bbox, text) in span_groups {
        let mut start = None;
        let mut end = None;
        for (index, glyph) in glyphs.iter().enumerate() {
            let in_x = glyph.origin_x >= bbox.x0 - 0.8 && glyph.origin_x <= bbox.x1 + 0.8;
            let in_y = (glyph.origin_y - line.baseline).abs() <= line.font_size.max(6.0) * 0.8;
            if in_x && in_y {
                start.get_or_insert(index);
                end = Some(index);
            }
        }
        if let (Some(start), Some(end)) = (start, end) {
            groups.push(InlineGroup {
                start,
                end,
                bbox,
                text,
            });
        }
    }
    groups
}

pub(super) fn find_formula_groups(
    line: &TextLine,
    glyphs: &[&GlyphBox],
    body_font: Option<&str>,
) -> Vec<(usize, usize)> {
    let Some(body_font) = body_font else {
        return Vec::new();
    };
    let line_size = line.font_size.max(6.0);
    let mut groups = Vec::new();
    let mut index = 0;
    while index < glyphs.len() {
        if !is_formula_start(glyphs, index, body_font, line_size) {
            index += 1;
            continue;
        }
        let mut start = index;
        // Include the full digit run before an operator (e.g. the "32" in
        // "32×32"), not just the digit immediately preceding the operator.
        while start > 0
            && !groups.last().is_some_and(|(_, end)| start - 1 <= *end)
            && is_body_digit(glyphs[start - 1], body_font)
            && glyph_gap(glyphs[start - 1], glyphs[start]) <= line_size * 0.3
        {
            start -= 1;
        }
        let mut end = index;
        while end + 1 < glyphs.len()
            && can_extend_formula(glyphs[end], glyphs[end + 1], body_font, line_size)
        {
            end += 1;
        }
        while start < end
            && glyphs[start]
                .text
                .chars()
                .all(is_formula_prefix_punctuation)
        {
            start += 1;
        }
        while end > start && glyphs[end].text.chars().all(is_formula_suffix_punctuation) {
            end -= 1;
        }
        let text: String = glyphs[start..=end]
            .iter()
            .map(|glyph| glyph.text.as_str())
            .collect();
        let width = glyphs[end].bbox.x1 - glyphs[start].bbox.x0;
        if is_valid_formula_text(&glyphs[start..=end], &text)
            && !is_single_letter_inside_word(glyphs, start, end, body_font)
            && width < line.bbox.width() * 0.75
        {
            groups.push((start, end));
        }
        index = (end + 1).max(index + 1);
    }
    groups
}

fn is_formula_start(glyphs: &[&GlyphBox], index: usize, body_font: &str, line_size: f64) -> bool {
    let glyph = glyphs[index];
    let Some(font) = glyph.font_name.as_deref() else {
        return false;
    };
    let nonbody = font != body_font;
    let math_font = is_math_font(Some(font));
    let script = glyph.font_size < line_size * 0.8;
    let single_letter = glyph.text.chars().count() == 1
        && glyph
            .text
            .chars()
            .next()
            .is_some_and(|ch| ch.is_alphabetic());
    math_font || (nonbody && (single_letter || is_math_token(&glyph.text) || script))
}

fn can_extend_formula(
    current: &GlyphBox,
    next: &GlyphBox,
    body_font: &str,
    line_size: f64,
) -> bool {
    if glyph_gap(current, next) > line_size * 1.0 {
        return false;
    }
    let Some(font) = next.font_name.as_deref() else {
        return false;
    };
    if font != body_font {
        return true;
    }
    let text = next.text.as_str();
    !text.is_empty()
        && text.chars().all(|ch| {
            ch.is_ascii_digit() || ch == '-' || is_math_delimiter(ch) || is_math_operator(ch)
        })
}

fn is_body_digit(glyph: &GlyphBox, body_font: &str) -> bool {
    glyph.font_name.as_deref() == Some(body_font)
        && glyph.text.chars().all(|ch| ch.is_ascii_digit())
}

fn glyph_gap(left: &GlyphBox, right: &GlyphBox) -> f64 {
    let advance = if left.advance > 0.1 {
        left.advance
    } else {
        left.bbox.width()
    };
    (right.origin_x - (left.origin_x + advance)).max(0.0)
}

fn is_math_token(text: &str) -> bool {
    !text.is_empty()
        && text.chars().all(|ch| {
            is_math_char(ch)
                || ch.is_ascii_digit()
                || ch == '-'
                || is_math_delimiter(ch)
                || is_math_operator(ch)
                || is_math_punctuation(ch)
        })
}

fn is_single_letter_inside_word(
    glyphs: &[&GlyphBox],
    start: usize,
    end: usize,
    body_font: &str,
) -> bool {
    if start != end || glyphs[start].text.chars().count() != 1 {
        return false;
    }
    let small_gap =
        |left: &GlyphBox, right: &GlyphBox| glyph_gap(left, right) < right.font_size.max(6.0) * 0.1;
    let body_letter = |glyph: &GlyphBox| {
        glyph.font_name.as_deref() == Some(body_font)
            && glyph.text.chars().all(|ch| ch.is_alphabetic())
    };
    (start > 0 && body_letter(glyphs[start - 1]) && small_gap(glyphs[start - 1], glyphs[start]))
        || (end + 1 < glyphs.len()
            && body_letter(glyphs[end + 1])
            && small_gap(glyphs[end], glyphs[end + 1]))
}

fn is_valid_formula_text(glyphs: &[&GlyphBox], text: &str) -> bool {
    if text.contains("://")
        || text.starts_with("www.")
        || text.contains(".php")
        || (text.contains('=') && (text.contains('&') || text.contains('?')))
    {
        return false;
    }
    // A group ending in a binary operator has no right operand. For example,
    // "8×" in "8× deeper" is prose; treating it as a formula makes small
    // translation models drop the placeholder.
    if text.chars().last().is_some_and(|ch| {
        matches!(
            unicode_math_class::class(ch),
            Some(unicode_math_class::MathClass::Binary)
        )
    }) {
        return false;
    }
    let has_letter = text.chars().any(|ch| ch.is_alphabetic());
    let has_operator = text.chars().any(is_math_operator);
    let has_math_font = glyphs
        .iter()
        .any(|glyph| is_math_font(glyph.font_name.as_deref()));
    if !has_letter && !has_operator {
        return false;
    }
    if has_math_font || has_operator {
        return true;
    }
    text.chars().count() == 1 && text.chars().next().is_some_and(|ch| ch.is_alphabetic())
}

fn is_math_font(name: Option<&str>) -> bool {
    let Some(name) = name else {
        return false;
    };
    let upper = name.to_ascii_uppercase();
    const MARKERS: &[&str] = &[
        "CMMI",
        "CMSY",
        "CMEX",
        "MSAM",
        "MSBM",
        "STIX",
        "XITS",
        "LATINMODERNMATH",
        "TEXGYRE",
        "ASANA",
        "LIBERTINUSMATH",
        "EUSM",
        "EUFM",
        "RSFS",
        "WASY",
        "MATH",
        "SYMBOL",
        "ZAPF",
        "TYPE3",
    ];
    MARKERS.iter().any(|marker| upper.contains(marker))
}

pub(super) fn is_math_glyph(glyph: &GlyphBox) -> bool {
    is_math_font(glyph.font_name.as_deref())
        || glyph.text.chars().any(is_math_char)
            && glyph.text.chars().filter(|c| is_math_char(*c)).count() * 2
                >= glyph.text.chars().count().max(1)
}

pub(super) fn math_char_ratio(text: &str) -> f64 {
    let total = text.chars().filter(|c| !c.is_whitespace()).count();
    if total == 0 {
        return 0.0;
    }
    let math = text.chars().filter(|c| is_math_char(*c)).count();
    math as f64 / total as f64
}

pub(super) fn is_math_char(ch: char) -> bool {
    const MATH_BLOCKS: [unicode_blocks::UnicodeBlock; 8] = [
        unicode_blocks::GREEK_AND_COPTIC,
        unicode_blocks::MATHEMATICAL_ALPHANUMERIC_SYMBOLS,
        unicode_blocks::ARROWS,
        unicode_blocks::MATHEMATICAL_OPERATORS,
        unicode_blocks::LETTERLIKE_SYMBOLS,
        unicode_blocks::SUPPLEMENTAL_MATHEMATICAL_OPERATORS,
        unicode_blocks::MISCELLANEOUS_MATHEMATICAL_SYMBOLS_A,
        unicode_blocks::MISCELLANEOUS_MATHEMATICAL_SYMBOLS_B,
    ];
    let in_math_block =
        unicode_blocks::find_unicode_block(ch).is_some_and(|block| MATH_BLOCKS.contains(&block));
    // The blocks above cover Unicode math symbols and Greek; these are the
    // common operators and symbols that live in Latin-1 or ASCII.
    in_math_block || matches!(ch, '±' | '×' | '÷' | '¬' | '°' | '=' | '+' | '<' | '>')
}

pub(super) fn is_math_operator(ch: char) -> bool {
    matches!(
        unicode_math_class::class(ch),
        Some(
            unicode_math_class::MathClass::Binary
                | unicode_math_class::MathClass::Relation
                | unicode_math_class::MathClass::Large
                | unicode_math_class::MathClass::Unary
                | unicode_math_class::MathClass::Vary
        )
    )
}

fn is_math_delimiter(ch: char) -> bool {
    matches!(
        unicode_math_class::class(ch),
        Some(
            unicode_math_class::MathClass::Opening
                | unicode_math_class::MathClass::Closing
                | unicode_math_class::MathClass::Fence
        )
    )
}

fn is_math_punctuation(ch: char) -> bool {
    matches!(
        unicode_math_class::class(ch),
        Some(unicode_math_class::MathClass::Punctuation)
    )
}

fn is_formula_prefix_punctuation(ch: char) -> bool {
    unicode_math_class::class(ch) == Some(unicode_math_class::MathClass::Closing)
        || is_math_punctuation(ch)
}

fn is_formula_suffix_punctuation(ch: char) -> bool {
    unicode_math_class::class(ch) == Some(unicode_math_class::MathClass::Opening)
        || is_math_punctuation(ch)
}
