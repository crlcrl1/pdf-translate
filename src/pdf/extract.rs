use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, anyhow, bail};
use hayro::hayro_interpret::font::{Glyph, GlyphRun};
use hayro::hayro_interpret::hayro_cmap::BfString;
use hayro::hayro_interpret::hayro_syntax::Pdf;
use hayro::hayro_interpret::hayro_syntax::page::{Page, Rotation};
use hayro::hayro_interpret::{
    ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterCache,
    InterpreterSettings, Paint, RenderContent,
};
use hayro::kurbo::{BezPath, PathEl, Point, Rect, Shape};
use rayon::prelude::*;

use crate::model::{BBox, ExtractedPage, GlyphBox, ImageBox, SourceGlyph};
use crate::pages::PageSelection;
use crate::pdf::content::{TextFontRun, page_original_content, text_font_runs};

const MAX_GLYPHS_PER_PAGE: usize = 1_000_000;
/// Below this many pages, thread setup and duplicated per-thread font caches
/// cost more than the parallel extraction saves.
const PARALLEL_PAGE_THRESHOLD: usize = 8;

pub fn extract_pdf(
    data: Vec<u8>,
    lopdf_document: &lopdf::Document,
    selection: &PageSelection,
) -> Result<Vec<ExtractedPage>> {
    let pdf = Pdf::new(data).map_err(|err| anyhow!("failed to parse PDF: {err:?}"))?;
    let lopdf_pages = lopdf_document.get_pages();

    let settings = InterpreterSettings {
        render_content: RenderContent::PAGE,
        ..Default::default()
    };
    let selected: Vec<(usize, &Page<'_>)> = pdf
        .pages()
        .iter()
        .enumerate()
        .filter(|(index, _)| selection.contains(index + 1))
        .collect();

    if selected.len() >= PARALLEL_PAGE_THRESHOLD {
        selected
            .par_iter()
            .map_init(InterpreterCache::new, |cache, (index, page)| {
                extract_page(lopdf_document, &lopdf_pages, cache, &settings, *index, page)
            })
            .collect()
    } else {
        let cache = InterpreterCache::new();
        selected
            .iter()
            .map(|(index, page)| {
                extract_page(
                    lopdf_document,
                    &lopdf_pages,
                    &cache,
                    &settings,
                    *index,
                    page,
                )
            })
            .collect()
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_page<'a>(
    document: &lopdf::Document,
    lopdf_pages: &BTreeMap<u32, lopdf::ObjectId>,
    cache: &InterpreterCache<'a>,
    settings: &InterpreterSettings,
    index: usize,
    page: &Page<'a>,
) -> Result<ExtractedPage> {
    let media = page.media_box();
    let crop = page.intersected_crop_box();
    let page_box = if crop.width() > 0.01 && crop.height() > 0.01 {
        crop
    } else {
        media
    };
    let crop_rect = Rect::new(page_box.x0, page_box.y0, page_box.x1, page_box.y1);
    let rotation = rotation_degrees(page.rotation());
    let transform = super::geometry::layout_transform(crop_rect, rotation);
    let bbox = transform.transform_rect_bbox(crop_rect);
    let mut collector = Collector::new(bbox);
    let mut context = Context::new(transform, bbox, cache, page.xref(), settings.clone());
    hayro::hayro_interpret::interpret_page(page, &mut context, &mut collector);
    if collector.limit_exceeded {
        bail!(
            "page {} contains too many glyphs ({}); the limit is {}",
            index + 1,
            collector.glyphs.len(),
            MAX_GLYPHS_PER_PAGE
        );
    }

    let glyph_signature = collector.sequence.signature();
    let mut glyphs = collector.glyphs;
    if glyphs.iter().any(|glyph| glyph.font_name.is_none())
        && let Some(page_id) = lopdf_pages.get(&(index as u32 + 1))
        && let Ok(mut runs) = page_font_runs(document, *page_id)
    {
        for run in &mut runs {
            let point = transform * Point::new(run.x, run.y);
            run.x = point.x;
            run.y = point.y;
        }
        assign_font_names(&mut glyphs, &runs);
    }
    assign_font_styles(&mut glyphs);

    Ok(ExtractedPage {
        index,
        x0: bbox.x0,
        y0: bbox.y0,
        width: bbox.width(),
        height: bbox.height(),
        rotation,
        layout_transform: transform.as_coeffs(),
        glyph_signature,
        glyphs,
        images: collector.images,
    })
}

fn page_font_runs(
    document: &lopdf::Document,
    page_id: lopdf::ObjectId,
) -> Result<Vec<TextFontRun>> {
    let (content, resources) = page_original_content(document, page_id)?;
    let mut fonts = HashMap::new();
    if let Some(font_dict) = resources.get(b"Font").ok().and_then(|object| match object {
        lopdf::Object::Reference(id) => document.get_dictionary(*id).ok(),
        value => value.as_dict().ok(),
    }) {
        for (name, value) in font_dict.iter() {
            let font = match value {
                lopdf::Object::Reference(id) => document.get_dictionary(*id).ok(),
                value => value.as_dict().ok(),
            };
            if let Some(font) = font
                && let Ok(base_font) = font.get(b"BaseFont")
                && let Ok(base_font) = base_font.as_name()
            {
                fonts.insert(
                    name.clone(),
                    String::from_utf8_lossy(base_font).into_owned(),
                );
            }
        }
    }

    text_font_runs(&content, &fonts)
}

#[derive(Clone, Copy)]
struct FontStyle {
    bold: bool,
    italic: bool,
    serif: bool,
    mono: bool,
}

fn font_style(name: &str) -> FontStyle {
    let lower = name.to_ascii_lowercase();
    let bold = lower.contains("medi")
        || lower.contains("bold")
        || lower.contains("black")
        || lower.contains("heavy")
        || lower.contains("semibold")
        || lower.contains("demi")
        || lower.contains("cmbx")
        || lower.contains("cmb10")
        || lower.contains("cmssbx");
    let italic = lower.contains("ital")
        || lower.contains("oblique")
        || lower.contains("cmmi")
        || lower.contains("cmti")
        || lower.contains("cmsl");
    let serif = lower.contains("rom")
        || lower.contains("times")
        || lower.contains("serif")
        || lower.contains("cmr")
        || lower.contains("cmbx")
        || lower.contains("cmmi")
        || lower.contains("cmsy");
    let mono = lower.contains("mono")
        || lower.contains("courier")
        || lower.contains("cmtt")
        || lower.contains("monl");
    FontStyle {
        bold,
        italic,
        serif,
        mono,
    }
}

fn assign_font_styles(glyphs: &mut [GlyphBox]) {
    let mut cache: HashMap<String, FontStyle> = HashMap::new();
    for glyph in glyphs {
        let Some(name) = glyph.font_name.as_deref() else {
            continue;
        };
        let style = match cache.get(name) {
            Some(style) => *style,
            None => {
                let style = font_style(name);
                cache.insert(name.to_owned(), style);
                style
            }
        };
        glyph.bold |= style.bold;
        glyph.italic |= style.italic;
        glyph.serif |= style.serif;
        glyph.mono |= style.mono;
    }
}

fn assign_font_names(glyphs: &mut [GlyphBox], runs: &[TextFontRun]) {
    for glyph in glyphs {
        if glyph.font_name.is_some() {
            continue;
        }
        let tolerance = 0.6;
        let mut best: Option<&TextFontRun> = None;
        for run in runs {
            if (run.y - glyph.origin_y).abs() > tolerance || run.x > glyph.origin_x + 0.6 {
                continue;
            }
            if best.is_none_or(|current| run.x > current.x) {
                best = Some(run);
            }
        }
        if let Some(run) = best {
            glyph.font_name = Some(run.font.clone());
        }
    }
}

fn rotation_degrees(rotation: Rotation) -> i32 {
    match rotation {
        Rotation::None => 0,
        Rotation::Horizontal => 90,
        Rotation::Flipped => 180,
        Rotation::FlippedHorizontal => 270,
    }
}

#[derive(Default)]
struct Collector {
    glyphs: Vec<GlyphBox>,
    images: Vec<ImageBox>,
    clips: Vec<(Rect, bool)>,
    groups: Vec<(f32, bool)>,
    marked: Vec<MarkedContent>,
    sequence: super::glyphs::GlyphSequence,
    glyph_count: usize,
    limit_exceeded: bool,
    /// Untransformed glyph outlines are expensive to decode (especially CFF
    /// charstrings). Pages repeat the same glyphs many times, so cache the
    /// bounding box per font/glyph pair.
    glyph_bounds: HashMap<u128, Option<Rect>>,
}

struct MarkedContent {
    actual_text: Option<String>,
    start: usize,
    artifact: bool,
}

impl Collector {
    fn new(page: Rect) -> Self {
        Self {
            clips: vec![(page, true)],
            ..Self::default()
        }
    }

    fn visibility(&self, bbox: BBox) -> Option<bool> {
        let rect = Rect::new(bbox.x0, bbox.y0, bbox.x1, bbox.y1);
        let mut protected = false;
        for (clip, rectangular) in &self.clips {
            if clip.intersect(rect).area() <= 0.0 {
                return None;
            }
            protected |= !rectangular || !clip.contains_rect(rect);
        }
        for (opacity, complex) in &self.groups {
            if *opacity <= 0.0 {
                return None;
            }
            protected |= *opacity < 1.0 || *complex;
        }
        Some(protected || self.marked.iter().any(|mark| mark.artifact))
    }
}

fn is_axis_aligned_rectangle(path: &BezPath) -> bool {
    let mut points = Vec::new();
    for element in path.elements() {
        match element {
            PathEl::MoveTo(point) if points.is_empty() => points.push(*point),
            PathEl::LineTo(point) => points.push(*point),
            PathEl::ClosePath => {}
            _ => return false,
        }
    }
    if points.len() == 5 && points.first() == points.last() {
        points.pop();
    }
    if points.len() != 4 {
        return false;
    }
    let bounds = path.bounding_box();
    let corners = [
        Point::new(bounds.x0, bounds.y0),
        Point::new(bounds.x1, bounds.y0),
        Point::new(bounds.x1, bounds.y1),
        Point::new(bounds.x0, bounds.y1),
    ];
    corners.iter().all(|point| points.contains(point))
        && (0..4).all(|i| {
            let a = points[i];
            let b = points[(i + 1) % 4];
            a.x == b.x || a.y == b.y
        })
}

impl<'a> Device<'a> for Collector {
    fn draw_path(&mut self, path: &BezPath, props: DrawProps<'a>, draw_mode: &DrawMode) {
        let DrawMode::Stroke(stroke) = draw_mode else {
            return;
        };
        let [PathEl::MoveTo(start), PathEl::LineTo(end)] = path.elements() else {
            return;
        };
        let start = props.transform * *start;
        let end = props.transform * *end;
        if (start.y - end.y).abs() > 0.05 {
            return;
        }
        if matches!(&props.paint, Paint::Color(color) if color.to_rgba().to_rgba8()[3] == 0) {
            return;
        }
        let x0 = start.x.min(end.x);
        let x1 = start.x.max(end.x);
        let width = x1 - x0;
        if self
            .visibility(BBox::new(x0, start.y - 0.1, x1, start.y + 0.1))
            .is_none()
        {
            return;
        }
        // TeX can draw an underscore as a short vector rule between glyphs.
        // Turn it into a real "_" in the extracted text and remember the rule
        // box so the composer can cover the original stroke. This avoids
        // leaving stray lines while still allowing the paragraph to translate.
        if let Some(glyph) = self.glyphs.iter_mut().rev().find(|glyph| {
            width >= glyph.font_size * 0.15
                && width <= glyph.font_size * 0.9
                && f64::from(stroke.line_width) <= glyph.font_size * 0.12
                && (glyph.origin_y - start.y).abs() <= glyph.font_size * 0.25
                && glyph.bbox.x1 <= x0 + glyph.font_size * 0.15
                && x0 - glyph.bbox.x1 <= glyph.font_size * 0.6
        }) {
            let convertible = glyph.text.chars().count() == 1
                && glyph
                    .text
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_alphanumeric());
            if convertible {
                glyph
                    .decorations
                    .push(BBox::new(x0, start.y - 0.1, x1, start.y + 0.1));
                if !glyph.text.ends_with('_') {
                    glyph.text.push('_');
                }
            } else {
                glyph.protected = true;
            }
        }
    }

    fn push_clip_path(&mut self, clip_path: &ClipPath) {
        self.clips.push((
            clip_path.path.bounding_box(),
            is_axis_aligned_rectangle(&clip_path.path),
        ));
    }

    fn push_transparency_group(
        &mut self,
        opacity: f32,
        mask: Option<hayro::hayro_interpret::SoftMask<'a>>,
        blend_mode: hayro::hayro_interpret::BlendMode,
    ) {
        self.groups.push((
            opacity,
            mask.is_some() || blend_mode != hayro::hayro_interpret::BlendMode::Normal,
        ));
    }

    fn draw_glyph_run(
        &mut self,
        glyph_run: &GlyphRun<'_, 'a>,
        props: DrawProps<'a>,
        draw_mode: &DrawMode,
    ) {
        let first = self.sequence.observe(glyph_run, draw_mode);
        let invisible = matches!(draw_mode, DrawMode::Invisible);
        let color = match &props.paint {
            Paint::Color(color) => color.to_rgba().to_rgba8(),
            Paint::Pattern(_) => [0, 0, 0, 255],
        };
        if invisible || color[3] == 0 {
            return;
        }

        for (index, positioned) in glyph_run.glyphs().iter().enumerate() {
            self.glyph_count += 1;
            if self.glyph_count > MAX_GLYPHS_PER_PAGE {
                self.limit_exceeded = true;
                return;
            }
            let glyph = &**positioned;
            let transform = props.transform * positioned.transform();
            let coeffs = transform.as_coeffs();
            let x_scale = (coeffs[0] * coeffs[0] + coeffs[1] * coeffs[1]).sqrt();
            let y_scale = (coeffs[2] * coeffs[2] + coeffs[3] * coeffs[3]).sqrt();
            let font_size = (x_scale.max(y_scale) * 1000.0).max(1.0);
            let angle = coeffs[1].atan2(coeffs[0]);
            let origin = transform * Point::ZERO;

            let text = positioned
                .as_unicode()
                .map(bf_string_to_string)
                .unwrap_or_default();

            let (font_name, bold, italic, serif, mono, glyph_id, advance_units) = match glyph {
                Glyph::Outline(outline) => {
                    let data = outline.font_data();
                    (
                        data.as_ref().and_then(|d| d.postscript_name.clone()),
                        data.as_ref()
                            .is_some_and(|d| d.weight.is_some_and(|w| w >= 600)),
                        data.as_ref().is_some_and(|d| d.is_italic),
                        data.as_ref().is_some_and(|d| d.is_serif),
                        data.as_ref().is_some_and(|d| d.is_monospace),
                        outline.glyph_id().to_u32(),
                        outline.advance_width().unwrap_or(0.0) as f64,
                    )
                }
                Glyph::Type3(outline) => (
                    Some("Type3".to_string()),
                    false,
                    false,
                    false,
                    false,
                    outline.char_code(),
                    outline.advance_width().unwrap_or(0.0) as f64,
                ),
            };

            let advance_origin = transform * Point::ZERO;
            let advance_end = transform * Point::new(advance_units, 0.0);
            let advance = (advance_end - advance_origin).length();

            let glyph_bbox = match glyph {
                Glyph::Outline(outline) => {
                    let key = outline.identifier().cache_key();
                    let bounds = *self.glyph_bounds.entry(key).or_insert_with(|| {
                        let bounds = outline.outline().bounding_box();
                        (bounds.width() > 0.01 && bounds.height() > 0.01).then_some(bounds)
                    });
                    if let Some(bounds) = bounds {
                        BBox::from_kurbo(transform.transform_rect_bbox(bounds))
                    } else {
                        BBox::new(
                            origin.x,
                            origin.y,
                            origin.x + advance,
                            origin.y + font_size * 0.1,
                        )
                    }
                }
                Glyph::Type3(_) => BBox::new(
                    origin.x,
                    origin.y - font_size * 0.2,
                    origin.x + advance,
                    origin.y + font_size * 0.8,
                ),
            };

            let Some(clipped) = self.visibility(glyph_bbox) else {
                continue;
            };
            let protected = clipped
                || color[3] < 255
                || props.soft_mask.is_some()
                || props.blend_mode != hayro::hayro_interpret::BlendMode::Normal
                || matches!(props.paint, Paint::Pattern(_))
                || !matches!(draw_mode, DrawMode::Fill(_));
            self.glyphs.push(GlyphBox {
                text,
                bbox: glyph_bbox,
                origin_x: origin.x,
                origin_y: origin.y,
                advance,
                font_size,
                angle,
                font_name,
                bold,
                italic,
                serif,
                mono,
                color,
                invisible,
                protected,
                decorations: Vec::new(),
                char_code: glyph.char_code(),
                glyph_id,
                sources: smallvec::smallvec![SourceGlyph {
                    ordinal: first + index,
                    code: glyph.char_code(),
                    x: origin.x,
                    y: origin.y
                }],
            });
        }
    }

    fn draw_image(&mut self, _image: Image<'a, '_>, props: ImageDrawProps<'a>) {
        let unit = Rect::new(0.0, 0.0, 1.0, 1.0);
        self.images.push(ImageBox {
            bbox: BBox::from_kurbo(props.transform.transform_rect_bbox(unit)),
        });
    }

    fn pop_clip(&mut self) {
        if self.clips.len() > 1 {
            self.clips.pop();
        }
    }

    fn pop_transparency_group(&mut self) {
        self.groups.pop();
    }

    fn begin_marked_content(
        &mut self,
        tag: &[u8],
        properties: Option<&hayro::hayro_interpret::hayro_syntax::object::Dict<'_>>,
    ) {
        let actual_text = properties
            .and_then(|dict| {
                dict.get::<hayro::hayro_interpret::hayro_syntax::object::String<'_>>(b"ActualText")
            })
            .and_then(|value| {
                lopdf::decode_text_string(&lopdf::Object::string_literal(value.as_bytes())).ok()
            });
        self.marked.push(MarkedContent {
            actual_text,
            start: self.glyphs.len(),
            artifact: tag == b"Artifact",
        });
    }

    fn end_marked_content(&mut self) {
        let Some(mark) = self.marked.pop() else {
            return;
        };
        let Some(text) = mark.actual_text else {
            return;
        };
        let Some(mut first) = self.glyphs.get(mark.start).cloned() else {
            return;
        };
        first.sources.clear();
        for glyph in &self.glyphs[mark.start..] {
            first.sources.extend_from_slice(&glyph.sources);
            first.bbox = first.bbox.union(glyph.bbox);
            first.protected |= glyph.protected
                || (glyph.origin_y - first.origin_y).abs() > first.font_size * 0.3
                || (glyph.angle - first.angle).abs() > 0.01;
        }
        self.glyphs.truncate(mark.start);
        if !text.is_empty() {
            first.text = text;
            first.advance = first.bbox.width();
            self.glyphs.push(first);
        }
    }
}

fn bf_string_to_string(value: BfString) -> String {
    match value {
        BfString::Char(ch) => ch.to_string(),
        BfString::String(string) => string,
    }
}
