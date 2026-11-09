use std::collections::{BTreeSet, HashMap};

use anyhow::{Result, bail};

use crate::model::{Align, FormulaRef, LayoutDocument, PageLayout, Paragraph, TextStyle};
use crate::pdf::font::{EmbeddedFont, FontMetrics, FontSource};
use crate::text::is_cjk;

#[derive(Debug, Clone)]
pub struct TypesetOptions {
    pub min_scale: f64,
    pub line_height: f64,
    pub max_expand: f64,
}

impl Default for TypesetOptions {
    fn default() -> Self {
        Self {
            min_scale: 0.72,
            line_height: 1.22,
            max_expand: 1.45,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TypesetSegment {
    pub text: String,
    pub x: f64,
    pub bold: bool,
    pub italic: bool,
}

#[derive(Debug, Clone)]
pub struct TypesetFormula {
    pub id: String,
    pub x: f64,
    pub baseline: f64,
    pub source_bbox: crate::model::BBox,
    pub source_baseline: f64,
}

#[derive(Debug, Clone)]
pub struct TypesetLine {
    pub baseline: f64,
    pub size: f64,
    pub color: [u8; 4],
    pub segments: Vec<TypesetSegment>,
    pub formulas: Vec<TypesetFormula>,
}

#[derive(Debug, Clone)]
pub struct TypesetParagraph {
    pub lines: Vec<TypesetLine>,
    pub scale: f64,
    pub overflow: bool,
}

#[derive(Debug)]
pub struct TypesetDocument {
    pub font: EmbeddedFont,
    pub bold_font: Option<EmbeddedFont>,
    pub paragraphs: HashMap<String, TypesetParagraph>,
}

#[derive(Debug, Default, Clone)]
pub struct TypesetStats {
    pub paragraphs: usize,
    pub overflow: usize,
    pub smallest_scale: f64,
    pub missing_glyphs: usize,
}

pub fn typeset_document(
    document: &LayoutDocument,
    font_source: &FontSource,
    bold_font_source: Option<&FontSource>,
    options: &TypesetOptions,
) -> Result<(TypesetDocument, TypesetStats)> {
    let mut regular_chars = BTreeSet::new();
    let mut bold_chars = BTreeSet::new();
    let mut candidates = Vec::new();
    for page in &document.pages {
        for paragraph in &page.paragraphs {
            let Some(text) = paragraph
                .translation
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            else {
                continue;
            };
            if paragraph.protected {
                continue;
            }
            let tokens = tokenize(text, &paragraph.formulas, paragraph.base_style);
            for (ch, style) in characters(&tokens) {
                if style.bold && bold_font_source.is_some() {
                    bold_chars.insert(ch);
                } else {
                    regular_chars.insert(ch);
                }
            }
            candidates.push((page, paragraph, tokens));
        }
    }
    if regular_chars.is_empty() && bold_chars.is_empty() {
        bail!("no translated text to typeset");
    }
    let font = font_source.embed(&font_source.metrics(regular_chars)?)?;
    let bold_font = if let Some(source) = bold_font_source.filter(|_| !bold_chars.is_empty()) {
        let mut font = source.embed(&source.metrics(bold_chars)?)?;
        font.pdf_name.push_str("-Bold");
        Some(font)
    } else {
        None
    };
    let mut stats = TypesetStats {
        smallest_scale: 1.0,
        ..Default::default()
    };
    let mut paragraphs = HashMap::new();
    let metrics = Faces {
        regular: &font.metrics,
        bold: bold_font.as_ref().map(|f| &f.metrics),
    };
    for (page, paragraph, tokens) in candidates {
        let missing: BTreeSet<_> = characters(&tokens)
            .filter(|(ch, style)| !metrics.for_style(*style).char_to_gid.contains_key(ch))
            .map(|(ch, _)| ch)
            .collect();
        if !missing.is_empty() {
            stats.missing_glyphs += 1;
            let codes = missing
                .iter()
                .take(12)
                .map(|ch| format!("U+{:04X}", *ch as u32))
                .collect::<Vec<_>>()
                .join(", ");
            tracing::warn!(
                "{}: font has no glyph for {codes}; preserving original paragraph",
                paragraph.id
            );
            continue;
        }
        let max_height = available_height(page, paragraph, options);
        let typeset = typeset_paragraph(page, paragraph, &tokens, metrics, options, max_height);
        stats.paragraphs += 1;
        stats.smallest_scale = stats.smallest_scale.min(typeset.scale);
        if typeset.overflow {
            stats.overflow += 1;
            tracing::warn!(
                "{}: translation does not fit; preserving original paragraph",
                paragraph.id
            );
            continue;
        }
        paragraphs.insert(paragraph.id.clone(), typeset);
    }

    Ok((
        TypesetDocument {
            font,
            bold_font,
            paragraphs,
        },
        stats,
    ))
}

fn available_height(page: &PageLayout, paragraph: &Paragraph, options: &TypesetOptions) -> f64 {
    let mut next_top: Option<f64> = None;
    for other in &page.paragraphs {
        if other.id == paragraph.id {
            continue;
        }
        // Only paragraphs below this one can constrain it.
        if other.bbox.y1 >= paragraph.bbox.y0 - 1.0 {
            continue;
        }
        let horizontal_overlap = paragraph.bbox.horizontal_overlap(other.bbox);
        let same_column = other.column == paragraph.column
            || other.column == crate::model::Column::Full
            || paragraph.column == crate::model::Column::Full;
        if horizontal_overlap < 0.15 && !same_column {
            continue;
        }
        next_top = Some(next_top.map_or(other.bbox.y1, |current| current.max(other.bbox.y1)));
    }

    for image in &page.images {
        if image.bbox.y1 <= paragraph.bbox.y0 && paragraph.bbox.horizontal_overlap(image.bbox) > 0.0
        {
            next_top = Some(next_top.map_or(image.bbox.y1, |top| top.max(image.bbox.y1)));
        }
    }
    let bottom = next_top
        .unwrap_or(page.y0 + page.height * 0.06)
        .max(page.y0);
    let available = (paragraph.bbox.y1 - bottom - 2.0).max(0.0);
    let expanded = paragraph.bbox.height().max(paragraph.font_size) * options.max_expand;
    available.min(expanded)
}

#[derive(Clone, Copy)]
struct Faces<'a> {
    regular: &'a FontMetrics,
    bold: Option<&'a FontMetrics>,
}

impl<'a> Faces<'a> {
    fn for_style(self, style: TextStyle) -> &'a FontMetrics {
        self.bold.filter(|_| style.bold).unwrap_or(self.regular)
    }
}

fn typeset_paragraph(
    page: &PageLayout,
    paragraph: &Paragraph,
    tokens: &[Token],
    metrics: Faces<'_>,
    options: &TypesetOptions,
    max_height: f64,
) -> TypesetParagraph {
    let base_size = paragraph.font_size.clamp(6.0, 42.0);
    let max_width = if matches!(
        paragraph.role,
        crate::model::Role::Title | crate::model::Role::Heading | crate::model::Role::Caption
    ) {
        paragraph.bbox.width().max(base_size * 8.0)
    } else {
        paragraph.bbox.width().max(base_size * 3.0)
    };
    let color = paragraph
        .lines(&page.lines)
        .next()
        .and_then(|line| line.spans.first())
        .map(|span| span.color)
        .unwrap_or([0, 0, 0, 255]);
    let mut chosen_size = base_size;
    let mut chosen_drafts: Vec<LineDraft> = Vec::new();
    let mut chosen_scale = 1.0;
    let mut overflow = true;

    let steps = ((1.0 - options.min_scale) / 0.02).ceil() as usize;
    for step in 0..=steps {
        let scale = (1.0 - step as f64 * 0.02).max(options.min_scale);
        let size = base_size * scale;
        let line_width = max_width.max(size);
        let drafts = wrap_tokens(tokens, line_width, size, metrics, &paragraph.formulas);
        let line_height = size * options.line_height;
        let total_height = drafts.len() as f64 * line_height;
        chosen_size = size;
        chosen_scale = scale;
        chosen_drafts = drafts;
        let fits = total_height <= max_height && !drafts_too_wide(&chosen_drafts, max_width);
        if fits || scale <= options.min_scale + 1e-9 {
            overflow = !fits;
            break;
        }
    }

    let line_height = chosen_size * options.line_height;
    let ascent =
        (metrics.regular.ascent_ratio() * chosen_size).clamp(chosen_size * 0.72, chosen_size);
    let mut lines = Vec::with_capacity(chosen_drafts.len());

    for (index, draft) in chosen_drafts.iter().enumerate() {
        let offset_x = match paragraph.align {
            Align::Left => paragraph.bbox.x0,
            Align::Center => paragraph.bbox.center_x() - draft.width * 0.5,
            Align::Right => paragraph.bbox.x1 - draft.width,
        };
        let baseline = paragraph.bbox.y1 - ascent - index as f64 * line_height;

        let segments = draft
            .segments
            .iter()
            .map(|segment| TypesetSegment {
                text: segment.text.clone(),
                x: offset_x + segment.x,
                bold: segment.bold,
                italic: segment.italic,
            })
            .collect::<Vec<_>>();
        let mut formulas = Vec::new();
        for (id, relative_x) in &draft.formulas {
            let source = &paragraph.formulas[*id];
            formulas.push(TypesetFormula {
                id: source.id.clone(),
                x: offset_x + relative_x,
                baseline,
                source_bbox: source.bbox,
                source_baseline: source.baseline,
            });
        }
        lines.push(TypesetLine {
            baseline,
            size: chosen_size,
            color,
            segments,
            formulas,
        });
    }

    TypesetParagraph {
        lines,
        scale: chosen_scale,
        overflow,
    }
}

fn drafts_too_wide(drafts: &[LineDraft], max_width: f64) -> bool {
    drafts.iter().any(|draft| draft.width > max_width + 0.01)
}

#[derive(Debug, Clone, Default)]
struct SegmentDraft {
    text: String,
    x: f64,
    bold: bool,
    italic: bool,
}

#[derive(Debug, Clone, Default)]
struct LineDraft {
    segments: Vec<SegmentDraft>,
    formulas: Vec<(usize, f64)>,
    width: f64,
    after_formula: bool,
}

impl LineDraft {
    fn push_text(&mut self, text: &str, width: f64, bold: bool, italic: bool) {
        if text.is_empty() {
            return;
        }
        if !self.after_formula
            && let Some(last) = self.segments.last_mut()
            && last.bold == bold
            && last.italic == italic
        {
            last.text.push_str(text);
        } else {
            self.segments.push(SegmentDraft {
                text: text.to_string(),
                x: self.width,
                bold,
                italic,
            });
            self.after_formula = false;
        }
        self.width += width;
    }
    /// Move one real trailing character with a punctuation mark at a line break.
    fn take_for_break(
        &mut self,
        closing: bool,
        faces: Faces<'_>,
        size: f64,
    ) -> Option<(char, f64, TextStyle)> {
        if self.after_formula {
            return None;
        }
        let last = self.segments.last_mut()?;
        let ch = last.text.chars().next_back()?;
        if !closing && !is_opening_punctuation(ch) {
            return None;
        }
        let style = TextStyle {
            bold: last.bold,
            italic: last.italic,
        };
        let width = faces.for_style(style).char_advance(ch, size);
        last.text.pop();
        self.width -= width;
        Some((ch, width, style))
    }
}

fn wrap_tokens(
    tokens: &[Token],
    max_width: f64,
    size: f64,
    faces: Faces<'_>,
    formulas: &[FormulaRef],
) -> Vec<LineDraft> {
    let mut lines: Vec<LineDraft> = Vec::new();
    let mut current = LineDraft::default();
    for token in tokens {
        match token {
            Token::Newline => push_draft(&mut lines, &mut current, faces, size),
            Token::Space(style) => {
                let metrics = faces.for_style(*style);
                let TextStyle { bold, italic } = *style;
                let width = metrics.char_advance(' ', size);
                if current.width > 0.0
                    && (current.after_formula
                        || !current
                            .segments
                            .last()
                            .is_some_and(|segment| segment.text.ends_with(' ')))
                {
                    current.push_text(" ", width, bold, italic);
                }
            }
            Token::Text(piece, style) => {
                let metrics = faces.for_style(*style);
                let TextStyle { bold, italic } = *style;
                let piece_width = metrics.text_width(piece, size, 0.0);
                if current.width + piece_width > max_width && current.width > 0.0 {
                    let moved = current.take_for_break(is_closing_punctuation(piece), faces, size);
                    push_draft(&mut lines, &mut current, faces, size);
                    if let Some((ch, width, style)) = moved {
                        current.push_text(&ch.to_string(), width, style.bold, style.italic);
                    }
                }

                if piece_width > max_width && current.width == 0.0 {
                    let mut buffer = String::new();
                    let mut width = 0.0;
                    for ch in piece.chars() {
                        let advance = metrics.char_advance(ch, size);
                        if width + advance > max_width && !buffer.is_empty() {
                            current.push_text(&buffer, width, bold, italic);
                            push_draft(&mut lines, &mut current, faces, size);
                            buffer.clear();
                            width = 0.0;
                        }
                        buffer.push(ch);
                        width += advance;
                    }
                    current.push_text(&buffer, width, bold, italic);
                } else {
                    current.push_text(piece, piece_width, bold, italic);
                }
            }
            Token::Formula(id) => {
                let formula = &formulas[*id];
                let width = formula.bbox.width();
                if current.width + width > max_width && current.width > 0.0 {
                    push_draft(&mut lines, &mut current, faces, size);
                }
                current.formulas.push((*id, current.width));
                current.width += width;
                current.after_formula = true;
            }
        }
    }

    push_draft(&mut lines, &mut current, faces, size);
    if lines.is_empty() {
        lines.push(LineDraft::default());
    }
    lines
}

fn push_draft(lines: &mut Vec<LineDraft>, current: &mut LineDraft, faces: Faces<'_>, size: f64) {
    if current.width > 0.0 || !current.segments.is_empty() || !current.formulas.is_empty() {
        let mut draft = std::mem::take(current);
        if !draft.after_formula
            && let Some(segment) = draft.segments.last_mut()
        {
            let trimmed = segment.text.trim_end();
            let removed = segment.text.len() - trimmed.len();
            if removed > 0 {
                draft.width -= faces
                    .for_style(TextStyle {
                        bold: segment.bold,
                        italic: segment.italic,
                    })
                    .text_width(&segment.text[trimmed.len()..], size, 0.0);
                segment.text = trimmed.to_string();
            }
        }
        if !draft
            .segments
            .iter()
            .all(|segment| segment.text.trim().is_empty())
            || !draft.formulas.is_empty()
        {
            lines.push(draft);
        }
    }
}

enum Token {
    Text(String, TextStyle),
    Space(TextStyle),
    Newline,
    Formula(usize),
}

fn tokenize(text: &str, formulas: &[FormulaRef], base: TextStyle) -> Vec<Token> {
    use crate::markup::{Piece, decode_literal, pieces};
    let mut tokens = Vec::new();
    let mut style = base;
    for piece in pieces(text) {
        match piece {
            Piece::Bold(on) => style.bold = base.bold || on,
            Piece::Italic(on) => style.italic = base.italic || on,
            Piece::Formula(id) => {
                if let Some(index) = formulas.iter().position(|f| f.id == id) {
                    tokens.push(Token::Formula(index));
                } else {
                    tokens.push(Token::Text(format!("<{id}>"), style));
                }
            }
            Piece::Text(plain) => {
                let mut word = String::new();
                for ch in decode_literal(plain).chars() {
                    if !ch.is_whitespace() && !is_cjk(ch) && !is_cjk_punctuation(ch) {
                        word.push(ch);
                        continue;
                    }
                    if !word.is_empty() {
                        tokens.push(Token::Text(std::mem::take(&mut word), style));
                    }
                    tokens.push(if ch == '\n' {
                        Token::Newline
                    } else if ch.is_whitespace() {
                        Token::Space(style)
                    } else {
                        Token::Text(ch.to_string(), style)
                    });
                }
                if !word.is_empty() {
                    tokens.push(Token::Text(word, style));
                }
            }
        }
    }
    tokens
}

fn characters(tokens: &[Token]) -> impl Iterator<Item = (char, TextStyle)> {
    tokens
        .iter()
        .filter_map(|token| match token {
            Token::Text(text, style) => Some((text.as_str(), *style)),
            Token::Space(style) => Some((" ", *style)),
            _ => None,
        })
        .flat_map(|(text, style)| text.chars().map(move |ch| (ch, style)))
}

#[cfg(test)]
fn wrap_text(
    text: &str,
    max_width: f64,
    size: f64,
    metrics: &crate::pdf::font::FontMetrics,
) -> Vec<String> {
    wrap_tokens(
        &tokenize(text, &[], TextStyle::default()),
        max_width,
        size,
        Faces {
            regular: metrics,
            bold: None,
        },
        &[],
    )
    .into_iter()
    .map(|line| {
        line.segments
            .into_iter()
            .map(|segment| segment.text)
            .collect()
    })
    .collect()
}

fn is_cjk_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '，' | '。'
            | '！'
            | '？'
            | '；'
            | '：'
            | '、'
            | '）'
            | '】'
            | '》'
            | '」'
            | '』'
            | '〕'
            | '〉'
            | '“'
            | '”'
            | '‘'
            | '’'
            | '…'
            | '—'
            | '（'
            | '【'
            | '《'
            | '「'
            | '『'
            | '〔'
            | '〈'
    )
}

fn is_closing_punctuation(piece: &str) -> bool {
    piece.chars().next().is_some_and(|ch| {
        matches!(
            ch,
            '，' | '。'
                | '！'
                | '？'
                | '；'
                | '：'
                | '、'
                | '）'
                | '】'
                | '》'
                | '」'
                | '』'
                | '〕'
                | '〉'
        )
    })
}

fn is_opening_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '（' | '【' | '《' | '「' | '『' | '〔' | '〈' | '“' | '‘'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn metrics() -> crate::pdf::font::FontMetrics {
        crate::pdf::font::FontMetrics {
            units_per_em: 1000.0,
            ascender: 880.0,
            descender: -120.0,
            cap_height: 700.0,
            bbox: [0.0, -120.0, 1000.0, 880.0],
            italic_angle: 0.0,
            char_to_gid: HashMap::new(),
            gid_advance: Vec::new(),
        }
    }

    #[test]
    fn wraps_chinese() {
        let text = "这是一个用于测试中文断行的句子，需要能够正常换行并保持标点不在行首。";
        let lines = wrap_text(text, 100.0, 10.0, &metrics());
        assert!(lines.len() >= 2);
        assert!(lines.iter().all(|line| !line.is_empty()));
        assert!(lines.iter().all(|line| {
            let first = line.chars().next().unwrap();
            !matches!(first, '，' | '。' | '；' | '：' | '、' | '）')
        }));
    }

    #[test]
    fn breaks_long_latin_words() {
        let text = "supercalifragilisticexpialidocious";
        let lines = wrap_text(text, 50.0, 10.0, &metrics());
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| !line.is_empty()));
    }

    #[test]
    fn keeps_explicit_newlines() {
        let lines = wrap_text("first\nsecond", 500.0, 10.0, &metrics());
        assert_eq!(lines, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn closing_punctuation_moves_with_previous_character() {
        let lines = wrap_text("一二三四五六。", 60.0, 10.0, &metrics());
        assert_eq!(lines, vec!["一二三四五".to_string(), "六。".to_string()]);
    }

    #[test]
    fn trailing_space_is_not_counted_in_line_width() {
        let drafts = wrap_tokens(
            &tokenize("一二三四五 六", &[], TextStyle::default()),
            51.0,
            10.0,
            Faces {
                regular: &metrics(),
                bold: None,
            },
            &[],
        );
        assert_eq!(drafts.len(), 2);
        let first = drafts[0]
            .segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>();
        assert_eq!(first, "一二三四五");
        assert!(
            drafts[0].width <= 51.01,
            "line width includes trimmed trailing space: {}",
            drafts[0].width
        );
    }
}
