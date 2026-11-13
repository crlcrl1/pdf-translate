mod algorithms;
mod fractions;
mod inline;
mod lines;
mod paragraphs;
mod tables;

use crate::model::{Column, ExtractedPage, LayoutDocument, LayoutStats, PageLayout, Role};
use inline::attach_inline_formulas;
use lines::{assign_columns, build_lines, filter_layout_artifacts, is_formula_line};
use paragraphs::{
    body_font_size, build_paragraphs, classify_paragraph, is_code_block,
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
    let mut running_headers =
        std::collections::HashMap::<String, std::collections::HashSet<usize>>::new();
    for page in &doc.pages {
        for paragraph in &page.paragraphs {
            if paragraph.bbox.y0 > page.y0 + page.height * 0.85 && paragraph.text.len() < 160 {
                running_headers
                    .entry(running_header_text(&paragraph.text))
                    .or_default()
                    .insert(page.index);
            }
        }
    }
    for page in &mut doc.pages {
        for paragraph in &mut page.paragraphs {
            if paragraph.bbox.y0 > page.y0 + page.height * 0.85
                && paragraph.role != Role::Title
                && running_headers
                    .get(&running_header_text(&paragraph.text))
                    .is_some_and(|pages| pages.len() >= 2)
            {
                paragraph.role = Role::Header;
                paragraph.protected = true;
            }
        }
    }
    if !options.translate_references {
        protect_references(&mut doc.pages);
    }
    doc.recompute_stats();
    doc
}

fn running_header_text(text: &str) -> String {
    // A sparse page can merge its page number into the running header. Ignore
    // standalone edge numbers when matching headers across pages, leaving
    // embedded digits and the original glyphs untouched.
    let words: Vec<_> = text.split_whitespace().collect();
    let mut range = 0..words.len();
    let number = |word: &str| word.chars().all(|ch| ch.is_ascii_digit());
    while range.len() > 1 && number(words[range.start]) {
        range.start += 1;
    }
    while range.len() > 1 && number(words[range.end - 1]) {
        range.end -= 1;
    }
    words[range].join(" ")
}

fn analyze_page(page: ExtractedPage) -> PageLayout {
    let frame = PageFrame {
        x0: page.x0,
        y0: page.y0,
        width: page.width,
        height: page.height,
    };
    let glyphs: Vec<_> = page.glyphs.into_iter().filter(|g| !g.invisible).collect();
    let glyphs = fractions::bind_numeric_fractions(glyphs, &page.rules);

    let mut lines = build_lines(glyphs);
    assign_columns(&mut lines, frame);
    // Infer the gutters before attaching displaced math fragments. Otherwise
    // a superscript can join an unrelated baseline in the opposite column.
    let mut rebuilt = Vec::new();
    for column in [Column::Full, Column::Left, Column::Right] {
        let glyphs = lines
            .iter()
            .filter(|line| line.column == column)
            .flat_map(|line| line.glyphs.iter().cloned())
            .collect();
        let mut column_lines = build_lines(glyphs);
        for line in &mut column_lines {
            line.column = column;
        }
        rebuilt.extend(column_lines);
    }
    lines = rebuilt;
    lines::split_caption_lines(&mut lines);
    filter_layout_artifacts(&mut lines, frame);
    for line in &mut lines {
        line.formula = is_formula_line(line);
    }
    lines::protect_cases(&mut lines);
    lines::protect_numbered_equations(&mut lines, frame);
    tables::protect_ruled_tables(&mut lines, &page.rules);
    // Display equations often dominate appendix pages. Their small fonts
    // must not make ordinary prose look like large section headings.
    let body_size = body_font_size(&lines);

    let mut paragraphs = build_paragraphs(&lines, page.index, frame, body_size);
    for paragraph in &mut paragraphs {
        paragraph.role = classify_paragraph(paragraph, &lines, page.index, frame, body_size);
    }
    merge_caption_continuations(&mut paragraphs);
    for paragraph in &mut paragraphs {
        paragraph.role = classify_paragraph(paragraph, &lines, page.index, frame, body_size);
        paragraph.protected |= matches!(
            paragraph.role,
            Role::Formula | Role::Header | Role::Footer | Role::PageNumber
        ) || paragraph.has_formula
            || is_code_block(paragraph, &lines);
    }
    protect_front_matter(&mut paragraphs, page.index);
    paragraphs::protect_attributions(&mut paragraphs, &lines, page.index);
    algorithms::protect_bodies(&mut paragraphs, &lines);
    protect_figure_text(&mut paragraphs, &page.images, &lines);
    protect_footnotes(&mut paragraphs, &lines, frame, body_size);
    for paragraph in &mut paragraphs {
        // Front-matter detection can promote body-sized text to a title.
        // Derive alignment only after its final role is known.
        paragraph.align = paragraph_alignment(paragraph, &lines, frame);
        let glyph_protected = paragraph
            .lines(&lines)
            .flat_map(|line| &line.glyphs)
            .any(|glyph| glyph.protected);
        paragraph.protected |= glyph_protected;
        paragraph.finalize_style(&lines);
        attach_inline_formulas(paragraph, &lines);
        include_formula_rules(paragraph, &page.images, &page.rules);
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
        rules: page.rules,
    }
}

pub(crate) fn include_formula_rules(
    paragraph: &mut crate::model::Paragraph,
    images: &[crate::model::ImageBox],
    rules: &[crate::model::BBox],
) {
    let overhang = (paragraph.font_size * 0.2).max(1.0);
    for formula in &mut paragraph.formulas {
        for rule in images
            .iter()
            .map(|image| image.bbox)
            .chain(rules.iter().copied())
        {
            // TeX bars may extend slightly past the exact ink bounds of the
            // radical or numerator. Associate the whole thin rule before
            // fitting, clipping and moving the original vector object.
            if rule.height() <= 1.5
                && rule.width() >= rule.height() * 3.0
                && rule.x0 >= formula.bbox.x0 - overhang
                && rule.x1 <= formula.bbox.x1 + overhang
                && rule.y0 >= formula.bbox.y0 - 0.75
                && rule.y1 <= formula.bbox.y1 + 0.75
            {
                formula.bbox = formula.bbox.union(rule);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::inline::{find_formula_groups, is_math_char};
    use super::paragraphs::make_paragraph;
    use super::*;
    use crate::model::{BBox, Column, GlyphBox, TextLine, TextSpan};

    const FRAME: PageFrame = PageFrame {
        x0: 0.0,
        y0: 0.0,
        width: 600.0,
        height: 800.0,
    };

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
            math_group: false,
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
    fn running_headers_match_when_a_sparse_page_merges_its_number() {
        let pages = [
            "GRAPH SYSTEMS IN DIMENSION 5",
            "GRAPH SYSTEMS IN DIMENSION 5 15",
        ]
        .into_iter()
        .enumerate()
        .map(|(index, header)| ExtractedPage {
            index: index + 1,
            x0: 0.0,
            y0: 0.0,
            width: 612.0,
            height: 792.0,
            rotation: 0,
            layout_transform: hayro::kurbo::Affine::IDENTITY.as_coeffs(),
            glyph_signature: [0; 32],
            images: Vec::new(),
            rules: Vec::new(),
            glyphs: vec![
                text_glyph(header, 72.0, 720.0),
                text_glyph("Ordinary body text remains translatable.", 72.0, 500.0),
            ],
        })
        .collect();
        let doc = analyze_document("fixture".into(), pages, LayoutOptions::default());
        for page in doc.pages {
            let header = &page.paragraphs[0];
            assert_eq!(header.role, Role::Header);
            assert!(header.protected);
            assert!(!page.paragraphs[1].protected);
        }
    }

    #[test]
    fn reference_heading_in_right_column_leaves_left_body_translatable() {
        let mut body = make_paragraph(
            &[text_line("[37]. Body before the references.", 50.0, 300.0)],
            vec![0],
            0,
            0,
        );
        body.column = Column::Left;
        let mut heading = make_paragraph(&[text_line("References", 320.0, 600.0)], vec![0], 0, 1);
        heading.column = Column::Right;
        heading.role = Role::Reference;
        let mut entry = make_paragraph(
            &[text_line("Author. Reference entry.", 320.0, 580.0)],
            vec![0],
            0,
            2,
        );
        entry.column = Column::Right;
        let mut page = analyze_page(ExtractedPage {
            index: 0,
            x0: 0.0,
            y0: 0.0,
            width: 612.0,
            height: 792.0,
            rotation: 0,
            layout_transform: hayro::kurbo::Affine::IDENTITY.as_coeffs(),
            glyph_signature: [0; 32],
            glyphs: Vec::new(),
            images: Vec::new(),
            rules: Vec::new(),
        });
        page.paragraphs = vec![body, heading, entry];
        protect_references(std::slice::from_mut(&mut page));
        assert!(!page.paragraphs[0].protected);
        assert!(page.paragraphs[1..].iter().all(|p| p.protected));
    }

    #[test]
    fn equations_with_many_exponents_are_not_translated_as_prose() {
        let mut line = text_line(
            "π(D5) = {[−1]40, [−1/2]480, [0]560, [1/2]480, [1]40}.",
            50.0,
            600.0,
        );
        line.math_ratio = 0.30;
        line.spans[0].formula = true;
        assert!(is_formula_line(&line));
        let mut prose = text_line(
            "[e(x1), e(x2), ..., e(xt)]. It is worth noting that the last hidden states have",
            50.0,
            580.0,
        );
        prose.math_ratio = 0.42;
        assert!(!is_formula_line(&prose));
    }

    #[test]
    fn prose_surrounding_dense_inline_math_is_not_a_display_equation() {
        let prose = "The normalized rewards are calculated using ";
        let expression = "𝛼 = 𝛽 + 𝛾 + 𝛿 + 𝜀 + 𝜁 + 𝜂 + 𝜃 + 𝜄 + 𝜅 + 𝜆 + 𝜇 + 𝜈";
        let mut line = text_line(&format!("{prose}{expression}"), 50.0, 600.0);
        line.spans[0].text = prose.into();
        let mut math = line.spans[0].clone();
        math.text = expression.into();
        math.formula = true;
        line.spans.push(math);
        line.math_ratio = 0.65;
        assert!(!is_formula_line(&line));
        line.text = expression.into();
        line.spans.remove(0);
        assert!(is_formula_line(&line));
    }

    #[test]
    fn short_formula_introductions_remain_translatable() {
        for (text, prose, math, formula) in [
            (
                "The derivative of 𝑓(𝜃) is:",
                "Thederivativeofis:",
                "𝑓(𝜃)",
                false,
            ),
            (
                "We obtain 𝑥 = 𝑦 + 𝑧 below:",
                "Weobtainbelow:",
                "𝑥 = 𝑦 + 𝑧",
                false,
            ),
            ("We set 𝑥 := 𝑦 + 𝑧.", "Weset.", "𝑥 := 𝑦 + 𝑧", false),
            ("We have 𝑥 = 𝑦.", "Wehave.", "𝑥 = 𝑦", false),
            (
                "J(𝜃) = policy(𝑥) + reference(𝑥).",
                "J()policy()reference()",
                "𝜃 = 𝑥 + 𝑥",
                true,
            ),
        ] {
            let mut line = text_line(text, 50.0, 600.0);
            line.math_ratio = 0.42;
            line.spans[0].text = prose.into();
            let mut expression = line.spans[0].clone();
            expression.text = math.into();
            expression.formula = true;
            line.spans.push(expression);
            assert_eq!(is_formula_line(&line), formula, "{text}");
        }
    }

    #[test]
    fn numbered_list_items_start_new_paragraphs() {
        let frame = PageFrame {
            x0: 0.0,
            y0: 0.0,
            width: 600.0,
            height: 800.0,
        };
        let lines = vec![
            text_line(
                "1. First list item with enough text to look like body",
                50.0,
                700.0,
            ),
            text_line(
                "2. Second list item with enough text to look like body",
                50.0,
                688.0,
            ),
        ];
        let paragraphs = build_paragraphs(&lines, 0, frame, 10.0);
        assert_eq!(
            paragraphs.len(),
            2,
            "list items must not be merged: {paragraphs:?}"
        );
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
            rules: Vec::new(),
        }];
        protect_references(&mut pages);
        assert!(pages[0].paragraphs.iter().all(|p| p.protected));
    }

    #[test]
    fn author_year_reference_pages_stay_protected_until_a_new_heading() {
        let mut heading = make_paragraph(&[text_line("References", 50.0, 700.0)], vec![0], 0, 0);
        heading.role = Role::Reference;
        let mut entry = make_paragraph(
            &[text_line("Alice Author. A paper title. 2024.", 50.0, 680.0)],
            vec![0],
            0,
            1,
        );
        entry.role = Role::Paragraph;
        let mut continuation = make_paragraph(
            &[text_line(
                "Bob Author. Another paper title. 2023.",
                50.0,
                650.0,
            )],
            vec![0],
            0,
            2,
        );
        continuation.role = Role::Paragraph;
        let mut appendix = make_paragraph(&[text_line("A. Appendix", 50.0, 650.0)], vec![0], 0, 3);
        appendix.role = Role::Heading;
        let mut body = make_paragraph(
            &[text_line("Appendix body text.", 50.0, 620.0)],
            vec![0],
            0,
            4,
        );
        body.role = Role::Paragraph;
        let mut pages = [
            PageLayout {
                index: 0,
                x0: 0.0,
                y0: 0.0,
                width: 600.0,
                height: 800.0,
                rotation: 0,
                glyph_signature: [0; 32],
                layout_transform: hayro::kurbo::Affine::IDENTITY.as_coeffs(),
                lines: Vec::new(),
                paragraphs: vec![heading.clone(), entry, continuation.clone()],
                images: Vec::new(),
                rules: Vec::new(),
            },
            PageLayout {
                index: 1,
                x0: 0.0,
                y0: 0.0,
                width: 600.0,
                height: 800.0,
                rotation: 0,
                glyph_signature: [0; 32],
                layout_transform: hayro::kurbo::Affine::IDENTITY.as_coeffs(),
                lines: Vec::new(),
                paragraphs: vec![continuation],
                images: Vec::new(),
                rules: Vec::new(),
            },
            PageLayout {
                index: 2,
                x0: 0.0,
                y0: 0.0,
                width: 600.0,
                height: 800.0,
                rotation: 0,
                glyph_signature: [0; 32],
                layout_transform: hayro::kurbo::Affine::IDENTITY.as_coeffs(),
                lines: Vec::new(),
                paragraphs: vec![appendix, body],
                images: Vec::new(),
                rules: Vec::new(),
            },
        ];
        protect_references(&mut pages);
        assert!(pages[0].paragraphs[1].protected);
        assert!(pages[1].paragraphs[0].protected);
        assert!(!pages[2].paragraphs[0].protected);
        assert!(!pages[2].paragraphs[1].protected);
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
            &[text_line(
                "[2, 3] support this appendix statement.",
                50.0,
                280.0,
            )],
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
            rules: Vec::new(),
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

    fn glyph_run(text: &str, x: f64, y: f64, font: &str) -> Vec<GlyphBox> {
        text.chars()
            .enumerate()
            .map(|(i, ch)| {
                let x = x + i as f64 * 5.0;
                let mut glyph = text_glyph(&ch.to_string(), x, y);
                glyph.bbox = BBox::new(x, y, x + 5.0, y + 8.0);
                glyph.advance = 5.0;
                glyph.font_name = Some(font.to_string());
                glyph
            })
            .collect()
    }

    #[test]
    fn list_symbols_keep_their_source_glyphs_regardless_of_body_font() {
        for marker in ['•', '▪', '◦', '‣', '·', '-', '*'] {
            for body_font in [Some("Body"), Some("Body-Bold"), None] {
                let text = format!("{marker} Ordinary list prose");
                let mut glyphs = glyph_run(&text, 0.0, 100.0, "Body");
                let symbol_bbox = glyphs[0].bbox;
                glyphs[0].sources.push(crate::model::SourceGlyph {
                    ordinal: 0,
                    code: marker as u32,
                    x: 0.0,
                    y: 100.0,
                });
                let lines = build_lines(glyphs);
                let refs: Vec<_> = lines[0]
                    .glyphs
                    .iter()
                    .filter(|g| !g.text.trim().is_empty())
                    .collect();
                let groups = find_formula_groups(&lines[0], &refs, body_font);
                assert_eq!(groups.first(), Some(&(0, 0)), "{marker} / {body_font:?}");
                let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
                attach_inline_formulas(&mut paragraph, &lines);
                assert_eq!(paragraph.formulas.len(), 1, "{marker}");
                let object = &paragraph.formulas[0];
                assert_eq!(object.text, marker.to_string());
                assert_eq!(object.bbox, symbol_bbox);
                assert_eq!(object.sources.len(), 1);
                assert!(paragraph.prompt_text.unwrap().starts_with("<f1> "));
            }
        }
    }

    #[test]
    fn numeric_fraction_rows_stay_together_before_line_clustering() {
        let mut glyphs = glyph_run("The average degree is ", 0.0, 100.0, "Body");
        glyphs.extend(glyph_run(" and then we continue.", 130.0, 100.0, "Body"));
        glyphs.extend(glyph_run("A nearby radical ", 0.0, 84.0, "Body"));
        let mut radical = glyph_run("√", 111.0, 95.0, "CMSY8");
        radical[0].bbox = BBox::new(111.0, 88.0, 116.0, 97.0);
        glyphs.extend(radical);
        let mut numbers = Vec::new();
        for (text, x, y) in [("120", 110.0, 104.0), ("37", 112.0, 96.0)] {
            let mut row = glyph_run(text, x, y, "Body-Script");
            for (i, g) in row.iter_mut().enumerate() {
                g.origin_x = x + i as f64 * 4.0;
                g.font_size = 8.0;
                g.advance = 4.0;
                g.bbox = BBox::new(g.origin_x, y, g.origin_x + 3.5, y + 5.0);
                g.sources.push(crate::model::SourceGlyph {
                    ordinal: numbers.len() + i,
                    code: g.text.as_bytes()[0].into(),
                    x: g.origin_x,
                    y,
                });
            }
            numbers.extend(row);
        }
        glyphs.extend(numbers);
        let rule = BBox::new(109.5, 102.5, 122.5, 102.9);
        let glyphs = fractions::bind_numeric_fractions(glyphs, &[rule]);
        let group = glyphs.iter().find(|g| g.math_group).unwrap();
        assert_eq!(group.text, "120/37");
        assert_eq!(group.sources.len(), 5);
        let lines = build_lines(glyphs);
        let index = lines
            .iter()
            .position(|l| l.text.contains("average"))
            .unwrap();
        let mut paragraph = make_paragraph(&lines, vec![index], 0, 0);
        attach_inline_formulas(&mut paragraph, &lines);
        assert_eq!(paragraph.formulas.len(), 1);
        assert_eq!(paragraph.formulas[0].text, "120/37");
        assert_eq!(paragraph.formulas[0].sources.len(), 5);
        assert!(paragraph.prompt_text.unwrap().contains("<f1>"));
        assert!(lines.iter().any(|line| line.text.contains('√')));
    }

    #[test]
    fn underlined_numbers_and_table_rules_do_not_form_fractions() {
        let mut glyphs = glyph_run("2026", 50.0, 100.0, "Body");
        glyphs.extend(glyph_run("2025", 150.0, 84.0, "Body"));
        let rules = [
            BBox::new(49.0, 98.0, 71.0, 98.4),
            BBox::new(40.0, 95.0, 190.0, 95.4),
        ];
        let result = fractions::bind_numeric_fractions(glyphs, &rules);
        assert!(result.iter().all(|g| !g.math_group));
        assert_eq!(result.len(), 8);
    }

    #[test]
    fn formula_delimiters_are_bound_as_complete_pairs() {
        let mut glyphs = glyph_run("The point is (", 0.0, 100.0, "Body");
        glyphs.extend(glyph_run("x", 70.0, 100.0, "CMMI10"));
        glyphs.extend(glyph_run(") and (", 75.0, 100.0, "Body"));
        glyphs.extend(glyph_run("y", 110.0, 100.0, "CMMI10"));
        glyphs.extend(glyph_run(" is positive).", 115.0, 100.0, "Body"));
        let lines = build_lines(glyphs);
        let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
        attach_inline_formulas(&mut paragraph, &lines);
        assert_eq!(paragraph.formulas[0].text, "(x)");
        assert_eq!(paragraph.formulas[1].text, "y");
        assert!(
            paragraph
                .prompt_text
                .unwrap()
                .contains("and (<f2> is positive)")
        );
    }

    #[test]
    fn inner_products_keep_scripted_italic_operands_and_closing_delimiters() {
        // Palatino math uses the same italic face as prose, not a dedicated
        // math font. The T and v around the small z must still belong to the
        // inner product, including when TeX adds space after its comma.
        for comma_gap in [0.0, 3.5] {
            let mut glyphs = Vec::new();
            let mut x = 0.0;
            for (text, font, size) in [
                ("Its coefficient ", "Body", 10.0),
                ("F", "Palatino-Italic", 10.0),
                ("v", "Palatino-Italic", 7.0),
                ("(", "CMR10", 10.0),
                ("z", "Palatino-Italic", 10.0),
                (") = ", "CMR10", 10.0),
                ("⟨", "CMSY10", 10.0),
                ("v", "Palatino-Italic", 10.0),
                (",", "Body", 10.0),
                ("T", "Palatino-Italic", 10.0),
                ("z", "Palatino-Italic", 7.0),
                ("v", "Palatino-Italic", 10.0),
                ("⟩", "CMSY10", 10.0),
                (" is uniformly continuous.", "Body", 10.0),
            ] {
                let y = if size < 10.0 { 98.0 } else { 100.0 };
                let mut run = glyph_run(text, x, y, font);
                for (index, glyph) in run.iter_mut().enumerate() {
                    glyph.font_size = size;
                    glyph.italic = font == "Palatino-Italic";
                    glyph.sources.push(crate::model::SourceGlyph {
                        ordinal: glyphs.len() + index,
                        code: glyph.text.chars().next().unwrap() as u32,
                        x: glyph.origin_x,
                        y: glyph.origin_y,
                    });
                }
                glyphs.extend(run);
                x += text.chars().count() as f64 * 5.0;
                if text == "," {
                    x += comma_gap;
                }
            }
            let lines = build_lines(glyphs);
            let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
            attach_inline_formulas(&mut paragraph, &lines);
            assert_eq!(paragraph.formulas.len(), 1, "{:?}", paragraph.formulas);
            let formula = &paragraph.formulas[0];
            assert_eq!(formula.text, "Fv(z)=⟨v,Tzv⟩");
            assert_eq!(formula.sources.len(), formula.text.chars().count());
            assert_eq!(formula.sources.last().unwrap().code, '⟩' as u32);
            assert_eq!(
                paragraph.prompt_text.as_deref(),
                Some("Its coefficient <f1> is uniformly continuous.")
            );
        }
    }

    #[test]
    fn scripted_variables_remain_formulas_without_math_fonts_or_operators() {
        for font in ["Body-Italic", "Body"] {
            for (expression, script_y) in [("Txv", 98.0), ("Tx", 98.0), ("L2", 104.0)] {
                let prefix = "Define ";
                let mut glyphs = glyph_run(prefix, 0.0, 100.0, "Body");
                let x = prefix.len() as f64 * 5.0;
                let mut math = glyph_run(expression, x, 100.0, font);
                math.iter_mut()
                    .for_each(|g| g.italic = font.ends_with("Italic"));
                math[1].font_size = 7.0;
                math[1].origin_y = script_y;
                math[1].bbox.y0 = script_y;
                math[1].bbox.y1 = script_y + 5.0;
                let bbox = math.iter().map(|g| g.bbox).reduce(BBox::union).unwrap();
                glyphs.extend(math);
                glyphs.extend(glyph_run(
                    " by this limit.",
                    x + expression.len() as f64 * 5.0,
                    100.0,
                    "Body",
                ));
                let lines = build_lines(glyphs);
                let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
                attach_inline_formulas(&mut paragraph, &lines);
                assert_eq!(paragraph.formulas.len(), 1, "{font}: {expression}");
                assert_eq!(paragraph.formulas[0].text, expression);
                assert_eq!(paragraph.formulas[0].bbox, bbox);
                assert_eq!(
                    paragraph.prompt_text.as_deref(),
                    Some("Define <f1> by this limit.")
                );
            }
        }
    }

    #[test]
    fn emphasized_words_small_caps_and_footnote_suffixes_are_not_math() {
        for (word, small, baseline) in [
            ("Txv", &[][..], 100.0),
            ("TEXT", &[1, 2, 3][..], 100.0),
            ("word1", &[4][..], 104.0),
        ] {
            let mut glyphs = glyph_run("The label ", 0.0, 100.0, "Body");
            let mut label = glyph_run(word, 50.0, 100.0, "Body-Italic");
            for (index, glyph) in label.iter_mut().enumerate() {
                glyph.italic = true;
                if small.contains(&index) {
                    glyph.font_size = 7.0;
                    glyph.origin_y = baseline;
                    glyph.bbox.y0 = baseline;
                    glyph.bbox.y1 = baseline + 5.0;
                }
            }
            glyphs.extend(label);
            glyphs.extend(glyph_run(
                " remains ordinary prose.",
                50.0 + word.len() as f64 * 5.0,
                100.0,
                "Body",
            ));
            let lines = build_lines(glyphs);
            let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
            attach_inline_formulas(&mut paragraph, &lines);
            assert!(
                paragraph.formulas.is_empty(),
                "{word}: {:?}",
                paragraph.formulas
            );
            assert!(paragraph.prompt_text.as_deref().unwrap().contains(word));
        }
    }

    #[test]
    fn nested_math_delimiters_keep_roman_operands_inside_the_formula() {
        for expression in [
            "⟨v,Tzv⟩",
            "⟪v,Tzv⟫",
            "⟨v,(Tzv)⟩",
            "⟨v,[Tzv]⟩",
            "⟨v,{Tzv}⟩",
            "⟨v,⟦Tzv⟧⟩",
            "⟨v,⌈Tzv⌉⟩",
            "⟨v,⌊Tzv⌋⟩",
        ] {
            let source = format!("The coefficient {expression} is continuous.");
            let mut glyphs = glyph_run(&source, 0.0, 100.0, "Body");
            for glyph in &mut glyphs {
                if !glyph.text.is_ascii() {
                    glyph.font_name = Some("CMSY10".into());
                }
            }
            let lines = build_lines(glyphs);
            let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
            attach_inline_formulas(&mut paragraph, &lines);
            assert_eq!(paragraph.formulas.len(), 1, "{expression}");
            assert_eq!(paragraph.formulas[0].text, expression);
            assert_eq!(
                paragraph.prompt_text.as_deref(),
                Some("The coefficient <f1> is continuous.")
            );
        }
    }

    #[test]
    fn delimited_prose_after_a_formula_stays_translatable() {
        for prose in ["(the map)", "[see text]", "{a value}", "(see)", "[sic]"] {
            let mut glyphs = glyph_run("Consider the variable ", 0.0, 100.0, "Body");
            let x = glyphs.len() as f64 * 5.0;
            glyphs.extend(glyph_run("x", x, 100.0, "CMMI10"));
            glyphs.extend(glyph_run(&format!(" {prose}."), x + 5.0, 100.0, "Body"));
            let lines = build_lines(glyphs);
            let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
            attach_inline_formulas(&mut paragraph, &lines);
            assert_eq!(paragraph.formulas.len(), 1);
            assert_eq!(paragraph.formulas[0].text, "x");
            assert_eq!(
                paragraph.prompt_text.as_deref(),
                Some(format!("Consider the variable <f1> {prose}.").as_str())
            );
        }
    }

    #[test]
    fn roman_math_calls_keep_their_scripts_and_delimiters() {
        let mut glyphs = glyph_run("The group W(H", 0.0, 100.0, "Body");
        let mut script = glyph_run("4", 65.0, 98.0, "Body-Script");
        script[0].font_size = 7.0;
        glyphs.extend(script);
        glyphs.extend(glyph_run(
            ") = ST(30) gives a (positive value) for author(s).",
            70.0,
            100.0,
            "Body",
        ));
        let lines = build_lines(glyphs);
        let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
        attach_inline_formulas(&mut paragraph, &lines);
        assert_eq!(paragraph.formulas.len(), 1);
        assert_eq!(paragraph.formulas[0].text, "W(H4)=ST(30)");
        assert_eq!(
            paragraph.prompt_text.unwrap(),
            "The group <f1> gives a (positive value) for author(s)."
        );
        let mut glyphs = glyph_run("We set dim(Harm", 0.0, 100.0, "Body");
        let mut script = glyph_run("4", 75.0, 98.0, "Body-Script");
        script[0].font_size = 7.0;
        glyphs.extend(script);
        glyphs.extend(glyph_run(
            ") and dim (a positive value).",
            80.0,
            100.0,
            "Body",
        ));
        let lines = build_lines(glyphs);
        let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
        attach_inline_formulas(&mut paragraph, &lines);
        assert_eq!(paragraph.formulas[0].text, "dim(Harm4)");
        assert!(paragraph.prompt_text.unwrap().contains("a positive value"));
    }

    #[test]
    fn short_prose_keeps_a_wide_inline_expression_as_one_object() {
        let mut glyphs = glyph_run("We set ", 0.0, 100.0, "Body");
        let expression = "x = y + z + w + u + v + r + s + t";
        glyphs.extend(glyph_run(expression, 35.0, 100.0, "CMMI10"));
        let lines = build_lines(glyphs);
        let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
        attach_inline_formulas(&mut paragraph, &lines);
        assert_eq!(paragraph.formulas.len(), 1);
        assert_eq!(paragraph.formulas[0].text, expression.replace(' ', ""));
        assert_eq!(paragraph.prompt_text.unwrap(), "We set <f1>");
    }

    #[test]
    fn font_changes_inside_words_do_not_create_formula_placeholders() {
        let mut glyphs = glyph_run("Overview of ", 0.0, 100.0, "Body");
        let mut emphasized = glyph_run("training", 60.0, 100.0, "Body-Bold");
        emphasized.iter_mut().for_each(|g| g.bold = true);
        glyphs.extend(emphasized);
        glyphs.extend(glyph_run(" with softmax", 100.0, 100.0, "Body"));
        let lines = build_lines(glyphs);
        let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
        paragraph.finalize_style(&lines);
        attach_inline_formulas(&mut paragraph, &lines);
        assert!(paragraph.formulas.is_empty(), "{:?}", paragraph.formulas);
        let prompt = paragraph.prompt_text.unwrap();
        assert!(
            prompt.contains("<b>training") && prompt.contains("</b>"),
            "{prompt}"
        );
    }

    #[test]
    fn apostrophe_suffixes_remain_prose_across_font_changes() {
        for (stem, suffix) in [("author'", "s"), ("isn’", "t"), ("we’", "d")] {
            let prefix = format!("An ordinary explanation of {stem}");
            let mut glyphs = glyph_run(&prefix, 0.0, 100.0, "Body");
            let mut x = prefix.chars().count() as f64 * 5.0;
            glyphs.extend(glyph_run(suffix, x, 100.0, "Body-Italic"));
            x += 5.0;
            let rest = " a useful statement involving ";
            glyphs.extend(glyph_run(rest, x, 100.0, "Body"));
            x += rest.len() as f64 * 5.0;
            glyphs.extend(glyph_run("x'", x, 100.0, "CMMI10"));
            let lines = build_lines(glyphs);
            let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
            attach_inline_formulas(&mut paragraph, &lines);
            assert_eq!(paragraph.formulas.len(), 1, "{:?}", paragraph.formulas);
            assert_eq!(paragraph.formulas[0].text, "x'");
            assert!(
                paragraph
                    .prompt_text
                    .unwrap()
                    .contains(&format!("{stem}{suffix}"))
            );
        }
    }

    #[test]
    fn punctuation_only_bold_transitions_do_not_create_emphasis_spans() {
        let mut glyphs = glyph_run("Lemma 1", 0.0, 100.0, "Body-Bold");
        glyphs.iter_mut().for_each(|g| g.bold = true);
        glyphs.extend(glyph_run(" (A useful result)", 35.0, 100.0, "Body"));
        let mut period = glyph_run(".", 125.0, 100.0, "Body-Bold");
        period[0].bold = true;
        glyphs.extend(period);
        glyphs.extend(glyph_run(" Ordinary prose follows.", 130.0, 100.0, "Body"));
        let lines = build_lines(glyphs);
        let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
        paragraph.finalize_style(&lines);
        attach_inline_formulas(&mut paragraph, &lines);
        let prompt = paragraph.prompt_text.unwrap();
        assert_eq!(prompt.matches("<b>").count(), 1, "{prompt}");
        assert!(prompt.contains("result). Ordinary prose"), "{prompt}");

        let mut glyphs = glyph_run("3.1. ", 0.0, 100.0, "Body");
        let mut heading = glyph_run("A result in ", 25.0, 100.0, "Body-Bold");
        heading.iter_mut().for_each(|g| g.bold = true);
        glyphs.extend(heading);
        glyphs.extend(glyph_run("V", 85.0, 100.0, "CMMI10"));
        let mut period = glyph_run(".", 90.0, 100.0, "Body-Bold");
        period[0].bold = true;
        glyphs.extend(period);
        glyphs.extend(glyph_run(" Ordinary prose follows.", 95.0, 100.0, "Body"));
        let lines = build_lines(glyphs);
        let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
        paragraph.finalize_style(&lines);
        attach_inline_formulas(&mut paragraph, &lines);
        let prompt = paragraph.prompt_text.unwrap();
        assert_eq!(prompt.matches("<b>").count(), 1, "{prompt}");
        assert!(prompt.contains("<f1>. </b>Ordinary prose"), "{prompt}");
    }

    #[test]
    fn isolated_decimal_point_styles_do_not_split_numbers_in_model_input() {
        for italic in [false, true] {
            let mut glyphs = glyph_run("The measured accuracy is 73", 0.0, 100.0, "Body");
            let x = glyphs.len() as f64 * 5.0;
            let mut point = glyph_run(".", x, 100.0, "Other");
            point[0].italic = italic;
            point[0].bold = !italic;
            glyphs.extend(point);
            let rest = "62% for ";
            glyphs.extend(glyph_run(rest, x + 5.0, 100.0, "Body"));
            glyphs.extend(glyph_run("x", x + 45.0, 100.0, "CMMI10"));
            let lines = build_lines(glyphs);
            let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
            attach_inline_formulas(&mut paragraph, &lines);
            let prompt = paragraph.prompt_text.unwrap();
            assert!(prompt.contains("73.62%"), "{prompt}");
            assert_eq!(paragraph.formulas.len(), 1);
            assert_eq!(paragraph.formulas[0].text, "x");
        }
    }

    #[test]
    fn emphasized_articles_and_pronouns_are_not_math_placeholders() {
        for prose in ["A finite set", "a finite set", "I consider"] {
            let prefix = "Definition with a long ordinary label. ";
            let mut glyphs = glyph_run(prefix, 0.0, 100.0, "Body");
            let x = prefix.len() as f64 * 5.0;
            let mut emphasized = glyph_run(prose, x, 100.0, "Body-Italic");
            emphasized.iter_mut().for_each(|g| g.italic = true);
            glyphs.extend(emphasized);
            glyphs.extend(glyph_run(
                " A",
                x + prose.len() as f64 * 5.0,
                100.0,
                "CMMI10",
            ));
            let lines = build_lines(glyphs);
            let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
            paragraph.finalize_style(&lines);
            attach_inline_formulas(&mut paragraph, &lines);
            assert_eq!(paragraph.formulas.len(), 1, "{:?}", paragraph.formulas);
            assert_eq!(paragraph.formulas[0].text, "A");
            assert!(paragraph.prompt_text.unwrap().contains(prose));
        }
    }

    #[test]
    fn parenthesized_prose_after_a_variable_stays_translatable() {
        for adjective in ["positive", "negative", "scalar"] {
            let mut glyphs = Vec::new();
            let mut x = 0.0;
            for (text, font) in [
                ("Consider a variable ", "Body"),
                ("x", "CMMI10"),
                (" (a ", "Body"),
                (adjective, "Body-Italic"),
                (" value) and its complement.", "Body"),
            ] {
                let mut run = glyph_run(text, x, 100.0, font);
                run.iter_mut()
                    .for_each(|g| g.italic = font == "Body-Italic");
                glyphs.extend(run);
                x += text.chars().count() as f64 * 5.0;
            }
            let lines = build_lines(glyphs);
            let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
            attach_inline_formulas(&mut paragraph, &lines);
            assert_eq!(paragraph.formulas.len(), 1, "{:?}", paragraph.formulas);
            assert_eq!(paragraph.formulas[0].text, "x");
            assert!(paragraph.prompt_text.unwrap().contains("(a "));
        }
    }

    #[test]
    fn a_prose_comma_separates_a_variable_from_a_named_operator() {
        let prefix = "For a literal ";
        let mut glyphs = glyph_run(prefix, 0.0, 100.0, "Body");
        let x = prefix.len() as f64 * 5.0;
        glyphs.extend(glyph_run("l", x, 100.0, "CMMI10"));
        glyphs.extend(glyph_run(
            ", var(l) denotes its variable.",
            x + 5.0,
            100.0,
            "Body",
        ));
        let lines = build_lines(glyphs);
        let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
        attach_inline_formulas(&mut paragraph, &lines);
        assert_eq!(
            paragraph
                .formulas
                .iter()
                .map(|f| f.text.as_str())
                .collect::<Vec<_>>(),
            ["l", "var(l)"]
        );
        assert!(paragraph.prompt_text.unwrap().contains("<f1>, <f2>"));
    }

    #[test]
    fn named_operators_and_unicode_symbols_remain_bound_objects() {
        for expression in [
            "arcsin(1/4)",
            "cos(2)",
            "PGL(H)",
            "clip(x,0,1)",
            "card(C)",
            "var(l)",
            "gcd(a,b)",
            "lcm(a,b)",
            "deg(v)",
            "sgn(x)",
            "arg(z)",
            "cot(x)",
            "sec(x)",
            "csc(x)",
            "coth(x)",
            "∗",
            "⪅",
        ] {
            let source = format!("Ordinary prose before {expression} and after the expression.");
            let lines = build_lines(glyph_run(&source, 0.0, 100.0, "Body"));
            let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
            attach_inline_formulas(&mut paragraph, &lines);
            assert_eq!(
                paragraph.formulas.len(),
                1,
                "{expression}: {:?}",
                paragraph.formulas
            );
            assert_eq!(paragraph.formulas[0].text, expression);
            let prompt = paragraph.prompt_text.unwrap();
            assert!(
                prompt.contains("Ordinary prose before") && prompt.contains("and after"),
                "{prompt}"
            );
        }
        let lines = build_lines(glyph_run(
            "We rank methods by their minimum error.",
            0.0,
            100.0,
            "Body",
        ));
        let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
        attach_inline_formulas(&mut paragraph, &lines);
        assert!(paragraph.formulas.is_empty());
    }

    #[test]
    fn infix_modulo_keeps_both_operands_and_enclosing_parentheses() {
        let mut glyphs = glyph_run("The condition (", 0.0, 100.0, "Body");
        glyphs.extend(glyph_run("x", 75.0, 100.0, "CMMI10"));
        glyphs.extend(glyph_run(" mod 7 = 2) is required.", 80.0, 100.0, "Body"));
        let lines = build_lines(glyphs);
        let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
        attach_inline_formulas(&mut paragraph, &lines);
        assert_eq!(paragraph.formulas.len(), 1);
        assert_eq!(paragraph.formulas[0].text, "(xmod7=2)");
        assert_eq!(
            paragraph.prompt_text.unwrap(),
            "The condition <f1> is required."
        );
    }

    #[test]
    fn urls_in_body_font_do_not_capture_surrounding_prose() {
        let lines = build_lines(glyph_run(
            "See the catalogue (https://example.org/data/index.html). More results follow.",
            0.0,
            100.0,
            "Body",
        ));
        let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
        attach_inline_formulas(&mut paragraph, &lines);
        assert_eq!(paragraph.formulas.len(), 1);
        assert_eq!(
            paragraph.formulas[0].text,
            "https://example.org/data/index.html"
        );
        let prompt = paragraph.prompt_text.unwrap();
        assert!(
            prompt.contains("See the catalogue") && prompt.contains("More results follow"),
            "{prompt}"
        );
    }

    #[test]
    fn superscript_symbols_complete_numeric_expressions() {
        let mut glyphs = glyph_run("The bound is 1.2 ⪅ 58.8", 0.0, 100.0, "Body");
        let x = glyphs.len() as f64 * 5.0;
        let mut degree = glyph_run("◦", x, 104.0, "CMSY7").remove(0);
        degree.font_size = 7.0;
        glyphs.push(degree);
        glyphs.extend(glyph_run(" in this example.", x + 5.0, 100.0, "Body"));
        let lines = build_lines(glyphs);
        let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
        attach_inline_formulas(&mut paragraph, &lines);
        assert!(
            paragraph.formulas.iter().any(|f| f.text.contains("⪅58.8◦")),
            "{:?}",
            paragraph.formulas
        );
        assert!(!paragraph.prompt_text.unwrap().contains('⪅'));
    }

    #[test]
    fn displaced_spacing_accents_stay_with_their_inline_math_base() {
        let mut glyphs = glyph_run("where ", 0.0, 100.0, "Body");
        glyphs.extend(glyph_run("𝐴", 33.0, 100.0, "CMMI10"));
        let mut accent = glyph_run("ˆ", 34.0, 101.7, "Body").remove(0);
        accent.bbox = BBox::new(34.0, 109.0, 37.0, 111.0);
        glyphs.push(accent);
        glyphs.extend(glyph_run(
            " is computed from group scores.",
            43.0,
            100.0,
            "Body",
        ));
        let lines = build_lines(glyphs);
        assert_eq!(lines.len(), 1);
        let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
        attach_inline_formulas(&mut paragraph, &lines);
        assert_eq!(paragraph.formulas.len(), 1);
        assert!(
            paragraph.formulas[0].text.contains('𝐴') && paragraph.formulas[0].text.contains('ˆ')
        );
        assert!(paragraph.prompt_text.unwrap().starts_with("where <f1>"));
    }

    #[test]
    fn detached_macrons_move_with_their_variables() {
        for accent in ["¯", "‾"] {
            let mut glyphs = glyph_run("where ", 0.0, 100.0, "Body");
            glyphs.extend(glyph_run("R", 33.0, 100.0, "CMMI10"));
            let mut bar = glyph_run(accent, 34.0, 103.0, "Body").remove(0);
            bar.bbox = BBox::new(34.0, 109.0, 37.0, 109.4);
            glyphs.push(bar);
            glyphs.extend(glyph_run(" is the mean reward.", 43.0, 100.0, "Body"));
            let lines = build_lines(glyphs);
            assert_eq!(lines.len(), 1);
            let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
            attach_inline_formulas(&mut paragraph, &lines);
            assert_eq!(paragraph.formulas.len(), 1);
            assert!(paragraph.formulas[0].text.contains(accent));
            assert!(paragraph.formulas[0].bbox.y1 >= 109.4);
        }
    }

    #[test]
    fn front_matter_handles_left_titles_and_papers_without_abstracts() {
        for with_abstract in [false, true] {
            let mut title = text_glyph("A MULTILINE TITLE ABOUT GENERAL METHODS", 72.0, 700.0);
            let mut continuation = text_glyph("AND THEIR APPLICATIONS", 72.0, 680.0);
            title.font_size = if with_abstract { 14.0 } else { 10.0 };
            continuation.font_size = title.font_size;
            // Body-sized journal titles use tighter leading.
            if !with_abstract {
                continuation.bbox.y0 += 5.0;
                continuation.bbox.y1 += 5.0;
                continuation.origin_y += 5.0;
            }
            let mut author = text_glyph("Two Author Names", 72.0, 650.0);
            author.font_size = 8.0;
            let mut heading = text_glyph(
                if with_abstract {
                    "Abstract"
                } else {
                    "1. Introduction"
                },
                72.0,
                610.0,
            );
            heading.font_name = Some("Subset+CMCSC10".into());
            let body = "This is ordinary prose with enough text to establish the normal body font size and explain the research problem.";
            let page = ExtractedPage {
                index: 0,
                x0: 0.0,
                y0: 0.0,
                width: 612.0,
                height: 792.0,
                rotation: 0,
                layout_transform: hayro::kurbo::Affine::IDENTITY.as_coeffs(),
                glyph_signature: [0; 32],
                images: Vec::new(),
                rules: Vec::new(),
                glyphs: vec![
                    title,
                    continuation,
                    author,
                    heading,
                    text_glyph(body, 72.0, 580.0),
                ],
            };
            let page = analyze_page(page);
            let title = &page.paragraphs[0];
            assert_eq!(title.role, Role::Title);
            assert_eq!(title.align, crate::model::Align::Left);
            assert!(!title.protected);
            assert!(
                title.text.contains("METHODS AND THEIR APPLICATIONS"),
                "{}",
                title.text
            );
            assert!(
                page.paragraphs
                    .iter()
                    .find(|p| p.text == "Two Author Names")
                    .unwrap()
                    .protected
            );
            assert!(
                page.paragraphs
                    .iter()
                    .filter(|p| p.text == body || p.role == Role::Heading)
                    .all(|p| !p.protected)
            );
        }
    }

    #[test]
    fn extension_delimiters_and_code_tokens_move_as_complete_objects() {
        let mut glyphs = glyph_run("Use ", 0.0, 100.0, "Body");
        glyphs.extend(glyph_run("r", 20.0, 100.0, "CMMI10"));
        for x in [25.0, 40.0] {
            let mut delimiter =
                glyph_run(if x == 25.0 { "(" } else { ")" }, x, 110.0, "CMEX10").remove(0);
            delimiter.bbox = BBox::new(x, 98.0, x + 5.0, 111.0);
            glyphs.push(delimiter);
        }
        glyphs.extend(glyph_run("x", 30.0, 100.0, "CMBX10"));
        let mut script = glyph_run("t", 35.0, 105.0, "CMMI7").remove(0);
        script.font_size = 7.0;
        script.bbox.y1 = 111.0;
        glyphs.push(script);
        glyphs.extend(glyph_run(" with ", 45.0, 100.0, "Body"));
        let mut code = glyph_run("<bot>", 75.0, 100.0, "Typewriter");
        code.iter_mut().for_each(|g| g.mono = true);
        glyphs.extend(code);
        glyphs.extend(glyph_run(" for training.", 100.0, 100.0, "Body"));
        let lines = build_lines(glyphs);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
        attach_inline_formulas(&mut paragraph, &lines);
        let objects: Vec<_> = paragraph.formulas.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(objects, ["r(xt)", "<bot>"]);
        assert!(paragraph.prompt_text.unwrap().contains("for training."));
    }

    #[test]
    fn established_gutter_splits_aligned_column_lines() {
        let mut lines = Vec::new();
        for row in 0..3 {
            for x in [50.0, 320.0] {
                let glyphs = glyph_run(
                    "Several ordinary words establish a column",
                    x,
                    600.0 - row as f64 * 15.0,
                    "Body",
                );
                lines.extend(build_lines(glyphs));
            }
        }
        let mut aligned = glyph_run("Left column has its own text", 160.0, 520.0, "Body");
        aligned.extend(glyph_run(
            "Right column starts here too",
            320.0,
            520.0,
            "Body",
        ));
        lines.extend(build_lines(aligned));
        assign_columns(
            &mut lines,
            PageFrame {
                x0: 0.0,
                y0: 0.0,
                width: 612.0,
                height: 792.0,
            },
        );
        assert_eq!(lines.len(), 8);
        assert!(lines.iter().all(|line| line.column != Column::Full));
        assert!(
            lines
                .iter()
                .all(|line| !(line.text.contains("Left") && line.text.contains("Right")))
        );
    }

    #[test]
    fn wrapped_numbered_items_are_not_headings() {
        let page = ExtractedPage {
            index: 1,
            x0: 0.0,
            y0: 0.0,
            width: 612.0,
            height: 792.0,
            rotation: 0,
            layout_transform: hayro::kurbo::Affine::IDENTITY.as_coeffs(),
            glyph_signature: [0; 32],
            images: Vec::new(),
            rules: Vec::new(),
            glyphs: [
                glyph_run("3. Upsample the resulting re-", 320.0, 400.0, "Body"),
                glyph_run("constructed images.", 335.0, 388.0, "Body"),
            ]
            .concat(),
        };
        let layout = analyze_page(page);
        assert_eq!(layout.paragraphs.len(), 1);
        assert_eq!(layout.paragraphs[0].role, Role::Paragraph);
        assert!(layout.paragraphs[0].text.contains("reconstructed images."));
    }

    #[test]
    fn accented_names_do_not_capture_neighboring_prose() {
        let mut glyphs = glyph_run("The bound is due to ", 0.0, 100.0, "Body");
        let last_letter = glyphs.iter_mut().rev().find(|g| g.text == "o").unwrap();
        // Some PDF fonts include the following word space in the advance.
        last_letter.advance = 10.0;
        let name_x = "The bound is due to ".chars().count() as f64 * 5.0;
        let end_x = name_x + "Ra˘ıgorodski˘ı".chars().count() as f64 * 5.0;
        glyphs.extend(glyph_run("Ra˘ıgorodski˘ı", name_x, 100.0, "Body"));
        glyphs.extend(glyph_run(" and others.", end_x, 100.0, "Body"));
        let lines = build_lines(glyphs);
        let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
        attach_inline_formulas(&mut paragraph, &lines);
        assert_eq!(paragraph.formulas.len(), 1);
        assert_eq!(paragraph.formulas[0].text, "Ra˘ıgorodski˘ı");
        assert!(
            paragraph
                .prompt_text
                .unwrap()
                .starts_with("The bound is due to")
        );
    }

    #[test]
    fn appendix_heading_ends_reference_protection_without_font_cues() {
        let page = ExtractedPage {
            index: 1,
            x0: 0.0,
            y0: 0.0,
            width: 612.0,
            height: 792.0,
            rotation: 0,
            layout_transform: hayro::kurbo::Affine::IDENTITY.as_coeffs(),
            glyph_signature: [0; 32],
            images: Vec::new(),
            rules: Vec::new(),
            glyphs: [
                glyph_run("References", 50.0, 500.0, "Body"),
                glyph_run("Author and title of the work.", 50.0, 480.0, "Body"),
                glyph_run("Appendix: Additional data", 50.0, 440.0, "Body"),
                glyph_run("We provide the data here.", 50.0, 415.0, "Body"),
            ]
            .concat(),
        };
        let document = analyze_document("test".into(), vec![page], Default::default());
        let paragraphs = &document.pages[0].paragraphs;
        assert!(
            paragraphs
                .iter()
                .find(|p| p.text.starts_with("Author"))
                .unwrap()
                .protected
        );
        for prefix in ["Appendix", "We provide"] {
            assert!(
                !paragraphs
                    .iter()
                    .find(|p| p.text.starts_with(prefix))
                    .unwrap()
                    .protected
            );
        }
    }

    #[test]
    fn table_protection_stops_at_intervening_body_text() {
        let mut caption = make_paragraph(
            &[text_line("Table 1: Measurements", 50.0, 500.0)],
            vec![0],
            0,
            0,
        );
        caption.role = Role::Caption;
        let mut cell_line = text_line("Method", 50.0, 480.0);
        cell_line.bbox.x1 = 100.0;
        let cell = make_paragraph(&[cell_line], vec![0], 0, 1);
        let body = make_paragraph(
            &[text_line(
                "This ordinary body paragraph explains the measurements in the table and starts a new discussion.",
                50.0,
                400.0,
            )],
            vec![0],
            0,
            2,
        );
        let mut short_line = text_line("One vertex of this type:", 50.0, 350.0);
        short_line.bbox.x1 = 160.0;
        let short = make_paragraph(&[short_line], vec![0], 0, 3);
        let mut paragraphs = vec![caption, cell, body, short];
        protect_figure_text(&mut paragraphs, &[], &[]);
        assert!(paragraphs[1].protected);
        assert!(!paragraphs[2].protected);
        assert!(!paragraphs[3].protected);
    }

    #[test]
    fn tall_tables_are_protected_on_either_side_of_translatable_captions() {
        for above in [true, false] {
            let caption_y = if above { 230.0 } else { 730.0 };
            let mut lines = vec![text_line("Table 2. Results", 50.0, caption_y)];
            for row in 0..32 {
                for (text, x, width) in [("Method", 50.0, 85.0), ("12.5", 300.0, 30.0)] {
                    let y = 700.0 - row as f64 * 14.0 - if row >= 17 { 14.0 } else { 0.0 };
                    let mut line = text_line(text, x, y);
                    line.bbox.x1 = x + width;
                    lines.push(line);
                }
            }
            // A single-cell group heading belongs to the same table.
            lines.push(text_line("Group of models", 90.0, 462.0));
            // Dense, title-case column headers may have only ordinary word
            // spacing and should not terminate the table as prose.
            lines.push(text_line(
                "Benchmark Method Language Reasoning Programming Accuracy",
                50.0,
                715.0,
            ));
            lines.push(text_line(
                "This ordinary paragraph explains the measurements and begins a new discussion.",
                50.0,
                180.0,
            ));
            lines.push(text_line("A short following sentence.", 50.0, 160.0));
            let mut paragraphs: Vec<_> = (0..lines.len())
                .map(|i| make_paragraph(&lines, vec![i], 0, i))
                .collect();
            paragraphs[0].role = Role::Caption;
            paragraphs[65].role = Role::Heading;
            protect_figure_text(&mut paragraphs, &[], &lines);
            assert!(!paragraphs[0].protected);
            assert!(
                paragraphs[1..67].iter().all(|p| p.protected),
                "table above caption: {above}"
            );
            assert!(paragraphs[67..].iter().all(|p| !p.protected));
        }
    }

    #[test]
    fn separated_figure_labels_survive_a_partial_tabular_region() {
        let mut lines = vec![text_line("Figure 1. Diagram", 50.0, 400.0)];
        lines[0].bbox.x1 = 450.0;
        for y in [420.0, 434.0, 600.0] {
            for x in [90.0, 300.0] {
                let mut line = text_line("Label", x, y);
                line.bbox.x1 = x + 30.0;
                lines.push(line);
            }
        }
        lines.push(text_line(
            "The following paragraph explains how the entire diagram works.",
            50.0,
            370.0,
        ));
        let mut paragraphs: Vec<_> = (0..lines.len())
            .map(|i| make_paragraph(&lines, vec![i], 0, i))
            .collect();
        paragraphs[0].role = Role::Caption;
        paragraphs[1].role = Role::Heading;
        protect_figure_text(&mut paragraphs, &[], &lines);
        assert!(paragraphs[1..7].iter().all(|p| p.protected));
        assert!(!paragraphs[0].protected);
        assert!(!paragraphs[7].protected);
    }

    #[test]
    fn stacked_table_captions_do_not_claim_the_same_table() {
        for captions_above in [true, false] {
            let mut lines = Vec::new();
            for (table, top, rows) in [(1, 700.0, 3), (2, 625.0, 5)] {
                let caption_y = if captions_above {
                    top + 24.0
                } else {
                    top - rows as f64 * 14.0 - 10.0
                };
                lines.push(text_line(
                    &format!("Table {table}. Results"),
                    50.0,
                    caption_y,
                ));
                for row in 0..rows {
                    let y = top - row as f64 * 14.0;
                    for x in [50.0, 300.0] {
                        let mut cell = text_line("Value", x, y);
                        cell.bbox.x1 = x + 40.0;
                        lines.push(cell);
                    }
                }
            }
            let mut paragraphs: Vec<_> = (0..lines.len())
                .map(|i| make_paragraph(&lines, vec![i], 0, i))
                .collect();
            for p in &mut paragraphs {
                if p.text.starts_with("Table") {
                    p.role = Role::Caption;
                }
            }
            protect_figure_text(&mut paragraphs, &[], &lines);
            for p in &paragraphs {
                assert_eq!(
                    p.protected,
                    p.role != Role::Caption,
                    "captions above: {captions_above}, {}",
                    p.text
                );
            }
        }
    }

    #[test]
    fn heading_role_does_not_invent_font_weight() {
        for bold in [false, true] {
            let mut line = text_line("A regular or bold heading", 50.0, 500.0);
            line.spans[0].bold = bold;
            let lines = vec![line];
            let mut paragraph = make_paragraph(&lines, vec![0], 0, 0);
            paragraph.role = Role::Heading;
            paragraph.finalize_style(&lines);
            assert_eq!(paragraph.base_style.bold, bold);
        }
    }

    #[test]
    fn mixed_emphasis_never_becomes_a_paragraph_wide_style() {
        for italic in [false, true] {
            let plain = text_line("Plain.", 50.0, 500.0);
            let mut emphasized = text_line(
                "Most of this paragraph is emphasized but the opening words must stay plain.",
                50.0,
                487.0,
            );
            emphasized.spans[0].bold = !italic;
            emphasized.spans[0].italic = italic;
            let lines = [plain, emphasized];
            let mut paragraph = make_paragraph(&lines, vec![0, 1], 0, 0);
            paragraph.finalize_style(&lines);
            assert!(!paragraph.base_style.bold);
            assert!(!paragraph.base_style.italic);
        }
    }

    #[test]
    fn lowercase_body_continuations_are_not_split_into_large_font_headings() {
        let mut first = text_line(
            "The ordinary body sentence on this page continues and should",
            50.0,
            500.0,
        );
        first.bbox.x1 = 500.0;
        let mut second = text_line("be preserved as a single paragraph.", 50.0, 486.0);
        second.bbox.x1 = 250.0;
        let lines = [first, second];
        let frame = PageFrame {
            x0: 0.0,
            y0: 0.0,
            width: 612.0,
            height: 792.0,
        };
        let paragraphs = build_paragraphs(&lines, 3, frame, 8.0);
        assert_eq!(paragraphs.len(), 1);
        assert_eq!(paragraphs[0].line_indices, vec![0, 1]);
    }

    #[test]
    fn front_matter_titles_use_the_final_role_for_alignment() {
        for x0 in [0.0, 100.0] {
            for (left, expected) in [
                (x0 + (612.0 - 240.0) / 2.0, crate::model::Align::Center),
                (x0 + 40.0, crate::model::Align::Left),
            ] {
                // A title at body size is recognized from its position above
                // the abstract, after the initial font-based classification.
                let title = text_glyph("A TITLE USING THE SAME SIZE AS BODY TEXT", left, 640.0);
                let mut author = text_glyph("The Author Name", x0 + 186.0, 610.0);
                author.font_size = 8.0;
                let page = ExtractedPage {
                    index: 0,
                    x0,
                    y0: 0.0,
                    width: 612.0,
                    height: 792.0,
                    rotation: 0,
                    layout_transform: hayro::kurbo::Affine::IDENTITY.as_coeffs(),
                    glyph_signature: [0; 32],
                    images: Vec::new(),
                    rules: Vec::new(),
                    glyphs: vec![
                        title,
                        author,
                        text_glyph(
                            "Abstract. Ordinary text introducing the paper.",
                            x0 + 72.0,
                            570.0,
                        ),
                    ],
                };
                let layout = analyze_page(page);
                let title = layout
                    .paragraphs
                    .iter()
                    .find(|p| p.text.starts_with("A TITLE"))
                    .unwrap();
                assert_eq!(title.role, Role::Title);
                assert_eq!(title.align, expected);
            }
        }
    }

    #[test]
    fn small_caps_section_headings_keep_wide_centered_and_left_alignments() {
        for (left, width, expected) in [
            (106.0, 400.0, crate::model::Align::Center),
            (50.0, 240.0, crate::model::Align::Left),
        ] {
            let mut heading = text_glyph(
                "2. General bounds derived from the structure of the problem",
                left,
                500.0,
            );
            heading.font_name = Some("Subset+CMCSC10".into());
            heading.bbox.x1 = left + width;
            heading.advance = width;
            let page = ExtractedPage {
                index: 1,
                x0: 0.0,
                y0: 0.0,
                width: 612.0,
                height: 792.0,
                rotation: 0,
                layout_transform: hayro::kurbo::Affine::IDENTITY.as_coeffs(),
                glyph_signature: [0; 32],
                images: Vec::new(),
                rules: Vec::new(),
                glyphs: vec![
                    heading,
                    text_glyph(
                        "Ordinary body text that explains the result in this section.",
                        72.0,
                        470.0,
                    ),
                ],
            };
            let page = analyze_page(page);
            assert_eq!(page.paragraphs[0].role, Role::Heading);
            assert_eq!(page.paragraphs[0].align, expected);
        }
    }

    #[test]
    fn run_in_headings_do_not_center_the_following_prose() {
        let mut heading = text_glyph("3.1. A method.", 72.0, 500.0);
        heading.bbox.x1 = 162.0;
        heading.advance = 90.0;
        heading.bold = true;
        let mut prose = text_glyph("Ordinary prose continues on the same line.", 167.0, 500.0);
        prose.bbox.x1 = 520.0;
        prose.advance = 353.0;
        let page = ExtractedPage {
            index: 1,
            x0: 0.0,
            y0: 0.0,
            width: 612.0,
            height: 792.0,
            rotation: 0,
            layout_transform: hayro::kurbo::Affine::IDENTITY.as_coeffs(),
            glyph_signature: [0; 32],
            images: Vec::new(),
            rules: Vec::new(),
            glyphs: vec![
                text_glyph(
                    "The preceding paragraph ends with an explanation.",
                    72.0,
                    514.0,
                ),
                heading,
                prose,
            ],
        };
        let page = analyze_page(page);
        assert_eq!(page.paragraphs.len(), 2);
        assert!(page.paragraphs[1].text.starts_with("3.1."));
        assert!(page.paragraphs[1].text.contains("Ordinary prose"));
        assert_eq!(page.paragraphs[1].role, Role::Paragraph);
        assert_eq!(page.paragraphs[1].align, crate::model::Align::Left);
    }

    #[test]
    fn lettered_appendices_without_a_dot_end_reference_protection() {
        let mut heading = text_glyph("A Supplementary experiments", 72.0, 500.0);
        heading.bold = true;
        let page = ExtractedPage {
            index: 2,
            x0: 0.0,
            y0: 0.0,
            width: 612.0,
            height: 792.0,
            rotation: 0,
            layout_transform: hayro::kurbo::Affine::IDENTITY.as_coeffs(),
            glyph_signature: [0; 32],
            images: Vec::new(),
            rules: Vec::new(),
            glyphs: vec![
                text_glyph("References", 72.0, 600.0),
                text_glyph("Author. An ordinary reference entry.", 72.0, 570.0),
                heading,
                text_glyph(
                    "This appendix contains results that must be translated.",
                    72.0,
                    470.0,
                ),
            ],
        };
        let doc = analyze_document("fixture".into(), vec![page], LayoutOptions::default());
        let paragraphs = &doc.pages[0].paragraphs;
        assert!(paragraphs[1].protected);
        assert_eq!(paragraphs[2].role, Role::Heading);
        assert!(!paragraphs[2].protected);
        assert!(!paragraphs[3].protected);
    }

    #[test]
    fn unlabelled_abstract_before_the_first_section_remains_translatable() {
        let mut title = text_glyph("A PAPER WITH AN UNLABELLED ABSTRACT", 186.0, 700.0);
        title.font_size = 14.0;
        let abstract_text =
            "This opening passage explains the problem and gives the principal results. ".repeat(5);
        let mut section = text_glyph("1. Introduction", 72.0, 500.0);
        section.bold = true;
        let page = ExtractedPage {
            index: 0,
            x0: 0.0,
            y0: 0.0,
            width: 612.0,
            height: 792.0,
            rotation: 0,
            layout_transform: hayro::kurbo::Affine::IDENTITY.as_coeffs(),
            glyph_signature: [0; 32],
            images: Vec::new(),
            rules: Vec::new(),
            glyphs: vec![
                title,
                text_glyph("Author Names", 186.0, 650.0),
                text_glyph(&abstract_text, 72.0, 600.0),
                section,
            ],
        };
        let page = analyze_page(page);
        let abstract_block = page
            .paragraphs
            .iter()
            .find(|p| p.text.starts_with("This opening"))
            .unwrap();
        assert_eq!(abstract_block.role, Role::Paragraph);
        assert!(!abstract_block.protected);
        assert!(
            page.paragraphs
                .iter()
                .find(|p| p.text == "Author Names")
                .unwrap()
                .protected
        );
        assert!(
            !page
                .paragraphs
                .iter()
                .find(|p| p.text == "1. Introduction")
                .unwrap()
                .protected
        );
    }

    #[test]
    fn wrapped_titles_are_translated_as_one_semantic_block() {
        let mut first = text_line("A LONG TITLE THAT WRAPS", 180.0, 700.0);
        let mut second = text_line("ONTO THE FOLLOWING LINE", 180.0, 677.0);
        first.font_size = 14.0;
        second.font_size = 14.0;
        let lines = [first, second];
        let paragraphs = build_paragraphs(
            &lines,
            0,
            PageFrame {
                x0: 0.0,
                y0: 0.0,
                width: 612.0,
                height: 792.0,
            },
            10.0,
        );
        assert_eq!(paragraphs.len(), 1);
        assert!(paragraphs[0].text.contains("WRAPS ONTO"));
    }

    #[test]
    fn roman_math_operators_do_not_make_display_equations_translatable() {
        let mut line = text_line("log ( ) + ∫ dx(0)dx(1)q (x(0), x(1)) log", 50.0, 400.0);
        line.math_ratio = 0.31;
        line.spans[0].text = "log".into();
        let mut operator = line.spans[0].clone();
        operator.text = "+ ∫".into();
        operator.formula = true;
        line.spans.push(operator);
        assert!(is_formula_line(&line));
    }

    #[test]
    fn short_math_prose_and_operator_only_rows_are_distinguished() {
        for prose in [
            "where depends only on",
            "By substituting we get",
            "vectors of belong to",
            "to Therefore defines",
        ] {
            let mut line = text_line(&format!("{prose} α ∈ G + β"), 72.0, 500.0);
            line.math_ratio = 0.65;
            line.spans[0].text = prose.into();
            let mut math = line.spans[0].clone();
            math.text = "α ∈ G + β".into();
            math.formula = true;
            line.spans.push(math);
            assert!(!is_formula_line(&line), "{prose}");
        }
        for name in ["dim", "card", "gcd", "lcm", "deg", "sgn"] {
            let mut operator = text_line(&format!("{name}(V)"), 250.0, 400.0);
            operator.spans[0].text = format!("{name}()");
            let mut math = operator.spans[0].clone();
            math.text = "V".into();
            math.formula = true;
            operator.spans.push(math);
            assert!(is_formula_line(&operator), "{name}");
        }
    }

    #[test]
    fn large_cases_braces_protect_all_rows_but_not_neighboring_prose() {
        let mut upper = text_line("1, if x > 0", 280.0, 500.0);
        let mut brace = text_glyph("{", 265.0, 490.0);
        brace.bbox = BBox::new(265.0, 480.0, 274.0, 515.0);
        upper.glyphs.push(brace);
        let mut lines = vec![
            upper,
            text_line("0, otherwise", 280.0, 485.0),
            text_line("The expression gives the required bound.", 72.0, 460.0),
        ];
        super::lines::protect_cases(&mut lines);
        assert!(lines[0].formula && lines[1].formula);
        assert!(!lines[2].formula);
    }

    #[test]
    fn indented_list_after_a_colon_starts_its_own_paragraph() {
        let lines = [
            text_line("The construction has these properties:", 72.0, 500.0),
            text_line("(1) Every item has a different condition.", 95.0, 486.0),
        ];
        let frame = PageFrame {
            x0: 0.0,
            y0: 0.0,
            width: 612.0,
            height: 792.0,
        };
        assert_eq!(build_paragraphs(&lines, 1, frame, 10.0).len(), 2);
    }

    #[test]
    fn paragraph_spacing_and_list_outdents_are_not_merged() {
        let frame = PageFrame {
            x0: 0.0,
            y0: 0.0,
            width: 612.0,
            height: 792.0,
        };
        let lines = [
            text_line("This paragraph has several ordinary lines", 72.0, 500.0),
            text_line("that continue with the usual leading", 72.0, 487.0),
            text_line("and finish with an ordinary sentence.", 72.0, 474.0),
            text_line("A new paragraph follows after extra space", 72.0, 457.0),
            text_line("and continues with the same leading.", 72.0, 444.0),
            text_line("(1) A list item has an indented first line", 95.0, 427.0),
            text_line("and ends its own separate sentence.", 95.0, 414.0),
            text_line(
                "The text after the list returns to the margin.",
                72.0,
                401.0,
            ),
        ];
        let paragraphs = build_paragraphs(&lines, 1, frame, 10.0);
        assert_eq!(
            paragraphs
                .iter()
                .map(|p| p.line_indices.len())
                .collect::<Vec<_>>(),
            [3, 2, 2, 1]
        );
        let lines = [
            text_line("(7) The last item ends here.", 90.0, 500.0),
            text_line("The following paragraph has its own indent", 84.0, 487.0),
            text_line("and continues at the regular body margin.", 72.0, 474.0),
        ];
        let paragraphs = build_paragraphs(&lines, 1, frame, 10.0);
        assert_eq!(paragraphs.len(), 2);
        assert_eq!(paragraphs[0].line_indices, [0]);
        assert_eq!(paragraphs[1].line_indices, [1, 2]);
    }

    #[test]
    fn proof_end_marks_separate_following_unindented_prose() {
        let frame = PageFrame {
            x0: 0.0,
            y0: 0.0,
            width: 612.0,
            height: 792.0,
        };
        for mark in ['□', '∎', '■'] {
            let lines = [
                text_line(
                    &format!("This proves the desired conclusion. {mark}"),
                    72.0,
                    500.0,
                ),
                text_line(
                    "The following paragraph starts a new discussion.",
                    72.0,
                    487.0,
                ),
            ];
            assert_eq!(build_paragraphs(&lines, 1, frame, 10.0).len(), 2);
        }
    }

    #[test]
    fn wide_wrapped_headings_join_and_left_anchored_headings_stay_left() {
        let frame = PageFrame {
            x0: 0.0,
            y0: 0.0,
            width: 612.0,
            height: 792.0,
        };
        let mut first = text_line("6 A Long Heading That Continues", 72.0, 500.0);
        first.bbox.x1 = 539.0;
        first.spans[0].bold = true;
        let mut second = text_line("On the Following Line", 92.0, 486.0);
        second.spans[0].bold = true;
        let body = text_line(
            "Appendix A.1 confirms this dependence across different settings.",
            72.0,
            460.0,
        );
        let lines = [first, second, body];
        let mut paragraphs = build_paragraphs(&lines, 1, frame, 10.0);
        assert_eq!(paragraphs.len(), 2);
        paragraphs[0].role = classify_paragraph(&paragraphs[0], &lines, 1, frame, 10.0);
        assert_eq!(paragraphs[0].role, Role::Heading);
        assert_eq!(
            paragraph_alignment(&paragraphs[0], &lines, frame),
            crate::model::Align::Left
        );
        assert_eq!(
            classify_paragraph(&paragraphs[1], &lines, 1, frame, 10.0),
            Role::Paragraph
        );
    }

    #[test]
    fn multiline_centered_prose_preserves_alignment_without_centering_indented_prose() {
        let frame = PageFrame {
            x0: 0.0,
            y0: 0.0,
            width: 612.0,
            height: 792.0,
        };
        // Include an off-center column: alignment belongs to the text block,
        // rather than to the page as a whole.
        for offset in [0.0, 150.0] {
            for (bounds, expected) in [
                ([(80.0, 280.0), (100.0, 260.0)], crate::model::Align::Center),
                ([(80.0, 280.0), (80.0, 240.0)], crate::model::Align::Left),
                ([(100.0, 280.0), (80.0, 280.0)], crate::model::Align::Left),
            ] {
                let mut lines = [
                    text_line("A quoted statement with two lines", 80.0, 500.0),
                    text_line("continues on its second line.", 80.0, 487.0),
                ];
                for (line, (x0, x1)) in lines.iter_mut().zip(bounds) {
                    line.bbox.x0 = x0 + offset;
                    line.bbox.x1 = x1 + offset;
                    line.spans[0].italic = true;
                }
                let mut paragraphs = build_paragraphs(&lines, 1, frame, 10.0);
                assert_eq!(paragraphs.len(), 1);
                paragraphs[0].role = Role::Paragraph;
                assert_eq!(paragraph_alignment(&paragraphs[0], &lines, frame), expected);
            }
        }
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
            rules: Vec::new(),
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

    #[test]
    fn synthetic_small_caps_use_the_full_size_without_promoting_mixed_case_or_scripts() {
        for (text, shifted, expected) in [
            ("A.3 Full Sets Of Prompts", false, 10.0),
            ("Introduction", false, 10.0),
            ("Introduction", true, 8.0),
        ] {
            let mut glyphs = glyph_run(text, 50.0, 500.0, "Body");
            for glyph in &mut glyphs {
                if glyph.text.chars().all(char::is_lowercase) {
                    glyph.text = glyph.text.to_uppercase();
                    glyph.font_size = 8.0;
                    if shifted {
                        glyph.origin_y -= 2.0;
                        glyph.bbox.y0 -= 2.0;
                        glyph.bbox.y1 -= 2.0;
                    }
                }
            }
            let lines = build_lines(glyphs);
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].font_size, expected, "{text}, shifted: {shifted}");
        }
        let mut glyphs = glyph_run("An ordinary sentence", 50.0, 500.0, "Body");
        glyphs[0].font_size = 12.0;
        assert_eq!(build_lines(glyphs)[0].font_size, 10.0);
    }

    #[test]
    fn textual_subscripts_do_not_turn_display_fragments_into_prose() {
        for label in ["dist", "train", "test"] {
            let mut glyphs = glyph_run("≤ D", 150.0, 500.0, "Body");
            let mut subscript = glyph_run(label, 165.0, 498.0, "Body");
            for g in &mut subscript {
                g.font_size = 7.0;
            }
            glyphs.extend(subscript);
            let lines = build_lines(glyphs);
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].font_size, 10.0);
            assert!(is_formula_line(&lines[0]), "{}", lines[0].text);
        }
        let line =
            build_lines(glyph_run("We bound D and obtain ≤ 1.", 50.0, 500.0, "Body")).remove(0);
        assert!(!is_formula_line(&line));
    }

    #[test]
    fn interval_endpoints_and_italic_set_names_are_complete_inline_objects() {
        let mut glyphs = glyph_run("The interval [a,b] in A is closed.", 50.0, 500.0, "Body");
        for (i, g) in glyphs.iter_mut().enumerate() {
            if matches!(i, 14 | 16) || g.text == "A" {
                g.italic = true;
                g.font_name = Some("Body-Italic".into());
            }
        }
        let lines = build_lines(glyphs);
        let mut paragraph = make_paragraph(&lines, vec![0], 1, 0);
        attach_inline_formulas(&mut paragraph, &lines);
        assert_eq!(
            paragraph
                .formulas
                .iter()
                .map(|f| f.text.as_str())
                .collect::<Vec<_>>(),
            ["[a,b]", "A"]
        );
        assert_eq!(
            paragraph.prompt_text.as_deref(),
            Some("The interval <f1> in <f2> is closed.")
        );
    }

    #[test]
    fn mathematical_calls_keep_textual_parameters_and_italic_correction() {
        let expression = "P(r,a|prompt,question)";
        let mut formula = glyph_run(expression, 150.0, 500.0, "Body");
        formula[0].font_name = Some("CMMI10".into());
        // TeX inserts an italic correction before the opening parenthesis.
        for g in &mut formula[1..] {
            g.origin_x += 1.4;
            g.bbox.x0 += 1.4;
            g.bbox.x1 += 1.4;
        }
        let line = build_lines(formula.clone()).remove(0);
        assert!(is_formula_line(&line));
        let mut glyphs = glyph_run("We maximize", 80.0, 500.0, "Body");
        glyphs.extend(formula);
        glyphs.extend(glyph_run(" over the parameters.", 261.4, 500.0, "Body"));
        let lines = build_lines(glyphs);
        assert!(!is_formula_line(&lines[0]));
        let mut paragraph = make_paragraph(&lines, vec![0], 1, 0);
        attach_inline_formulas(&mut paragraph, &lines);
        assert_eq!(paragraph.formulas.len(), 1);
        assert_eq!(paragraph.formulas[0].text, expression);
        assert!(
            paragraph
                .prompt_text
                .unwrap()
                .contains("over the parameters")
        );
    }

    #[test]
    fn equation_numbers_bind_qualifiers_without_protecting_adjacent_prose() {
        let mut lines = vec![
            text_line("We obtain the following bound.", 50.0, 520.0),
            text_line("1 ≤ f(x) ≤ 2 for almost every x", 180.0, 500.0),
            text_line("(5.12)", 510.0, 500.0),
            text_line("The exceptional set can depend on x.", 50.0, 480.0),
        ];
        lines[2].bbox.x1 = 540.0;
        lines::protect_numbered_equations(&mut lines, FRAME);
        assert!(lines[1].formula);
        assert!(!lines[0].formula && !lines[3].formula);
        lines[1].formula = false;
        lines[2].bbox.x0 = 50.0;
        lines[2].bbox.x1 = 80.0;
        lines::protect_numbered_equations(&mut lines, FRAME);
        assert!(
            !lines[1].formula,
            "a numbered list is not an equation number"
        );
    }

    #[test]
    fn ruled_prose_tables_preserve_cells_and_keep_captions_separate_on_either_side() {
        for above in [true, false] {
            let caption_y = if above { 620.0 } else { 480.0 };
            let mut lines = vec![text_line(
                "Table 2: Example reasoning paths.",
                50.0,
                caption_y,
            )];
            for y in [580.0, 565.0, 550.0, 535.0, 520.0] {
                let mut cell = text_line(
                    "This is a long sentence inside a table cell, with ordinary prose words.",
                    50.0,
                    y,
                );
                cell.font_size = 9.0;
                cell.glyphs[0].font_size = 9.0;
                lines.push(cell);
            }
            lines.push(text_line(
                "Ordinary body text following the table remains translatable.",
                50.0,
                440.0,
            ));
            let rules = [
                BBox::new(48.0, 600.0, 295.0, 600.3),
                BBox::new(48.0, 500.0, 295.0, 500.3),
            ];
            tables::protect_ruled_tables(&mut lines, &rules);
            let paragraphs = build_paragraphs(&lines, 1, FRAME, 10.0);
            let caption = paragraphs
                .iter()
                .find(|p| p.text.starts_with("Table"))
                .unwrap();
            assert!(!caption.protected);
            assert!(!caption.text.contains("inside a table"));
            assert!(
                paragraphs
                    .iter()
                    .filter(|p| p.text.contains("inside a table"))
                    .all(|p| p.protected)
            );
            assert!(
                !paragraphs
                    .iter()
                    .find(|p| p.text.starts_with("Ordinary"))
                    .unwrap()
                    .protected
            );
        }
    }

    #[test]
    fn adjacent_captions_split_without_a_page_wide_column_gutter() {
        let mut glyphs = glyph_run("Table 3: Scores", 50.0, 500.0, "Body");
        glyphs.extend(glyph_run("Figure 5: Accuracy", 137.0, 500.0, "Body"));
        let mut lines = build_lines(glyphs);
        assert_eq!(lines.len(), 1);
        lines::split_caption_lines(&mut lines);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "Table 3: Scores");
        assert_eq!(lines[1].text, "Figure 5: Accuracy");
    }

    #[test]
    fn an_unruled_numeric_row_does_not_absorb_its_caption() {
        let mut glyphs = glyph_run("Unweighted sum (majority vote)", 50.0, 500.0, "Body");
        glyphs.extend(glyph_run("74.4 ± 0.1", 215.0, 500.0, "Body"));
        glyphs.extend(glyph_run(
            "Table 1: Accuracy comparison.",
            50.0,
            486.0,
            "Body",
        ));
        let lines = build_lines(glyphs);
        let paragraphs = build_paragraphs(&lines, 1, FRAME, 10.0);
        assert!(
            paragraphs
                .iter()
                .any(|p| p.text == "Table 1: Accuracy comparison."),
            "{paragraphs:?}"
        );
    }
}
