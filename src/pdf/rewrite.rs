//! Rewrite text in place, using an instrumented, in-memory interpretation pass.
//! The probe PDF is never saved to the output. No original-page snapshots are made.
mod instrument;
mod trace;

use crate::model::{BBox, PageLayout};
use crate::typeset::TypesetDocument;
use anyhow::{Context as _, Result, ensure};
use hayro::hayro_interpret::InterpreterCache;
use hayro::hayro_syntax::Pdf;
use hayro::kurbo::{Affine, BezPath, PathEl, Point};
use instrument::{Instrumenter, probe_font};
use lopdf::content::{Content, Operation};
use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use trace::trace_page;

const TRACE_TAG: &[u8] = b"PdfTranslateInternalTrace";
const MAX_DEPTH: usize = 40;
const MAX_FORM_INVOCATIONS: usize = 10_000;
const MAX_TEXT_OPERATIONS: usize = 200_000;

pub struct FormulaDrawing {
    pub bbox: BBox,
    pub paragraph: String,
    pub id: String,
    pub content: String,
}

struct Step {
    operation: Operation,
    call: Option<usize>,
    rise: f64,
    scopes: Vec<usize>,
    child: Option<ObjectId>,
}

struct Plan {
    page: usize,
    stream: ObjectId,
    steps: Vec<Step>,
    resources: Dictionary,
    is_page: bool,
}

#[derive(Default)]
struct Call {
    matrix: Option<Affine>,
    glyphs: Vec<RecordedGlyph>,
    vertical: bool,
    recorded: bool,
}

struct RecordedGlyph {
    code: u32,
    origin: Point,
    remove: bool,
}

fn resolved_dict(doc: &Document, object: Option<&Object>) -> Option<Dictionary> {
    match object? {
        Object::Dictionary(dict) => Some(dict.clone()),
        Object::Reference(id) => doc.get_dictionary(*id).ok().cloned(),
        _ => None,
    }
}

type Drawings = HashMap<usize, Vec<FormulaDrawing>>;

pub fn outline_formulas(
    doc: &mut Document,
    pages: &[PageLayout],
    typeset: &TypesetDocument,
) -> Result<Drawings> {
    let selected: Vec<_> = pages
        .iter()
        .filter(|page| {
            page.paragraphs
                .iter()
                .any(|p| !p.formulas.is_empty() && typeset.paragraphs.contains_key(&p.id))
        })
        .collect();
    collect_traces(doc, &selected, typeset, None)
}

pub fn remove_translated(
    doc: &mut Document,
    pages: &[PageLayout],
    typeset: &TypesetDocument,
) -> Result<Drawings> {
    let selected: Vec<_> = pages
        .iter()
        .filter(|page| {
            page.paragraphs
                .iter()
                .any(|p| typeset.paragraphs.contains_key(&p.id))
        })
        .collect();
    let page_ids = doc.get_pages();
    let probe = probe_font(doc);
    let mut builder = Instrumenter::new(doc, probe);
    for page in &selected {
        let page_id = *page_ids
            .get(&(page.index as u32 + 1))
            .context("missing source page")?;
        builder.page(page.index, page_id)?;
    }
    let mut calls: Vec<_> = (0..builder.calls).map(|_| Call::default()).collect();
    let plans = builder.plans;
    let drawings = collect_traces(doc, &selected, typeset, Some(&mut calls))?;
    // Plans are postorder: an edited Form invalidates marked content in its parent.
    let mut modified_streams = HashSet::new();
    for plan in plans {
        let page_id = page_ids[&(plan.page as u32 + 1)];
        apply_plan(doc, plan, page_id, probe, &calls, &mut modified_streams)?;
    }
    Ok(drawings)
}

fn collect_traces(
    doc: &mut Document,
    pages: &[&PageLayout],
    typeset: &TypesetDocument,
    mut calls: Option<&mut [Call]>,
) -> Result<Drawings> {
    let mut drawings = HashMap::new();
    if pages.is_empty() {
        return Ok(drawings);
    }
    let mut data = Vec::new();
    doc.save_to(&mut data)?;
    let pdf = Pdf::new(data)
        .map_err(|error| anyhow::anyhow!("failed to interpret rewrite plan: {error:?}"))?;
    let cache = InterpreterCache::new();
    for &page in pages {
        let source = pdf
            .pages()
            .get(page.index)
            .context("missing page in rewrite interpretation")?;
        drawings.insert(
            page.index,
            trace_page(page, typeset, source, &cache, calls.as_deref_mut())?,
        );
    }
    Ok(drawings)
}

fn apply_plan(
    doc: &mut Document,
    plan: Plan,
    page_id: ObjectId,
    probe: ObjectId,
    calls: &[Call],
    modified_streams: &mut HashSet<ObjectId>,
) -> Result<()> {
    let mut removed_scopes: HashSet<usize> = HashSet::new();
    let mut rewritten: Vec<Vec<Operation>> = Vec::new();
    let mut modified = false;
    for step in &plan.steps {
        if step.child.is_some_and(|id| modified_streams.contains(&id)) {
            modified = true;
            removed_scopes.extend(step.scopes.iter().copied());
        }
        let Some(id) = step.call else {
            rewritten.push(vec![step.operation.clone()]);
            continue;
        };
        let call = &calls[id];
        let changed = call.glyphs.iter().any(|g| g.remove);
        if changed {
            removed_scopes.extend(step.scopes.iter().copied());
            modified = true;
        }
        rewritten.push(rewrite_text(step, call, changed)?);
    }
    let (operations, kept_properties) = clean_properties(doc, &plan, removed_scopes, rewritten);
    let content = Content { operations }.encode()?;
    let mut resources = plan.resources;
    resources.set("Properties", kept_properties);
    if let Ok(fonts) = resources.get_mut(b"Font").and_then(Object::as_dict_mut) {
        let probe_names: Vec<_> = fonts
            .iter()
            .filter(|(_, value)| value.as_reference().ok() == Some(probe))
            .map(|(name, _)| name.clone())
            .collect();
        for name in probe_names {
            fonts.remove(&name);
        }
    }
    let stream = doc.get_object_mut(plan.stream)?.as_stream_mut()?;
    stream.set_plain_content(content);
    if plan.is_page {
        doc.get_dictionary_mut(page_id)?.set("Resources", resources);
    } else {
        stream.dict.set("Resources", resources);
    }
    if modified {
        modified_streams.insert(plan.stream);
    }
    Ok(())
}

fn rewrite_text(step: &Step, call: &Call, changed: bool) -> Result<Vec<Operation>> {
    let mut ops = Vec::new();
    if !changed {
        if let Some(matrix) = call.matrix {
            ops.push(matrix_op(matrix));
        }
        ops.push(step.operation.clone());
    } else if call.glyphs.iter().any(|g| !g.remove) {
        ensure!(
            !call.vertical,
            "mixed vertical text cannot be safely rewritten"
        );
        let bytes = character_bytes(&step.operation, &call.glyphs)?;
        let [a, b, c, d, _, _] = call
            .matrix
            .context("missing text cursor trace")?
            .as_coeffs();
        for (glyph, bytes) in call.glyphs.iter().zip(bytes) {
            if glyph.remove {
                continue;
            }
            let origin = glyph.origin - hayro::kurbo::Vec2::new(c * step.rise, d * step.rise);
            ops.push(matrix_op(Affine::new([a, b, c, d, origin.x, origin.y])));
            ops.push(Operation::new(
                "Tj",
                vec![Object::String(bytes, lopdf::StringFormat::Hexadecimal)],
            ));
        }
    }
    Ok(ops)
}

fn clean_properties(
    doc: &Document,
    plan: &Plan,
    removed_scopes: HashSet<usize>,
    mut rewritten: Vec<Vec<Operation>>,
) -> (Vec<Operation>, Dictionary) {
    let properties = resolved_dict(doc, plan.resources.get(b"Properties").ok()).unwrap_or_default();
    for index in removed_scopes {
        if let Some(op) = rewritten[index].first_mut()
            && op.operator == "BDC"
            && op.operands.len() == 2
        {
            let property = match &op.operands[1] {
                Object::Name(name) => properties.get(name).ok(),
                object => Some(object),
            };
            if let Some(mut dict) = resolved_dict(doc, property) {
                dict.remove(b"ActualText");
                dict.remove(b"Alt");
                op.operands[1] = Object::Dictionary(dict);
            }
        }
    }
    let operations = rewritten.into_iter().flatten().collect::<Vec<_>>();
    let used_properties: HashSet<_> = operations
        .iter()
        .filter(|op| matches!(op.operator.as_str(), "BDC" | "DP"))
        .filter_map(|op| op.operands.get(1).and_then(|o| o.as_name().ok()))
        .collect();
    let mut kept_properties = Dictionary::new();
    for (name, value) in &properties {
        if used_properties.contains(name.as_slice()) {
            kept_properties.set(name.clone(), value.clone());
        }
    }
    (operations, kept_properties)
}

fn matrix_op(matrix: Affine) -> Operation {
    Operation::new(
        "Tm",
        matrix.as_coeffs().into_iter().map(Object::from).collect(),
    )
}

/// Match character codes reported by the font interpreter to source bytes.
/// Reject ambiguous encodings instead of silently moving a retained glyph.
fn character_bytes(operation: &Operation, glyphs: &[RecordedGlyph]) -> Result<Vec<Vec<u8>>> {
    let strings: Vec<&[u8]> = if operation.operator == "TJ" {
        operation
            .operands
            .first()
            .context("missing text operand")?
            .as_array()?
            .iter()
            .filter_map(|o| o.as_str().ok())
            .collect()
    } else {
        vec![
            operation
                .operands
                .first()
                .context("missing text operand")?
                .as_str()?,
        ]
    };
    let bytes: Vec<_> = strings
        .iter()
        .flat_map(|bytes| bytes.iter().copied())
        .collect();
    let mut offsets = vec![vec![0usize]];
    let boundaries: HashSet<_> = strings
        .iter()
        .scan(0, |pos, bytes| {
            *pos += bytes.len();
            Some(*pos)
        })
        .collect();
    for glyph in glyphs {
        let mut next = Vec::new();
        for &start in offsets.last().unwrap() {
            let end = boundaries
                .iter()
                .copied()
                .filter(|end| *end > start)
                .min()
                .unwrap_or(bytes.len());
            for count in 1..=4.min(end.saturating_sub(start)) {
                let code = bytes[start..start + count]
                    .iter()
                    .fold(0u32, |value, byte| (value << 8) | u32::from(*byte));
                if code == glyph.code {
                    next.push(start + count);
                }
            }
        }
        next.sort_unstable();
        next.dedup();
        ensure!(
            !next.is_empty() && next.len() < 1000,
            "unsupported character-code partition"
        );
        offsets.push(next);
    }
    ensure!(
        offsets.last().unwrap().contains(&bytes.len()),
        "character-code count differs from interpreter"
    );
    let mut end = bytes.len();
    let mut result = Vec::new();
    for (index, glyph) in glyphs.iter().enumerate().rev() {
        let candidates: Vec<_> = offsets[index]
            .iter()
            .copied()
            .filter(|start| {
                *start < end
                    && end - start <= 4
                    && !boundaries
                        .iter()
                        .any(|bound| *bound > *start && *bound < end)
                    && bytes[*start..end]
                        .iter()
                        .fold(0u32, |value, byte| (value << 8) | u32::from(*byte))
                        == glyph.code
            })
            .collect();
        ensure!(candidates.len() == 1, "ambiguous character-code partition");
        let start = candidates[0];
        result.push(bytes[start..end].to_vec());
        end = start;
    }
    result.reverse();
    Ok(result)
}

fn write_path(out: &mut String, path: &BezPath) {
    let mut current = Point::ZERO;
    let mut start = Point::ZERO;
    for element in path.elements() {
        match *element {
            PathEl::MoveTo(p) => {
                let _ = writeln!(out, "{:.6} {:.6} m", p.x, p.y);
                current = p;
                start = p;
            }
            PathEl::LineTo(p) => {
                let _ = writeln!(out, "{:.6} {:.6} l", p.x, p.y);
                current = p;
            }
            PathEl::QuadTo(control, p) => {
                let a = current.lerp(control, 2.0 / 3.0);
                let b = p.lerp(control, 2.0 / 3.0);
                let _ = writeln!(
                    out,
                    "{:.6} {:.6} {:.6} {:.6} {:.6} {:.6} c",
                    a.x, a.y, b.x, b.y, p.x, p.y
                );
                current = p;
            }
            PathEl::CurveTo(a, b, p) => {
                let _ = writeln!(
                    out,
                    "{:.6} {:.6} {:.6} {:.6} {:.6} {:.6} c",
                    a.x, a.y, b.x, b.y, p.x, p.y
                );
                current = p;
            }
            PathEl::ClosePath => {
                out.push_str("h\n");
                current = start;
            }
        }
    }
}
