use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use hayro::kurbo::{Affine, Point};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};

use crate::model::{BBox, PageLayout, Paragraph};
use crate::pdf::content::page_resources_clone;
use crate::pdf::font::EmbeddedFont;
use crate::typeset::{TypesetDocument, TypesetLine, TypesetParagraph, TypesetSegment};

pub const FONT_RESOURCE_NAME: &str = "Fpdfzh";
pub const BOLD_FONT_RESOURCE_NAME: &str = "FpdfzhB";
pub const ITALIC_FONT_RESOURCE_NAME: &str = "FpdfzhI";
pub const SYMBOL_FONT_RESOURCE_NAME: &str = "FpdfzhS";

struct ResourceNames {
    regular: Vec<String>,
    bold: Vec<String>,
    italic: Vec<String>,
    symbol: Vec<String>,
    formulas: HashMap<String, HashMap<String, String>>,
}

impl ResourceNames {
    fn for_page(
        document: &Document,
        page_id: ObjectId,
        regular_count: usize,
        bold_count: usize,
        italic_count: usize,
        symbol_count: usize,
    ) -> Result<Self> {
        let resources = page_resources_clone(document, page_id)?;
        let unique = |category: &[u8], base: &str| -> String {
            let dictionary = resources
                .get(category)
                .ok()
                .and_then(|object| match object {
                    Object::Reference(id) => document.get_dictionary(*id).ok(),
                    Object::Dictionary(dict) => Some(dict),
                    _ => None,
                });
            let mut name = base.to_string();
            let mut suffix = 0;
            while dictionary.is_some_and(|dict| dict.has(name.as_bytes())) {
                suffix += 1;
                name = format!("{base}_{suffix}");
            }
            name
        };
        Ok(Self {
            regular: (0..regular_count)
                .map(|i| unique(b"Font", &format!("{FONT_RESOURCE_NAME}U{i}")))
                .collect(),
            bold: (0..bold_count)
                .map(|i| unique(b"Font", &format!("{BOLD_FONT_RESOURCE_NAME}U{i}")))
                .collect(),
            italic: (0..italic_count)
                .map(|i| unique(b"Font", &format!("{ITALIC_FONT_RESOURCE_NAME}U{i}")))
                .collect(),
            symbol: (0..symbol_count)
                .map(|i| unique(b"Font", &format!("{SYMBOL_FONT_RESOURCE_NAME}U{i}")))
                .collect(),
            formulas: Default::default(),
        })
    }
}

fn write_matrix(out: &mut String, matrix: Affine) {
    let [a, b, c, d, e, f] = matrix.as_coeffs();
    let _ = writeln!(out, "{a:.8} {b:.8} {c:.8} {d:.8} {e:.8} {f:.8} cm");
}

fn page_matrix(page: &PageLayout) -> Affine {
    Affine::new(page.layout_transform)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeMode {
    /// Keep the original page content and paint background patches over it.
    Overlay,
    /// Delete translated glyphs from source streams and draw their translations.
    Clean,
}

#[derive(Debug, Clone)]
pub struct ComposeOptions {
    pub mode: ComposeMode,
    pub cover_original: bool,
    pub cover_rgb: [f64; 3],
    pub debug_boxes: bool,
    pub keep_annotations: bool,
}

impl Default for ComposeOptions {
    fn default() -> Self {
        Self {
            mode: ComposeMode::Clean,
            cover_original: true,
            cover_rgb: [1.0, 1.0, 1.0],
            debug_boxes: false,
            keep_annotations: false,
        }
    }
}

pub fn write_translated_pdf(
    mut document: Document,
    output: &Path,
    pages: &[PageLayout],
    typeset: &TypesetDocument,
    options: &ComposeOptions,
) -> Result<()> {
    let font = &typeset.font;
    let bold_font = typeset.bold_font.as_ref();
    let italic_font = typeset.italic_font.as_ref();
    let symbol_font = typeset.symbol_font.as_ref();
    if document.objects.values().any(|object| {
        object
            .as_dict()
            .is_ok_and(|dict| dict.has(b"ByteRange") && dict.has(b"Contents"))
    }) {
        bail!("signed PDFs cannot be rewritten while preserving signatures; use an unsigned copy");
    }
    let version = document
        .version
        .split_once('.')
        .and_then(|(major, minor)| Some((major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?)))
        .unwrap_or((1, 0));
    if version < (1, 6) {
        document.version = "1.6".to_string();
    }
    let rewritten = match options.mode {
        ComposeMode::Clean => super::rewrite::remove_translated(&mut document, pages, typeset)?,
        ComposeMode::Overlay => super::rewrite::outline_formulas(&mut document, pages, typeset)?,
    };
    let font_ids = add_embedded_font(&mut document, font, false)?;
    let bold_font_ids = match bold_font {
        Some(bold) => add_embedded_font(&mut document, bold, false)?,
        None => Vec::new(),
    };
    let italic_font_ids = match italic_font {
        Some(italic) => add_embedded_font(&mut document, italic, true)?,
        None => Vec::new(),
    };
    let symbol_font_ids = match symbol_font {
        Some(symbol) => add_embedded_font(&mut document, symbol, false)?,
        None => Vec::new(),
    };
    let page_ids = document.get_pages();

    for page in pages {
        let Some(page_id) = page_ids.get(&(page.index as u32 + 1)).copied() else {
            continue;
        };
        let translated = translated_paragraphs(page, typeset);
        if translated.is_empty() {
            continue;
        }

        let mut names = ResourceNames::for_page(
            &document,
            page_id,
            font_ids.len(),
            bold_font_ids.len(),
            italic_font_ids.len(),
            symbol_font_ids.len(),
        )?;
        install_fonts(
            &mut document,
            page_id,
            &names,
            &font_ids,
            &bold_font_ids,
            &italic_font_ids,
            &symbol_font_ids,
        )?;
        if !options.keep_annotations {
            remap_link_annotations(&mut document, page_id, page, &translated)?;
        }

        if let Some(rewritten) = rewritten.get(&page.index) {
            for (index, formula) in rewritten.iter().enumerate() {
                let mut name = format!("PdfTranslateFormula{index}");
                let resources = page_resources_clone(&document, page_id)?;
                let xobjects = resources.get(b"XObject").ok().and_then(|o| match o {
                    Object::Dictionary(dict) => Some(dict),
                    Object::Reference(id) => document.get_dictionary(*id).ok(),
                    _ => None,
                });
                while xobjects.is_some_and(|dict| dict.has(name.as_bytes())) {
                    name.push('_');
                }
                let bbox = formula.bbox;
                let form = document.add_object(Stream::new(dictionary! {
                    "Type" => "XObject", "Subtype" => "Form",
                    "BBox" => vec![bbox.x0.into(), bbox.y0.into(), bbox.x1.into(), bbox.y1.into()],
                    "Resources" => dictionary! {},
                }, formula.content.as_bytes().to_vec()));
                install_resource(&mut document, page_id, "XObject", &name, form)?;
                names
                    .formulas
                    .entry(formula.paragraph.clone())
                    .or_default()
                    .insert(formula.id.clone(), name);
            }
        }
        let fonts = TranslationFonts {
            font,
            bold_font,
            italic_font,
            symbol_font,
            names: &names,
        };
        let content = build_translation_ops(page, &translated, &fonts, options)?;
        append_page_content(&mut document, page_id, content.into_bytes())?;
    }
    // Saving a rewritten page alone leaves detached source streams in lopdf's
    // object map. Remove them physically, including old shared Form copies.
    document.prune_objects();

    crate::output::atomic_write(output, |file| Ok(document.save_to(file)?))
}

/// Move link rectangles onto the translated text. Links that do not overlap
/// any translated paragraph keep their original position; other annotation
/// types (form widgets, comments, redaction marks, signatures, ...) are
/// preserved because deleting them can lose data or expose covered content.
fn remap_link_annotations(
    document: &mut Document,
    page_id: ObjectId,
    page: &PageLayout,
    translated: &[(&Paragraph, &TypesetParagraph)],
) -> Result<()> {
    let annots = match document.get_object(page_id)?.as_dict()?.get(b"Annots") {
        Ok(annots) => annots.clone(),
        Err(_) => return Ok(()),
    };
    let array = match annots {
        Object::Array(array) => array,
        Object::Reference(id) => match document.get_object(id)? {
            Object::Array(array) => array.clone(),
            _ => return Ok(()),
        },
        _ => return Ok(()),
    };

    let mut kept = Vec::with_capacity(array.len());
    let mut changed = false;
    for annot in array {
        if !is_link_annotation(document, &annot) {
            kept.push(annot);
            continue;
        }
        let (dict, id) = match &annot {
            Object::Reference(id) => match document.get_dictionary(*id) {
                Ok(dict) => (dict.clone(), Some(*id)),
                Err(_) => {
                    kept.push(annot);
                    continue;
                }
            },
            Object::Dictionary(dict) => (dict.clone(), None),
            _ => {
                kept.push(annot);
                continue;
            }
        };
        let Some(rect) = dict.get(b"Rect").ok().and_then(object_rect) else {
            kept.push(annot);
            continue;
        };
        let Some(mapped) = remap_rect(rect, page, translated) else {
            kept.push(annot);
            continue;
        };
        let values = Object::Array(vec![
            mapped.x0.into(),
            mapped.y0.into(),
            mapped.x1.into(),
            mapped.y1.into(),
        ]);
        match id {
            Some(id) => {
                let target = document.get_object_mut(id)?.as_dict_mut()?;
                target.set("Rect", values);
                target.remove(b"QuadPoints");
                kept.push(annot);
            }
            None => {
                let mut dict = dict;
                dict.set("Rect", values);
                dict.remove(b"QuadPoints");
                kept.push(Object::Dictionary(dict));
            }
        }
        changed = true;
    }
    if !changed {
        return Ok(());
    }
    document
        .get_object_mut(page_id)?
        .as_dict_mut()?
        .set("Annots", Object::Array(kept));
    Ok(())
}

/// Map a link rectangle from source paragraph geometry into the typeset
/// translation, proportionally within each overlapping paragraph. Translation
/// rewraps lines, so there is no line-to-line correspondence; the paragraph
/// box is the stable anchor shared by both sides.
fn remap_rect(
    rect: BBox,
    page: &PageLayout,
    translated: &[(&Paragraph, &TypesetParagraph)],
) -> Option<BBox> {
    let to_layout = page_matrix(page);
    let layout_rect = transform_rect(rect, to_layout);
    let mut mapped: Option<BBox> = None;
    for (paragraph, typeset) in translated {
        let source = paragraph.bbox;
        if !source.intersects(layout_rect) {
            continue;
        }
        let Some(dest) = typeset
            .lines
            .iter()
            .map(|line| line.bbox)
            .reduce(BBox::union)
        else {
            continue;
        };
        if source.width() <= 1e-6
            || source.height() <= 1e-6
            || dest.width() <= 1e-6
            || dest.height() <= 1e-6
        {
            continue;
        }
        let clip = BBox::new(
            layout_rect.x0.max(source.x0),
            layout_rect.y0.max(source.y0),
            layout_rect.x1.min(source.x1),
            layout_rect.y1.min(source.y1),
        );
        let scale_x = dest.width() / source.width();
        let scale_y = dest.height() / source.height();
        let piece = BBox::new(
            dest.x0 + (clip.x0 - source.x0) * scale_x,
            dest.y0 + (clip.y0 - source.y0) * scale_y,
            dest.x0 + (clip.x1 - source.x0) * scale_x,
            dest.y0 + (clip.y1 - source.y0) * scale_y,
        );
        mapped = Some(mapped.map_or(piece, |current| current.union(piece)));
    }
    let mapped = mapped?;
    (mapped.width() > 1e-6 && mapped.height() > 1e-6)
        .then(|| transform_rect(mapped, to_layout.inverse()))
}

fn transform_rect(rect: BBox, matrix: Affine) -> BBox {
    let corners = [
        matrix * Point::new(rect.x0, rect.y0),
        matrix * Point::new(rect.x1, rect.y0),
        matrix * Point::new(rect.x0, rect.y1),
        matrix * Point::new(rect.x1, rect.y1),
    ];
    BBox::new(
        corners.iter().map(|p| p.x).fold(f64::INFINITY, f64::min),
        corners.iter().map(|p| p.y).fold(f64::INFINITY, f64::min),
        corners
            .iter()
            .map(|p| p.x)
            .fold(f64::NEG_INFINITY, f64::max),
        corners
            .iter()
            .map(|p| p.y)
            .fold(f64::NEG_INFINITY, f64::max),
    )
}

fn object_rect(object: &Object) -> Option<BBox> {
    let Object::Array(values) = object else {
        return None;
    };
    if values.len() != 4 {
        return None;
    }
    let mut coords = [0.0; 4];
    for (slot, value) in coords.iter_mut().zip(values) {
        *slot = match value {
            Object::Integer(number) => *number as f64,
            Object::Real(number) => f64::from(*number),
            _ => return None,
        };
    }
    Some(BBox::new(
        coords[0].min(coords[2]),
        coords[1].min(coords[3]),
        coords[0].max(coords[2]),
        coords[1].max(coords[3]),
    ))
}

fn is_link_annotation(document: &Document, annot: &Object) -> bool {
    let dict = match annot {
        Object::Reference(id) => match document.get_dictionary(*id) {
            Ok(dict) => dict,
            Err(_) => return false,
        },
        Object::Dictionary(dict) => dict,
        _ => return false,
    };
    match dict.get(b"Subtype") {
        Ok(Object::Name(name)) => name == b"Link",
        Ok(Object::Reference(id)) => document
            .get_object(*id)
            .ok()
            .and_then(|object| object.as_name().ok())
            .is_some_and(|name| name == b"Link"),
        _ => false,
    }
}

fn translated_paragraphs<'a>(
    page: &'a PageLayout,
    typeset: &'a TypesetDocument,
) -> Vec<(&'a Paragraph, &'a TypesetParagraph)> {
    page.paragraphs
        .iter()
        .filter_map(|paragraph| {
            typeset
                .paragraphs
                .get(&paragraph.id)
                .map(|typeset| (paragraph, typeset))
        })
        .collect()
}

fn add_embedded_font(
    document: &mut Document,
    font: &EmbeddedFont,
    italic: bool,
) -> Result<Vec<ObjectId>> {
    let file_id = if font.is_cff {
        document.add_object(
            Stream::new(
                dictionary! {
                    "Subtype" => "OpenType",
                    "Length1" => font.subset.len() as i64,
                },
                font.subset.clone(),
            )
            .with_compression(false),
        )
    } else {
        document.add_object(
            Stream::new(
                dictionary! {
                    "Length1" => font.subset.len() as i64,
                },
                font.subset.clone(),
            )
            .with_compression(false),
        )
    };

    let bbox = font.metrics.bbox;
    let metric_scale = 1000.0 / font.metrics.units_per_em;
    let flags = if italic { 4 | 64 } else { 4 };
    let mut descriptor = dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => Object::Name(font.pdf_name.as_bytes().to_vec()),
            "Flags" => flags,
            "FontBBox" => Object::Array(vec![
                bbox[0].into(),
                bbox[1].into(),
                bbox[2].into(),
                bbox[3].into(),
            ]),
            "ItalicAngle" => font.metrics.italic_angle,
            "Ascent" => font.metrics.ascender * metric_scale,
            "Descent" => font.metrics.descender * metric_scale,
            "CapHeight" => font.metrics.cap_height * metric_scale,
            "StemV" => 80,
    };
    descriptor.set(
        if font.is_cff {
            "FontFile3"
        } else {
            "FontFile2"
        },
        file_id,
    );
    let descriptor_id = document.add_object(descriptor);

    let widths = Object::Array(font.widths_array());
    let cid_font = if font.is_cff {
        dictionary! {
            "Type" => "Font",
            "Subtype" => "CIDFontType0",
            "BaseFont" => Object::Name(font.pdf_name.as_bytes().to_vec()),
            "CIDSystemInfo" => dictionary! {
                "Registry" => Object::string_literal("Adobe"),
                "Ordering" => Object::string_literal("Identity"),
                "Supplement" => 0,
            },
            "FontDescriptor" => Object::Reference(descriptor_id),
            "DW" => 1000,
            "W" => widths,
        }
    } else {
        dictionary! {
            "Type" => "Font",
            "Subtype" => "CIDFontType2",
            "BaseFont" => Object::Name(font.pdf_name.as_bytes().to_vec()),
            "CIDSystemInfo" => dictionary! {
                "Registry" => Object::string_literal("Adobe"),
                "Ordering" => Object::string_literal("Identity"),
                "Supplement" => 0,
            },
            "FontDescriptor" => Object::Reference(descriptor_id),
            "DW" => 1000,
            "W" => widths,
            "CIDToGIDMap" => "Identity",
        }
    };
    let cid_font_id = document.add_object(cid_font);

    let mut ids = Vec::new();
    for variant in 0..font.unicode_maps.len() {
        let to_unicode_id =
            document.add_object(Stream::new(dictionary! {}, font.to_unicode_cmap(variant)));
        ids.push(document.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type0",
            "BaseFont" => Object::Name(font.pdf_name.as_bytes().to_vec()),
            "Encoding" => "Identity-H",
            "DescendantFonts" => Object::Array(vec![Object::Reference(cid_font_id)]),
            "ToUnicode" => Object::Reference(to_unicode_id),
        }));
    }
    Ok(ids)
}

fn install_fonts(
    document: &mut Document,
    page_id: ObjectId,
    names: &ResourceNames,
    regular: &[ObjectId],
    bold: &[ObjectId],
    italic: &[ObjectId],
    symbol: &[ObjectId],
) -> Result<()> {
    for (name, id) in names
        .regular
        .iter()
        .zip(regular)
        .chain(names.bold.iter().zip(bold))
        .chain(names.italic.iter().zip(italic))
        .chain(names.symbol.iter().zip(symbol))
    {
        install_resource(document, page_id, "Font", name, *id)?;
    }
    Ok(())
}

fn install_resource(
    document: &mut Document,
    page_id: ObjectId,
    category: &str,
    name: &str,
    id: ObjectId,
) -> Result<()> {
    let mut resources = page_resources_clone(document, page_id)?;
    let mut entries = match resources.get(category.as_bytes()) {
        Ok(Object::Dictionary(dict)) => dict.clone(),
        Ok(Object::Reference(id)) => document.get_dictionary(*id)?.clone(),
        _ => Dictionary::new(),
    };
    entries.set(name, Object::Reference(id));
    resources.set(category, entries);
    document
        .get_dictionary_mut(page_id)?
        .set("Resources", resources);
    Ok(())
}

fn append_page_content(document: &mut Document, page_id: ObjectId, content: Vec<u8>) -> Result<()> {
    let original = document.get_page_contents(page_id);
    let prefix = document.add_object(Stream::new(dictionary! {}, b"q\n".to_vec()));
    let mut suffix = b"\nQ\n".to_vec();
    suffix.extend_from_slice(&content);
    let suffix = document.add_object(Stream::new(dictionary! {}, suffix));
    let contents = std::iter::once(prefix)
        .chain(original)
        .chain(std::iter::once(suffix))
        .map(Object::Reference)
        .collect::<Vec<_>>();
    document
        .get_object_mut(page_id)?
        .as_dict_mut()?
        .set("Contents", contents);
    Ok(())
}

/// The embedded faces plus their page resource names, bundled so the drawing
/// helpers stay cohesive and under the argument-count lint.
struct TranslationFonts<'a> {
    font: &'a EmbeddedFont,
    bold_font: Option<&'a EmbeddedFont>,
    italic_font: Option<&'a EmbeddedFont>,
    symbol_font: Option<&'a EmbeddedFont>,
    names: &'a ResourceNames,
}

/// Pick the face that actually carries a glyph: the style's own face first,
/// then any borrowed face, with the symbol face as last resort. Mirrors
/// `typeset::Faces::for_char` so measurement and drawing agree.
fn run_face<'a>(
    ch: char,
    bold: bool,
    italic: bool,
    fonts: &'a TranslationFonts<'a>,
) -> (&'a EmbeddedFont, &'a Vec<String>, bool) {
    let preferred = if italic {
        fonts.italic_font
    } else if bold {
        fonts.bold_font
    } else {
        None
    };
    if let Some(face) = preferred.filter(|face| face.char_to_code.contains_key(&ch)) {
        let resource = if italic {
            &fonts.names.italic
        } else {
            &fonts.names.bold
        };
        return (face, resource, !italic);
    }
    for (face, resource, is_bold_face) in [
        (Some(fonts.font), &fonts.names.regular, false),
        (fonts.bold_font, &fonts.names.bold, true),
        (fonts.italic_font, &fonts.names.italic, false),
        (fonts.symbol_font, &fonts.names.symbol, false),
    ] {
        if let Some(face) = face.filter(|face| face.char_to_code.contains_key(&ch)) {
            return (face, resource, is_bold_face);
        }
    }
    (fonts.font, &fonts.names.regular, false)
}

fn write_text_run(
    out: &mut String,
    segment: &TypesetSegment,
    line: &TypesetLine,
    fonts: &TranslationFonts<'_>,
) {
    let TypesetSegment {
        text,
        x,
        bold,
        italic,
        color,
    } = segment;
    let (bold, italic) = (*bold, *italic);
    let (size, baseline) = (line.size, line.baseline);
    if text.is_empty() {
        return;
    }
    let [r, g, b, _] = *color;
    let mut runs: Vec<(&EmbeddedFont, &Vec<String>, bool, String)> = Vec::new();
    for ch in text.chars() {
        let (face, resource, is_bold_face) = run_face(ch, bold, italic, fonts);
        match runs.last_mut() {
            Some((last_face, _, _, buffer)) if std::ptr::eq(*last_face, face) => buffer.push(ch),
            _ => runs.push((face, resource, is_bold_face, ch.to_string())),
        }
    }
    let _ = writeln!(out, "q");
    let _ = writeln!(out, "BT\n0 Tc\n0 Tw\n100 Tz\n0 Ts");
    let _ = writeln!(
        out,
        "{:.4} {:.4} {:.4} rg",
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0
    );
    // A real italic face (Kaiti) is upright; only synthesize slant when no
    // italic face exists at all.
    let skew = if italic && fonts.italic_font.is_none() {
        0.21
    } else {
        0.0
    };
    let _ = writeln!(out, "1 0 {skew:.4} 1 {x:.3} {baseline:.3} Tm");
    for (face, resource, is_bold_face, run) in runs {
        if bold && !is_bold_face {
            let _ = writeln!(
                out,
                "{:.4} {:.4} {:.4} RG",
                f64::from(r) / 255.0,
                f64::from(g) / 255.0,
                f64::from(b) / 255.0
            );
            let _ = writeln!(out, "{:.3} w", (size * 0.025).max(0.2));
            let _ = writeln!(out, "2 Tr");
        } else {
            let _ = writeln!(out, "0 Tr");
        }
        for (variant, piece) in face.variant_runs(&run) {
            let encoded = face.encode_text(piece);
            let _ = writeln!(out, "/{} {size:.3} Tf\n<{encoded}> Tj", resource[variant]);
        }
    }
    let _ = writeln!(out, "ET");
    let _ = writeln!(out, "Q");
}

fn write_fill_rect(out: &mut String, bbox: BBox) {
    let _ = writeln!(
        out,
        "{:.3} {:.3} {:.3} {:.3} re f",
        bbox.x0,
        bbox.y0,
        bbox.width(),
        bbox.height()
    );
}

fn build_translation_ops(
    page: &PageLayout,
    translated: &[(&Paragraph, &TypesetParagraph)],
    fonts: &TranslationFonts<'_>,
    options: &ComposeOptions,
) -> Result<String> {
    let cover = options.mode == ComposeMode::Overlay;
    let mut out = String::from("q\n");
    write_matrix(&mut out, page_matrix(page).inverse());

    if cover && options.cover_original {
        out.push_str("q\n");
        let _ = writeln!(
            out,
            "{:.4} {:.4} {:.4} rg",
            options.cover_rgb[0], options.cover_rgb[1], options.cover_rgb[2]
        );
        for (paragraph, _) in translated {
            for line in paragraph.lines(&page.lines) {
                write_fill_rect(&mut out, line.bbox.inflate(1.4, 1.2));
            }
        }
        out.push_str("Q\n");
    }

    for (paragraph, typeset_paragraph) in translated {
        for line in &typeset_paragraph.lines {
            for segment in &line.segments {
                write_text_run(&mut out, segment, line, fonts);
            }
            for formula in &line.formulas {
                let scale = formula.scale;
                let dx = formula.x - formula.source_bbox.x0 * scale;
                let dy = formula.baseline - formula.source_baseline * scale;
                let dest = BBox::new(
                    formula.source_bbox.x0 * scale + dx,
                    formula.source_bbox.y0 * scale + dy,
                    formula.source_bbox.x1 * scale + dx,
                    formula.source_bbox.y1 * scale + dy,
                );
                let _ = writeln!(out, "/Span << /ActualText ( ) >> BDC");
                let _ = writeln!(out, "q");
                let _ = writeln!(
                    out,
                    "{:.3} {:.3} {:.3} {:.3} re W n",
                    dest.x0,
                    dest.y0,
                    dest.width(),
                    dest.height()
                );
                let _ = writeln!(out, "{scale:.4} 0 0 {scale:.4} {dx:.3} {dy:.3} cm");
                let name = fonts
                    .names
                    .formulas
                    .get(&paragraph.id)
                    .and_then(|entries| entries.get(&formula.id))
                    .context("inline formula drawing is missing")?;
                let _ = writeln!(out, "/{name} Do");
                let _ = writeln!(out, "Q");
                let _ = writeln!(out, "EMC");
            }
        }
    }

    if options.debug_boxes {
        out.push_str("q\n0 0 1 RG\n0.4 w\n");
        for (paragraph, _) in translated {
            let bbox: BBox = paragraph.bbox;
            let _ = writeln!(
                out,
                "{:.3} {:.3} {:.3} {:.3} re S",
                bbox.x0,
                bbox.y0,
                bbox.width(),
                bbox.height()
            );
        }
        out.push_str("Q\n");
    }

    out.push_str("Q\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Align, Column, Role};
    use lopdf::dictionary;

    fn empty_page() -> PageLayout {
        PageLayout {
            index: 0,
            x0: 0.0,
            y0: 0.0,
            width: 612.0,
            height: 792.0,
            rotation: 0,
            layout_transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            glyph_signature: [0; 32],
            lines: Vec::new(),
            paragraphs: Vec::new(),
            images: Vec::new(),
            rules: Vec::new(),
        }
    }

    fn document_with_annotations() -> (Document, ObjectId, ObjectId, ObjectId) {
        let mut document = Document::with_version("1.7");
        let page_id = document.new_object_id();
        let link_id = document.new_object_id();
        let square_id = document.new_object_id();
        document.objects.insert(
            link_id,
            Object::Dictionary(dictionary! {
                "Type" => "Annot",
                "Subtype" => "Link",
                "Rect" => Object::Array(vec![10.into(), 20.into(), 30.into(), 40.into()]),
            }),
        );
        document.objects.insert(
            square_id,
            Object::Dictionary(dictionary! {
                "Type" => "Annot",
                "Subtype" => "Square",
                "Rect" => Object::Array(vec![10.into(), 20.into(), 30.into(), 40.into()]),
            }),
        );
        document.objects.insert(
            page_id,
            Object::Dictionary(dictionary! {
                "Type" => "Page",
                "Annots" => Object::Array(vec![
                    Object::Reference(link_id),
                    Object::Reference(square_id),
                ]),
            }),
        );
        (document, page_id, link_id, square_id)
    }

    #[test]
    fn annotations_without_translated_text_are_untouched() {
        let (mut document, page_id, link_id, square_id) = document_with_annotations();
        remap_link_annotations(&mut document, page_id, &empty_page(), &[]).unwrap();
        let page = document.get_dictionary(page_id).unwrap();
        let Object::Array(annots) = page.get(b"Annots").unwrap() else {
            panic!("Annots is not an array");
        };
        assert_eq!(annots.len(), 2);
        for id in [link_id, square_id] {
            let rect = document
                .get_dictionary(id)
                .unwrap()
                .get(b"Rect")
                .unwrap()
                .as_array()
                .unwrap();
            assert_eq!(rect[0].as_i64().unwrap(), 10);
            assert_eq!(rect[3].as_i64().unwrap(), 40);
        }
    }

    #[test]
    fn link_rect_maps_proportionally_into_translated_lines() {
        let (mut document, page_id, link_id, square_id) = document_with_annotations();
        // Link covers the top half of the source paragraph.
        document
            .get_object_mut(link_id)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set(
                "Rect",
                Object::Array(vec![100.into(), 150.into(), 200.into(), 200.into()]),
            );
        let paragraph = Paragraph {
            id: "p0-0".into(),
            page: 0,
            bbox: BBox::new(100.0, 100.0, 200.0, 200.0),
            text: String::new(),
            line_indices: Vec::new(),
            base_style: Default::default(),
            font_size: 10.0,
            column: Column::Full,
            role: Role::Paragraph,
            align: Align::Left,
            protected: false,
            has_formula: false,
            formulas: Vec::new(),
            prompt_text: None,
            translation: None,
        };
        let typeset = TypesetParagraph {
            lines: vec![TypesetLine {
                baseline: 140.0,
                size: 10.0,
                bbox: BBox::new(100.0, 100.0, 300.0, 150.0),
                segments: Vec::new(),
                formulas: Vec::new(),
            }],
            scale: 1.0,
            overflow: false,
        };
        let page = empty_page();
        remap_link_annotations(&mut document, page_id, &page, &[(&paragraph, &typeset)]).unwrap();
        let link = document.get_dictionary(link_id).unwrap();
        let rect = link.get(b"Rect").unwrap().as_array().unwrap();
        let values: Vec<f64> = rect
            .iter()
            .map(|value| match value {
                Object::Integer(number) => *number as f64,
                Object::Real(number) => f64::from(*number),
                _ => panic!("unexpected rect entry"),
            })
            .collect();
        // Top half of the source maps to the top half of the wider, shorter
        // translated line box: x stretches 100..300, y maps 150..200 -> 125..150.
        assert_eq!(values, vec![100.0, 125.0, 300.0, 150.0]);
        let square = document.get_dictionary(square_id).unwrap();
        assert_eq!(square.get(b"Rect").unwrap().as_array().unwrap().len(), 4);
    }
}
