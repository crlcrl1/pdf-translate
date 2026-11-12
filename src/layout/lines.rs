use super::{
    PageFrame,
    inline::{
        is_math_char, is_math_glyph, is_math_operator, is_named_math_operator, math_char_ratio,
    },
    paragraphs::is_roman_numeral_digit,
};
use crate::model::{BBox, Column, GlyphBox, TextLine, TextSpan};
use crate::text::{collapse_whitespace, is_accent};
use std::cmp::Ordering;

pub(super) fn build_lines(glyphs: Vec<GlyphBox>) -> Vec<TextLine> {
    if glyphs.is_empty() {
        return Vec::new();
    }

    let mut glyphs = glyphs;
    glyphs.sort_by(|a, b| {
        let aa = angle_bucket(a.angle);
        let bb = angle_bucket(b.angle);
        aa.cmp(&bb)
            .then_with(|| {
                b.origin_y
                    .partial_cmp(&a.origin_y)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| {
                a.origin_x
                    .partial_cmp(&b.origin_x)
                    .unwrap_or(Ordering::Equal)
            })
    });

    let mut lines: Vec<LineAcc> = Vec::new();

    for glyph in glyphs {
        let mut best: Option<(usize, f64)> = None;

        for (index, line) in lines.iter().enumerate() {
            let angle_delta = normalize_angle(glyph.angle - line.angle).abs();
            if angle_delta > 0.12 {
                continue;
            }
            let dy = (line.baseline - glyph.origin_y).abs();
            let tolerance = (line.font_size.max(glyph.font_size) * 0.28).max(1.2);
            if dy > tolerance {
                continue;
            }

            let expanded = line.bbox.inflate(
                glyph.font_size.max(line.font_size) * 1.6,
                glyph.font_size.max(line.font_size) * 0.6,
            );
            if !expanded.intersects(glyph.bbox) {
                continue;
            }

            let gap = horizontal_gap(line.bbox, glyph.bbox);
            if gap > glyph.font_size.max(line.font_size) * 2.0 {
                continue;
            }

            let score = dy + angle_delta * 20.0 + gap * 0.02;
            if best.is_none_or(|(_, best_score)| score < best_score) {
                best = Some((index, score));
            }
        }

        if let Some((index, _)) = best {
            let line = &mut lines[index];
            line.bbox = line.bbox.union(glyph.bbox);
            if glyph.font_size > line.font_size {
                line.font_size = glyph.font_size;
                line.baseline = glyph.origin_y;
            }
            line.glyphs.push(glyph);
        } else {
            lines.push(LineAcc {
                bbox: glyph.bbox,
                baseline: glyph.origin_y,
                font_size: glyph.font_size,
                angle: glyph.angle,
                glyphs: vec![glyph],
            });
        }
    }

    for line in &mut lines {
        normalize_baseline(line);
    }
    let mut lines = merge_line_fragments(lines);
    // Several fragments can bridge one visual line (e.g. a fraction between
    // two prose runs). Repeat only while a merge makes progress.
    loop {
        let count = lines.len();
        lines = merge_line_fragments(merge_script_fragments(lines));
        if lines.len() == count {
            break;
        }
    }
    let mut result: Vec<TextLine> = lines
        .into_iter()
        .filter_map(build_line)
        .filter(|line| !line.text.trim().is_empty() || line.formula)
        .collect();

    result.sort_by(|a, b| {
        b.bbox
            .y1
            .partial_cmp(&a.bbox.y1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.bbox.x0.partial_cmp(&b.bbox.x0).unwrap_or(Ordering::Equal))
    });
    result
}

fn normalize_baseline(line: &mut LineAcc) {
    let mut bases: Vec<_> = line
        .glyphs
        .iter()
        .filter(|glyph| {
            // Extension glyphs and accents have synthetic origins; only ordinary
            // base glyphs can establish a reading line's typographic baseline.
            !glyph
                .font_name
                .as_deref()
                .is_some_and(|name| name.to_ascii_uppercase().contains("CMEX"))
                && !glyph.text.chars().all(is_accent)
        })
        .collect();
    let size = bases.iter().map(|g| g.font_size).fold(0.0, f64::max);
    bases.retain(|g| g.font_size >= size * 0.95);
    bases.sort_by(|a, b| a.origin_y.total_cmp(&b.origin_y));
    if let Some(glyph) = bases.get(bases.len() / 2) {
        line.baseline = glyph.origin_y;
        line.font_size = size;
    }
}

fn merge_line_fragments(fragments: Vec<LineAcc>) -> Vec<LineAcc> {
    let math_boxes: Vec<_> = fragments
        .iter()
        .filter(|fragment| fragment.glyphs.iter().any(is_math_glyph))
        .map(|fragment| fragment.bbox)
        .collect();
    let mut merged: Vec<LineAcc> = Vec::new();
    for fragment in fragments {
        let candidate = merged.iter_mut().find(|line| {
            let max_size = line.font_size.max(fragment.font_size);
            (line.baseline - fragment.baseline).abs() < max_size * 0.15
                && (line.angle - fragment.angle).abs() < 0.1
                && (horizontal_gap(line.bbox, fragment.bbox) < max_size * 2.0
                    || (horizontal_gap(line.bbox, fragment.bbox) < max_size * 6.0
                        && math_boxes.iter().any(|bbox| {
                            let (left, right) = if line.bbox.x0 < fragment.bbox.x0 {
                                (line.bbox, fragment.bbox)
                            } else {
                                (fragment.bbox, line.bbox)
                            };
                            bbox.x0 >= left.x1 - max_size * 0.5
                                && bbox.x1 <= right.x0 + max_size * 0.5
                                && bbox.width() > (right.x0 - left.x1) * 0.4
                                && bbox.y0 < line.baseline + max_size
                                && bbox.y1 > line.baseline - max_size * 0.5
                        })))
        });

        if let Some(line) = candidate {
            line.bbox = line.bbox.union(fragment.bbox);
            if fragment.font_size > line.font_size {
                line.font_size = fragment.font_size;
                line.baseline = fragment.baseline;
            }
            line.glyphs.extend(fragment.glyphs);
        } else {
            merged.push(fragment);
        }
    }
    merged
}

fn merge_script_fragments(mut lines: Vec<LineAcc>) -> Vec<LineAcc> {
    // Superscripts and subscripts often lie outside the baseline tolerance.
    // Attach them before filtering short fragments or identifying paragraphs,
    // so an inline R^4 is moved as a single object, including its exponent.
    let mut order: Vec<_> = (0..lines.len()).collect();
    order.sort_by(|&a, &b| lines[a].font_size.total_cmp(&lines[b].font_size));
    for index in order {
        if lines[index].glyphs.is_empty() {
            continue;
        }
        let script = &lines[index];
        let accent_piece = script.bbox.height() < script.font_size * 0.6
            && script.glyphs.iter().all(|g| g.text.chars().all(is_accent));
        let math_piece = script.glyphs.iter().any(is_math_glyph)
            && script.glyphs.iter().all(|g| {
                is_math_glyph(g)
                    || g.font_size < script.font_size * 0.8
                    || g.text.chars().all(|ch| !ch.is_alphabetic())
            });
        let candidate = lines
            .iter()
            .enumerate()
            .filter(|(i, line)| {
                *i != index
                    && !line.glyphs.is_empty()
                    && (script.angle - line.angle).abs() < 0.1
                    && horizontal_gap(script.bbox, line.bbox) < line.font_size * 1.1
                    && ((script.font_size < line.font_size * 0.86
                        && ((script.bbox.width() < line.font_size * 4.5
                            && script.glyphs.iter().map(|g| g.text.chars().count()).sum::<usize>() <= 12)
                            // Numerators and textual scripts can be wider than
                            // a few glyphs. Bind them to the surrounding line
                            // when their entire horizontal extent lies inside it.
                            || (script.glyphs.iter().any(is_math_glyph)
                                && script.bbox.x0 >= line.bbox.x0
                                && script.bbox.x1 <= line.bbox.x1
                                && script.bbox.width() < line.bbox.width() * 0.85))
                        && (script.baseline - line.baseline).abs() < line.font_size * 0.85)
                        || (accent_piece
                            && line.glyphs.iter().any(|base| {
                                base.text.chars().any(char::is_alphabetic)
                                    && script.bbox.horizontal_overlap(base.bbox) > 0.5
                                    && script.bbox.y0 >= base.bbox.y1 - line.font_size * 0.2
                                    && script.bbox.y0 <= base.bbox.y1 + line.font_size * 0.5
                            }))
                        || (math_piece
                            && script.glyphs.iter().map(|g| g.text.len()).sum::<usize>() <= 20
                            && line.bbox.width() > script.bbox.width() + line.font_size * 1.5
                            && script.bbox.y1.min(line.bbox.y1) - script.bbox.y0.max(line.bbox.y0)
                                > script.bbox.height().min(line.bbox.height()) * 0.25))
            })
            .min_by(|(_, a), (_, b)| {
                let distance = |line: &LineAcc| {
                    let overlap =
                        script.bbox.y1.min(line.bbox.y1) - script.bbox.y0.max(line.bbox.y0);
                    // TeX extension fonts put a delimiter's origin far above
                    // its visible ink. Match that ink, not its nominal baseline.
                    -overlap / script.bbox.height().min(line.bbox.height()).max(0.1)
                        + horizontal_gap(script.bbox, line.bbox) / line.font_size * 0.05
                };
                distance(a).total_cmp(&distance(b))
            })
            .map(|(i, _)| i);
        if let Some(target) = candidate {
            let glyphs = std::mem::take(&mut lines[index].glyphs);
            lines[target].bbox = lines[target].bbox.union(lines[index].bbox);
            lines[target].glyphs.extend(glyphs);
        }
    }
    lines.retain(|line| !line.glyphs.is_empty());
    lines
}

fn build_line(mut line: LineAcc) -> Option<TextLine> {
    if line.angle.abs() > 0.2 {
        // Rotated text (margins, watermarks, sideways figure labels) is kept
        // untouched by the overlay composer; do not feed it to paragraph layout.
        return None;
    }
    line.glyphs
        .sort_by(|a, b| a.bbox.x0.partial_cmp(&b.bbox.x0).unwrap_or(Ordering::Equal));

    let mut spans: Vec<TextSpan> = Vec::new();
    let mut line_text = String::new();
    let mut previous_x1: Option<f64> = None;
    let mut previous_glyph: Option<&GlyphBox> = None;
    let mut math_glyphs = 0usize;
    let mut total_glyphs = 0usize;

    for glyph in &line.glyphs {
        if let Some(previous) = &previous_glyph
            && previous.text == glyph.text
            && previous.bbox.intersects(glyph.bbox)
            && (previous.origin_x - glyph.origin_x).abs() < previous.font_size * 0.2
            && (previous.origin_y - glyph.origin_y).abs() < previous.font_size * 0.2
        {
            continue;
        }

        total_glyphs += 1;
        let math = is_math_glyph(glyph);
        if math {
            math_glyphs += 1;
        }

        let text = glyph.text.as_str();
        if let Some(prev_x1) = previous_x1 {
            let gap = glyph.bbox.x0 - prev_x1;
            if gap > glyph.font_size.max(4.0) * 0.22
                && !line_text.ends_with(char::is_whitespace)
                && !line_text.ends_with('_')
                && !text.starts_with(char::is_whitespace)
                && !text.is_empty()
            {
                line_text.push(' ');
            }
        }

        line_text.push_str(text);
        previous_x1 = Some(glyph.bbox.x1.max(glyph.bbox.x0 + glyph.advance * 0.5));
        previous_glyph = Some(glyph);

        let formula = math || math_char_ratio(text) > 0.5;
        let style_matches = spans.last().is_some_and(|span| {
            span.font_name == glyph.font_name
                && (span.font_size - glyph.font_size).abs() < 0.2
                && span.color == glyph.color
                && span.bold == glyph.bold
                && span.italic == glyph.italic
                && span.formula == formula
        });

        if style_matches && let Some(span) = spans.last_mut() {
            span.text.push_str(text);
            span.bbox = span.bbox.union(glyph.bbox);
        } else {
            spans.push(TextSpan {
                text: text.to_string(),
                bbox: glyph.bbox,
                font_size: glyph.font_size,
                font_name: glyph.font_name.clone(),
                bold: glyph.bold,
                italic: glyph.italic,
                serif: glyph.serif,
                mono: glyph.mono,
                color: glyph.color,
                formula,
            });
        }
    }

    let text = collapse_whitespace(line_text.trim());
    if text.is_empty() && math_glyphs == 0 {
        return None;
    }

    let bbox = line.bbox;
    // A single large delimiter must not turn an ordinary prose line into a
    // heading, or move its baseline away from the surrounding text.
    let mut prose: Vec<_> = line
        .glyphs
        .iter()
        .filter(|g| !is_math_glyph(g) && g.text.chars().any(|ch| ch.is_alphabetic()))
        .collect();
    prose.sort_by(|a, b| a.font_size.total_cmp(&b.font_size));
    let (baseline, font_size) = if prose.len() >= 4 {
        let dominant = prose[prose.len() / 2];
        (dominant.origin_y, dominant.font_size)
    } else {
        (line.baseline, line.font_size)
    };
    let math_ratio = if total_glyphs == 0 {
        0.0
    } else {
        math_glyphs as f64 / total_glyphs as f64
    };

    Some(TextLine {
        bbox,
        baseline,
        text,
        spans,
        font_size,
        column: Column::Full,
        formula: false,
        math_ratio,
        glyphs: line.glyphs,
    })
}

struct LineAcc {
    glyphs: Vec<GlyphBox>,
    bbox: BBox,
    baseline: f64,
    font_size: f64,
    angle: f64,
}

pub(super) fn filter_layout_artifacts(lines: &mut Vec<TextLine>, frame: PageFrame) {
    lines.retain(|line| {
        let text = line.text.trim();
        if text.is_empty() || text.len() > 4 {
            return true;
        }
        let all_number_like = text
            .chars()
            .all(|ch| ch.is_ascii_digit() || is_roman_numeral_digit(ch));
        if !all_number_like {
            return true;
        }
        let in_margin = line.bbox.y1 < frame.y0 + frame.height * 0.07
            || line.bbox.y0 > frame.y0 + frame.height * 0.93;
        let tiny = line.bbox.width() < frame.width * 0.06;
        // Page numbers and footnote markers in the middle of a column are
        // artifacts for layout purposes; the original page is restored by the
        // clean composer's untouched original content outside translated text.
        !(tiny && !in_margin)
    });
}

pub(super) fn assign_columns(lines: &mut Vec<TextLine>, frame: PageFrame) {
    let mid = frame.center_x();
    let gutter_margin = frame.width * 0.035;
    let narrow: Vec<&TextLine> = lines
        .iter()
        .filter(|line| {
            line.bbox.width() < frame.width * 0.55
                && line.text.chars().count() >= 25
                && !(line.bbox.x0 < mid - gutter_margin && line.bbox.x1 > mid + gutter_margin)
        })
        .collect();
    let left = narrow
        .iter()
        .filter(|line| line.bbox.center_x() < mid)
        .count();
    let right = narrow
        .iter()
        .filter(|line| line.bbox.center_x() >= mid)
        .count();
    let two_columns = left >= 3 && right >= 3;
    if two_columns {
        let mut split = Vec::new();
        for line in std::mem::take(lines) {
            // Aligned lines in adjacent columns can be closer than the normal
            // word-fragment tolerance. Split at the established gutter before
            // building paragraphs, including adjacent table/figure captions.
            let boundary = line
                .glyphs
                .windows(2)
                .enumerate()
                .filter(|(_, pair)| {
                    let center = (pair[0].bbox.x1 + pair[1].bbox.x0) * 0.5;
                    (center - mid).abs() < frame.width * 0.1
                        && pair[1].bbox.x0 - pair[0].bbox.x1 > line.font_size * 1.1
                })
                .max_by(|(_, a), (_, b)| {
                    (a[1].bbox.x0 - a[0].bbox.x1).total_cmp(&(b[1].bbox.x0 - b[0].bbox.x1))
                })
                .map(|(i, _)| i + 1);
            if let Some(boundary) = boundary.filter(|&i| i >= 3 && line.glyphs.len() - i >= 3) {
                for glyphs in [&line.glyphs[..boundary], &line.glyphs[boundary..]] {
                    let bbox = glyphs.iter().map(|g| g.bbox).reduce(BBox::union).unwrap();
                    if let Some(part) = build_line(LineAcc {
                        glyphs: glyphs.to_vec(),
                        bbox,
                        baseline: line.baseline,
                        font_size: line.font_size,
                        angle: glyphs[0].angle,
                    }) {
                        split.push(part);
                    }
                }
            } else {
                split.push(line);
            }
        }
        *lines = split;
    }
    for line in lines {
        let crosses_gutter =
            line.bbox.x0 < mid - gutter_margin && line.bbox.x1 > mid + gutter_margin;
        line.column = if !two_columns || line.bbox.width() > frame.width * 0.62 || crosses_gutter {
            Column::Full
        } else if line.bbox.center_x() < mid {
            Column::Left
        } else {
            Column::Right
        };
    }
}

pub(super) fn is_formula_line(line: &TextLine) -> bool {
    if line.text.trim().is_empty() {
        return false;
    }
    let span_formula = line.spans.iter().any(|span| span.formula);
    let math_ratio = line.math_ratio;
    let char_ratio = math_char_ratio(&line.text);
    let text_len = line.text.chars().count();
    let math_ops = line.text.chars().filter(|ch| is_math_operator(*ch)).count();
    let display_like = text_len <= 40 && math_ops >= 2 && char_ratio > 0.08;
    let prose_word = line.spans.iter().filter(|span| !span.formula).any(|span| {
        span.text
            .split(|ch: char| !ch.is_alphabetic())
            .any(|word| word.chars().count() >= 3 && !is_named_math_operator(word))
    });
    if (span_formula && !prose_word) || is_named_math_operator(line.text.trim()) {
        return true;
    }

    // Even a short sentence introducing an equation can have a high math
    // ratio. Count its ordinary letters separately: inline formula handling
    // preserves the expression while the surrounding prose is translated.
    let prose_letters = line
        .spans
        .iter()
        .filter(|span| !span.formula)
        .map(|span| span.text.chars().filter(|ch| ch.is_alphabetic()).count())
        .sum::<usize>();
    let mut opening = line.text.split_whitespace();
    let sentence_subject = opening
        .next()
        .is_some_and(|word| matches!(word, "We" | "we"))
        && opening.next().is_some_and(|word| {
            word.len() >= 3
                && word.chars().all(|ch| ch.is_ascii_alphabetic())
                && !is_named_math_operator(word)
        });
    let mut words = line.text.split_whitespace();
    let sentence_intro = line.text.trim_end().ends_with(':')
        && (0..2).all(|_| {
            words.next().is_some_and(|word| {
                word.len() >= 2
                    && word.chars().all(|ch| ch.is_ascii_alphabetic())
                    && !is_named_math_operator(word)
            })
        });
    let prose_words = line
        .text
        .split_whitespace()
        .map(|word| word.trim_matches(|ch: char| ch.is_ascii_punctuation()))
        .filter(|word| {
            word.len() >= 2
                && word.chars().all(|ch| ch.is_alphabetic())
                && !is_named_math_operator(word)
        })
        .count();
    if (prose_letters >= 24 && (char_ratio < 0.2 || prose_words >= 4))
        || (prose_letters >= 16 && prose_words >= 3)
        || (sentence_subject && prose_word && prose_letters >= 5)
        || (sentence_intro && prose_word && prose_letters >= 12 && char_ratio < 0.35)
    {
        return false;
    }
    if span_formula
        && line
            .text
            .chars()
            .filter(|ch| ch.is_ascii_alphabetic())
            .count()
            <= 3
        && (math_ops >= 1 || char_ratio > 0.08)
    {
        return true;
    }
    if line.text.chars().any(|ch| ch.is_ascii_digit())
        && line.text.chars().all(|ch| {
            ch.is_whitespace()
                || ch.is_ascii_digit()
                || ch.is_ascii_punctuation()
                || is_math_char(ch)
                || is_math_operator(ch)
        })
    {
        return true;
    }

    span_formula && (math_ratio > 0.35 || char_ratio > 0.2)
        || math_ratio > 0.55
        || (char_ratio > 0.35 && line.text.chars().filter(|c| is_math_char(*c)).count() >= 2)
        || display_like
}

/// A large extensible brace binds every row of a cases expression. Its text
/// rows may contain ordinary words, but moving one row independently destroys
/// the equation. Preserve the original objects throughout the brace's height.
pub(super) fn protect_cases(lines: &mut [TextLine]) {
    let braces: Vec<_> = lines
        .iter()
        .flat_map(|line| &line.glyphs)
        .filter(|g| g.text == "{" && g.bbox.height() > g.font_size * 1.7)
        .map(|g| (g.bbox, g.font_size))
        .collect();
    for (brace, size) in braces {
        for line in lines.iter_mut() {
            if line.bbox.y0 < brace.y1
                && line.bbox.y1 > brace.y0
                && line.bbox.x1 > brace.x0
                && line.font_size <= size * 1.15
                && (line.bbox.x0 >= brace.x0 - size * 0.5
                    || line.spans.iter().any(|span| span.formula))
            {
                line.formula = true;
            }
        }
    }
}

fn horizontal_gap(a: BBox, b: BBox) -> f64 {
    if a.x1 < b.x0 {
        b.x0 - a.x1
    } else if b.x1 < a.x0 {
        a.x0 - b.x1
    } else {
        0.0
    }
}

fn angle_bucket(angle: f64) -> i32 {
    (angle / 0.05).round() as i32
}

fn normalize_angle(angle: f64) -> f64 {
    let mut angle = angle;
    while angle > std::f64::consts::PI {
        angle -= std::f64::consts::TAU;
    }
    while angle < -std::f64::consts::PI {
        angle += std::f64::consts::TAU;
    }
    angle
}
