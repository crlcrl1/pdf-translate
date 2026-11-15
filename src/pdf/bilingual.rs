//! Compose a bilingual PDF: each output page contains the original page on the
//! left and the translated page on the right.
mod links;

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use lopdf::{Document, Object, ObjectId, Stream, dictionary};

use super::content::page_original_content;
use hayro::kurbo::{Affine, Rect};

const GUTTER: f64 = 12.0;

/// Write a side-by-side PDF from an original PDF and a translated PDF with the
/// same page count and page geometry.
pub fn write_bilingual_pdf(
    original_path: &Path,
    translated_path: &Path,
    output_path: &Path,
) -> Result<()> {
    let mut document = super::preflight::load_pdf(original_path)?.document;
    let original_pages = document.get_pages();
    let mut translated = super::preflight::load_pdf(translated_path)?.document;
    let translated_pages = translated.get_pages();
    if original_pages.len() != translated_pages.len() {
        bail!(
            "bilingual output needs matching page counts ({} original, {} translated)",
            original_pages.len(),
            translated_pages.len()
        );
    }

    // Import the translated page objects, resources and fonts into the
    // original document with non-overlapping object IDs.
    let offset = document.max_id + 1;
    translated.renumber_objects_with(offset);
    let translated_pages = translated.get_pages();
    let original_destinations = links::named_destinations(&document)?;
    let translated_destinations = links::named_destinations(&translated)?;
    let translated_max_id = translated.max_id;
    document.objects.extend(translated.objects);
    document.max_id = document.max_id.max(translated_max_id);

    let mut new_pages = Vec::with_capacity(original_pages.len());
    let mut placements = HashMap::new();
    for (page_number, original_id) in &original_pages {
        let translated_id = *translated_pages
            .get(page_number)
            .with_context(|| format!("translated PDF is missing page {page_number}"))?;
        let original_size = page_size(&document, *original_id)?;
        let translated_size = page_size(&document, translated_id)?;
        if (original_size.0 - translated_size.0).abs() > 0.5
            || (original_size.1 - translated_size.1).abs() > 0.5
        {
            bail!("page {page_number} has different sizes in the original and translated PDF");
        }

        let original_form = page_form(&mut document, *original_id)?;
        let translated_form = page_form(&mut document, translated_id)?;
        let (width, height) = original_size;
        let content = format!(
            "q 1 0 0 1 0 0 cm /PTOrig Do Q\nq 1 0 0 1 {x:.3} 0 cm /PTTrans Do Q\n",
            x = width + GUTTER
        );
        let content_id = document.add_object(Stream::new(dictionary! {}, content.into_bytes()));
        let resources = dictionary! {
            "XObject" => dictionary! {
                "PTOrig" => original_form,
                "PTTrans" => translated_form,
            },
        };
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "MediaBox" => vec![
                0.into(),
                0.into(),
                (width * 2.0 + GUTTER).into(),
                height.into(),
            ],
            "Resources" => resources,
            "Contents" => content_id,
        });
        for (source_id, x) in [(*original_id, 0.0), (translated_id, width + GUTTER)] {
            placements.insert(
                source_id,
                links::PagePlacement {
                    page: page_id,
                    transform: Affine::translate((x, 0.0)) * page_transform(&document, source_id)?,
                    bounds: Rect::new(x, 0.0, x + width, height),
                },
            );
        }
        new_pages.push(page_id);
    }

    // Resolve links only once every destination page has its final object ID.
    // Each source has its own named destinations, even when their names match.
    for (page_number, original_id) in &original_pages {
        let translated_id = translated_pages[page_number];
        let mut annots = links::copy_page_links(
            &mut document,
            *original_id,
            &placements,
            &original_destinations,
        )?;
        annots.extend(links::copy_page_links(
            &mut document,
            translated_id,
            &placements,
            &translated_destinations,
        )?);
        if !annots.is_empty() {
            document
                .get_dictionary_mut(placements[original_id].page)?
                .set("Annots", annots);
        }
    }

    let pages_id = document.add_object(dictionary! {
        "Type" => "Pages",
        "Kids" => new_pages
            .iter()
            .map(|id| Object::Reference(*id))
            .collect::<Vec<_>>(),
        "Count" => new_pages.len() as i64,
    });
    for page_id in &new_pages {
        document
            .get_dictionary_mut(*page_id)?
            .set("Parent", pages_id);
    }
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    document.prune_objects();

    crate::output::atomic_write(output_path, |file| Ok(document.save_to(file)?))
}

/// Wrap one page's content and resources in a Form XObject. The page's crop
/// box is translated to the origin so the form can be placed anywhere.
fn page_form(document: &mut Document, page_id: ObjectId) -> Result<ObjectId> {
    let (content, resources) = page_original_content(document, page_id)?;
    let (x0, y0, x1, y1) = page_box(document, page_id)?;
    let mut form = dictionary! {
        "Type" => "XObject",
        "Subtype" => "Form",
        "BBox" => vec![x0.into(), y0.into(), x1.into(), y1.into()],
        "Resources" => Object::Dictionary(resources),
        "Matrix" => page_transform(document, page_id)?.as_coeffs().into_iter().map(Object::from).collect::<Vec<_>>(),
    };
    if let Some(group) = inherited_page_attribute(document, page_id, b"Group")? {
        form.set("Group", group);
    }
    Ok(document.add_object(Stream::new(form, content)))
}

fn page_size(document: &Document, page_id: ObjectId) -> Result<(f64, f64)> {
    let (x0, y0, x1, y1) = page_box(document, page_id)?;
    let bounds = page_transform(document, page_id)?.transform_rect_bbox(Rect::new(x0, y0, x1, y1));
    Ok((bounds.width(), bounds.height()))
}

fn page_transform(document: &Document, page_id: ObjectId) -> Result<Affine> {
    let (x0, y0, x1, y1) = page_box(document, page_id)?;
    let rotation = inherited_page_attribute(document, page_id, b"Rotate")?
        .and_then(|value| {
            document
                .dereference(&value)
                .ok()
                .and_then(|(_, v)| v.as_i64().ok())
        })
        .unwrap_or(0)
        .rem_euclid(360);
    if rotation % 90 != 0 {
        bail!("page rotation must be a multiple of 90 degrees");
    }
    let transform = super::geometry::layout_transform(Rect::new(x0, y0, x1, y1), rotation as i32);
    let bounds = transform.transform_rect_bbox(Rect::new(x0, y0, x1, y1));
    Ok(Affine::translate((-bounds.x0, -bounds.y0)) * transform)
}

fn page_box(document: &Document, page_id: ObjectId) -> Result<(f64, f64, f64, f64)> {
    let media = inherited_page_attribute(document, page_id, b"MediaBox")?
        .context("page has no MediaBox")?;
    let crop = inherited_page_attribute(document, page_id, b"CropBox")?.unwrap_or(media);
    let array = document
        .dereference(&crop)?
        .1
        .as_array()
        .context("page box is not an array")?;
    if array.len() != 4 {
        bail!("page box must have four numbers");
    }
    let value = |object: &Object| -> Result<f64> {
        if let Ok(value) = object.as_i64() {
            return Ok(value as f64);
        }
        Ok(object.as_float()? as f64)
    };
    Ok((
        value(&array[0])?,
        value(&array[1])?,
        value(&array[2])?,
        value(&array[3])?,
    ))
}

/// Resolve an inheritable page attribute, including values on parent Pages
/// nodes. The returned object may contain references into `document`.
fn inherited_page_attribute(
    document: &Document,
    page_id: ObjectId,
    key: &[u8],
) -> Result<Option<Object>> {
    let mut current = Some(page_id);
    let mut visited = std::collections::HashSet::new();
    while let Some(id) = current {
        if !visited.insert(id) {
            bail!("page tree contains a cycle");
        }
        let node = document.get_dictionary(id)?;
        if let Ok(value) = node.get(key) {
            match document.dereference(value) {
                Ok((_, Object::Null)) | Err(_) => {}
                Ok(_) => return Ok(Some(value.clone())),
            }
        }
        current = match node.get(b"Parent") {
            Ok(Object::Reference(parent)) => Some(*parent),
            _ => None,
        };
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Document, Object, Stream, dictionary};
    use std::path::Path;

    fn one_page_pdf(path: &Path, text: &str) {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let font_id = document.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        });
        let resources = dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        };
        let content = format!("BT /F1 12 Tf 72 700 Td ({text}) Tj ET");
        let content_id = document.add_object(Stream::new(dictionary! {}, content.into_bytes()));
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => resources,
            "Contents" => content_id,
        });
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);
        document.save(path).unwrap();
    }

    #[test]
    fn composes_original_and_translation_side_by_side() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.pdf");
        let translated = dir.path().join("translated.pdf");
        let output = dir.path().join("bilingual.pdf");
        one_page_pdf(&original, "Original");
        one_page_pdf(&translated, "Translation");

        write_bilingual_pdf(&original, &translated, &output).unwrap();

        let document = Document::load(&output).unwrap();
        let pages = document.get_pages();
        assert_eq!(pages.len(), 1);
        let page = document.get_dictionary(pages[&1]).unwrap();
        let media = page.get(b"MediaBox").unwrap().as_array().unwrap();
        let width = media[2]
            .as_float()
            .map(f64::from)
            .or_else(|_| media[2].as_i64().map(|value| value as f64))
            .unwrap();
        assert!((width - (612.0 * 2.0 + GUTTER)).abs() < 0.1);
        let content = document.get_page_content(pages[&1]);
        assert!(
            content
                .array_windows()
                .any(|window| window == b"/PTOrig Do")
        );
        assert!(
            content
                .array_windows()
                .any(|window| window == b"/PTTrans Do")
        );
    }

    #[test]
    fn copies_links_and_resolves_named_targets_on_each_side() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original.pdf");
        let translated = dir.path().join("translated.pdf");
        let output = dir.path().join("bilingual.pdf");
        for (path, x) in [(&original, 72), (&translated, 90)] {
            one_page_pdf(path, "Links");
            let mut doc = Document::load(path).unwrap();
            let first = doc.get_pages()[&1];
            let second = doc.add_object(doc.get_dictionary(first).unwrap().clone());
            let pages = doc
                .get_dictionary(first)
                .unwrap()
                .get(b"Parent")
                .unwrap()
                .as_reference()
                .unwrap();
            doc.get_dictionary_mut(pages)
                .unwrap()
                .set("Kids", vec![first.into(), second.into()]);
            doc.get_dictionary_mut(pages).unwrap().set("Count", 2);
            let target = vec![
                second.into(),
                "XYZ".into(),
                x.into(),
                650.into(),
                Object::Null,
            ];
            let leaf = doc.add_object(dictionary! {
                "Names" => vec![Object::string_literal("reference"), Object::Array(target.clone())],
            });
            doc.catalog_mut().unwrap().set(
                "Names",
                dictionary! {
                    "Dests" => dictionary! { "Kids" => vec![Object::Reference(leaf)] },
                },
            );
            doc.catalog_mut().unwrap().set(
                "Dests",
                dictionary! {
                    "legacy" => dictionary! { "D" => target.clone() },
                },
            );
            let uri = doc.add_object(dictionary! {
                "Subtype" => "Link", "Rect" => vec![x.into(), 700.into(), (x + 40).into(), 712.into()], "P" => first,
                "A" => dictionary! { "S" => "URI", "URI" => Object::string_literal("https://example.org/paper") },
            });
            let goto = doc.add_object(dictionary! {
                "S" => "GoTo", "D" => Object::string_literal("reference"),
            });
            let internal = doc.add_object(dictionary! {
                "Subtype" => "Link", "Rect" => vec![x.into(), 680.into(), (x + 40).into(), 692.into()], "A" => goto,
            });
            let direct = Object::Dictionary(dictionary! {
                "Subtype" => "Link", "Rect" => vec![x.into(), 660.into(), (x + 40).into(), 672.into()], "Dest" => "legacy",
            });
            let annots = doc.add_object(vec![uri.into(), internal.into(), direct]);
            doc.get_dictionary_mut(first).unwrap().set("Annots", annots);
            doc.save(path).unwrap();
        }

        write_bilingual_pdf(&original, &translated, &output).unwrap();
        let doc = Document::load(output).unwrap();
        let pages = doc.get_pages();
        let annots = doc
            .get_dictionary(pages[&1])
            .unwrap()
            .get(b"Annots")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(annots.len(), 6);
        for (side, links) in annots.as_chunks::<3>().0.iter().enumerate() {
            let x = if side == 0 {
                72.0
            } else {
                90.0 + 612.0 + GUTTER
            };
            for (index, link) in links.iter().enumerate() {
                let link = doc.dereference(link).unwrap().1.as_dict().unwrap();
                assert_eq!(link.get(b"P").unwrap().as_reference().unwrap(), pages[&1]);
                assert_eq!(
                    link.get(b"Rect").unwrap().as_array().unwrap()[0]
                        .as_float()
                        .unwrap(),
                    x as f32
                );
                let target = if index == 2 {
                    link.get(b"Dest").unwrap()
                } else {
                    let action = link.get_deref(b"A", &doc).unwrap().as_dict().unwrap();
                    if index == 0 {
                        assert_eq!(
                            action.get(b"URI").unwrap().as_str().unwrap(),
                            b"https://example.org/paper"
                        );
                        continue;
                    }
                    action.get(b"D").unwrap()
                }
                .as_array()
                .unwrap();
                assert_eq!(target[0].as_reference().unwrap(), pages[&2]);
                assert_eq!(target[2].as_float().unwrap(), x as f32);
                assert_eq!(target[3].as_float().unwrap(), 650.0);
            }
        }
        // No link may keep the obsolete source page trees reachable.
        assert_eq!(
            doc.objects
                .values()
                .filter(|object| object.type_name().ok() == Some(b"Page".as_slice()))
                .count(),
            2
        );
    }

    #[test]
    fn rotated_cropped_pages_render_upright_on_both_sides() {
        for rotation in [0, 90, 180, 270] {
            let dir = tempfile::tempdir().unwrap();
            let input = dir.path().join("input.pdf");
            let output = dir.path().join("bilingual.pdf");
            one_page_pdf(&input, "Rotation and crop");
            let mut document = Document::load(&input).unwrap();
            let page = document.get_pages()[&1];
            let parent = document
                .get_dictionary(page)
                .unwrap()
                .get(b"Parent")
                .unwrap()
                .as_reference()
                .unwrap();
            let dict = document.get_dictionary_mut(parent).unwrap();
            dict.set("Rotate", rotation);
            dict.set(
                "CropBox",
                vec![20.into(), 30.into(), 592.into(), 762.into()],
            );
            let link = document.add_object(dictionary! {
                "Subtype" => "Link", "P" => page,
                "Rect" => vec![72.into(), 700.into(), 112.into(), 712.into()],
                "QuadPoints" => vec![72.into(), 712.into(), 112.into(), 712.into(), 72.into(), 700.into(), 112.into(), 700.into()],
                "Border" => vec![0.into(), 0.into(), 0.into()],
                "Dest" => vec![page.into(), "XYZ".into(), 90.into(), 650.into(), Object::Null],
            });
            document
                .get_dictionary_mut(page)
                .unwrap()
                .set("Annots", vec![Object::Reference(link)]);
            document.save(&input).unwrap();
            write_bilingual_pdf(&input, &input, &output).unwrap();
            let output_doc = Document::load(&output).unwrap();
            let output_page = output_doc.get_pages()[&1];
            let output_links = output_doc.get_page_annotations(output_page).unwrap();
            assert_eq!(output_links.len(), 2);
            let (expected_rect, expected_point, expected_quad) = match rotation {
                0 => (
                    [52., 670., 92., 682.],
                    [70., 620.],
                    [52., 682., 92., 682., 52., 670., 92., 670.],
                ),
                90 => (
                    [670., 480., 682., 520.],
                    [620., 502.],
                    [682., 520., 682., 480., 670., 520., 670., 480.],
                ),
                180 => (
                    [480., 50., 520., 62.],
                    [502., 112.],
                    [520., 50., 480., 50., 520., 62., 480., 62.],
                ),
                _ => (
                    [50., 52., 62., 92.],
                    [112., 70.],
                    [50., 52., 50., 92., 62., 52., 62., 92.],
                ),
            };
            for (side, link) in output_links.iter().enumerate() {
                let offset = side as f64 * (if rotation % 180 == 0 { 572. } else { 732. } + GUTTER);
                for (key, expected) in [
                    (b"Rect".as_slice(), expected_rect.as_slice()),
                    (b"QuadPoints".as_slice(), expected_quad.as_slice()),
                ] {
                    let actual = link.get(key).unwrap().as_array().unwrap();
                    for (index, expected) in expected.iter().enumerate() {
                        assert_eq!(
                            f64::from(actual[index].as_float().unwrap()),
                            expected + if index % 2 == 0 { offset } else { 0. }
                        );
                    }
                }
                let dest = link.get(b"Dest").unwrap().as_array().unwrap();
                assert_eq!(dest[0].as_reference().unwrap(), output_page);
                assert_eq!(
                    f64::from(dest[2].as_float().unwrap()),
                    expected_point[0] + offset
                );
                assert_eq!(f64::from(dest[3].as_float().unwrap()), expected_point[1]);
            }
            let render = |path: &Path| {
                let pdf = hayro::hayro_syntax::Pdf::new(std::fs::read(path).unwrap()).unwrap();
                hayro::render(
                    &pdf.pages()[0],
                    &hayro::RenderCache::new(),
                    &Default::default(),
                    &Default::default(),
                    &Default::default(),
                )
            };
            let source = render(&input);
            let combined = render(&output);
            let width = source.width() as usize;
            assert_eq!(combined.width() as usize, width * 2 + GUTTER as usize);
            assert_eq!(combined.height(), source.height());
            for (left, row) in source.data_as_u8_slice().chunks_exact(width * 4).zip(
                combined
                    .data_as_u8_slice()
                    .chunks_exact(combined.width() as usize * 4),
            ) {
                assert_eq!(
                    left,
                    &row[..width * 4],
                    "rotation {rotation}, original side"
                );
                assert_eq!(
                    left,
                    &row[(width + GUTTER as usize) * 4..],
                    "rotation {rotation}, translated side"
                );
            }
        }
    }
}
