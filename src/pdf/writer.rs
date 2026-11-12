use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use hayro::kurbo::Affine;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream, dictionary};

use crate::model::{BBox, PageLayout, Paragraph};
use crate::pdf::content::page_resources_clone;
use crate::pdf::font::EmbeddedFont;
use crate::typeset::{TypesetDocument, TypesetLine, TypesetParagraph, TypesetSegment};

pub const FONT_RESOURCE_NAME: &str = "Fpdfzh";
pub const BOLD_FONT_RESOURCE_NAME: &str = "FpdfzhB";

struct ResourceNames {
    regular: Vec<String>,
    bold: Vec<String>,
    formulas: HashMap<String, HashMap<String, String>>,
}

impl ResourceNames {
    fn for_page(
        document: &Document,
        page_id: ObjectId,
        regular_count: usize,
        bold_count: usize,
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
    let font_ids = add_embedded_font(&mut document, font)?;
    let bold_font_ids = match bold_font {
        Some(bold) => add_embedded_font(&mut document, bold)?,
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

        let mut names =
            ResourceNames::for_page(&document, page_id, font_ids.len(), bold_font_ids.len())?;
        install_fonts(&mut document, page_id, &names, &font_ids, &bold_font_ids)?;
        if !options.keep_annotations {
            remove_link_annotations(&mut document, page_id)?;
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
        let content = build_translation_ops(page, &translated, font, bold_font, &names, options)?;
        append_page_content(&mut document, page_id, content.into_bytes())?;
    }
    // Saving a rewritten page alone leaves detached source streams in lopdf's
    // object map. Remove them physically, including old shared Form copies.
    document.prune_objects();

    crate::output::atomic_write(output, |file| Ok(document.save_to(file)?))
}

/// Remove only link annotations. Other annotations (form widgets, comments,
/// redaction marks, signatures, ...) are preserved because deleting them can
/// lose data or expose content that was intentionally covered.
fn remove_link_annotations(document: &mut Document, page_id: ObjectId) -> Result<()> {
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
        if is_link_annotation(document, &annot) {
            changed = true;
        } else {
            kept.push(annot);
        }
    }
    if !changed {
        return Ok(());
    }

    let page = document.get_object_mut(page_id)?.as_dict_mut()?;
    if kept.is_empty() {
        page.remove(b"Annots");
    } else {
        page.set("Annots", Object::Array(kept));
    }
    Ok(())
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

fn add_embedded_font(document: &mut Document, font: &EmbeddedFont) -> Result<Vec<ObjectId>> {
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
    let mut descriptor = dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => Object::Name(font.pdf_name.as_bytes().to_vec()),
            "Flags" => 4,
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
) -> Result<()> {
    for (name, id) in names
        .regular
        .iter()
        .zip(regular)
        .chain(names.bold.iter().zip(bold))
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

fn write_text_run(
    out: &mut String,
    segment: &TypesetSegment,
    line: &TypesetLine,
    font: &EmbeddedFont,
    bold_font: Option<&EmbeddedFont>,
    names: &ResourceNames,
) {
    let TypesetSegment {
        text,
        x,
        bold,
        italic,
    } = segment;
    let (bold, italic) = (*bold, *italic);
    let (size, baseline, color) = (line.size, line.baseline, line.color);
    if text.is_empty() {
        return;
    }
    let use_bold_font = bold && bold_font.is_some();
    let active_font = bold_font.filter(|_| bold).unwrap_or(font);
    let [r, g, b, _] = color;
    let resource = if use_bold_font {
        &names.bold
    } else {
        &names.regular
    };
    let _ = writeln!(out, "q");
    let _ = writeln!(out, "BT\n0 Tr\n0 Tc\n0 Tw\n100 Tz\n0 Ts");
    let _ = writeln!(
        out,
        "{:.4} {:.4} {:.4} rg",
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0
    );
    if bold && !use_bold_font {
        let _ = writeln!(
            out,
            "{:.4} {:.4} {:.4} RG",
            f64::from(r) / 255.0,
            f64::from(g) / 255.0,
            f64::from(b) / 255.0
        );
        let _ = writeln!(out, "{:.3} w", (size * 0.025).max(0.2));
        let _ = writeln!(out, "2 Tr");
    }
    let skew = if italic { 0.21 } else { 0.0 };
    let _ = writeln!(out, "1 0 {skew:.4} 1 {x:.3} {baseline:.3} Tm");
    for (variant, run) in active_font.variant_runs(text) {
        let encoded = active_font.encode_text(run);
        let _ = writeln!(out, "/{} {size:.3} Tf\n<{encoded}> Tj", resource[variant]);
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
    font: &EmbeddedFont,
    bold_font: Option<&EmbeddedFont>,
    names: &ResourceNames,
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
                write_text_run(&mut out, segment, line, font, bold_font, names);
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
                let name = names
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
    use lopdf::dictionary;

    #[test]
    fn only_link_annotations_are_removed() {
        let mut document = Document::with_version("1.7");
        let page_id = document.new_object_id();
        let link_id = document.new_object_id();
        let square_id = document.new_object_id();
        document.objects.insert(
            link_id,
            Object::Dictionary(dictionary! {
                "Type" => "Annot",
                "Subtype" => "Link",
            }),
        );
        document.objects.insert(
            square_id,
            Object::Dictionary(dictionary! {
                "Type" => "Annot",
                "Subtype" => "Square",
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

        remove_link_annotations(&mut document, page_id).unwrap();
        let page = document.get_dictionary(page_id).unwrap();
        let Object::Array(annots) = page.get(b"Annots").unwrap() else {
            panic!("Annots is not an array");
        };
        assert_eq!(annots.len(), 1);
        assert!(matches!(annots[0], Object::Reference(id) if id == square_id));
    }
}
