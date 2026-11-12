//! Carry PDF font state into extraction without guessing from glyph positions.
use anyhow::{Context, Result, ensure};
use lopdf::{
    Dictionary, Document, Object, ObjectId, Stream,
    content::{Content, Operation},
    dictionary,
};
use std::collections::HashSet;

use crate::pages::PageSelection;
use crate::pdf::content::{MAX_CONTENT_STREAM_BYTES, page_original_content};

pub(super) const FONT_TAG: &[u8] = b"PdfTranslateFont";

/// Add non-painting marked-content scopes to a temporary interpretation copy.
/// Text matrices, cursor advances, glyph order and the source PDF stay intact.
pub(super) fn tagged_pdf(source: &Document, selection: &PageSelection) -> Result<Vec<u8>> {
    let mut document = source.clone();
    let pages = document.get_pages();
    let mut tagger = FontTagger {
        document: &mut document,
        graphic_forms: HashSet::new(),
        visiting: HashSet::new(),
        forms: 0,
    };
    for (_, page) in pages
        .iter()
        .filter(|(number, _)| selection.contains(**number as usize))
    {
        let (bytes, resources) = page_original_content(tagger.document, *page)?;
        let (bytes, resources) = tagger.stream(&bytes, resources, None, 0)?;
        let content = tagger
            .document
            .add_object(Stream::new(dictionary! {}, bytes));
        let page = tagger.document.get_dictionary_mut(*page)?;
        page.set("Contents", content);
        page.set("Resources", resources);
    }
    let mut bytes = Vec::new();
    document.save_to(&mut bytes)?;
    Ok(bytes)
}

struct FontTagger<'a> {
    document: &'a mut Document,
    graphic_forms: HashSet<ObjectId>,
    visiting: HashSet<ObjectId>,
    forms: usize,
}

impl FontTagger<'_> {
    fn dictionary(&self, value: Option<&Object>) -> Option<Dictionary> {
        self.document
            .dereference(value?)
            .ok()?
            .1
            .as_dict()
            .ok()
            .cloned()
    }

    fn stream(
        &mut self,
        bytes: &[u8],
        mut resources: Dictionary,
        inherited_font: Option<String>,
        depth: usize,
    ) -> Result<(Vec<u8>, Dictionary)> {
        ensure!(depth <= 64, "font extraction Form nesting exceeds 64");
        let fonts = self
            .dictionary(resources.get(b"Font").ok())
            .unwrap_or_default();
        let original_xobjects = self
            .dictionary(resources.get(b"XObject").ok())
            .unwrap_or_default();
        let mut xobjects = original_xobjects.clone();
        let mut current_font = inherited_font;
        let mut saved_fonts = Vec::new();
        let mut output = Vec::new();
        for mut operation in Content::decode(bytes)?.operations {
            match operation.operator.as_str() {
                "q" => saved_fonts.push(current_font.clone()),
                "Q" => current_font = saved_fonts.pop().flatten(),
                "Tf" => {
                    current_font = operation
                        .operands
                        .first()
                        .and_then(|o| o.as_name().ok())
                        .and_then(|name| self.dictionary(fonts.get(name).ok()))
                        .and_then(|font| {
                            font.get(b"BaseFont")
                                .ok()?
                                .as_name()
                                .ok()
                                .map(|n| String::from_utf8_lossy(n).into_owned())
                        });
                }
                "Do" => {
                    if let Some(name) = operation.operands.first().and_then(|o| o.as_name().ok())
                        && let Ok(reference) = original_xobjects.get(name)
                        && let Ok(id) = reference.as_reference()
                        && !self.graphic_forms.contains(&id)
                    {
                        let source = self.document.get_object(id)?.as_stream()?;
                        if source
                            .dict
                            .get(b"Subtype")
                            .and_then(Object::as_name)
                            .is_ok_and(|kind| kind == b"Form")
                        {
                            let source = source.clone();
                            let data =
                                source.get_plain_content_with_limit(MAX_CONTENT_STREAM_BYTES)?;
                            if Content::decode(&data)?.operations.iter().any(|op| {
                                matches!(op.operator.as_str(), "Tj" | "TJ" | "'" | "\"" | "Do")
                            }) {
                                self.forms += 1;
                                ensure!(
                                    self.forms <= 10_000,
                                    "too many text Form invocations for font extraction"
                                );
                                ensure!(self.visiting.insert(id), "cyclic Form in font extraction");
                                let child_resources = self
                                    .dictionary(source.dict.get(b"Resources").ok())
                                    .unwrap_or_else(|| resources.clone());
                                let (data, child_resources) = self.stream(
                                    &data,
                                    child_resources,
                                    current_font.clone(),
                                    depth + 1,
                                )?;
                                self.visiting.remove(&id);
                                let mut form = source;
                                form.set_plain_content(data);
                                form.dict.set("Resources", child_resources);
                                let child = self.document.add_object(form);
                                let mut name = format!("PdfTranslateFontForm{}", child.0);
                                while xobjects.has(name.as_bytes()) {
                                    name.push('_');
                                }
                                xobjects.set(name.clone(), child);
                                operation.operands[0] = Object::Name(name.into_bytes());
                            } else {
                                self.graphic_forms.insert(id);
                            }
                        }
                    }
                }
                _ => {}
            }
            if matches!(operation.operator.as_str(), "Tj" | "TJ" | "'" | "\"")
                && let Some(font) = &current_font
            {
                output.push(Operation::new(
                    "BDC",
                    vec![
                        Object::Name(FONT_TAG.to_vec()),
                        dictionary! {"Name" => Object::string_literal(font.as_bytes())}.into(),
                    ],
                ));
                output.push(operation);
                output.push(Operation::new("EMC", vec![]));
            } else {
                output.push(operation);
            }
        }
        resources.set("XObject", xobjects);
        Ok((
            Content { operations: output }
                .encode()
                .context("encoding font scopes")?,
            resources,
        ))
    }
}
