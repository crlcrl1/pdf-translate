use std::collections::{BTreeSet, HashMap};

use anyhow::{Result, bail};

use crate::model::{Align, BBox, FormulaRef, LayoutDocument, PageLayout, Paragraph, TextStyle};
use crate::pdf::font::{EmbeddedFont, FontMetrics, FontSource};
use crate::text::{bullet_marker, is_cjk};

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
    pub color: [u8; 4],
}

#[derive(Debug, Clone)]
pub struct TypesetFormula {
    pub id: String,
    pub x: f64,
    pub baseline: f64,
    pub source_bbox: crate::model::BBox,
    pub source_baseline: f64,
    pub scale: f64,
}

#[derive(Debug, Clone)]
pub struct TypesetLine {
    pub baseline: f64,
    pub size: f64,
    pub bbox: BBox,
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
    pub italic_font: Option<EmbeddedFont>,
    pub symbol_font: Option<EmbeddedFont>,
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
    italic_font_source: Option<&FontSource>,
    symbol_font_source: Option<&FontSource>,
    options: &TypesetOptions,
) -> Result<(TypesetDocument, TypesetStats)> {
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
            candidates.push((page, paragraph, tokens));
        }
    }
    let all_chars: BTreeSet<char> = candidates
        .iter()
        .flat_map(|(_, _, tokens)| characters(tokens).map(|(ch, _)| ch))
        .collect();
    let regular_present: BTreeSet<char> = font_source
        .metrics(all_chars.iter().copied())?
        .char_to_gid
        .into_keys()
        .collect();
    let bold_present = present_chars(bold_font_source, &all_chars)?;
    let italic_present = present_chars(italic_font_source, &all_chars)?;
    let symbol_present = present_chars(symbol_font_source, &all_chars)?;
    let mut regular_chars = BTreeSet::new();
    let mut bold_chars = BTreeSet::new();
    let mut italic_chars = BTreeSet::new();
    let mut symbol_chars = BTreeSet::new();
    for (_, _, tokens) in &candidates {
        for (ch, style) in characters(tokens) {
            match pick_face(
                style,
                ch,
                &regular_present,
                bold_present.as_ref(),
                italic_present.as_ref(),
                symbol_present.as_ref(),
            ) {
                Some(FaceKind::Regular) => {
                    regular_chars.insert(ch);
                }
                Some(FaceKind::Bold) => {
                    bold_chars.insert(ch);
                }
                Some(FaceKind::Italic) => {
                    italic_chars.insert(ch);
                }
                Some(FaceKind::Symbol) => {
                    symbol_chars.insert(ch);
                }
                None => {}
            }
        }
    }
    if regular_chars.is_empty()
        && bold_chars.is_empty()
        && italic_chars.is_empty()
        && symbol_chars.is_empty()
    {
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
    let italic_font = if let Some(source) = italic_font_source.filter(|_| !italic_chars.is_empty())
    {
        let mut font = source.embed(&source.metrics(italic_chars)?)?;
        font.pdf_name.push_str("-Italic");
        Some(font)
    } else {
        None
    };
    let symbol_font = if let Some(source) = symbol_font_source.filter(|_| !symbol_chars.is_empty())
    {
        let mut font = source.embed(&source.metrics(symbol_chars)?)?;
        font.pdf_name.push_str("-Symbol");
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
        italic: italic_font.as_ref().map(|f| &f.metrics),
        symbol: symbol_font.as_ref().map(|f| &f.metrics),
    };
    // Place captions first so any upward adjustment also constrains the
    // downward expansion of the preceding translated paragraph.
    candidates.sort_by_key(|(_, paragraph, _)| paragraph.role != crate::model::Role::Caption);
    for (page, paragraph, tokens) in candidates {
        let missing: BTreeSet<_> = characters(&tokens)
            .filter(|(ch, _)| !metrics.has_any(*ch))
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
        let max_height = available_height(page, paragraph, options, &paragraphs);
        let expanded = paragraph.bbox.height().max(paragraph.font_size) * options.max_expand;
        let max_rise = available_caption_rise(page, paragraph, &paragraphs)
            .min((expanded - max_height).max(0.0));
        let typeset = typeset_paragraph(
            page, paragraph, &tokens, metrics, options, max_height, max_rise,
        );
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
            italic_font,
            symbol_font,
            paragraphs,
        },
        stats,
    ))
}

fn paragraph_obstacle(paragraph: &Paragraph, placed: &HashMap<String, TypesetParagraph>) -> BBox {
    if paragraph.role == crate::model::Role::Caption
        && let Some(caption) = placed.get(&paragraph.id)
    {
        caption
            .lines
            .iter()
            .fold(paragraph.bbox, |bbox, line| bbox.union(line.bbox))
    } else {
        paragraph.bbox
    }
}

fn available_caption_rise(
    page: &PageLayout,
    paragraph: &Paragraph,
    placed: &HashMap<String, TypesetParagraph>,
) -> f64 {
    if paragraph.role != crate::model::Role::Caption {
        return 0.0;
    }
    // Borrow at most half an em above a caption, only when needed to fit it.
    // Keep the original lower boundary and all graphic clearances intact.
    let mut top = (paragraph.bbox.y1 + paragraph.font_size * 0.5).min(page.y0 + page.height - 2.0);
    for bbox in page
        .paragraphs
        .iter()
        .filter(|other| other.id != paragraph.id)
        .map(|other| paragraph_obstacle(other, placed))
        .chain(page.images.iter().map(|image| image.bbox))
        .chain(
            page.rules
                .iter()
                .copied()
                .filter(|rule| rule.width() > paragraph.font_size * 4.0),
        )
    {
        if bbox.y1 > paragraph.bbox.y1 && paragraph.bbox.horizontal_overlap(bbox) > 0.0 {
            top = top.min(bbox.y0 - 2.0);
        }
    }
    (top - paragraph.bbox.y1).max(0.0)
}

fn available_height(
    page: &PageLayout,
    paragraph: &Paragraph,
    options: &TypesetOptions,
    placed: &HashMap<String, TypesetParagraph>,
) -> f64 {
    let mut next_top: Option<f64> = None;
    for other in &page.paragraphs {
        if other.id == paragraph.id {
            continue;
        }
        let bbox = paragraph_obstacle(other, placed);
        // Only paragraphs below this one can constrain it.
        if bbox.y1 >= paragraph.bbox.y0 - 1.0 {
            continue;
        }
        let horizontal_overlap = paragraph.bbox.horizontal_overlap(bbox);
        let same_column = other.column == paragraph.column
            || other.column == crate::model::Column::Full
            || paragraph.column == crate::model::Column::Full;
        if horizontal_overlap < 0.15 && !same_column {
            continue;
        }
        next_top = Some(next_top.map_or(bbox.y1, |current| current.max(bbox.y1)));
    }

    for image in &page.images {
        if image.bbox.y1 <= paragraph.bbox.y0 && paragraph.bbox.horizontal_overlap(image.bbox) > 0.0
        {
            next_top = Some(next_top.map_or(image.bbox.y1, |top| top.max(image.bbox.y1)));
        }
    }
    // Vector figures and table borders may extend above their text labels.
    // Their top rule must stop expansion just as the top of a raster image does.
    for rule in &page.rules {
        if rule.y1 <= paragraph.bbox.y0
            && rule.width() > paragraph.font_size * 4.0
            && paragraph.bbox.horizontal_overlap(*rule) > 0.15
        {
            next_top = Some(next_top.map_or(rule.y1, |top| top.max(rule.y1)));
        }
    }
    let bottom = next_top
        .unwrap_or(page.y0 + page.height * 0.06)
        .max(page.y0);
    let available = (paragraph.bbox.y1 - bottom - 2.0).max(0.0);
    let expanded = paragraph.bbox.height().max(paragraph.font_size) * options.max_expand;
    available.min(expanded)
}

fn available_heading_width(page: &PageLayout, paragraph: &Paragraph) -> f64 {
    let mut left = paragraph.bbox.x0;
    let mut right = paragraph.bbox.x1;
    for body in &page.paragraphs {
        if body.column == paragraph.column && body.role == crate::model::Role::Paragraph {
            left = left.min(body.bbox.x0);
            right = right.max(body.bbox.x1);
        }
    }
    left = left.max(page.x0);
    right = right.min(page.x0 + page.width);
    for bbox in page
        .paragraphs
        .iter()
        .filter(|p| p.id != paragraph.id)
        .map(|p| p.bbox)
        .chain(page.images.iter().map(|image| image.bbox))
    {
        if bbox.y0 < paragraph.bbox.y1 && bbox.y1 > paragraph.bbox.y0 {
            if bbox.x0 >= paragraph.bbox.x1 {
                right = right.min(bbox.x0 - 2.0);
            } else if bbox.x1 <= paragraph.bbox.x0 {
                left = left.max(bbox.x1 + 2.0);
            }
        }
    }
    let width = match paragraph.align {
        Align::Left => right - paragraph.bbox.x0,
        Align::Center => {
            2.0 * (paragraph.bbox.center_x() - left).min(right - paragraph.bbox.center_x())
        }
        Align::Right => paragraph.bbox.x1 - left,
    };
    width.max(paragraph.bbox.width())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FaceKind {
    Regular,
    Bold,
    Italic,
    Symbol,
}

fn present_chars(
    source: Option<&FontSource>,
    chars: &BTreeSet<char>,
) -> Result<Option<BTreeSet<char>>> {
    match source {
        Some(source) => Ok(Some(
            source
                .metrics(chars.iter().copied())?
                .char_to_gid
                .into_keys()
                .collect(),
        )),
        None => Ok(None),
    }
}

/// Prefer the style's own face, then borrow any face that actually has the
/// glyph; the symbol face is the last resort before preserving the paragraph.
fn pick_face(
    style: TextStyle,
    ch: char,
    regular: &BTreeSet<char>,
    bold: Option<&BTreeSet<char>>,
    italic: Option<&BTreeSet<char>>,
    symbol: Option<&BTreeSet<char>>,
) -> Option<FaceKind> {
    let preferred = if style.italic {
        FaceKind::Italic
    } else if style.bold {
        FaceKind::Bold
    } else {
        FaceKind::Regular
    };
    for kind in [
        preferred,
        FaceKind::Regular,
        FaceKind::Bold,
        FaceKind::Italic,
        FaceKind::Symbol,
    ] {
        let set = match kind {
            FaceKind::Regular => Some(regular),
            FaceKind::Bold => bold,
            FaceKind::Italic => italic,
            FaceKind::Symbol => symbol,
        };
        if set.is_some_and(|set| set.contains(&ch)) {
            return Some(kind);
        }
    }
    None
}

#[derive(Clone, Copy)]
struct Faces<'a> {
    regular: &'a FontMetrics,
    bold: Option<&'a FontMetrics>,
    italic: Option<&'a FontMetrics>,
    symbol: Option<&'a FontMetrics>,
}

impl<'a> Faces<'a> {
    fn for_char(self, style: TextStyle, ch: char) -> &'a FontMetrics {
        let preferred = if style.italic {
            self.italic
        } else if style.bold {
            self.bold
        } else {
            None
        };
        if let Some(face) = preferred.filter(|face| face.char_to_gid.contains_key(&ch)) {
            return face;
        }
        for face in [Some(self.regular), self.bold, self.italic, self.symbol] {
            if let Some(face) = face.filter(|face| face.char_to_gid.contains_key(&ch)) {
                return face;
            }
        }
        self.regular
    }

    fn text_width(self, style: TextStyle, text: &str, size: f64, letter_spacing: f64) -> f64 {
        text.chars()
            .map(|ch| self.for_char(style, ch).char_advance(ch, size))
            .sum::<f64>()
            + text.chars().count().saturating_sub(1) as f64 * letter_spacing
    }

    fn has_any(self, ch: char) -> bool {
        self.regular.char_to_gid.contains_key(&ch)
            || self.bold.is_some_and(|f| f.char_to_gid.contains_key(&ch))
            || self.italic.is_some_and(|f| f.char_to_gid.contains_key(&ch))
            || self.symbol.is_some_and(|f| f.char_to_gid.contains_key(&ch))
    }
}

struct ListPrefix {
    formula: usize,
    indent: f64,
    body_start: usize,
}

fn list_prefix(page: &PageLayout, paragraph: &Paragraph, tokens: &[Token]) -> Option<ListPrefix> {
    let marker = bullet_marker(&paragraph.text)?;
    let Token::Formula(index) = tokens.first()? else {
        return None;
    };
    let formula = &paragraph.formulas[*index];
    if formula.text != marker.to_string() {
        return None;
    }
    let line = paragraph.lines(&page.lines).next()?;
    let body = line
        .glyphs
        .iter()
        .find(|glyph| !glyph.text.trim().is_empty() && glyph.bbox.x0 >= formula.bbox.x1)?;
    let indent = body.bbox.x0 - paragraph.bbox.x0;
    (indent > formula.bbox.width() && indent < paragraph.bbox.width()).then(|| ListPrefix {
        formula: *index,
        indent,
        body_start: 1 + tokens[1..]
            .iter()
            .take_while(|token| matches!(token, Token::Space(_)))
            .count(),
    })
}

fn typeset_paragraph(
    page: &PageLayout,
    paragraph: &Paragraph,
    tokens: &[Token],
    metrics: Faces<'_>,
    options: &TypesetOptions,
    max_height: f64,
    max_rise: f64,
) -> TypesetParagraph {
    let base_size = paragraph.font_size.clamp(6.0, 42.0);
    let max_width = if paragraph.role == crate::model::Role::Heading {
        // A short English heading can become longer in translation. Use
        // available column space without moving its original alignment axis.
        available_heading_width(page, paragraph)
    } else if matches!(
        paragraph.role,
        crate::model::Role::Title | crate::model::Role::Caption
    ) {
        paragraph.bbox.width().max(base_size * 8.0)
    } else {
        paragraph.bbox.width().max(base_size * 3.0)
    };
    let mut chosen_size = base_size;
    let mut chosen_drafts: Vec<LineDraft> = Vec::new();
    let mut chosen_baselines = Vec::new();
    let mut chosen_bounds = Vec::new();
    let mut chosen_scale = 1.0;
    let mut chosen_rise = 0.0;
    let mut overflow = true;
    let list = list_prefix(page, paragraph, tokens);
    let body_tokens = list
        .as_ref()
        .map_or(tokens, |list| &tokens[list.body_start..]);

    let steps = ((1.0 - options.min_scale) / 0.02).ceil() as usize;
    for step in 0..=steps {
        let scale = (1.0 - step as f64 * 0.02).max(options.min_scale);
        let size = base_size * scale;
        let indent = list.as_ref().map_or(0.0, |list| list.indent * scale);
        let line_width = (max_width - indent).max(size);
        let mut drafts = wrap_tokens(
            body_tokens,
            line_width,
            size,
            metrics,
            &paragraph.formulas,
            scale,
        );
        if let Some(list) = &list {
            // The bullet-to-body gap and hanging indent belong to the PDF's
            // layout, not to whitespace the translation model may drop.
            for draft in &mut drafts {
                for segment in &mut draft.segments {
                    segment.x += indent;
                }
                for (_, x) in &mut draft.formulas {
                    *x += indent;
                }
                draft.width += indent;
            }
            drafts[0].formulas.insert(0, (list.formula, 0.0));
        }
        if paragraph.role == crate::model::Role::Title
            && paragraph.formulas.is_empty()
            && !tokens.iter().any(|token| matches!(token, Token::Newline))
        {
            balance_title_lines(tokens, &mut drafts, line_width, size, metrics);
        }
        let (baselines, bounds, total_height) = draft_baselines(
            &drafts,
            size,
            metrics,
            &paragraph.formulas,
            options.line_height,
            scale,
        );
        chosen_size = size;
        chosen_scale = scale;
        chosen_drafts = drafts;
        chosen_baselines = baselines;
        chosen_bounds = bounds;
        chosen_rise = (total_height - max_height).max(0.0).min(max_rise);
        let fits =
            total_height <= max_height + max_rise && !drafts_too_wide(&chosen_drafts, max_width);
        if fits || scale <= options.min_scale + 1e-9 {
            overflow = !fits;
            break;
        }
    }

    let mut lines = Vec::with_capacity(chosen_drafts.len());

    for (index, draft) in chosen_drafts.iter().enumerate() {
        let offset_x = match paragraph.align {
            Align::Left => paragraph.bbox.x0,
            Align::Center => paragraph.bbox.center_x() - draft.width * 0.5,
            Align::Right => paragraph.bbox.x1 - draft.width,
        };
        let baseline = paragraph.bbox.y1 + chosen_rise - chosen_baselines[index];

        let segments = draft
            .segments
            .iter()
            .map(|segment| TypesetSegment {
                text: segment.text.clone(),
                x: offset_x + segment.x,
                bold: segment.style.bold,
                italic: segment.style.italic,
                color: segment.style.color.unwrap_or([0, 0, 0, 255]),
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
                scale: chosen_scale,
            });
        }
        let (ascent, descent) = chosen_bounds[index];
        let mut x0 = f64::INFINITY;
        let mut x1 = f64::NEG_INFINITY;
        for segment in &segments {
            let style = TextStyle {
                bold: segment.bold,
                italic: segment.italic,
                color: Some(segment.color),
            };
            x0 = x0.min(segment.x);
            x1 = x1.max(segment.x + metrics.text_width(style, &segment.text, chosen_size, 0.0));
        }
        for formula in &formulas {
            x0 = x0.min(formula.x);
            x1 = x1.max(formula.x + formula.source_bbox.width() * formula.scale);
        }
        if !x0.is_finite() {
            x0 = offset_x;
            x1 = offset_x;
        }
        lines.push(TypesetLine {
            baseline,
            size: chosen_size,
            bbox: BBox::new(x0, baseline - descent, x1, baseline + ascent),
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

fn draft_baselines(
    drafts: &[LineDraft],
    size: f64,
    metrics: Faces<'_>,
    formulas: &[FormulaRef],
    line_height: f64,
    formula_scale: f64,
) -> (Vec<f64>, Vec<(f64, f64)>, f64) {
    let mut baselines = Vec::with_capacity(drafts.len());
    let mut bounds = Vec::with_capacity(drafts.len());
    let mut previous_descent = 0.0;
    let mut height = 0.0;
    for draft in drafts {
        let mut ascent: f64 = 0.0;
        let mut descent: f64 = 0.0;
        for segment in &draft.segments {
            for ch in segment.text.chars().filter(|ch| !ch.is_whitespace()) {
                let face = metrics.for_char(segment.style, ch);
                let (bottom, top) = face
                    .char_vertical_bounds
                    .get(&ch)
                    .copied()
                    .unwrap_or((face.descender / face.units_per_em, face.ascent_ratio()));
                ascent = ascent.max(top * size);
                descent = descent.max(-bottom * size);
            }
        }
        for &(index, _) in &draft.formulas {
            let formula = &formulas[index];
            ascent = ascent.max((formula.bbox.y1 - formula.baseline) * formula_scale);
            descent = descent.max((formula.baseline - formula.bbox.y0) * formula_scale);
        }
        // An empty explicit line still advances by the configured line height.
        if ascent + descent == 0.0 {
            ascent = size;
        }
        let baseline = baselines.last().map_or(ascent, |previous| {
            previous + (size * line_height).max(previous_descent + ascent + 0.5)
        });
        height = baseline + descent;
        baselines.push(baseline);
        bounds.push((ascent, descent));
        previous_descent = descent;
    }
    (baselines, bounds, height)
}

fn drafts_too_wide(drafts: &[LineDraft], max_width: f64) -> bool {
    drafts.iter().any(|draft| draft.width > max_width + 0.01)
}

fn balance_title_lines(
    tokens: &[Token],
    drafts: &mut Vec<LineDraft>,
    width: f64,
    size: f64,
    faces: Faces<'_>,
) {
    if drafts.len() < 2 {
        return;
    }
    let count = drafts.len();
    let mut low = drafts.iter().map(|line| line.width).sum::<f64>() / count as f64;
    let mut high = width;
    // Retain the existing line count and font size, but avoid a nearly full
    // first line followed by a single-character final title line.
    for _ in 0..10 {
        let candidate_width = (low + high) / 2.0;
        let candidate = wrap_tokens(tokens, candidate_width, size, faces, &[], 1.0);
        if candidate.len() > count || drafts_too_wide(&candidate, candidate_width) {
            low = candidate_width;
        } else {
            high = candidate_width;
            *drafts = candidate;
        }
    }
}

#[derive(Debug, Clone, Default)]
struct SegmentDraft {
    text: String,
    x: f64,
    style: TextStyle,
}

#[derive(Debug, Clone, Default)]
struct LineDraft {
    segments: Vec<SegmentDraft>,
    formulas: Vec<(usize, f64)>,
    width: f64,
    after_formula: bool,
}

impl LineDraft {
    fn ends_with_formula_and_punctuation(&self) -> bool {
        self.formulas.last().is_some_and(|(_, x)| {
            self.segments
                .iter()
                .filter(|segment| segment.x >= *x)
                .all(|segment| {
                    segment
                        .text
                        .chars()
                        .all(|ch| ch.is_whitespace() || is_closing_punctuation_char(ch))
                })
        })
    }

    fn push_text(&mut self, text: &str, width: f64, style: TextStyle) {
        if text.is_empty() {
            return;
        }
        if !self.after_formula
            && let Some(last) = self.segments.last_mut()
            && last.style == style
        {
            last.text.push_str(text);
        } else {
            self.segments.push(SegmentDraft {
                text: text.to_string(),
                x: self.width,
                style,
            });
            self.after_formula = false;
        }
        self.width += width;
    }
    /// Keep a closing mark with the preceding character or complete Latin token.
    fn take_for_break(
        &mut self,
        closing: bool,
        faces: Faces<'_>,
        size: f64,
    ) -> Option<(String, f64, TextStyle)> {
        if self.after_formula {
            return None;
        }
        let last = self.segments.last_mut()?;
        if closing {
            let trimmed = last.text.trim_end().len();
            let removed = last.text.split_off(trimmed);
            self.width -= faces.text_width(last.style, &removed, size, 0.0);
        }
        let mut ch = last.text.chars().next_back()?;
        if !closing && !is_opening_punctuation(ch) {
            return None;
        }
        let style = last.style;
        let mut start = last.text.len() - ch.len_utf8();
        if closing {
            // A run such as “）。 ” belongs with the preceding content, not
            // on a separate line containing only the closing marks.
            while is_closing_punctuation_char(ch) && start > 0 {
                ch = last.text[..start].chars().next_back()?;
                start -= ch.len_utf8();
            }
        }
        if closing && ch.is_ascii_alphanumeric() {
            // Moving just the last digit turns 2025 into "202" / "5）".
            // The same rule applies to decimals, citations and Latin words.
            start = last.text[..start + ch.len_utf8()]
                .char_indices()
                .rev()
                .take_while(|(_, ch)| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
                .last()
                .map_or(start, |(index, _)| index);
        }
        let moved = last.text.split_off(start);
        let width = faces.text_width(style, &moved, size, 0.0);
        self.width -= width;
        Some((moved, width, style))
    }
}

fn wrap_tokens(
    tokens: &[Token],
    max_width: f64,
    size: f64,
    faces: Faces<'_>,
    formulas: &[FormulaRef],
    formula_scale: f64,
) -> Vec<LineDraft> {
    let mut lines: Vec<LineDraft> = Vec::new();
    let mut current = LineDraft::default();
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::Newline => push_draft(&mut lines, &mut current, faces, size),
            Token::Space(style) => {
                let width = faces.for_char(*style, ' ').char_advance(' ', size);
                if current.width > 0.0
                    && (current.after_formula
                        || !current
                            .segments
                            .last()
                            .is_some_and(|segment| segment.text.ends_with(' ')))
                {
                    current.push_text(" ", width, *style);
                }
            }
            Token::Text(piece, style) => {
                let piece_width = faces.text_width(*style, piece, size, 0.0);
                // Font/style switches can split a single decimal or Latin word
                // into several tokens (e.g. 0<i>.</i>0%). Fit those fragments
                // together, including adjacent closing punctuation.
                let mut required_width = piece_width;
                if piece
                    .chars()
                    .all(|ch| !is_cjk(ch) && !is_cjk_punctuation(ch))
                {
                    for next in &tokens[index + 1..] {
                        let Token::Text(text, style) = next else {
                            break;
                        };
                        if !is_closing_punctuation(text)
                            && text.chars().any(|ch| is_cjk(ch) || is_cjk_punctuation(ch))
                        {
                            break;
                        }
                        required_width += faces.text_width(*style, text, size, 0.0);
                    }
                }
                if required_width > max_width {
                    required_width = piece_width;
                }
                let bound_to_formula =
                    is_closing_punctuation(piece) && current.ends_with_formula_and_punctuation();
                if current.width + required_width > max_width
                    && current.width > 0.0
                    && !bound_to_formula
                {
                    let moved = current.take_for_break(is_closing_punctuation(piece), faces, size);
                    push_draft(&mut lines, &mut current, faces, size);
                    if let Some((text, width, style)) = moved {
                        current.push_text(&text, width, style);
                    }
                }

                if piece_width > max_width && current.width == 0.0 {
                    let mut buffer = String::new();
                    let mut width = 0.0;
                    for ch in piece.chars() {
                        let advance = faces.for_char(*style, ch).char_advance(ch, size);
                        if width + advance > max_width && !buffer.is_empty() {
                            current.push_text(&buffer, width, *style);
                            push_draft(&mut lines, &mut current, faces, size);
                            buffer.clear();
                            width = 0.0;
                        }
                        buffer.push(ch);
                        width += advance;
                    }
                    current.push_text(&buffer, width, *style);
                } else {
                    current.push_text(piece, piece_width, *style);
                }
            }
            Token::Formula(id) => {
                let formula = &formulas[*id];
                let width = formula.bbox.width() * formula_scale;
                let mut required_width = width;
                let mut spaces = 0.0;
                for next in &tokens[index + 1..] {
                    match next {
                        Token::Space(style) => {
                            spaces += faces.for_char(*style, ' ').char_advance(' ', size)
                        }
                        Token::Text(text, style)
                            if text.chars().all(is_closing_punctuation_char) =>
                        {
                            required_width += spaces + faces.text_width(*style, text, size, 0.0);
                            spaces = 0.0;
                        }
                        _ => break,
                    }
                }
                if current.width + required_width > max_width && current.width > 0.0 {
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
                draft.width -=
                    faces.text_width(segment.style, &segment.text[trimmed.len()..], size, 0.0);
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
            Piece::Color(color) => style.color = color.or(base.color),
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
            italic: None,
            symbol: None,
        },
        &[],
        1.0,
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
    piece
        .chars()
        .next()
        .is_some_and(is_closing_punctuation_char)
}

fn is_closing_punctuation_char(ch: char) -> bool {
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
            | '”'
            | '’'
            | '.'
            | ','
            | ';'
            | ':'
            | '!'
            | '?'
            | ')'
            | ']'
            | '}'
            | '%'
            | '％'
            | '‰'
            | '‱'
            | '°'
    )
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
            char_vertical_bounds: HashMap::new(),
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
        assert_eq!(
            wrap_text("一二三四（白色）。", 80.0, 10.0, &metrics()),
            vec!["一二三四（白".to_string(), "色）。".to_string()]
        );
    }

    #[test]
    fn closing_punctuation_does_not_split_years_decimals_or_latin_words() {
        for token in ["2025", "0.80", "CoT"] {
            let text = format!("一二{token}）");
            let width = metrics().text_width(&format!("一二{token}"), 10.0, 0.0) + 0.1;
            let lines = wrap_text(&text, width, 10.0, &metrics());
            assert_eq!(lines, vec!["一二".to_string(), format!("{token}）")]);
        }
        assert_eq!(
            wrap_text("一二2025 ]", 44.1, 10.0, &metrics()),
            vec!["一二".to_string(), "2025]".to_string()]
        );
    }

    #[test]
    fn closing_marks_stay_with_a_formula_including_spaces_and_style_changes() {
        let metrics = metrics();
        let formulas = vec![FormulaRef {
            sources: Vec::new(),
            id: "f1".into(),
            text: "x".into(),
            bbox: crate::model::BBox {
                x0: 0.0,
                y0: 0.0,
                x1: 30.0,
                y1: 10.0,
            },
            baseline: 0.0,
        }];
        for suffix in [
            "。",
            " 。",
            "<i>）</i>。",
            ".",
            "%",
            " %",
            "<b>%</b>",
            "‰",
            "°",
        ] {
            let drafts = wrap_tokens(
                &tokenize(
                    &format!("一二<f1>{suffix}"),
                    &formulas,
                    TextStyle::default(),
                ),
                50.0,
                10.0,
                Faces {
                    regular: &metrics,
                    bold: None,
                    italic: None,
                    symbol: None,
                },
                &formulas,
                1.0,
            );
            assert_eq!(drafts.len(), 2, "{suffix}");
            assert_eq!(drafts[1].formulas.len(), 1);
            assert!(!drafts[1].segments.is_empty());
            assert!(!drafts_too_wide(&drafts, 50.0));
        }
        // An oversized bound group stays intact so font fitting can shrink it.
        let drafts = wrap_tokens(
            &tokenize("<f1>）。", &formulas, TextStyle::default()),
            30.0,
            10.0,
            Faces {
                regular: &metrics,
                bold: None,
                italic: None,
                symbol: None,
            },
            &formulas,
            1.0,
        );
        assert_eq!(drafts.len(), 1);
        assert!(drafts_too_wide(&drafts, 30.0));
    }

    #[test]
    fn style_changes_do_not_break_decimals_years_or_words() {
        for (marked, plain) in [
            ("0<i>.</i>0%", "0.0%"),
            ("90<i>.</i>6%", "90.6%"),
            ("20<b>25</b>", "2025"),
            ("Co<b>T</b>", "CoT"),
        ] {
            let width = metrics().text_width(plain, 10.0, 0.0) + 10.1;
            for suffix in ["", "）"] {
                assert_eq!(
                    wrap_text(&format!("一二{marked}{suffix}"), width, 10.0, &metrics()),
                    vec!["一二".to_owned(), format!("{plain}{suffix}")],
                );
            }
        }
    }

    #[test]
    fn title_balancing_avoids_an_orphan_without_adding_lines() {
        let metrics = metrics();
        let faces = Faces {
            regular: &metrics,
            bold: None,
            italic: None,
            symbol: None,
        };
        let tokens = tokenize("一二三四五六七八九十甲", &[], TextStyle::default());
        let mut drafts = wrap_tokens(&tokens, 100.0, 10.0, faces, &[], 1.0);
        assert_eq!(drafts.len(), 2);
        balance_title_lines(&tokens, &mut drafts, 100.0, 10.0, faces);
        assert_eq!(drafts.len(), 2);
        assert!((drafts[0].width - drafts[1].width).abs() <= 10.01);
    }

    #[test]
    fn for_char_borrows_faces_that_have_the_glyph() {
        let mut regular = metrics();
        regular.char_to_gid.insert('ø', 1);
        let mut italic = metrics();
        italic.char_to_gid.insert('译', 2);
        let mut symbol = metrics();
        symbol.char_to_gid.insert('⟩', 3);
        let faces = Faces {
            regular: &regular,
            bold: None,
            italic: Some(&italic),
            symbol: Some(&symbol),
        };
        let italic_style = TextStyle {
            bold: false,
            italic: true,
            color: None,
        };
        assert!(std::ptr::eq(faces.for_char(italic_style, '译'), &italic));
        assert!(std::ptr::eq(faces.for_char(italic_style, 'ø'), &regular));
        assert!(std::ptr::eq(faces.for_char(italic_style, '⟩'), &symbol));
        assert!(std::ptr::eq(
            faces.for_char(TextStyle::default(), '⟩'),
            &symbol
        ));
        assert!(faces.has_any('⟩'));
        assert!(!faces.has_any('\u{31350}'));
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
                italic: None,
                symbol: None,
            },
            &[],
            1.0,
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
