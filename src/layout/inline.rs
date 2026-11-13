use crate::markup::{escape_literal, style_close, style_open};
use crate::model::{BBox, FormulaRef, GlyphBox, Paragraph, TextLine, TextStyle};
use crate::text::{bullet_marker, collapse_whitespace, is_accent, is_cjk};

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
    let has_style = prompt_lines.iter().any(|line| {
        crate::markup::pieces(line).any(|piece| !matches!(piece, crate::markup::Piece::Text(_)))
    });
    if formulas.is_empty() && !has_style {
        let escaped = escape_literal(&paragraph.text);
        paragraph.prompt_text = (escaped != paragraph.text).then_some(escaped);
        return;
    }
    let joined = crate::markup::coalesce_styles(&prompt_lines.join("\n"));
    let mut prompt = String::new();
    for (index, line) in joined.lines().enumerate() {
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
    paragraph.prompt_text = Some(crate::markup::coalesce_styles(&prompt));
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
    let accents = find_accented_words(&glyphs);
    let mut groups = find_code_groups(&glyphs);
    for (start, end) in find_formula_groups(line, &glyphs, body_font.as_deref()) {
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
    groups.extend(accents);
    groups.sort_by_key(|g| g.start);
    let mut merged: Vec<InlineGroup> = Vec::new();
    for group in groups {
        if let Some(previous) = merged.last_mut()
            && group.start <= previous.end
        {
            previous.end = previous.end.max(group.end);
            previous.bbox = previous.bbox.union(group.bbox);
            previous.text = glyphs[previous.start..=previous.end]
                .iter()
                .map(|g| g.text.as_str())
                .collect();
        } else {
            merged.push(group);
        }
    }
    let mut groups = merged.into_iter().peekable();
    let mut prompt = String::new();
    let mut previous_x1 = None;
    let mut style = TextStyle::default();
    let mut after_formula = false;
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
            prompt.push_str(&style_close(style));
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
            after_formula = true;
            i = group.end + 1;
        } else {
            let next = TextStyle {
                // A lone styled period can be a PDF font transition, not an
                // emphasized phrase (notably decimal points in math fonts).
                // Retain punctuation styling within an existing styled run.
                bold: glyph.bold
                    && !base.bold
                    && (style.bold
                        || after_formula
                        || glyph.text.chars().any(char::is_alphanumeric)),
                italic: glyph.italic
                    && !base.italic
                    && (style.italic
                        || after_formula
                        || glyph.text.chars().any(char::is_alphanumeric)),
                color: (glyph.color != base.color.unwrap_or([0, 0, 0, 255])).then_some(glyph.color),
            };
            if next != style {
                prompt.push_str(&style_close(style));
                prompt.push_str(&style_open(next));
                style = next;
            }
            prompt.push_str(&escape_literal(&glyph.text));
            previous_x1 = Some(glyph.bbox.x1.max(glyph.bbox.x0 + glyph.advance * 0.5));
            after_formula = false;
            i += 1;
        }
    }
    prompt.push_str(&style_close(style));
    collapse_whitespace(&prompt)
}

fn find_code_groups(glyphs: &[&GlyphBox]) -> Vec<InlineGroup> {
    let mut groups = Vec::new();
    let mut index = 0;
    while index < glyphs.len() {
        if !glyphs[index].mono {
            index += 1;
            continue;
        }
        let start = index;
        while index + 1 < glyphs.len()
            && glyphs[index + 1].mono
            && glyph_gap(glyphs[index], glyphs[index + 1]) < glyphs[index].font_size * 0.5
        {
            index += 1;
        }
        groups.push(InlineGroup {
            start,
            end: index,
            bbox: glyphs[start..=index]
                .iter()
                .map(|g| g.bbox)
                .reduce(BBox::union)
                .unwrap(),
            text: glyphs[start..=index]
                .iter()
                .map(|g| g.text.as_str())
                .collect(),
        });
        index += 1;
    }
    groups
}

fn find_accented_words(glyphs: &[&GlyphBox]) -> Vec<InlineGroup> {
    let mut result = Vec::new();
    for (index, glyph) in glyphs.iter().enumerate() {
        // TeX names may use separate spacing accents and dotless i/j, absent
        // from CJK fonts. Keep the original name as an inline vector object so
        // one uncommon Latin glyph does not suppress the surrounding prose.
        if !glyph
            .text
            .chars()
            .any(|ch| matches!(ch, '\u{0131}' | '\u{0237}') || is_accent(ch))
            || result
                .last()
                .is_some_and(|group: &InlineGroup| index <= group.end)
        {
            continue;
        }
        let word = |g: &GlyphBox| g.text.chars().all(|ch| ch.is_alphabetic() || is_accent(ch));
        let mut start = index;
        let mut end = index;
        while start > 0
            && word(glyphs[start - 1])
            && glyph_gap(glyphs[start - 1], glyphs[start]) < glyph.font_size * 0.22
            && glyphs[start].bbox.x0 - glyphs[start - 1].bbox.x1 < glyph.font_size * 0.22
        {
            start -= 1;
        }
        while end + 1 < glyphs.len()
            && word(glyphs[end + 1])
            && glyph_gap(glyphs[end], glyphs[end + 1]) < glyph.font_size * 0.22
            && glyphs[end + 1].bbox.x0 - glyphs[end].bbox.x1 < glyph.font_size * 0.22
        {
            end += 1;
        }
        let bbox = glyphs[start..=end]
            .iter()
            .map(|g| g.bbox)
            .reduce(BBox::union)
            .unwrap();
        result.push(InlineGroup {
            start,
            end,
            bbox,
            text: glyphs[start..=end]
                .iter()
                .map(|g| g.text.as_str())
                .collect(),
        });
    }
    result
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

fn find_url_groups(line: &TextLine, glyphs: &[&GlyphBox]) -> Vec<InlineGroup> {
    let mut groups = Vec::new();
    let mut start = 0;
    while start < glyphs.len() {
        // A style span can contain prose and a URL in the same font. Bind
        // only the URL's glyphs, never the surrounding author names or text.
        let prefix: String = glyphs[start..]
            .iter()
            .take(10)
            .map(|g| g.text.as_str())
            .collect();
        if !["http://", "https://", "www.", "doi.org/", "ftp://"]
            .iter()
            .any(|prefix_text| prefix.to_ascii_lowercase().starts_with(prefix_text))
        {
            start += 1;
            continue;
        }
        let mut end = start;
        let mut depth = 0_i32;
        while end + 1 < glyphs.len() {
            let next = glyphs[end + 1];
            if glyph_gap(glyphs[end], next) > line.font_size * 0.22
                || next.text.chars().any(char::is_whitespace)
                || (depth == 0 && next.text.chars().any(|ch| matches!(ch, ')' | ']' | '}')))
            {
                break;
            }
            for ch in next.text.chars() {
                depth += i32::from(matches!(ch, '(' | '[' | '{'));
                depth -= i32::from(matches!(ch, ')' | ']' | '}'));
            }
            end += 1;
        }
        while end > start
            && glyphs[end]
                .text
                .chars()
                .all(|ch| matches!(ch, '.' | ',' | ';' | ':'))
        {
            end -= 1;
        }
        groups.push(InlineGroup {
            start,
            end,
            bbox: glyphs[start..=end]
                .iter()
                .map(|g| g.bbox)
                .reduce(BBox::union)
                .unwrap(),
            text: glyphs[start..=end]
                .iter()
                .map(|g| g.text.as_str())
                .collect(),
        });
        start = end + 1;
    }
    groups
}

pub(super) fn find_formula_groups(
    line: &TextLine,
    glyphs: &[&GlyphBox],
    body_font: Option<&str>,
) -> Vec<(usize, usize)> {
    // List symbols are original layout objects even when they share the prose
    // font. Font voting must not alternate between a source bullet and a CJK
    // replacement as the amount of bold text changes between list items.
    // Keep the symbol separate from any expression at the start of the item.
    let bullet = bullet_marker(&line.text).is_some_and(|marker| {
        glyphs.first().is_some_and(|glyph| {
            let mut chars = glyph.text.chars();
            chars.next() == Some(marker) && chars.next().is_none()
        })
    });
    let mut groups = if bullet { vec![(0, 0)] } else { Vec::new() };
    let Some(body_font) = body_font else {
        return groups;
    };
    let line_size = line.font_size.max(6.0);
    let mut index = usize::from(bullet);
    while index < glyphs.len() {
        let named_end = math_call_end(glyphs, index, line_size)
            .or_else(|| delimited_math_end(glyphs, index, line_size));
        if !is_formula_start(glyphs, index, body_font, line_size) && named_end.is_none() {
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
        // Validate a complete short argument before applying word heuristics.
        // In Palatino, e.g. ⟨v, T_z v⟩, the operands share a prose italic face
        // and can otherwise be mistaken for a word after the comma.
        let mut end = named_end.unwrap_or(index);
        while end + 1 < glyphs.len() {
            if is_prose_article(glyphs, end + 1, line_size) {
                break;
            }
            let argument_depth: i32 = glyphs[start..=end]
                .iter()
                .flat_map(|g| g.text.chars())
                .map(|ch| {
                    i32::from(math_closing_delimiter(ch).is_some())
                        - i32::from(
                            unicode_math_class::class(ch)
                                == Some(unicode_math_class::MathClass::Closing),
                        )
                })
                .sum();
            let roman_argument = argument_depth > 0
                && glyph_gap(glyphs[end], glyphs[end + 1]) < line_size * 0.3
                && glyphs[end + 1].text.chars().count() == 1
                && glyphs[end + 1]
                    .text
                    .chars()
                    .all(|ch| ch.is_ascii_alphabetic());
            if glyph_gap(glyphs[end], glyphs[end + 1]) <= line_size * 0.5
                && !(argument_depth == 0 && glyphs[end].text.chars().all(is_math_punctuation))
                && let Some(operator_end) = math_call_end(glyphs, end + 1, line_size)
                    .or_else(|| delimited_math_end(glyphs, end + 1, line_size))
            {
                end = operator_end;
            } else if (roman_argument
                || can_extend_formula(glyphs[end], glyphs[end + 1], body_font, line_size))
                && !is_single_letter_inside_word(glyphs, end + 1, end + 1, body_font, line_size)
            {
                end += 1;
            } else {
                break;
            }
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
        // Include an opening delimiter when this group already owns its
        // closing mate. Leaving only '(' in the prompt makes models add a
        // second ')' without knowing one is inside the protected object.
        while start > 0 && glyph_gap(glyphs[start - 1], glyphs[start]) < line_size * 0.5 {
            let mut chars = glyphs[start - 1].text.chars();
            let Some(open) = chars.next() else { break };
            let Some(close) = math_closing_delimiter(open) else {
                break;
            };
            if chars.next().is_some() {
                break;
            }
            let balance: i32 = glyphs[start..=end]
                .iter()
                .flat_map(|g| g.text.chars())
                .map(|ch| i32::from(ch == open) - i32::from(ch == close))
                .sum();
            if balance >= 0 {
                break;
            }
            start -= 1;
        }
        let text: String = glyphs[start..=end]
            .iter()
            .map(|glyph| glyph.text.as_str())
            .collect();
        let width = glyphs[end].bbox.x1 - glyphs[start].bbox.x0;
        let outside_prose = glyphs[..start]
            .iter()
            .chain(glyphs[end + 1..].iter())
            .filter(|g| !is_math_glyph(g) && g.font_size >= line_size * 0.8)
            .flat_map(|g| g.text.chars())
            .filter(|ch| ch.is_ascii_alphabetic())
            .count();
        if (named_end.is_some() || is_valid_formula_text(&glyphs[start..=end], &text))
            && !is_single_letter_inside_word(glyphs, start, end, body_font, line_size)
            // A short introduction can precede an expression occupying most
            // of the line. Never expose that expression as prose just because
            // it exceeds the width guard for an otherwise formula-only line.
            && (width < line.bbox.width() * 0.75 || outside_prose >= 3)
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
    let math_font = is_math_glyph(glyph);
    let script = glyph.font_size < line_size * 0.8;
    let single_letter = glyph.text.chars().count() == 1
        && glyph
            .text
            .chars()
            .next()
            .is_some_and(|ch| ch.is_alphabetic());
    if !math_font
        && single_letter
        && index > 0
        && !is_math_glyph(glyphs[index - 1])
        && glyphs[index - 1].text.chars().all(|ch| ch.is_alphabetic())
        && !is_script_pair(glyphs[index - 1], glyph)
        && glyph_gap(glyphs[index - 1], glyph) < line_size * 0.1
    {
        return false;
    }
    if is_prose_article(glyphs, index, line_size) {
        return false;
    }
    math_font
        || (nonbody && (single_letter || is_math_token(&glyph.text) || script))
        || (single_letter
            && glyphs.get(index + 1).is_some_and(|next| {
                next.font_size < glyph.font_size && is_script_pair(glyph, next)
            }))
}

fn is_prose_article(glyphs: &[&GlyphBox], index: usize, line_size: f64) -> bool {
    let glyph = glyphs[index];
    !is_math_glyph(glyph)
        && glyph.font_size >= line_size * 0.8
        && matches!(glyph.text.as_str(), "A" | "a" | "I")
        && glyphs.get(index + 1..index + 3).is_some_and(|next| {
            // An article can precede an emphasized noun in another font, as
            // in "x (a positive literal)". Do not bind it to the preceding x.
            next.iter().all(|g| {
                !is_math_glyph(g)
                    && (!glyph.italic || g.italic)
                    && g.text.chars().all(|ch| ch.is_alphabetic())
                    && (g.font_size - glyph.font_size).abs() < line_size * 0.1
            }) && glyph_gap(glyph, next[0]) < line_size * 0.6
                && glyph_gap(next[0], next[1]) < line_size * 0.1
        })
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
    if next.math_group || is_math_font(Some(font)) || next.font_size < line_size * 0.8 {
        return true;
    }
    if is_script_pair(current, next) {
        return true;
    }
    if font != body_font
        && next.text.chars().count() == 1
        && next.text.chars().all(|ch| ch.is_alphabetic())
        && glyph_gap(current, next) < line_size * 0.3
    {
        return true;
    }
    if font != body_font
        && current.font_name.as_deref() == Some(font)
        && glyph_gap(current, next) < line_size * 0.12
        && next.italic
    {
        return true;
    }
    let text = next.text.as_str();
    !text.is_empty()
        && text.chars().all(|ch| {
            ch.is_ascii_digit()
                || ch == '-'
                || is_math_delimiter(ch)
                || is_math_operator(ch)
                || is_math_punctuation(ch)
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

fn is_script_pair(first: &GlyphBox, second: &GlyphBox) -> bool {
    let (base, script) = if first.font_size >= second.font_size {
        (first, second)
    } else {
        (second, first)
    };
    let shift = (base.origin_y - script.origin_y).abs();
    let (left, right) = if first.origin_x <= second.origin_x {
        (first, second)
    } else {
        (second, first)
    };
    // A size change alone also occurs in small caps. A true script is smaller,
    // displaced from the base's baseline and tightly attached horizontally.
    script.font_size < base.font_size * 0.8
        && shift >= base.font_size * 0.1
        && shift <= base.font_size * 0.85
        && glyph_gap(left, right) < base.font_size * 0.22
        && [base, script]
            .iter()
            .all(|glyph| !glyph.text.is_empty() && glyph.text.chars().all(char::is_alphanumeric))
}

/// Recognize equation fragments without counting scripted labels or the
/// arguments of a mathematical call as ordinary prose words.
pub(super) fn is_math_only(line: &TextLine) -> bool {
    let glyphs: Vec<_> = line
        .glyphs
        .iter()
        .filter(|g| !g.text.trim().is_empty())
        .collect();
    let mut prose = String::new();
    let mut evidence = false;
    let mut index = 0;
    while index < glyphs.len() {
        let glyph = glyphs[index];
        if let Some(end) = math_call_end(&glyphs, index, line.font_size)
            .or_else(|| delimited_math_end(&glyphs, index, line.font_size))
        {
            evidence = true;
            prose.push(' ');
            index = end + 1;
            continue;
        }
        if index > 0
            && glyph.font_size < glyphs[index - 1].font_size
            && is_script_pair(glyphs[index - 1], glyph)
        {
            evidence = true;
            let first = glyph;
            index += 1;
            while index < glyphs.len()
                && (glyphs[index].font_size - first.font_size).abs() < first.font_size * 0.1
                && (glyphs[index].origin_y - first.origin_y).abs() < first.font_size * 0.2
                && glyph_gap(glyphs[index - 1], glyphs[index]) < first.font_size * 0.22
                && glyphs[index].text.chars().all(char::is_alphanumeric)
            {
                index += 1;
            }
            prose.push(' ');
            continue;
        }
        if is_math_glyph(glyph) {
            evidence = true;
            prose.push(' ');
        } else {
            evidence |= glyph.text.chars().any(is_math_operator);
            if index > 0 && glyph_gap(glyphs[index - 1], glyph) > line.font_size * 0.12 {
                prose.push(' ');
            }
            prose.push_str(&glyph.text);
        }
        index += 1;
    }
    evidence
        && prose
            .split(|ch: char| !ch.is_alphabetic())
            .all(|word| word.chars().count() <= 1 || is_named_math_operator(word))
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
    line_size: f64,
) -> bool {
    if start != end
        || glyphs[start].font_size < line_size * 0.8
        || glyphs[start].text.chars().count() != 1
        || !glyphs[start].text.chars().all(|ch| ch.is_alphabetic())
        || is_math_glyph(glyphs[start])
    {
        return false;
    }
    let small_gap =
        |left: &GlyphBox, right: &GlyphBox| glyph_gap(left, right) < right.font_size.max(6.0) * 0.1;
    let body_letter = |glyph: &GlyphBox| {
        (glyph.font_name.as_deref() == Some(body_font) || !is_math_glyph(glyph))
            && glyph.text.chars().all(|ch| ch.is_alphabetic())
            && !is_script_pair(glyphs[start], glyph)
    };
    (start > 0 && body_letter(glyphs[start - 1]) && small_gap(glyphs[start - 1], glyphs[start]))
        // Possessives and contractions can switch fonts after an apostrophe.
        // Keep the suffix in prose; actual mathematical primes still have a
        // math-font operand and are excluded by the checks above.
        || (start > 1
            && matches!(glyphs[start - 1].text.as_str(), "'" | "’")
            && !is_math_glyph(glyphs[start - 1])
            && body_letter(glyphs[start - 2])
            && small_gap(glyphs[start - 2], glyphs[start - 1])
            && small_gap(glyphs[start - 1], glyphs[start]))
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
    if text.chars().count() > 1
        && text.chars().last().is_some_and(|ch| {
            matches!(
                unicode_math_class::class(ch),
                Some(unicode_math_class::MathClass::Binary)
            )
        })
        && glyphs.last().is_some_and(|last| {
            // Superscript symbols can complete an expression (degrees,
            // charges, etc.) even when Unicode calls them binary operators.
            last.font_size >= glyphs[0].font_size * 0.8
                && last.origin_y <= glyphs[0].origin_y + glyphs[0].font_size * 0.2
        })
    {
        return false;
    }
    let has_letter = text.chars().any(|ch| ch.is_alphabetic());
    let has_operator = text.chars().any(is_math_operator);
    let has_math_font = glyphs
        .iter()
        .any(|glyph| glyph.math_group || is_math_font(glyph.font_name.as_deref()));
    if !has_letter && !has_operator {
        return false;
    }
    if has_math_font
        || has_operator
        || glyphs
            .windows(2)
            .any(|pair| is_script_pair(pair[0], pair[1]))
    {
        return true;
    }
    text.chars().count() == 1 && text.chars().next().is_some_and(|ch| ch.is_alphabetic())
}

pub(super) fn is_named_math_operator(word: &str) -> bool {
    matches!(
        word,
        "log"
            | "ln"
            | "exp"
            | "sin"
            | "cos"
            | "tan"
            | "cot"
            | "sec"
            | "csc"
            | "sinh"
            | "cosh"
            | "tanh"
            | "coth"
            | "arcsin"
            | "arccos"
            | "arctan"
            | "lim"
            | "limsup"
            | "liminf"
            | "det"
            | "card"
            | "deg"
            | "gcd"
            | "lcm"
            | "mod"
            | "sgn"
            | "arg"
            | "diag"
            | "argmin"
            | "argmax"
            | "min"
            | "max"
            | "sup"
            | "inf"
            | "std"
            | "Var"
            | "var"
            | "Cov"
            | "Pr"
            | "rank"
            | "dim"
            | "ker"
            | "tr"
            | "Tr"
            | "GL"
            | "PGL"
            | "SL"
            | "PSL"
            | "SO"
            | "SU"
            | "Sym"
            | "Hom"
            | "End"
            | "Aut"
            | "span"
            | "Re"
            | "Im"
            | "clip"
    )
}

fn named_operator_end(glyphs: &[&GlyphBox], start: usize, line_size: f64) -> Option<usize> {
    let mut end = start;
    let mut word = String::new();
    while end < glyphs.len() && glyphs[end].text.chars().all(|ch| ch.is_ascii_alphabetic()) {
        if end > start && glyph_gap(glyphs[end - 1], glyphs[end]) > line_size * 0.12 {
            break;
        }
        word.push_str(&glyphs[end].text);
        end += 1;
    }
    if !is_named_math_operator(&word) {
        return None;
    }
    let next = glyphs.get(end)?;
    // A prose occurrence of "min" or "rank" is not a math object. Require
    // an adjacent argument, delimiter or script before protecting the word.
    (glyph_gap(glyphs[end - 1], next) <= line_size * 0.5
        && (is_math_glyph(next)
            || next.font_size < line_size * 0.8
            || next
                .text
                .chars()
                .all(|ch| ch.is_ascii_digit() || is_math_delimiter(ch))))
    .then_some(end - 1)
}

fn math_call_end(glyphs: &[&GlyphBox], start: usize, line_size: f64) -> Option<usize> {
    if let Some(end) = named_operator_end(glyphs, start, line_size) {
        if glyphs.get(end + 1).is_some_and(|g| g.text == "(")
            && let Some(close) = math_argument_end(glyphs, end + 1, line_size, 16)
        {
            return Some(close);
        }
        return Some(end);
    }
    if start > 0
        && !is_math_glyph(glyphs[start - 1])
        && glyphs[start - 1]
            .text
            .chars()
            .all(|ch| ch.is_ascii_alphabetic())
        && glyph_gap(glyphs[start - 1], glyphs[start]) < line_size * 0.12
    {
        return None;
    }
    // Roman-font identifiers can have scripted arguments too. Recognize a
    // tightly set short call as a whole instead of exposing "W(H<f1>" to the
    // translator when only its subscript and closing parenthesis look like math.
    let mut end = start;
    let mut name = String::new();
    while end < glyphs.len() && glyphs[end].text.chars().all(|ch| ch.is_ascii_alphabetic()) {
        if end > start && glyph_gap(glyphs[end - 1], glyphs[end]) > line_size * 0.12 {
            return None;
        }
        name.push_str(&glyphs[end].text);
        if name.len() > 4 {
            return None;
        }
        end += 1;
    }
    if name.is_empty()
        || (name.len() > 1 && !name.chars().all(|ch| ch.is_ascii_uppercase()))
        || glyphs.get(end)?.text != "("
        || glyph_gap(glyphs[end - 1], glyphs[end])
            > line_size
                * if is_math_glyph(glyphs[end - 1]) {
                    0.3
                } else {
                    0.12
                }
    {
        return None;
    }
    math_argument_end(glyphs, end, line_size, 16)
}

fn delimited_math_end(glyphs: &[&GlyphBox], open: usize, line_size: f64) -> Option<usize> {
    let end = math_argument_end(glyphs, open, line_size, 3)?;
    // Short prose such as "x (see)" or "x [sic]" also has balanced brackets.
    // Unlike a named function call, a bare group needs its own math evidence.
    let text: String = glyphs[open + 1..end]
        .iter()
        .map(|g| g.text.as_str())
        .collect();
    let variable_list = text.contains(',')
        && text.chars().any(char::is_alphabetic)
        && text.split(',').all(|part| {
            let part = part.trim();
            part.chars().count() == 1 && part.chars().all(char::is_alphanumeric)
        });
    (variable_list
        || glyphs[open..=end]
            .iter()
            .any(|glyph| is_math_glyph(glyph) || glyph.font_size < line_size * 0.8))
    .then_some(end)
}

fn math_argument_end(
    glyphs: &[&GlyphBox],
    open: usize,
    line_size: f64,
    max_letters: usize,
) -> Option<usize> {
    let mut opening = glyphs.get(open)?.text.chars();
    let mut closing = vec![math_closing_delimiter(opening.next()?)?];
    if opening.next().is_some() {
        return None;
    }
    let mut letters = 0;
    for i in open + 1..glyphs.len().min(open + 128) {
        if glyph_gap(glyphs[i - 1], glyphs[i]) > line_size * 0.5 {
            return None;
        }
        // Roman names inside a known operator are part of its argument, but
        // a sequence of ordinary words separated by spaces is still prose.
        if glyphs[i - 1]
            .text
            .chars()
            .all(|ch| ch.is_ascii_alphabetic())
            && glyphs[i].text.chars().all(|ch| ch.is_ascii_alphabetic())
            && glyph_gap(glyphs[i - 1], glyphs[i]) > line_size * 0.12
        {
            return None;
        }
        for ch in glyphs[i].text.chars() {
            if closing.is_empty() {
                return None;
            }
            letters = if ch.is_ascii_alphabetic() {
                letters + 1
            } else {
                0
            };
            if letters > max_letters {
                return None;
            }
            if let Some(close) = math_closing_delimiter(ch) {
                closing.push(close);
            } else if unicode_math_class::class(ch) == Some(unicode_math_class::MathClass::Closing)
            {
                if closing.pop() != Some(ch) {
                    return None;
                }
            } else if !(ch.is_ascii_alphanumeric()
                || is_math_char(ch)
                || is_math_token(&ch.to_string()))
            {
                return None;
            }
        }
        if closing.is_empty() {
            return (i > open + 1).then_some(i);
        }
    }
    None
}

fn math_closing_delimiter(open: char) -> Option<char> {
    Some(match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '⟨' => '⟩',
        '⟪' => '⟫',
        '⟦' => '⟧',
        '⌈' => '⌉',
        '⌊' => '⌋',
        _ => return None,
    })
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
    glyph.math_group
        || is_math_font(glyph.font_name.as_deref())
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
