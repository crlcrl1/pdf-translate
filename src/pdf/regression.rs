//! End-to-end PDF regressions, using original, portable fixture fonts.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use hayro::hayro_syntax::Pdf;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};

use crate::layout::{LayoutOptions, analyze_document};
use crate::model::{BBox, LayoutDocument};
use crate::pages::PageSelection;
use crate::typeset::{TypesetDocument, TypesetOptions, TypesetStats, typeset_document};

use super::font::FontSource;
use super::writer::{ComposeMode, ComposeOptions, write_translated_pdf};

const BODY: &str =
    "BT /F1 14 Tf 72 520 Td (This regular paragraph contains enough words for translation.) Tj ET";

struct Fixture {
    dir: PathBuf,
    doc: Document,
    page: ObjectId,
    input: PathBuf,
}

impl Fixture {
    fn new(content: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pdf-regression-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("input.pdf");
        let mut doc = Document::with_version("1.1");
        let pages = doc.new_object_id();
        let font = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        });
        let stream = doc.add_object(Stream::new(dictionary! {}, content.as_bytes().to_vec()));
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => stream,
            "Resources" => dictionary! {
                "Font" => dictionary! {"F1" => font},
                "ExtGState" => dictionary! {
                    "Half" => dictionary! {"Type" => "ExtGState", "ca" => 0.5, "CA" => 0.5},
                    "Zero" => dictionary! {"Type" => "ExtGState", "ca" => 0, "CA" => 0},
                },
            },
        });
        doc.objects.insert(
            pages,
            dictionary! {
                "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
            }
            .into(),
        );
        let root = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
        doc.trailer.set("Root", root);
        Self {
            dir,
            doc,
            page,
            input,
        }
    }

    fn resources(&mut self) -> &mut Dictionary {
        self.doc
            .get_dictionary_mut(self.page)
            .unwrap()
            .get_mut(b"Resources")
            .unwrap()
            .as_dict_mut()
            .unwrap()
    }

    fn save(&mut self) {
        self.doc.save(&self.input).unwrap();
    }

    fn add_type1_font(&mut self, resource: &str, base: &str) {
        let font = self.doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => Object::Name(base.as_bytes().to_vec()),
        });
        self.resources()
            .get_mut(b"Font")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set(resource, font);
    }

    fn layout(&mut self) -> LayoutDocument {
        self.save();
        analyze_document(
            self.input.display().to_string(),
            extract_pages(&self.input, &PageSelection::all()).unwrap(),
            LayoutOptions::default(),
        )
    }

    fn compose(&mut self, mode: ComposeMode, extension: &str, translation: &str) -> PathBuf {
        let mut layout = self.layout();
        for paragraph in &mut layout.pages[0].paragraphs {
            if !paragraph.protected {
                paragraph.translation = Some(translation.to_string());
            }
        }
        let (typeset, stats) = typeset(&layout, extension, 0.72);
        assert!(
            !typeset.paragraphs.is_empty(),
            "no translated paragraph: {stats:?}"
        );
        assert_eq!(stats.overflow, 0);
        self.write_layout(
            &layout,
            &typeset,
            mode,
            &format!("{mode:?}-{extension}.pdf"),
        )
    }

    fn write_layout(
        &self,
        layout: &LayoutDocument,
        typeset: &TypesetDocument,
        mode: ComposeMode,
        name: &str,
    ) -> PathBuf {
        let output = self.dir.join(name);
        write_translated_pdf(
            self.doc.clone(),
            &output,
            &layout.pages,
            typeset,
            &ComposeOptions {
                mode,
                ..Default::default()
            },
        )
        .unwrap();
        output
    }

    fn compose_first_word(&mut self, chars: usize) -> PathBuf {
        let mut layout = self.layout();
        let extracted = extract_pages(&self.input, &PageSelection::all()).unwrap();
        let bbox = extracted[0].glyphs[..chars]
            .iter()
            .map(|g| g.bbox)
            .reduce(BBox::union)
            .unwrap();
        let paragraph = layout.pages[0]
            .paragraphs
            .iter_mut()
            .find(|p| !p.protected)
            .unwrap();
        paragraph.bbox = bbox;
        paragraph.translation = Some("译".to_string());
        let index = paragraph.line_indices[0];
        let line = &mut layout.pages[0].lines[index];
        line.bbox = bbox;
        line.glyphs.truncate(chars);
        let (set, stats) = typeset(&layout, "ttf", 0.72);
        assert_eq!(stats.overflow, 0);
        self.write_layout(&layout, &set, ComposeMode::Clean, "partial.pdf")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn typeset(
    layout: &LayoutDocument,
    extension: &str,
    min_scale: f64,
) -> (TypesetDocument, TypesetStats) {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/test.{extension}"));
    let font = FontSource::load(path, None).unwrap();
    typeset_document(
        layout,
        &font,
        None,
        &TypesetOptions {
            min_scale,
            ..Default::default()
        },
    )
    .unwrap()
}

fn extract_pages(
    path: &Path,
    selection: &PageSelection,
) -> anyhow::Result<Vec<crate::model::ExtractedPage>> {
    let input = super::preflight::load_pdf(path)?;
    super::extract_pdf(input.data, &input.document, selection)
}

fn visible_text(path: &Path) -> String {
    extract_pages(path, &PageSelection::all())
        .unwrap()
        .into_iter()
        .flat_map(|page| page.glyphs)
        .map(|glyph| glyph.text)
        .collect()
}

fn render(path: &Path) -> hayro::vello_cpu::Pixmap {
    let pdf = Pdf::new(std::fs::read(path).unwrap()).unwrap();
    hayro::render(
        &pdf.pages()[0],
        &hayro::RenderCache::new(),
        &Default::default(),
        &Default::default(),
        &Default::default(),
    )
}

fn region(
    pixmap: &hayro::vello_cpu::Pixmap,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> Vec<u8> {
    (y..y + height)
        .flat_map(|row| {
            let start = (row * usize::from(pixmap.width()) + x) * 4;
            pixmap.data_as_u8_slice()[start..start + width * 4]
                .iter()
                .copied()
        })
        .collect()
}

fn object_contains(object: &Object, needle: &[u8]) -> bool {
    match object {
        Object::String(bytes, _) | Object::Name(bytes) => {
            bytes.windows(needle.len()).any(|part| part == needle)
        }
        Object::Array(values) => values.iter().any(|value| object_contains(value, needle)),
        Object::Dictionary(dict) => dict.iter().any(|(_, value)| object_contains(value, needle)),
        Object::Stream(stream) => {
            stream
                .dict
                .iter()
                .any(|(_, value)| object_contains(value, needle))
                || stream
                    .get_plain_content()
                    .unwrap()
                    .windows(needle.len())
                    .any(|part| part == needle)
        }
        _ => false,
    }
}

fn assert_absent_from_all_objects(path: &Path, needle: &[u8]) {
    let doc = Document::load(path).unwrap();
    assert!(
        !doc.objects
            .values()
            .any(|object| object_contains(object, needle)),
        "source bytes or rewrite probes remain: {}",
        String::from_utf8_lossy(needle)
    );
}

#[test]
fn clean_removes_source_streams_and_never_writes_page_snapshots() {
    let mut fixture = Fixture::new(BODY);
    let output = fixture.compose(ComposeMode::Clean, "ttf", "Translation.");
    for needle in [
        b"This regular paragraph".as_slice(),
        b"OrigPage",
        b"PdfTranslateInternalTrace",
        b"CursorProbe",
    ] {
        assert_absent_from_all_objects(&output, needle);
    }
    let mut doc = Document::load(&output).unwrap();
    let objects = doc.objects.clone();
    for id in doc.prune_objects() {
        assert!(
            objects[&id].as_stream().is_ok_and(|stream| stream
                .dict
                .get(b"Type")
                .and_then(Object::as_name)
                .is_ok_and(|name| name == b"XRef")),
            "detached source object {id:?}"
        );
    }
}

#[test]
fn partial_tj_removal_keeps_advances_rise_scaling_and_following_text() {
    let mut fixture = Fixture::new(
        "BT /F1 14 Tf 1.2 Tc 4 Tw 80 Tz 5 Ts 72 520 Td [(SECRET) -120 ( KEEP retained words for translation.)] TJ ( NEXT) Tj ET",
    );
    let output = fixture.compose_first_word(6);
    let text = visible_text(&output);
    assert!(!text.contains("SECRET"));
    assert!(
        text.contains("KEEP retained words for translation. NEXT"),
        "{text}"
    );
    assert_eq!(
        region(&render(&fixture.input), 150, 240, 330, 50),
        region(&render(&output), 150, 240, 330, 50)
    );
    assert_absent_from_all_objects(&output, b"SECRET");
}

#[test]
fn partial_cid_text_keeps_distinct_two_byte_codes() {
    for extension in ["ttf", "otf"] {
        let mut fixture = Fixture::new(BODY);
        let source = fixture.compose(ComposeMode::Clean, extension, "SECRET KEEP retained words.");
        fixture.doc = Document::load(source).unwrap();
        fixture.page = fixture.doc.get_pages()[&1];
        let output = fixture.compose_first_word(6);
        let text = visible_text(&output);
        assert!(!text.contains("SECRET"));
        assert!(text.contains("KEEP retained words."), "{extension}: {text}");
        assert_eq!(
            region(&render(&fixture.input), 150, 240, 250, 50),
            region(&render(&output), 150, 240, 250, 50)
        );
    }
}

#[test]
fn reused_forms_are_rewritten_per_invocation() {
    let mut fixture = Fixture::new("/Shared Do q 1 0 0 1 0 -490 cm /Shared Do Q");
    let original_form = fixture.doc.add_object(Stream::new(dictionary! {
        "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(),0.into(),612.into(),792.into()],
    }, BODY.as_bytes().to_vec()));
    fixture
        .resources()
        .set("XObject", dictionary! {"Shared" => original_form});
    let output = fixture.compose(ComposeMode::Clean, "ttf", "Translation.");
    let text = visible_text(&output);
    assert_eq!(text.matches("This regular paragraph").count(), 1, "{text}");
    assert_eq!(text.matches("Translation.").count(), 1);
    assert_eq!(
        region(&render(&fixture.input), 60, 744, 420, 40),
        region(&render(&output), 60, 744, 420, 40)
    );
    let doc = Document::load(&output).unwrap();
    assert!(!doc.objects.contains_key(&original_form));
    assert_absent_from_all_objects(&output, b"CursorProbe");
}

#[test]
fn actualtext_is_removed_from_changed_scopes_and_unused_properties() {
    for nested in [false, true] {
        let mut fixture = Fixture::new(BODY);
        let property = fixture.doc.add_object(dictionary! {
            "ActualText" => Object::string_literal("Semantic secret paragraph must disappear."),
            "Alt" => Object::string_literal("Alternate secret text must disappear."),
        });
        let resources = fixture.resources();
        resources.set("Properties", dictionary! {"Semantic" => property});
        let inner = if nested {
            let form = fixture.doc.add_object(Stream::new(dictionary! {
                "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(),0.into(),612.into(),792.into()],
            }, BODY.as_bytes().to_vec()));
            fixture
                .resources()
                .set("XObject", dictionary! {"Shared" => form});
            "/Shared Do"
        } else {
            BODY
        };
        let content = fixture.doc.add_object(Stream::new(
            dictionary! {},
            format!("/Span /Semantic BDC {inner} EMC").into_bytes(),
        ));
        fixture
            .doc
            .get_dictionary_mut(fixture.page)
            .unwrap()
            .set("Contents", content);
        let output = fixture.compose(ComposeMode::Clean, "ttf", "Translation.");
        assert_absent_from_all_objects(&output, b"Semantic secret");
        assert_absent_from_all_objects(&output, b"Alternate secret");
        assert_absent_from_all_objects(&output, b"This regular paragraph");
    }
}

#[test]
fn list_bullets_keep_the_same_original_shape_across_mixed_font_items() {
    let mut fixture = Fixture::new(
        "BT /F1 12 Tf 0 0 1 rg 72 700 Td (\\267) Tj ET
         BT /Bold 12 Tf 0 g 90 700 Td (Short label. ) Tj /F1 12 Tf
         (Ordinary prose dominates this first list item and continues) Tj ET
         BT /F1 12 Tf 90 686 Td (on a second indented line.) Tj ET
         BT /F1 12 Tf 0 0 1 rg 72 660 Td (\\267) Tj ET
         BT /Bold 12 Tf 0 g 90 660 Td (A much longer emphasized label dominates this item. ) Tj
         /F1 12 Tf (More prose) Tj ET
         BT /F1 12 Tf 90 646 Td (wraps with a hanging indent.) Tj ET",
    );
    fixture.add_type1_font("Bold", "Helvetica-Bold");
    let mut layout = fixture.layout();
    let items: Vec<_> = layout.pages[0]
        .paragraphs
        .iter_mut()
        .filter(|p| p.text.starts_with('•'))
        .collect();
    assert_eq!(items.len(), 2);
    for (index, item) in items.into_iter().enumerate() {
        assert!(!item.protected);
        assert_eq!(item.formulas.len(), 1);
        assert_eq!(item.formulas[0].text, "•");
        let gap = if index == 0 { " " } else { "" };
        item.translation = Some(format!("<f1>{gap}<b>译译</b>{}", "译".repeat(70)));
    }
    // The portable replacement font deliberately has no bullet glyph. Both
    // bullets must still render from the original PDF, with their source color.
    let (set, stats) = typeset(&layout, "ttf", 0.72);
    assert_eq!(stats.overflow, 0, "{stats:?}");
    assert_eq!(set.paragraphs.len(), 2);
    let original = render(&fixture.input);
    let blue_bounds = |image: &hayro::vello_cpu::Pixmap, bbox: BBox| {
        let pixels = image.data_as_u8_slice().as_chunks::<4>().0;
        let width = image.width() as usize;
        pixels
            .iter()
            .enumerate()
            .filter_map(|(i, rgba)| {
                let x = (i % width) as f64;
                let y = 792.0 - (i / width) as f64;
                (rgba[2] > 100
                    && rgba[0] < 80
                    && rgba[1] < 80
                    && x >= bbox.x0 - 1.0
                    && x <= bbox.x1 + 1.0
                    && y >= bbox.y0 - 1.0
                    && y <= bbox.y1 + 1.0)
                    .then_some(BBox::new(x, y, x + 1.0, y + 1.0))
            })
            .reduce(BBox::union)
            .expect("the original bullet disappeared")
    };
    for mode in [ComposeMode::Clean, ComposeMode::Overlay] {
        let output = fixture.write_layout(&layout, &set, mode, &format!("bullets-{mode:?}.pdf"));
        let pixels = render(&output);
        for (id, item) in &set.paragraphs {
            assert_eq!(item.scale, 1.0);
            assert!(item.lines.len() >= 2);
            let source_item = layout.pages[0]
                .paragraphs
                .iter()
                .find(|p| p.id == *id)
                .unwrap();
            let body_x = source_item
                .lines(&layout.pages[0].lines)
                .next()
                .unwrap()
                .glyphs
                .iter()
                .find(|g| g.text == "S" || g.text == "A")
                .unwrap()
                .bbox
                .x0;
            for line in &item.lines {
                assert!((line.segments[0].x - body_x).abs() < 0.001);
            }
            let bullet = &item.lines[0].formulas[0];
            let source = bullet.source_bbox;
            let placed = BBox::new(
                bullet.x,
                bullet.baseline + source.y0 - bullet.source_baseline,
                bullet.x + source.width(),
                bullet.baseline + source.y1 - bullet.source_baseline,
            );
            let before = blue_bounds(&original, source);
            let after = blue_bounds(&pixels, placed);
            assert!((before.width() - after.width()).abs() <= 1.0);
            assert!((before.height() - after.height()).abs() <= 1.0);
        }
    }
}

#[test]
fn inline_formula_is_an_isolated_vector_drawing() {
    let mut fixture = Fixture::new(
        "BT /F1 14 Tf 72 520 Td (Here is x as an inline formula for translation.) Tj ET",
    );
    let mut layout = fixture.layout();
    let extracted = extract_pages(&fixture.input, &PageSelection::all()).unwrap();
    let glyph = extracted[0].glyphs.iter().find(|g| g.text == "x").unwrap();
    let paragraph = layout.pages[0]
        .paragraphs
        .iter_mut()
        .find(|p| !p.protected)
        .unwrap();
    paragraph.formulas = vec![crate::model::FormulaRef {
        id: "f1".into(),
        sources: glyph.sources.to_vec(),
        text: "x".into(),
        bbox: glyph.bbox,
        baseline: glyph.origin_y,
    }];
    paragraph.translation = Some("译<f1>译".into());
    let (set, stats) = typeset(&layout, "ttf", 0.72);
    assert_eq!(stats.overflow, 0);
    let output = fixture.write_layout(&layout, &set, ComposeMode::Clean, "formula.pdf");
    assert_absent_from_all_objects(&output, b"Here is x");
    assert_absent_from_all_objects(&output, b"OrigPage");
    let doc = Document::load(&output).unwrap();
    let forms: Vec<_> = doc
        .objects
        .values()
        .filter_map(|o| o.as_stream().ok())
        .filter(|s| {
            s.dict
                .get(b"Subtype")
                .and_then(Object::as_name)
                .is_ok_and(|n| n == b"Form")
        })
        .collect();
    assert_eq!(forms.len(), 1);
    let operations = lopdf::content::Content::decode(&forms[0].get_plain_content().unwrap())
        .unwrap()
        .operations;
    assert!(operations.iter().any(|op| op.operator == "f"));
    assert!(
        operations
            .iter()
            .all(|op| !matches!(op.operator.as_str(), "BT" | "Tj" | "TJ" | "Do"))
    );
    assert_eq!(visible_text(&output), "译译");

    let overlay = fixture.write_layout(&layout, &set, ComposeMode::Overlay, "formula-overlay.pdf");
    assert!(visible_text(&overlay).contains("Here is x as an inline formula"));
    let doc = Document::load(&overlay).unwrap();
    let original = fixture.doc.get_page_contents(fixture.page);
    assert!(
        original
            .iter()
            .all(|id| doc.get_page_contents(fixture.page).contains(id))
    );
    assert!(doc.objects.values().any(|o| o.as_stream().is_ok_and(|s| {
        s.dict
            .get(b"Subtype")
            .and_then(Object::as_name)
            .is_ok_and(|name| name == b"Form")
    })));
}

#[test]
fn vector_and_stencil_rules_move_with_formulas_and_leave_no_source_rule() {
    for representation in ["stroke", "inline", "xobject"] {
        let mut fixture = Fixture::new(
            "BT /F1 14 Tf 72 520 Td (Here is x as an inline formula for translation.) Tj ET",
        );
        fixture.save();
        let source = extract_pages(&fixture.input, &PageSelection::all()).unwrap();
        let glyph = source[0].glyphs.iter().find(|g| g.text == "x").unwrap();
        let bbox = if representation == "stroke" {
            glyph.bbox.inflate(0.0, 2.0)
        } else {
            glyph.bbox
        };
        let y = if representation == "stroke" {
            glyph.bbox.y1 + 1.0
        } else {
            glyph.bbox.y1 - 0.5
        };
        let content = if representation == "stroke" {
            format!("q 1 0 0 RG 1 w {} {y} m {} {y} l S Q", bbox.x0, bbox.x1).into_bytes()
        } else {
            let mut bytes =
                format!("q 1 0 0 rg {} 0 0 1 {} {y} cm ", bbox.width(), bbox.x0).into_bytes();
            if representation == "inline" {
                bytes.extend_from_slice(b"BI /W 1 /H 1 /IM true /BPC 1 ID \0 EI ");
            } else {
                let image = fixture.doc.add_object(Stream::new(
                    dictionary! {
                        "Type" => "XObject", "Subtype" => "Image", "Width" => 1, "Height" => 1,
                        "ImageMask" => true, "BitsPerComponent" => 1,
                    },
                    vec![0],
                ));
                fixture
                    .resources()
                    .set("XObject", dictionary! {"Rule" => image});
                bytes.extend_from_slice(b"/Rule Do ");
            }
            bytes.extend_from_slice(b"Q");
            bytes
        };
        let rule = fixture.doc.add_object(Stream::new(dictionary! {}, content));
        let original = fixture.doc.get_page_contents(fixture.page)[0];
        fixture.doc.get_dictionary_mut(fixture.page).unwrap().set(
            "Contents",
            vec![Object::Reference(original), Object::Reference(rule)],
        );
        let mut layout = fixture.layout();
        let images = layout.pages[0].images.clone();
        let paragraph = layout.pages[0]
            .paragraphs
            .iter_mut()
            .find(|p| !p.protected)
            .unwrap();
        paragraph.formulas = vec![crate::model::FormulaRef {
            id: "f1".into(),
            sources: glyph.sources.to_vec(),
            text: "x".into(),
            bbox,
            baseline: glyph.origin_y,
        }];
        paragraph.translation = Some("译<f1>译".into());
        crate::layout::include_formula_rules(paragraph, &images, &[]);
        if representation != "stroke" {
            assert!(paragraph.formulas[0].bbox.y1 > glyph.bbox.y1);
        }
        let (set, stats) = typeset(&layout, "ttf", 0.72);
        assert_eq!(stats.overflow, 0);
        let out = fixture.write_layout(&layout, &set, ComposeMode::Clean, "formula-rule.pdf");
        let image = render(&out);
        let red = |p: &[u8; 4]| p[0] > p[1].saturating_add(40) && p[1] < 200;
        assert!(
            image.data_as_u8_slice().as_chunks::<4>().0.iter().any(red),
            "the {representation} formula rule disappeared"
        );
        let old = region(
            &image,
            bbox.x0.floor() as usize,
            (792.0 - y - 2.0).floor() as usize,
            bbox.width().ceil() as usize + 1,
            5,
        );
        assert!(
            !old.as_chunks::<4>().0.iter().any(red),
            "the original {representation} rule was left behind"
        );
    }
}

#[test]
fn stacked_inline_fractions_move_long_scripts_and_overhanging_rules_together() {
    let mut fixture = Fixture::new(
        "BT /F1 14 Tf 72 520 Td (The normalized reward is) Tj ET
         BT /F2 14 Tf 245 520 Td (a) Tj /F1 14 Tf 8 0 Td (=) Tj ET
         BT /F2 8 Tf 265 524 Td (a) Tj /F1 8 Tf 5 0 Td (-mean\\(population\\)) Tj ET
         BT /F1 8 Tf 285 513 Td (std\\(sample\\)) Tj ET
         BT /F1 14 Tf 342 520 Td (after centering the scores.) Tj ET
         q 1 0 0 RG 0.6 w 263.5 521 m 336 521 l S Q",
    );
    fixture.add_type1_font("F2", "Symbol");
    let mut layout = fixture.layout();
    let paragraphs = &mut layout.pages[0].paragraphs;
    assert_eq!(
        paragraphs.len(),
        1,
        "detached numerator or denominator: {paragraphs:#?}"
    );
    let paragraph = &mut paragraphs[0];
    assert!(!paragraph.protected);
    let fraction = paragraph
        .formulas
        .iter()
        .find(|f| f.sources.iter().any(|source| source.ordinal == 26))
        .unwrap_or_else(|| panic!("fraction was split: {paragraph:#?}"));
    for ordinal in 26..55 {
        assert!(
            fraction
                .sources
                .iter()
                .any(|source| source.ordinal == ordinal),
            "detached fraction glyph {ordinal}: {fraction:?}"
        );
    }
    assert!(
        fraction.bbox.x0 <= 263.5 && fraction.bbox.x1 >= 336.0,
        "{fraction:?}"
    );
    let fraction_id = fraction.id.clone();
    paragraph.translation = Some(
        paragraph
            .formulas
            .iter()
            .fold("译".to_owned(), |s, f| format!("{s}<{}>译", f.id)),
    );
    let (set, stats) = typeset(&layout, "ttf", 0.72);
    assert_eq!(stats.overflow, 0);
    let placed = set
        .paragraphs
        .values()
        .next()
        .unwrap()
        .lines
        .iter()
        .flat_map(|line| &line.formulas)
        .find(|f| f.id == fraction_id)
        .unwrap();
    // The original 72.5 pt red fraction bar must remain complete after the
    // formula has moved, including its overhang beyond the numerator ink.
    for mode in [ComposeMode::Clean, ComposeMode::Overlay] {
        let out = fixture.write_layout(&layout, &set, mode, &format!("fraction-{mode:?}.pdf"));
        let image = render(&out);
        let red = |p: &[u8; 4]| p[0] > p[1].saturating_add(30) && p[1] < 230;
        let mut xs = Vec::new();
        for (i, pixel) in image
            .data_as_u8_slice()
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
        {
            if red(pixel) {
                xs.push(i % 612);
            }
        }
        assert!(!xs.is_empty(), "fraction bar disappeared in {mode:?}");
        let width = xs.iter().max().unwrap() - xs.iter().min().unwrap() + 1;
        assert!(
            (width as f64 - 72.5 * placed.scale).abs() <= 2.0,
            "clipped fraction bar: {width}"
        );
        assert!(
            !region(&image, 262, 269, 76, 4)
                .as_chunks::<4>()
                .0
                .iter()
                .any(red),
            "source bar left behind in {mode:?}"
        );
    }
}

#[test]
fn numbered_serif_algorithm_preserves_body_and_translates_caption() {
    let mut fixture = Fixture::new(
        "BT /F1 11 Tf 72 650 Td (Algorithm 2 Sampling procedure) Tj ET
         BT /F1 11 Tf 72 635 Td (Input: a collection of samples) Tj ET
         BT /F1 11 Tf 72 620 Td (1: Initialize scores and counters) Tj ET
         BT /F1 11 Tf 72 605 Td (2: Update the selected samples) Tj ET
         BT /F1 11 Tf 72 590 Td (Output: the normalized scores) Tj ET
         BT /F1 11 Tf 72 555 Td (The following paragraph explains the procedure in detail.) Tj ET",
    );
    let mut layout = fixture.layout();
    let body: Vec<_> = layout.pages[0]
        .paragraphs
        .iter()
        .filter(|p| p.bbox.y0 > 580.0 && p.bbox.y1 < 645.0)
        .collect();
    assert!(!body.is_empty());
    assert!(body.iter().all(|p| p.protected), "{body:#?}");
    let candidates: Vec<_> = layout.pages[0]
        .paragraphs
        .iter()
        .filter(|p| !p.protected)
        .collect();
    assert_eq!(candidates.len(), 2, "{candidates:#?}");
    assert!(
        candidates
            .iter()
            .any(|p| p.text.starts_with("Algorithm 2") && !p.text.contains("Input"))
    );
    let original = render(&fixture.input);
    for paragraph in &mut layout.pages[0].paragraphs {
        if !paragraph.protected {
            paragraph.translation = Some("译".into());
        }
    }
    let (set, stats) = typeset(&layout, "ttf", 0.72);
    assert_eq!(stats.overflow, 0);
    let out = fixture.write_layout(&layout, &set, ComposeMode::Clean, "algorithm.pdf");
    assert_eq!(
        region(&original, 70, 146, 400, 58),
        region(&render(&out), 70, 146, 400, 58)
    );
    assert_absent_from_all_objects(&out, b"Sampling procedure");
}

#[test]
fn recursive_form_is_rejected_before_rewrite_interpretation() {
    let mut fixture = Fixture::new(BODY);
    let mut layout = fixture.layout();
    layout.pages[0].paragraphs[0].translation = Some("Translation.".into());
    let (set, _) = typeset(&layout, "ttf", 0.72);
    let form = fixture.doc.new_object_id();
    fixture.doc.objects.insert(form, Stream::new(dictionary! {
        "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(),0.into(),612.into(),792.into()],
        "Resources" => dictionary! {"XObject" => dictionary! {"Self" => form}},
    }, b"/Self Do".to_vec()).into());
    let resources = fixture.resources();
    resources.set("XObject", dictionary! {"Self" => form});
    let content = fixture.doc.add_object(Stream::new(
        dictionary! {},
        format!("{BODY} /Self Do").into_bytes(),
    ));
    fixture
        .doc
        .get_dictionary_mut(fixture.page)
        .unwrap()
        .set("Contents", content);
    fixture.save();
    let error = write_translated_pdf(
        fixture.doc.clone(),
        &fixture.dir.join("cycle.pdf"),
        &layout.pages,
        &set,
        &Default::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("cyclic Form"), "{error:#}");
}

#[test]
fn hidden_text_does_not_become_translatable() {
    for content in [
        format!("q 0 0 1 1 re W n {BODY} Q"),
        format!("q /Zero gs {BODY} Q"),
        BODY.replace("BT", "BT 3 Tr"),
    ] {
        let mut fixture = Fixture::new(&content);
        let layout = fixture.layout();
        assert!(layout.pages[0].paragraphs.is_empty(), "{content}");
    }
}

#[test]
fn partially_clipped_and_translucent_text_is_protected() {
    for content in [
        format!("q 72 510 140 30 re W n {BODY} Q"),
        format!("q /Half gs {BODY} Q"),
        format!("q 0 0 m 612 0 l 0 792 l h W n {BODY} Q"),
    ] {
        let mut fixture = Fixture::new(&content);
        let layout = fixture.layout();
        assert!(!layout.pages[0].paragraphs.is_empty());
        assert!(
            layout.pages[0].paragraphs.iter().all(|p| p.protected),
            "{content}"
        );
    }
}

#[test]
fn separate_vector_underscores_become_text_and_are_covered() {
    let mut fixture =
        Fixture::new("BT /F1 14 Tf 72 520 Td (This paragraph has variable conv3) Tj ET");
    fixture.save();
    let glyphs = extract_pages(&fixture.input, &PageSelection::all()).unwrap();
    let last = glyphs[0].glyphs.last().unwrap();
    let x = last.origin_x + last.advance;
    let contents = fixture.doc.get_page_content(fixture.page);
    let extra = format!(
        " q 0.4 w {x} 520.2 m {} 520.2 l S Q BT /F1 14 Tf {} 520 Td (1 in the prose.) Tj ET",
        x + 4.0,
        x + 4.0
    );
    let id = fixture.doc.add_object(Stream::new(
        dictionary! {},
        [contents, extra.into_bytes()].concat(),
    ));
    fixture
        .doc
        .get_dictionary_mut(fixture.page)
        .unwrap()
        .set("Contents", id);
    let layout = fixture.layout();
    assert!(
        layout.pages[0]
            .paragraphs
            .iter()
            .all(|paragraph| !paragraph.protected)
    );
    assert!(
        layout.pages[0]
            .paragraphs
            .iter()
            .any(|paragraph| paragraph.text.contains("conv3_1")),
        "vector underscore was not converted to text: {:?}",
        layout.pages[0]
            .paragraphs
            .iter()
            .map(|paragraph| &paragraph.text)
            .collect::<Vec<_>>()
    );
    assert!(
        layout.pages[0]
            .paragraphs
            .iter()
            .flat_map(|paragraph| paragraph.lines(&layout.pages[0].lines))
            .flat_map(|line| &line.glyphs)
            .any(|glyph| !glyph.decorations.is_empty()),
        "vector underscore decoration was not recorded"
    );
}

#[test]
fn underline_before_its_letter_does_not_protect_prose_or_erase_the_background() {
    let mut fixture = Fixture::new(
        r"q 0.7 0.8 0.9 rg 60 480 500 70 re f Q
         BT /F1 14 Tf 72 520 Td (An abstract about a method \() Tj ET",
    );
    fixture.save();
    let extracted = extract_pages(&fixture.input, &PageSelection::all()).unwrap();
    let last = extracted[0].glyphs.last().unwrap();
    let x = last.origin_x + last.advance;
    let extra = format!(
        r"q 0.4 w {x} 518.5 m {} 518.5 l S Q BT /F1 14 Tf {x} 520 Td (Chain of thought\) continues here.) Tj ET",
        x + 9.0,
    );
    let content = [
        fixture.doc.get_page_content(fixture.page),
        extra.into_bytes(),
    ]
    .concat();
    let stream = fixture.doc.add_object(Stream::new(dictionary! {}, content));
    fixture
        .doc
        .get_dictionary_mut(fixture.page)
        .unwrap()
        .set("Contents", stream);
    let layout = fixture.layout();
    assert!(layout.pages[0].paragraphs.iter().all(|p| !p.protected));
    assert!(
        layout.pages[0]
            .paragraphs
            .iter()
            .any(|p| p.text.contains("(Chain"))
    );
    let output = fixture.compose(ComposeMode::Clean, "ttf", "Short.");
    assert!(!visible_text(&output).contains("abstract"));
    let image = render(&output);
    // The original underline was on a colored panel. Clean rewriting must
    // remove its paint operation instead of covering it with a white patch.
    let pixels = region(&image, x as usize, 792 - 520, 9, 4);
    assert!(
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|rgba| rgba[0] < 230 && rgba[2] > 190)
    );
}

#[test]
fn font_state_follows_show_operations_and_form_inheritance() {
    let mut fixture = Fixture::new(
        "BT /F1 12 Tf 1 0 0 1 72 500 Tm (A) Tj /Italic 12 Tf (B) Tj /F1 12 Tf (C) Tj ET
         q BT /Italic 12 Tf 1 0 0 1 72 470 Tm (D) Tj ET Q
         BT 1 0 0 1 72 440 Tm (E) Tj ET /Inherited Do",
    );
    let italic = fixture.doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Times-Italic",
    });
    fixture
        .resources()
        .get_mut(b"Font")
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Italic", italic);
    let form = fixture.doc.add_object(Stream::new(dictionary! {
        "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
    }, b"BT 1 0 0 1 72 410 Tm (F) Tj ET".to_vec()));
    fixture
        .resources()
        .set("XObject", dictionary! {"Inherited" => form});
    fixture.save();
    let extracted = extract_pages(&fixture.input, &PageSelection::all()).unwrap();
    let actual: Vec<_> = extracted[0]
        .glyphs
        .iter()
        .map(|g| (g.text.as_str(), g.font_name.as_deref().unwrap()))
        .collect();
    assert_eq!(
        actual,
        [
            ("A", "Helvetica"),
            ("B", "Times-Italic"),
            ("C", "Helvetica"),
            ("D", "Times-Italic"),
            ("E", "Helvetica"),
            ("F", "Helvetica")
        ]
    );
}

#[test]
fn scaled_text_matrices_keep_math_font_names_and_scripts_local() {
    let mut fixture = Fixture::new(
        "BT /F1 1 Tf 10 0 0 10 72 520 Tm (This is ordinary prose before ) Tj
         /Math 1 Tf 15 0 Td (R) Tj /F1 1 Tf 7 0 0 7 229 524 Tm (4) Tj
         10 0 0 10 240 520 Tm ( and more ordinary prose after it.) Tj ET",
    );
    fixture.add_type1_font("Math", "Symbol");
    let layout = fixture.layout();
    let page = &layout.pages[0];
    let paragraph = page
        .paragraphs
        .iter()
        .find(|p| p.text.contains("ordinary prose"))
        .unwrap();
    assert!(!paragraph.protected);
    let script = page
        .lines
        .iter()
        .flat_map(|l| &l.glyphs)
        .find(|g| g.text == "4")
        .unwrap();
    assert!(paragraph.formulas.iter().any(|f| {
        f.sources
            .iter()
            .any(|s| s.ordinal == script.sources[0].ordinal)
    }));
    for glyph in page
        .lines
        .iter()
        .flat_map(|l| &l.glyphs)
        .filter(|g| g.origin_x < 200.0)
    {
        assert_eq!(glyph.font_name.as_deref(), Some("Helvetica"));
    }
}

#[test]
fn repeated_graphic_forms_do_not_exhaust_the_text_rewrite_budget() {
    let mut fixture = Fixture::new(&format!("{BODY} {}", "/Dot Do ".repeat(10_001)));
    let marker = fixture.doc.add_object(Stream::new(dictionary! {
        "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(),0.into(),5.into(),5.into()],
    }, b"0 0 4 4 re f".to_vec()));
    fixture
        .resources()
        .set("XObject", dictionary! {"Dot" => marker});
    let output = fixture.compose(ComposeMode::Clean, "ttf", "Short.");
    assert!(visible_text(&output).contains("Short."));
    assert!(!visible_text(&output).contains("regular paragraph"));
    assert_eq!(
        region(&render(&fixture.input), 0, 788, 4, 4),
        region(&render(&output), 0, 788, 4, 4)
    );
}

#[test]
fn multiline_actualtext_is_protected() {
    let mut fixture = Fixture::new(&format!(
        "/Span << /ActualText (Two lines of semantic text.) >> BDC {BODY} {} EMC",
        BODY.replace("520", "500")
    ));
    let layout = fixture.layout();
    assert!(layout.pages[0].paragraphs.iter().all(|p| p.protected));
}

#[test]
fn marked_content_uses_actualtext_including_utf16_and_empty_values() {
    for (actual, expected) in [
        (
            "(Correct semantic text must be extracted.)",
            "Correct semantic text must be extracted.",
        ),
        ("<FEFF8BD100200041>", "译 A"),
        ("()", ""),
    ] {
        let mut fixture = Fixture::new(&format!("/Span << /ActualText {actual} >> BDC {BODY} EMC"));
        fixture.save();
        assert_eq!(visible_text(&fixture.input), expected);
    }
}

#[test]
fn clean_does_not_double_paint_transparency() {
    let mut fixture = Fixture::new(&format!("q /Half gs 1 0 0 rg 72 120 140 80 re f Q {BODY}"));
    let output = fixture.compose(ComposeMode::Clean, "ttf", "Translated paragraph.");
    assert_eq!(
        region(&render(&fixture.input), 65, 580, 160, 100),
        region(&render(&output), 65, 580, 160, 100)
    );
    assert!(visible_text(&output).contains("Translated paragraph."));
}

#[test]
fn offset_columns_do_not_leave_text_on_internal_clip_seams() {
    let content = [(72, 400), (72, 386), (350, 401), (350, 387)]
        .map(|(x, y)| {
            format!("BT /F1 12 Tf {x} {y} Td (Several words form a full paragraph.) Tj ET")
        })
        .join("\n");
    let mut fixture = Fixture::new(&content);
    let output = fixture.compose(ComposeMode::Clean, "ttf", "Short.");
    // The short replacement ends before x=120; the remainder must be empty.
    let before = region(&render(&fixture.input), 125, 399, 145, 14);
    let after = region(&render(&output), 125, 399, 145, 14);
    assert!(before.iter().any(|byte| *byte != 0));
    assert!(
        after.iter().all(|byte| *byte == 0),
        "original glyph fragments remain"
    );
}

#[test]
fn legacy_form_inherits_page_fonts_after_clean_composition() {
    let mut fixture = Fixture::new(&format!("{BODY} /Legacy Do"));
    let form = fixture.doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        },
        b"BT /F1 12 Tf 72 30 Td (Inherited footer font) Tj ET".to_vec(),
    ));
    let resources = fixture.resources();
    resources.set("XObject", dictionary! {"Legacy" => form});
    let output = fixture.compose(ComposeMode::Clean, "ttf", "Translation.");
    let before = region(&render(&fixture.input), 65, 740, 200, 40);
    assert!(before.iter().any(|byte| *byte != 0));
    assert_eq!(before, region(&render(&output), 65, 740, 200, 40));
}

#[test]
fn text_clipping_survives_clean_fallback() {
    let mut fixture = Fixture::new(&format!(
        "q BT /F1 50 Tf 7 Tr 72 150 Td (MASK) Tj ET 1 0 0 rg 72 120 200 100 re f Q {BODY}"
    ));
    let output = fixture.compose(ComposeMode::Clean, "ttf", "Translated paragraph.");
    assert_eq!(
        region(&render(&fixture.input), 60, 560, 230, 130),
        region(&render(&output), 60, 560, 230, 130)
    );
    assert!(visible_text(&output).contains("Translated paragraph."));
}

#[test]
fn overlay_resets_ctm_clip_and_text_rendering_state() {
    let mut baseline = Fixture::new(BODY);
    let expected = baseline.compose(ComposeMode::Overlay, "ttf", "Translated paragraph.");
    for content in [
        format!("{BODY} 1 0 0 1 100 0 cm"),
        format!("{BODY} 0 0 1 1 re W n"),
        format!("{BODY} BT 3 Tr 80 Tz 10 Tc 8 Tw 20 Ts ET"),
    ] {
        let mut fixture = Fixture::new(&content);
        let output = fixture.compose(ComposeMode::Overlay, "ttf", "Translated paragraph.");
        assert_eq!(
            render(&expected).data_as_u8_slice(),
            render(&output).data_as_u8_slice()
        );
    }
}

#[test]
fn generated_font_names_do_not_replace_existing_resources() {
    let content = format!("{BODY} BT /FpdfzhU0 12 Tf 72 30 Td (FOOTER) Tj ET /OrigPage Do");
    let mut fixture = Fixture::new(&content);
    let form = fixture.doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        },
        b"0 0 1 rg 72 120 100 50 re f".to_vec(),
    ));
    let resources = fixture.resources();
    let fonts = resources.get_mut(b"Font").unwrap().as_dict_mut().unwrap();
    fonts.set("FpdfzhU0", fonts.get(b"F1").unwrap().clone());
    resources.set("XObject", dictionary! {"OrigPage" => form});
    let output = fixture.compose(ComposeMode::Clean, "ttf", "Translated paragraph.");
    let before = render(&fixture.input);
    let after = render(&output);
    assert_eq!(
        region(&before, 50, 610, 140, 175),
        region(&after, 50, 610, 140, 175)
    );
}

#[test]
fn rotation_and_crop_offsets_produce_upright_translation() {
    for rotation in [0, 90, 180, 270] {
        let crop = hayro::kurbo::Rect::new(-20.0, 30.0, 592.0, 762.0);
        let transform = super::geometry::layout_transform(crop, rotation);
        let [a, b, c, d, e, f] = transform.inverse().as_coeffs();
        let mut fixture = Fixture::new(&format!("{a} {b} {c} {d} {e} {f} cm {BODY}"));
        let page = fixture.doc.get_dictionary_mut(fixture.page).unwrap();
        page.set(
            "MediaBox",
            vec![(-20).into(), 30.into(), 592.into(), 762.into()],
        );
        page.set(
            "CropBox",
            vec![(-20).into(), 30.into(), 592.into(), 762.into()],
        );
        page.set("Rotate", rotation);
        for mode in [ComposeMode::Clean, ComposeMode::Overlay] {
            let output = fixture.compose(mode, "ttf", "Translated paragraph.");
            let extracted = extract_pages(&output, &PageSelection::all()).unwrap();
            let glyph = extracted[0].glyphs.iter().find(|g| g.text == "T").unwrap();
            assert!(glyph.angle.abs() < 0.001, "rotation {rotation}");
            assert!(
                (glyph.origin_x - 72.0).abs() < 1.0,
                "{rotation}: {}",
                glyph.origin_x
            );
            assert!((490.0..540.0).contains(&glyph.origin_y));
            let image = render(&output);
            let expected = if rotation % 180 == 0 {
                (612, 732)
            } else {
                (732, 612)
            };
            assert_eq!((image.width(), image.height()), expected);
        }
    }
}

#[test]
fn unicode_aliases_survive_ttf_and_cff_embedding() {
    for extension in ["ttf", "otf"] {
        let mut fixture = Fixture::new(BODY);
        let output = fixture.compose(ComposeMode::Clean, extension, "Different symbols: … ⋯");
        assert!(visible_text(&output).contains("Different symbols: … ⋯"));
        assert!(
            render(&output)
                .data_as_u8_slice()
                .iter()
                .any(|byte| *byte != 0)
        );
        let pdf = Document::load(&output).unwrap();
        assert_eq!(pdf.version, "1.6");
        for object in pdf.objects.values() {
            if let Ok(dict) = object.as_dict()
                && dict
                    .get(b"Type")
                    .and_then(Object::as_name)
                    .is_ok_and(|name| name == b"FontDescriptor")
                && dict.has(b"FontFile2")
            {
                let ascent = dict.get(b"Ascent").unwrap().as_float().unwrap();
                assert!((ascent - 800.0).abs() < 1.0, "unnormalized ascent {ascent}");
            }
        }
    }
}

#[test]
fn missing_glyphs_and_unfittable_text_preserve_the_original() {
    for min_scale in [0.72, 0.73] {
        for (translation, missing) in [
            ("Unmapped: \u{31350}".to_string(), true),
            ("Long text ".repeat(300), false),
        ] {
            let mut fixture = Fixture::new(BODY);
            let mut layout = fixture.layout();
            let paragraph = layout.pages[0]
                .paragraphs
                .iter_mut()
                .find(|p| !p.protected)
                .unwrap();
            paragraph.translation = Some(translation);
            let (set, stats) = typeset(&layout, "ttf", min_scale);
            assert!(set.paragraphs.is_empty());
            assert_eq!(
                (stats.missing_glyphs, stats.overflow),
                if missing { (1, 0) } else { (0, 1) }
            );
        }
    }
}

#[test]
fn exact_minimum_scale_is_attempted_even_between_steps() {
    let mut fixture = Fixture::new(BODY);
    let mut layout = fixture.layout();
    let paragraph = layout.pages[0]
        .paragraphs
        .iter_mut()
        .find(|p| !p.protected)
        .unwrap();
    paragraph.translation = Some("Translation.".to_string());
    let font = FontSource::load(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test.ttf"),
        None,
    )
    .unwrap();
    let metrics = font.metrics("Translation.".chars()).unwrap();
    let top_ink = metrics
        .char_vertical_bounds
        .values()
        .map(|b| b.1)
        .fold(0.0, f64::max);
    let bottom_ink = metrics
        .char_vertical_bounds
        .values()
        .map(|b| b.0)
        .fold(0.0, f64::min);
    let target_height = paragraph.font_size * (top_ink - bottom_ink) * 0.73 + 0.0001;
    let top = paragraph.bbox.y1;
    paragraph.bbox.y0 = top - 2.0;
    layout.pages[0].images.push(crate::model::ImageBox {
        bbox: BBox::new(0.0, 0.0, 612.0, top - target_height - 2.0),
    });
    let (set, stats) = typeset(&layout, "ttf", 0.73);
    assert_eq!(set.paragraphs.len(), 1);
    assert_eq!(stats.overflow, 0);
    assert!((stats.smallest_scale - 0.73).abs() < 0.00001);
}

#[test]
fn inline_objects_shrink_with_text_and_render_at_the_fitted_position() {
    let mut fixture = Fixture::new(
        "BT /F1 14 Tf 72 520 Td (Here is ) Tj 0 0 1 rg (x) Tj 0 0 0 rg ( as an inline object in ordinary prose.) Tj ET",
    );
    let mut layout = fixture.layout();
    let extracted = extract_pages(&fixture.input, &PageSelection::all()).unwrap();
    let glyph = extracted[0].glyphs.iter().find(|g| g.text == "x").unwrap();
    let paragraph = layout.pages[0]
        .paragraphs
        .iter_mut()
        .find(|p| !p.protected)
        .unwrap();
    paragraph.formulas = vec![crate::model::FormulaRef {
        id: "f1".into(),
        text: "x".into(),
        bbox: glyph.bbox,
        baseline: glyph.origin_y,
        sources: glyph.sources.to_vec(),
    }];
    paragraph.translation = Some("译<f1>译".into());
    let font = FontSource::load(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test.ttf"),
        None,
    )
    .unwrap();
    let metrics = font.metrics(['译']).unwrap();
    let (bottom, top) = metrics.char_vertical_bounds[&'译'];
    let ascent = (top * paragraph.font_size).max(glyph.bbox.y1 - glyph.origin_y);
    let descent = (-bottom * paragraph.font_size).max(glyph.origin_y - glyph.bbox.y0);
    let target_height = (ascent + descent) * 0.81;
    let top = paragraph.bbox.y1;
    paragraph.bbox.y0 = top - 1.0;
    layout.pages[0].images.push(crate::model::ImageBox {
        bbox: BBox::new(0.0, 0.0, 612.0, top - target_height - 2.0),
    });
    let (set, stats) = typeset(&layout, "ttf", 0.72);
    assert_eq!(stats.overflow, 0);
    let placed = set.paragraphs.values().next().unwrap();
    assert!(placed.scale <= 0.81 && placed.scale >= 0.72);
    let object = &placed.lines[0].formulas[0];
    assert_eq!(object.scale, placed.scale);
    let output = fixture.write_layout(&layout, &set, ComposeMode::Clean, "scaled-formula.pdf");
    assert_eq!(visible_text(&output), "译译");
    let pixels = render(&output);
    let mut blue = 0;
    let width = pixels.width() as usize;
    for (i, rgba) in pixels
        .data_as_u8_slice()
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
    {
        if rgba[2] > 100 && rgba[0] < 80 && rgba[1] < 80 {
            blue += 1;
            let x = (i % width) as f64;
            let y = 792.0 - (i / width) as f64;
            assert!(x >= object.x - 1.0 && x <= object.x + glyph.bbox.width() * object.scale + 1.0);
            assert!(y >= object.baseline + (glyph.bbox.y0 - glyph.origin_y) * object.scale - 1.0);
            assert!(y <= object.baseline + (glyph.bbox.y1 - glyph.origin_y) * object.scale + 1.0);
        }
    }
    assert!(blue > 0, "the original vector object disappeared");
}

#[test]
fn longer_headings_use_free_column_space_without_crossing_images() {
    for obstacle in [false, true] {
        let mut fixture = Fixture::new(
            "BT /F1 14 Tf 72 650 Td (1. Example) Tj ET \
             BT /F1 14 Tf 72 600 Td (This ordinary paragraph explains the example in detail.) Tj ET",
        );
        let mut layout = fixture.layout();
        let heading = &mut layout.pages[0].paragraphs[0];
        heading.role = crate::model::Role::Heading;
        heading.protected = false;
        heading.translation = Some("译".repeat(12));
        let id = heading.id.clone();
        if obstacle {
            layout.pages[0].images.push(crate::model::ImageBox {
                bbox: BBox::new(180.0, 640.0, 250.0, 670.0),
            });
        }
        let (set, _) = typeset(&layout, "ttf", 0.72);
        if !obstacle {
            let heading = &set.paragraphs[&id];
            assert_eq!(heading.lines.len(), 1);
            assert_eq!(heading.scale, 1.0);
        } else if let Some(heading) = set.paragraphs.get(&id) {
            for line in &heading.lines {
                for segment in &line.segments {
                    let right =
                        segment.x + set.font.metrics.text_width(&segment.text, line.size, 0.0);
                    assert!(right <= 178.01);
                }
            }
        }
    }
}

#[test]
fn expansion_stops_before_an_image() {
    let mut fixture = Fixture::new(BODY);
    let mut layout = fixture.layout();
    let paragraph = layout.pages[0]
        .paragraphs
        .iter_mut()
        .find(|p| !p.protected)
        .unwrap();
    paragraph.translation = Some("Translated paragraph.".to_string());
    let bottom = paragraph.bbox.y0;
    layout.pages[0].images.push(crate::model::ImageBox {
        bbox: BBox::new(0.0, 0.0, 612.0, bottom),
    });
    let (set, _) = typeset(&layout, "ttf", 0.72);
    for paragraph in set.paragraphs.values() {
        assert!(paragraph.lines.iter().all(|line| line.baseline >= bottom));
    }
}

#[test]
fn oversized_compressed_stream_is_rejected_before_interpretation() {
    let mut fixture = Fixture::new(BODY);
    let mut stream = Stream::new(dictionary! {}, vec![b' '; 65 * 1024 * 1024]);
    stream.compress().unwrap();
    assert!(stream.content.len() < 100_000);
    let id = fixture.doc.add_object(stream);
    fixture
        .doc
        .get_dictionary_mut(fixture.page)
        .unwrap()
        .set("Contents", id);
    fixture.save();
    let error = extract_pages(&fixture.input, &PageSelection::all()).unwrap_err();
    assert!(format!("{error:#}").contains("limit"), "{error:#}");
}

#[test]
fn signed_rewrites_are_rejected() {
    let mut fixture = Fixture::new(BODY);
    let mut layout = fixture.layout();
    layout.pages[0].paragraphs[0].translation = Some("Translation.".to_string());
    let (set, _) = typeset(&layout, "ttf", 0.72);
    fixture.doc.add_object(dictionary! {"Type" => "Sig", "ByteRange" => vec![0.into(), 20.into()], "Contents" => Object::string_literal("signature")});
    fixture.save();
    let error = write_translated_pdf(
        fixture.doc.clone(),
        &fixture.dir.join("signed.pdf"),
        &layout.pages,
        &set,
        &Default::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("signed PDFs"));
    assert!(!fixture.dir.join("signed.pdf").exists());
}

#[test]
fn clean_preserves_a_rotated_glyph_inside_a_translated_line() {
    let mut fixture = Fixture::new(
        "BT /F1 16 Tf 1 0 0 1 72 400 Tm (This sentence has room for translation.) Tj ET
         BT /F1 7 Tf 0 1 -1 0 130 402 Tm (Z) Tj ET",
    );
    let output = fixture.compose(ComposeMode::Clean, "ttf", "Short.");
    let text = visible_text(&output);
    assert!(text.contains('Z'), "{text}");
    assert!(text.contains("Short."), "{text}");
    assert!(!text.contains("This sentence"), "{text}");
}

#[test]
fn figure_and_table_captions_remain_translatable_with_their_continuations() {
    let mut fixture = Fixture::new(
        "BT /Bold 12 Tf 1 0 0 1 72 540 Tm (Figure 1. Baseline) Tj ET
         BT /F1 8 Tf 1 0 0 1 72 530 Tm (continued results) Tj ET
         BT /F1 12 Tf 1 0 0 1 72 320 Tm (Table 2. Results) Tj ET",
    );
    let bold = fixture.doc.add_object(
        dictionary! {"Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica-Bold"},
    );
    fixture
        .resources()
        .get_mut(b"Font")
        .unwrap()
        .as_dict_mut()
        .unwrap()
        .set("Bold", bold);
    let layout = fixture.layout();
    let paras = &layout.pages[0].paragraphs;
    let caption = paras.iter().find(|p| p.text.starts_with("Figure")).unwrap();
    assert!(
        caption
            .prompt_text
            .as_deref()
            .unwrap()
            .contains("continued results")
    );
    assert!(
        !paras
            .iter()
            .find(|p| p.text.starts_with("Table"))
            .unwrap()
            .protected
    );
}

#[test]
fn code_listings_keep_their_pixels_and_inline_code_keeps_its_prose_translatable() {
    let mut fixture = Fixture::new(
        "BT /Code 10 Tf 0 0.4 0 rg 72 690 Td (theorem square :) Tj
         12 -14 Td (have h := 1) Tj
         0 -14 Td (-- preserve indentation and this comment) Tj ET
         BT /F1 11 Tf 0 g 72 580 Td (This regular sentence uses ) Tj
         /Code 11 Tf (identifier) Tj /F1 11 Tf ( in ordinary prose.) Tj ET",
    );
    fixture.add_type1_font("Code", "Courier");
    let mut layout = fixture.layout();
    let body = layout.pages[0]
        .paragraphs
        .iter_mut()
        .find(|p| p.text.starts_with("This regular"))
        .unwrap();
    assert!(!body.protected);
    assert_eq!(body.formulas.len(), 1);
    body.translation = Some("译<f1>译".to_owned());
    assert!(
        layout.pages[0]
            .paragraphs
            .iter()
            .filter(|p| p.bbox.y0 > 640.0)
            .all(|p| p.protected)
    );
    let (set, stats) = typeset(&layout, "ttf", 0.72);
    assert_eq!(stats.overflow, 0);
    for mode in [ComposeMode::Clean, ComposeMode::Overlay] {
        let output = fixture.write_layout(&layout, &set, mode, &format!("code-{mode:?}.pdf"));
        assert_eq!(
            region(&render(&fixture.input), 60, 80, 490, 90),
            region(&render(&output), 60, 80, 490, 90)
        );
    }
}

#[test]
fn equation_heavy_pages_do_not_turn_regular_prose_into_bold_headings() {
    let mut content = String::new();
    for i in 0..16 {
        content.push_str(&format!(
            "BT /Math 8 Tf 72 {} Td (a=b+c+d+e+f+g+h) Tj ET\n",
            540 - i * 12
        ));
    }
    content.push_str("BT /F1 11 Tf 72 300 Td (reward model. Gradient Coefficient:) Tj ET
                     BT /F1 11 Tf 72 260 Td (where the coefficient is computed from group scores.) Tj ET");
    let mut fixture = Fixture::new(&content);
    fixture.add_type1_font("Math", "Symbol");
    let mut layout = fixture.layout();
    for p in layout.pages[0]
        .paragraphs
        .iter_mut()
        .filter(|p| p.text.starts_with("reward") || p.text.starts_with("where"))
    {
        assert_eq!(p.role, crate::model::Role::Paragraph);
        assert!(!p.base_style.bold);
        p.translation = Some("Translation.".to_owned());
    }
    let (set, stats) = typeset(&layout, "ttf", 0.72);
    assert_eq!(stats.paragraphs, 2);
    assert!(
        set.paragraphs
            .values()
            .flat_map(|p| &p.lines)
            .flat_map(|l| &l.segments)
            .all(|s| !s.bold)
    );
}

#[test]
fn literal_markup_survives_actualtext_and_clean_typesetting() {
    let text = "The literal <f1> and <b> syntax belongs in this text.";
    let content =
        format!("/Span << /ActualText ({text}) >> BDC BT /F1 15 Tf 72 550 Td ({text}) Tj ET EMC");
    let mut fixture = Fixture::new(&content);
    let layout = fixture.layout();
    let paragraph = &layout.pages[0].paragraphs[0];
    assert!(paragraph.formulas.is_empty());
    let prompt = paragraph.prompt_text.as_deref().unwrap();
    assert!(prompt.contains("&lt;f1&gt;") && prompt.contains("&lt;b&gt;"));
    let out = fixture.compose(ComposeMode::Clean, "ttf", prompt);
    assert!(visible_text(&out).contains("<f1> and <b>"));
}

#[test]
fn overlay_with_deep_forms_leaves_original_content_streams_intact() {
    let mut fixture = Fixture::new(&format!("{BODY} /Nested Do"));
    let mut resources = dictionary! {};
    for depth in 0..42 {
        let bytes = if depth == 0 {
            b"0 0 m 1 1 l S".as_slice()
        } else {
            b"/Nested Do"
        };
        let form = fixture.doc.add_object(Stream::new(dictionary! {
            "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(),0.into(),1.into(),1.into()],
            "Resources" => resources,
        }, bytes.to_vec()));
        resources = dictionary! {"XObject" => dictionary! {"Nested" => form}};
    }
    let objects = resources.get(b"XObject").unwrap().clone();
    fixture.resources().set("XObject", objects);
    let orig = fixture.doc.get_page_contents(fixture.page);
    let out = fixture.compose(ComposeMode::Overlay, "ttf", "Short.");
    let doc = Document::load(&out).unwrap();
    let contents = doc.get_page_contents(fixture.page);
    for id in orig {
        assert!(contents.contains(&id));
        assert_eq!(
            doc.get_object(id)
                .unwrap()
                .as_stream()
                .unwrap()
                .get_plain_content()
                .unwrap(),
            fixture
                .doc
                .get_object(id)
                .unwrap()
                .as_stream()
                .unwrap()
                .get_plain_content()
                .unwrap()
        );
    }
    assert!(visible_text(&out).contains("Short."));
}

#[cfg(feature = "embedded-fonts")]
#[test]
fn glyph_coverage_depends_on_the_font_actually_used() {
    let mut fixture = Fixture::new(BODY);
    let mut layout = fixture.layout();
    let regular = super::find_default_font(None).unwrap();
    let bold = FontSource::load("tests/fixtures/test.ttf", None).unwrap();
    for (text, missing) in [("中文", 0), ("<b>中文</b>", 1)] {
        layout.pages[0].paragraphs[0].translation = Some(text.into());
        let (_, stats) =
            typeset_document(&layout, &regular, Some(&bold), &Default::default()).unwrap();
        assert_eq!(stats.missing_glyphs, missing);
    }
}

#[test]
fn output_uses_the_pdf_that_was_analyzed_even_if_input_is_replaced() {
    let mut fixture = Fixture::new(BODY);
    fixture.save();
    let (original, mut layout) =
        crate::load_layout(&fixture.input, "1-", Default::default()).unwrap();
    std::fs::write(&fixture.input, b"This file has been replaced.").unwrap();
    layout.pages[0].paragraphs[0].translation = Some("From the original.".into());
    let (set, _) = typeset(&layout, "ttf", 0.72);
    let out = fixture.dir.join("snapshot.pdf");
    write_translated_pdf(original, &out, &layout.pages, &set, &Default::default()).unwrap();
    assert!(visible_text(&out).contains("From the original."));
}

#[test]
fn mismatched_glyph_provenance_aborts_without_replacing_an_output() {
    let mut fixture = Fixture::new(BODY);
    let mut layout = fixture.layout();
    layout.pages[0].paragraphs[0].translation = Some("Short.".into());
    layout.pages[0].lines[0].glyphs[0].sources[0].x += 5.0;
    let (set, _) = typeset(&layout, "ttf", 0.72);
    let out = fixture.dir.join("prior-output.pdf");
    std::fs::write(&out, b"prior contents").unwrap();
    let error = write_translated_pdf(
        fixture.doc.clone(),
        &out,
        &layout.pages,
        &set,
        &Default::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("source glyph"));
    assert_eq!(std::fs::read(out).unwrap(), b"prior contents");
}

#[test]
fn coincident_form_invocations_have_distinct_source_glyphs() {
    let mut fixture = Fixture::new("/Shared Do /Shared Do");
    let body = "BT /F1 14 Tf 72 520 Td (Identical text at the same position.) Tj ET";
    let form = fixture.doc.add_object(Stream::new(dictionary! {
        "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
    }, body.as_bytes().to_vec()));
    fixture
        .resources()
        .set("XObject", dictionary! {"Shared" => form});
    let mut layout = fixture.layout();
    let page = &mut layout.pages[0];
    let paragraph = page.paragraphs.iter_mut().find(|p| !p.protected).unwrap();
    paragraph.translation = Some("Short.".into());
    // Select only the first invocation. The two runs share the same Form,
    // character codes, transforms and bounds.
    let first_run_len = "Identical text at the same position.".len();
    for &index in &paragraph.line_indices {
        page.lines[index]
            .glyphs
            .retain(|g| g.sources[0].ordinal < first_run_len);
    }
    let (set, _) = typeset(&layout, "ttf", 0.72);
    let out = fixture.write_layout(&layout, &set, ComposeMode::Clean, "coincident.pdf");
    let text = visible_text(&out);
    assert_eq!(
        text.matches("Identical text at the same position.").count(),
        1,
        "{text}"
    );
    assert!(text.contains("Short."));
}
