use super::{
    PageFrame,
    inline::{is_math_char, is_math_glyph, is_math_operator, math_char_ratio},
    paragraphs::is_roman_numeral_digit,
};
use crate::model::{BBox, Column, GlyphBox, TextLine, TextSpan};
use crate::text::collapse_whitespace;
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

    let lines = merge_line_fragments(lines);
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

fn merge_line_fragments(fragments: Vec<LineAcc>) -> Vec<LineAcc> {
    let mut merged: Vec<LineAcc> = Vec::new();
    for fragment in fragments {
        let candidate = merged.iter_mut().find(|line| {
            let max_size = line.font_size.max(fragment.font_size);
            (line.baseline - fragment.baseline).abs() < max_size * 0.15
                && (line.angle - fragment.angle).abs() < 0.1
                && horizontal_gap(line.bbox, fragment.bbox) < max_size * 2.0
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
    let baseline = line.baseline;
    let font_size = line.font_size;
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

pub(super) fn assign_columns(lines: &mut [TextLine], frame: PageFrame) {
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

    span_formula && (math_ratio > 0.35 || char_ratio > 0.2)
        || math_ratio > 0.55
        || (char_ratio > 0.35 && line.text.chars().filter(|c| is_math_char(*c)).count() >= 2)
        || display_like
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
