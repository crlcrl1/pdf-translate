//! Bind the original page's ordered glyph paints to instrumented text operations.
use std::collections::HashMap;
use std::fmt::Write as _;

use super::{Call, FormulaDrawing, RecordedGlyph, TRACE_TAG, write_path};
use crate::model::{BBox, PageLayout, SourceGlyph};
use crate::pdf::glyphs::GlyphSequence;
use crate::typeset::TypesetDocument;
use anyhow::{Result, ensure};
use hayro::hayro_interpret::font::{Glyph, GlyphRun};
use hayro::hayro_interpret::{
    ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterCache,
    InterpreterSettings, RenderContent,
};
use hayro::hayro_syntax::{object::Dict, page::Page};
use hayro::kurbo::{Affine, BezPath, PathEl, Point, Rect};

struct Target {
    source: SourceGlyph,
    remove: bool,
    formula: Option<usize>,
}

struct Trace<'a> {
    calls: Option<&'a mut [Call]>,
    active: Vec<Option<(usize, bool)>>,
    sequence: GlyphSequence,
    targets: HashMap<usize, Target>,
    drawings: Vec<FormulaDrawing>,
    error: Option<&'static str>,
    rules: Vec<BBox>,
}

impl Trace<'_> {
    fn target(&mut self, source: SourceGlyph) -> &mut Target {
        self.targets.entry(source.ordinal).or_insert(Target {
            source,
            remove: false,
            formula: None,
        })
    }

    fn remove_rule(&mut self) {
        if let Some((id, false)) = self.active.iter().rev().flatten().next().copied()
            && let Some(call) = self
                .calls
                .as_deref_mut()
                .and_then(|calls| calls.get_mut(id))
        {
            call.remove_rule = true;
        }
    }
}

pub(super) fn trace_page<'a>(
    page: &PageLayout,
    typeset: &TypesetDocument,
    source: &Page<'a>,
    cache: &InterpreterCache<'a>,
    calls: Option<&mut [Call]>,
) -> Result<Vec<FormulaDrawing>> {
    let clean = calls.is_some();
    let mut trace = Trace {
        calls,
        active: Vec::new(),
        sequence: Default::default(),
        targets: HashMap::new(),
        drawings: Vec::new(),
        error: None,
        rules: Vec::new(),
    };
    for paragraph in page
        .paragraphs
        .iter()
        .filter(|p| typeset.paragraphs.contains_key(&p.id))
    {
        if clean {
            for glyph in paragraph.lines(&page.lines).flat_map(|l| &l.glyphs) {
                trace.rules.extend_from_slice(&glyph.decorations);
                ensure!(
                    !glyph.sources.is_empty(),
                    "missing source glyph identity on page {}",
                    page.index + 1
                );
                for &source in &glyph.sources {
                    trace.target(source).remove = true;
                }
            }
        }
        for formula in &paragraph.formulas {
            let index = trace.drawings.len();
            for &source in &formula.sources {
                let target = trace.target(source);
                ensure!(
                    target.formula.is_none_or(|i| i == index),
                    "glyph belongs to overlapping inline formulas"
                );
                target.formula = Some(index);
            }
            trace.drawings.push(FormulaDrawing {
                paragraph: paragraph.id.clone(),
                id: formula.id.clone(),
                bbox: formula.bbox,
                content: String::new(),
            });
        }
    }

    let mut context = Context::new(
        Affine::new(page.layout_transform),
        Rect::new(
            page.x0,
            page.y0,
            page.x0 + page.width,
            page.y0 + page.height,
        ),
        cache,
        source.xref(),
        InterpreterSettings {
            render_content: RenderContent::PAGE,
            ..Default::default()
        },
    );
    hayro::hayro_interpret::interpret_page(source, &mut context, &mut trace);
    ensure!(
        trace.error.is_none(),
        "cannot identify source glyph on page {}: {}",
        page.index + 1,
        trace.error.unwrap_or_default()
    );
    ensure!(
        trace.sequence.signature() == page.glyph_signature && trace.targets.is_empty(),
        "source glyph sequence differs from extraction on page {}",
        page.index + 1
    );
    ensure!(
        trace.drawings.iter().all(|f| !f.content.is_empty()),
        "empty inline formula on page {}",
        page.index + 1
    );
    Ok(trace.drawings)
}

impl<'a> Device<'a> for Trace<'_> {
    fn draw_path(&mut self, path: &BezPath, props: DrawProps<'a>, mode: &DrawMode) {
        let [PathEl::MoveTo(start), PathEl::LineTo(end)] = path.elements() else {
            return;
        };
        let DrawMode::Stroke(stroke) = mode else {
            return;
        };
        let start = props.transform * *start;
        let end = props.transform * *end;
        if (start.y - end.y).abs() > 0.05 {
            return;
        }
        let thickness = f64::from(stroke.line_width) * props.transform.determinant().abs().sqrt();
        // Fractions and overbars are PDF paths, not glyphs. Move the complete
        // original rule with the formula instead of leaving it in the source
        // position after all surrounding glyphs have been removed.
        let formula = self.drawings.iter().position(|formula| {
            let bbox = formula.bbox;
            start.x.min(end.x) >= bbox.x0 - 1.0
                && start.x.max(end.x) <= bbox.x1 + 1.0
                && start.y >= bbox.y0
                && start.y <= bbox.y1
                && thickness <= 1.5
        });
        if let Some(index) = formula {
            let color = match &props.paint {
                hayro::hayro_interpret::Paint::Color(color) => color.to_rgba().to_rgba8(),
                _ => [0, 0, 0, 255],
            };
            let out = &mut self.drawings[index].content;
            let _ = writeln!(
                out,
                "q {} {} {} RG {thickness} w {} {} m {} {} l S Q",
                f64::from(color[0]) / 255.,
                f64::from(color[1]) / 255.,
                f64::from(color[2]) / 255.,
                start.x,
                start.y,
                end.x,
                end.y
            );
        }
        let matches = formula.is_some()
            || self.rules.iter().any(|rule| {
                (start.x.min(end.x) - rule.x0).abs() < 0.05
                    && (start.x.max(end.x) - rule.x1).abs() < 0.05
                    && (start.y - end.y).abs() < 0.05
                    && (start.y - (rule.y0 + rule.y1) * 0.5).abs() < 0.05
            });
        if matches {
            self.remove_rule();
        }
    }
    fn draw_image(&mut self, image: Image<'a, '_>, props: ImageDrawProps<'a>) {
        let Image::Stencil(stencil) = image else {
            return;
        };
        let bbox = BBox::from_kurbo(
            props
                .transform
                .transform_rect_bbox(Rect::new(0.0, 0.0, 1.0, 1.0)),
        );
        if bbox.height() > 1.5
            || bbox.width() < bbox.height() * 3.0
            || props.soft_mask.is_some()
            || props.blend_mode != hayro::hayro_interpret::BlendMode::Normal
            || u64::from(stencil.width()) * u64::from(stencil.height()) > 4096
        {
            return;
        }
        let Some(index) = self.drawings.iter().position(|formula| {
            bbox.x0 >= formula.bbox.x0 - 1.0
                && bbox.x1 <= formula.bbox.x1 + 1.0
                && bbox.y0 >= formula.bbox.y0
                && bbox.y1 <= formula.bbox.y1
        }) else {
            return;
        };
        let mut solid_color = None;
        stencil.with_stencil(
            |mask, paint| {
                if mask.data.iter().all(|alpha| *alpha == 255)
                    && let hayro::hayro_interpret::Paint::Color(color) = paint
                {
                    let rgba = color.to_rgba().to_rgba8();
                    if rgba[3] == 255 {
                        solid_color = Some(rgba);
                    }
                }
            },
            None,
        );
        let Some(color) = solid_color else {
            return;
        };
        // Some PDF producers encode fraction and radical bars as solid tiny
        // stencil images. Preserve their exact rectangle in the formula Form.
        let formula = &mut self.drawings[index];
        formula.bbox = formula.bbox.union(bbox);
        let _ = writeln!(
            formula.content,
            "q {} {} {} rg {} {} {} {} re f Q",
            f64::from(color[0]) / 255.,
            f64::from(color[1]) / 255.,
            f64::from(color[2]) / 255.,
            bbox.x0,
            bbox.y0,
            bbox.width(),
            bbox.height()
        );
        self.remove_rule();
    }
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn pop_clip(&mut self) {}
    fn push_transparency_group(
        &mut self,
        _: f32,
        _: Option<hayro::hayro_interpret::SoftMask<'a>>,
        _: hayro::hayro_interpret::BlendMode,
    ) {
    }
    fn pop_transparency_group(&mut self) {}
    fn begin_marked_content(&mut self, tag: &[u8], props: Option<&Dict<'_>>) {
        let marker = if self.calls.is_some() && tag == TRACE_TAG {
            props.and_then(|p| Some((p.get::<u32>(b"Id")? as usize, p.get::<bool>(b"Probe")?)))
        } else {
            None
        };
        self.active.push(marker);
    }
    fn end_marked_content(&mut self) {
        self.active.pop();
    }
    fn draw_glyph_run(&mut self, run: &GlyphRun<'_, 'a>, props: DrawProps<'a>, mode: &DrawMode) {
        let marker = self.active.iter().rev().flatten().next().copied();
        let mut call = None;
        if let Some((id, probe)) = marker {
            let Some(c) = self
                .calls
                .as_deref_mut()
                .and_then(|calls| calls.get_mut(id))
            else {
                self.error = Some("invalid probe marker");
                return;
            };
            if probe {
                c.matrix = run.glyphs().first().map(|g| g.transform());
                return;
            }
            c.vertical =
                run.writing_mode() != hayro::hayro_interpret::hayro_cmap::WritingMode::Horizontal;
            call = Some(c);
        }
        let first = self.sequence.observe(run, mode);
        let repeated = call.as_ref().is_some_and(|c| c.recorded);
        for (i, glyph) in run.glyphs().iter().enumerate() {
            let target = self.targets.remove(&(first + i));
            let remove = target.as_ref().is_some_and(|t| t.remove);
            if let Some(target) = target {
                let world = props.transform * glyph.transform();
                let origin = world * Point::ZERO;
                if target.source.code != glyph.char_code()
                    || (origin.x - target.source.x).abs() > 0.05
                    || (origin.y - target.source.y).abs() > 0.05
                {
                    self.error = Some("selected paint has a different code or position");
                    return;
                }
                if remove && call.is_none() {
                    self.error = Some("selected paint has no text operation");
                    return;
                }
                if let Some(index) = target.formula {
                    let Glyph::Outline(outline) = &**glyph else {
                        self.error = Some("cannot outline a Type3 inline glyph");
                        return;
                    };
                    let color = match &props.paint {
                        hayro::hayro_interpret::Paint::Color(color) => color.to_rgba().to_rgba8(),
                        _ => [0, 0, 0, 255],
                    };
                    let out = &mut self.drawings[index].content;
                    let _ = writeln!(
                        out,
                        "q {} {} {} rg",
                        f64::from(color[0]) / 255.,
                        f64::from(color[1]) / 255.,
                        f64::from(color[2]) / 255.
                    );
                    write_path(out, &(world * outline.outline()));
                    out.push_str("f\nQ\n");
                }
            }
            if let Some(c) = call.as_deref_mut() {
                if repeated {
                    let Some(prev) = c.glyphs.get_mut(i).filter(|g| g.code == glyph.char_code())
                    else {
                        self.error = Some("inconsistent repeated glyph callback");
                        return;
                    };
                    prev.remove |= remove;
                } else {
                    c.glyphs.push(RecordedGlyph {
                        code: glyph.char_code(),
                        origin: glyph.transform() * Point::ZERO,
                        remove,
                    });
                }
            }
        }
        if let Some(c) = call {
            c.recorded = true;
        }
    }
}
