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
    let target_height = paragraph.font_size * 1.22 * 0.73 + 0.0001;
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
fn oversized_images_and_signed_rewrites_are_rejected() {
    let mut fixture = Fixture::new(BODY);
    fixture.doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image", "Width" => 1_000_000,
            "Height" => 1_000_000, "Filter" => "DCTDecode",
        },
        vec![],
    ));
    fixture.save();
    assert!(
        super::preflight::load_pdf(&fixture.input)
            .err()
            .unwrap()
            .to_string()
            .contains("dimensions")
    );

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
fn caption_prompt_includes_merged_continuation_and_table_caption_stays_protected() {
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
        paras
            .iter()
            .find(|p| p.text.starts_with("Table"))
            .unwrap()
            .protected
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
