mod inline;
mod lines;
mod paragraphs;

use crate::model::{ExtractedPage, LayoutDocument, LayoutStats, PageLayout, Role};
use inline::attach_inline_formulas;
use lines::{assign_columns, build_lines, filter_layout_artifacts, is_formula_line};
use paragraphs::{
    body_font_size, build_paragraphs, caption_kind, classify_paragraph,
    merge_caption_continuations, paragraph_alignment, protect_figure_text, protect_footnotes,
    protect_front_matter, protect_references,
};
use rayon::prelude::*;

#[derive(Debug, Clone, Copy, Default)]
pub struct LayoutOptions {
    pub translate_references: bool,
}

#[derive(Debug, Clone, Copy)]
struct PageFrame {
    x0: f64,
    y0: f64,
    width: f64,
    height: f64,
}

impl PageFrame {
    fn center_x(self) -> f64 {
        self.x0 + self.width * 0.5
    }

    fn bottom(self) -> f64 {
        self.y0
    }
}

pub fn analyze_document(
    source: String,
    extracted: Vec<ExtractedPage>,
    options: LayoutOptions,
) -> LayoutDocument {
    let pages: Vec<PageLayout> = if extracted.len() >= 4 {
        extracted.into_par_iter().map(analyze_page).collect()
    } else {
        extracted.into_iter().map(analyze_page).collect()
    };
    let mut doc = LayoutDocument {
        source,
        stats: LayoutStats::default(),
        pages,
    };
    if !options.translate_references {
        protect_references(&mut doc.pages);
    }
    doc.recompute_stats();
    doc
}

fn analyze_page(page: ExtractedPage) -> PageLayout {
    let frame = PageFrame {
        x0: page.x0,
        y0: page.y0,
        width: page.width,
        height: page.height,
    };
    let glyphs: Vec<_> = page.glyphs.into_iter().filter(|g| !g.invisible).collect();

    let mut lines = build_lines(glyphs);
    filter_layout_artifacts(&mut lines, frame);
    assign_columns(&mut lines, frame);
    let body_size = body_font_size(&lines);

    for line in &mut lines {
        line.formula = is_formula_line(line);
    }

    let mut paragraphs = build_paragraphs(&lines, page.index, frame, body_size);
    for paragraph in &mut paragraphs {
        paragraph.role = classify_paragraph(paragraph, &lines, page.index, frame, body_size);
    }
    merge_caption_continuations(&mut paragraphs);
    for paragraph in &mut paragraphs {
        paragraph.role = classify_paragraph(paragraph, &lines, page.index, frame, body_size);
        paragraph.align = paragraph_alignment(paragraph, frame);
        paragraph.protected = matches!(
            paragraph.role,
            Role::Formula | Role::Header | Role::Footer | Role::PageNumber
        ) || paragraph.has_formula
            || (paragraph.role == Role::Caption && caption_kind(&paragraph.text) == Some(true));
    }
    protect_front_matter(&mut paragraphs, page.index);
    protect_figure_text(&mut paragraphs, &page.images);
    protect_footnotes(&mut paragraphs, &lines, frame, body_size);
    for paragraph in &mut paragraphs {
        let glyph_protected = paragraph
            .lines(&lines)
            .flat_map(|line| &line.glyphs)
            .any(|glyph| glyph.protected);
        paragraph.protected |= glyph_protected;
        paragraph.finalize_style(&lines);
        attach_inline_formulas(paragraph, &lines);
    }

    PageLayout {
        index: page.index,
        x0: page.x0,
        y0: page.y0,
        width: page.width,
        height: page.height,
        rotation: page.rotation,
        layout_transform: page.layout_transform,
        glyph_signature: page.glyph_signature,
        lines,
        paragraphs,
        images: page.images,
    }
}

#[cfg(test)]
mod tests {
    use super::inline::{find_formula_groups, is_math_char};
    use super::paragraphs::make_paragraph;
    use super::*;
    use crate::model::{BBox, Column, GlyphBox, TextLine, TextSpan};

    fn text_glyph(text: &str, x: f64, y: f64) -> GlyphBox {
        GlyphBox {
            text: text.to_string(),
            bbox: BBox::new(x, y, x + 240.0, y + 10.0),
            origin_x: x,
            origin_y: y,
            advance: 240.0,
            font_size: 10.0,
            angle: 0.0,
            font_name: Some("Body".to_string()),
            bold: false,
            italic: false,
            serif: true,
            mono: false,
            color: [0, 0, 0, 255],
            invisible: false,
            protected: false,
            decorations: Vec::new(),
            char_code: 0,
            glyph_id: 0,
            sources: Default::default(),
        }
    }

    fn text_line(text: &str, x: f64, y: f64) -> TextLine {
        let glyph = text_glyph(text, x, y);
        TextLine {
            bbox: glyph.bbox,
            baseline: y,
            text: text.to_string(),
            spans: vec![TextSpan {
                text: text.to_string(),
                bbox: glyph.bbox,
                font_size: 10.0,
                font_name: Some("Body".to_string()),
                bold: false,
                italic: false,
                serif: true,
                mono: false,
                color: [0, 0, 0, 255],
                formula: false,
            }],
            font_size: 10.0,
            column: Column::Full,
            formula: false,
            math_ratio: 0.0,
            glyphs: vec![glyph],
        }
    }

    #[test]
    fn first_line_indent_starts_a_new_paragraph() {
        let frame = PageFrame {
            x0: 0.0,
            y0: 0.0,
            width: 600.0,
            height: 800.0,
        };
        let lines = vec![
            text_line("A body line that ends a sentence.", 50.0, 700.0),
            text_line("An indented first line starts here.", 62.0, 686.0),
            text_line("and continues on the next line.", 50.0, 672.0),
        ];
        let paragraphs = build_paragraphs(&lines, 0, frame, 10.0);
        assert_eq!(
            paragraphs.len(),
            2,
            "first-line indentation should split paragraphs: {paragraphs:?}"
        );
    }

    #[test]
    fn reference_continuations_are_protected_after_paragraph_split() {
        let mut heading = make_paragraph(&[text_line("References", 50.0, 700.0)], vec![0], 0, 0);
        heading.role = Role::Reference;
        let mut first = make_paragraph(
            &[text_line("[1] A reference entry.", 50.0, 680.0)],
            vec![0],
            0,
            1,
        );
        first.role = Role::Paragraph;
        let mut continuation = make_paragraph(
            &[text_line("continued reference text.", 62.0, 660.0)],
            vec![0],
            0,
            2,
        );
        continuation.role = Role::Paragraph;
        let mut pages = [PageLayout {
            index: 0,
            x0: 0.0,
            y0: 0.0,
            width: 600.0,
            height: 800.0,
            rotation: 0,
            glyph_signature: [0; 32],
            layout_transform: hayro::kurbo::Affine::IDENTITY.as_coeffs(),
            lines: Vec::new(),
            paragraphs: vec![heading, first, continuation],
            images: Vec::new(),
        }];
        protect_references(&mut pages);
        assert!(pages[0].paragraphs.iter().all(|p| p.protected));
    }

    #[test]
    fn appendix_after_references_on_same_page_is_not_protected() {
        let mut heading = make_paragraph(&[text_line("References", 50.0, 700.0)], vec![0], 0, 0);
        heading.role = Role::Reference;
        let mut entry = make_paragraph(
            &[text_line("[1] A reference entry.", 50.0, 680.0)],
            vec![0],
            0,
            1,
        );
        entry.role = Role::Paragraph;
        let mut appendix = make_paragraph(&[text_line("A. Appendix", 50.0, 300.0)], vec![0], 0, 2);
        appendix.role = Role::Heading;
        let mut body = make_paragraph(
            &[text_line("Appendix body text.", 50.0, 280.0)],
            vec![0],
            0,
            3,
        );
        body.role = Role::Paragraph;
        let mut pages = [PageLayout {
            index: 0,
            x0: 0.0,
            y0: 0.0,
            width: 600.0,
            height: 800.0,
            rotation: 0,
            glyph_signature: [0; 32],
            layout_transform: hayro::kurbo::Affine::IDENTITY.as_coeffs(),
            lines: Vec::new(),
            paragraphs: vec![heading, entry, appendix, body],
            images: Vec::new(),
        }];
        protect_references(&mut pages);
        assert!(pages[0].paragraphs[0].protected);
        assert!(pages[0].paragraphs[1].protected);
        assert!(!pages[0].paragraphs[2].protected);
        assert!(!pages[0].paragraphs[3].protected);
    }

    #[test]
    fn math_chars_are_detected_by_unicode_block() {
        assert!(is_math_char('α'));
        assert!(is_math_char('∑'));
        assert!(is_math_char('≤'));
        assert!(is_math_char('='));
        assert!(!is_math_char('a'));
        assert!(!is_math_char('中'));
    }

    #[test]
    fn multidigit_dimensions_become_one_formula() {
        fn glyph_at(text: &str, x: f64, font: &str) -> GlyphBox {
            let mut glyph = text_glyph(text, x, 100.0);
            glyph.bbox = BBox::new(x, 100.0, x + 7.0, 110.0);
            glyph.advance = 7.0;
            glyph.font_name = Some(font.to_string());
            glyph
        }

        let glyphs = [
            glyph_at("3", 0.0, "Body"),
            glyph_at("2", 7.0, "Body"),
            glyph_at("×", 14.0, "CMSY9"),
            glyph_at("3", 21.0, "Body"),
            glyph_at("2", 28.0, "Body"),
        ];
        let refs: Vec<&GlyphBox> = glyphs.iter().collect();
        let line = text_line("32×32", 0.0, 100.0);
        let groups = find_formula_groups(&line, &refs, Some("Body"));
        assert_eq!(groups.len(), 1);
        let (start, end) = groups[0];
        let text: String = refs[start..=end]
            .iter()
            .map(|glyph| glyph.text.as_str())
            .collect();
        assert_eq!(text, "32×32");
    }

    #[test]
    fn nonzero_media_box_origin_does_not_turn_body_into_header() {
        let page = ExtractedPage {
            index: 0,
            x0: 100.0,
            y0: 200.0,
            width: 612.0,
            height: 792.0,
            rotation: 0,
            glyph_signature: [0; 32],
            layout_transform: hayro::kurbo::Affine::IDENTITY.as_coeffs(),
            glyphs: vec![text_glyph(
                "Body text that is not a running header",
                172.0,
                920.0,
            )],
            images: Vec::new(),
        };
        let layout = analyze_page(page);
        assert!(
            layout
                .paragraphs
                .iter()
                .any(|paragraph| !paragraph.protected && paragraph.text.contains("Body text")),
            "body text was classified as a protected header: {:?}",
            layout.paragraphs
        );
    }
}
