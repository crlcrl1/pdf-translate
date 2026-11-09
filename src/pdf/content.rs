use anyhow::{Context, Result};
use lopdf::content::Content;
use lopdf::{Dictionary, Document, ObjectId};

/// Upper bound for a decoded content stream. Keeps malformed or malicious
/// compression bombs from allocating unbounded memory while still being far
/// larger than any normal page content stream.
pub const MAX_CONTENT_STREAM_BYTES: usize = 64 * 1024 * 1024;

/// A text-show operation start position and the BaseFont used there.
pub struct TextFontRun {
    pub x: f64,
    pub y: f64,
    pub font: String,
}

/// Collect the BaseFont active at each text-show operation. Positions are in
/// page space and let extraction attach font names to glyphs, including Type 1
/// fonts whose metadata hayro does not expose.
pub fn text_font_runs(
    content: &[u8],
    fonts: &std::collections::HashMap<Vec<u8>, String>,
) -> Result<Vec<TextFontRun>> {
    let parsed = Content::decode(content).context("failed to decode content stream")?;
    let mut ctm = Mat::IDENTITY;
    let mut stack: Vec<Mat> = Vec::new();
    let mut text_matrix = Mat::IDENTITY;
    let mut leading = 0.0;
    let mut in_text = false;
    let mut current_font: Option<String> = None;
    let mut runs = Vec::new();

    for operation in parsed.operations {
        let op = operation.operator.as_str();
        if !in_text {
            match op {
                "BT" => {
                    in_text = true;
                    text_matrix = Mat::IDENTITY;
                    current_font = None;
                }
                "q" => stack.push(ctm),
                "Q" => ctm = stack.pop().unwrap_or(Mat::IDENTITY),
                "cm" => {
                    if let Some(matrix) = matrix_operands(&operation) {
                        ctm = ctm.mul(matrix);
                    }
                }
                _ => {}
            }
            continue;
        }

        match op {
            "ET" => in_text = false,
            "q" => stack.push(ctm),
            "Q" => ctm = stack.pop().unwrap_or(Mat::IDENTITY),
            "cm" => {
                if let Some(matrix) = matrix_operands(&operation) {
                    ctm = ctm.mul(matrix);
                }
            }
            "Tf" => {
                if let Some(lopdf::Object::Name(name)) = operation.operands.first() {
                    current_font = fonts.get(name).cloned();
                }
            }
            "Tm" => {
                if let Some(matrix) = matrix_operands(&operation) {
                    text_matrix = matrix;
                }
            }
            "Td" | "TD" => {
                if let Some((tx, ty)) = pair_operands(&operation) {
                    text_matrix = text_matrix.translate(tx, ty);
                    if op == "TD" {
                        leading = -ty;
                    }
                }
            }
            "T*" => text_matrix = text_matrix.translate(0.0, -leading),
            "TL" => {
                if let Some(value) = operation
                    .operands
                    .first()
                    .and_then(|object| object.as_float().ok())
                {
                    leading = f64::from(value);
                }
            }
            "Tj" | "TJ" | "'" | "\"" => {
                if op == "'" || op == "\"" {
                    text_matrix = text_matrix.translate(0.0, -leading);
                }
                if let Some(font) = &current_font {
                    let origin = ctm.mul(text_matrix).origin();
                    runs.push(TextFontRun {
                        x: origin.0,
                        y: origin.1,
                        font: font.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(runs)
}

fn pair_operands(operation: &lopdf::content::Operation) -> Option<(f64, f64)> {
    if operation.operands.len() != 2 {
        return None;
    }
    Some((
        f64::from(operation.operands[0].as_float().ok()?),
        f64::from(operation.operands[1].as_float().ok()?),
    ))
}

fn matrix_operands(operation: &lopdf::content::Operation) -> Option<Mat> {
    if operation.operands.len() != 6 {
        return None;
    }
    let mut values = [0.0; 6];
    for (index, value) in operation.operands.iter().enumerate() {
        values[index] = f64::from(value.as_float().ok()?);
    }
    Some(Mat(values))
}

#[derive(Clone, Copy)]
struct Mat([f64; 6]);

impl Mat {
    const IDENTITY: Self = Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    fn mul(self, other: Self) -> Self {
        let [a, b, c, d, e, f] = self.0;
        let [g, h, i, j, k, l] = other.0;
        Self([
            a * g + b * i,
            a * h + b * j,
            c * g + d * i,
            c * h + d * j,
            e * g + f * i + k,
            e * h + f * j + l,
        ])
    }

    fn translate(self, tx: f64, ty: f64) -> Self {
        self.mul(Self([1.0, 0.0, 0.0, 1.0, tx, ty]))
    }

    fn origin(self) -> (f64, f64) {
        (self.0[4], self.0[5])
    }
}

pub(super) fn page_resources_clone(document: &Document, page_id: ObjectId) -> Result<Dictionary> {
    let (direct_resources, resource_refs) = document.get_page_resources(page_id)?;
    if let Some(resources) = direct_resources {
        return Ok(resources.clone());
    }
    if let Some(reference) = resource_refs.first() {
        return Ok(document.get_dictionary(*reference)?.clone());
    }
    Ok(Dictionary::new())
}

pub(super) fn page_original_content(
    document: &Document,
    page_id: ObjectId,
) -> Result<(Vec<u8>, Dictionary)> {
    let mut content = Vec::new();
    for stream_id in document.get_page_contents(page_id) {
        let stream = document.get_object(stream_id)?.as_stream()?;
        content.extend_from_slice(&stream.get_plain_content_with_limit(MAX_CONTENT_STREAM_BYTES)?);
        content.push(b'\n');
    }
    let resources = page_resources_clone(document, page_id)?;
    Ok((content, resources))
}
