//! Match validated color scopes and bound inline objects to their actual
//! typeset positions. Paragraph scaling alone cannot follow translated words.
use crate::markup::{Piece, decode_literal, pieces};
use crate::model::{BBox, PageLayout, Paragraph};
use crate::text::expand_latin_ligatures;
use crate::typeset::TypesetParagraph;

struct PositionedChar {
    ch: char,
    color: [u8; 4],
    bbox: BBox,
    line: usize,
}

struct Scope {
    color: [u8; 4],
    text: Vec<char>,
}

struct TextRegion {
    source: Vec<BBox>,
    target: Vec<BBox>,
}

struct InlineRegion {
    source: BBox,
    target: BBox,
}

pub(super) struct TextMap {
    regions: Vec<TextRegion>,
    inline: Vec<InlineRegion>,
    source_boxes: Vec<BBox>,
}

impl TextMap {
    pub(super) fn new(page: &PageLayout, translated: &[(&Paragraph, &TypesetParagraph)]) -> Self {
        let mut regions = Vec::new();
        let mut inline = Vec::new();
        let mut source_boxes = Vec::new();
        for (paragraph, typeset) in translated {
            let source_chars = paragraph
                .lines(&page.lines)
                .enumerate()
                .flat_map(|(line, source)| {
                    source.glyphs.iter().flat_map(move |glyph| {
                        normalized(&glyph.text)
                            .into_iter()
                            .map(move |ch| PositionedChar {
                                ch,
                                color: glyph.color,
                                bbox: glyph.bbox,
                                line,
                            })
                    })
                })
                .collect::<Vec<_>>();
            source_boxes.extend(
                source_chars
                    .iter()
                    .filter(|ch| ch.ch.is_alphanumeric())
                    .map(|ch| ch.bbox),
            );
            let target_chars = typeset
                .lines
                .iter()
                .enumerate()
                .flat_map(|(line, target)| {
                    target.segments.iter().flat_map(move |segment| {
                        segment
                            .text
                            .chars()
                            .zip(&segment.glyph_boxes)
                            .filter(|(ch, _)| !ch.is_whitespace())
                            .map(move |(ch, bbox)| PositionedChar {
                                ch,
                                color: segment.color,
                                bbox: *bbox,
                                line,
                            })
                    })
                })
                .collect::<Vec<_>>();
            let source_scopes = scopes(paragraph.prompt_text.as_deref().unwrap_or(&paragraph.text));
            let target_scopes = scopes(paragraph.translation.as_deref().unwrap_or_default());
            // Translation validation preserves the order and color of these
            // scopes, including repeated names/years. No fuzzy word matching.
            if source_scopes.len() == target_scopes.len()
                && source_scopes
                    .iter()
                    .zip(&target_scopes)
                    .all(|(a, b)| a.color == b.color)
            {
                let (mut source_cursor, mut target_cursor) = (0, 0);
                for (source_scope, target_scope) in source_scopes.iter().zip(&target_scopes) {
                    let source = locate(&source_chars, source_scope, &mut source_cursor);
                    let target = locate(&target_chars, target_scope, &mut target_cursor);
                    if let (Some(source), Some(target)) = (source, target) {
                        regions.push(TextRegion {
                            source: source.iter().map(|ch| ch.bbox).collect(),
                            target: line_boxes(target),
                        });
                    }
                }
            }
            for formula in typeset.lines.iter().flat_map(|line| &line.formulas) {
                let source = formula.source_bbox;
                inline.push(InlineRegion {
                    source,
                    target: BBox::new(
                        formula.x,
                        formula.baseline + (source.y0 - formula.source_baseline) * formula.scale,
                        formula.x + source.width() * formula.scale,
                        formula.baseline + (source.y1 - formula.source_baseline) * formula.scale,
                    ),
                });
            }
        }
        Self {
            regions,
            inline,
            source_boxes,
        }
    }

    pub(super) fn map(&self, rect: BBox) -> Option<Vec<BBox>> {
        let mut boxes = Vec::new();
        let mut covered = Vec::new();
        for region in &self.regions {
            // A color run can include several adjacent links. Only transfer
            // the whole scope when this annotation covers all its characters.
            if !region.source.is_empty() && region.source.iter().all(|b| contains_center(rect, *b))
            {
                boxes.extend(region.target.iter().copied());
                covered.extend(region.source.iter().copied());
            }
        }
        for region in &self.inline {
            // A link can cover just the footnote marker inside a larger
            // preserved formula, so map its intersection within that object.
            let source = region.source;
            if source.width() <= 0.0
                || source.height() <= 0.0
                || !source.intersects(rect)
                || !self
                    .source_boxes
                    .iter()
                    .any(|b| contains_center(rect, *b) && contains_center(source, *b))
            {
                continue;
            }
            let clip = BBox::new(
                rect.x0.max(source.x0),
                rect.y0.max(source.y0),
                rect.x1.min(source.x1),
                rect.y1.min(source.y1),
            );
            let target = region.target;
            let (sx, sy) = (
                target.width() / source.width(),
                target.height() / source.height(),
            );
            boxes.push(BBox::new(
                target.x0 + (clip.x0 - source.x0) * sx,
                target.y0 + (clip.y0 - source.y0) * sy,
                target.x0 + (clip.x1 - source.x0) * sx,
                target.y0 + (clip.y1 - source.y0) * sy,
            ));
            covered.push(clip);
        }
        // A paragraph-wide annotation must not shrink to just its colored
        // words. Require the matched scopes to account for all linked glyphs.
        let complete = self
            .source_boxes
            .iter()
            .filter(|b| contains_center(rect, **b))
            .all(|b| covered.iter().any(|source| contains_center(*source, *b)));
        (!boxes.is_empty() && complete).then_some(boxes)
    }
}

fn contains_center(outer: BBox, inner: BBox) -> bool {
    let (x, y) = (inner.center_x(), (inner.y0 + inner.y1) * 0.5);
    x >= outer.x0 && x <= outer.x1 && y >= outer.y0 && y <= outer.y1
}

fn normalized(text: &str) -> Vec<char> {
    expand_latin_ligatures(text)
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect()
}

fn scopes(text: &str) -> Vec<Scope> {
    let mut scopes = Vec::new();
    let mut current: Option<usize> = None;
    for piece in pieces(text) {
        match piece {
            Piece::Color(Some(color)) => {
                current = Some(scopes.len());
                scopes.push(Scope {
                    color,
                    text: Vec::new(),
                });
            }
            Piece::Color(None) => current = None,
            Piece::Text(text) => {
                if let Some(index) = current {
                    scopes[index].text.extend(normalized(&decode_literal(text)));
                }
            }
            _ => {}
        }
    }
    scopes
}

fn locate<'a>(
    chars: &'a [PositionedChar],
    scope: &Scope,
    cursor: &mut usize,
) -> Option<&'a [PositionedChar]> {
    if scope.text.is_empty() {
        return None;
    }
    let start = *cursor
        + chars[*cursor..]
            .windows(scope.text.len())
            .position(|window| {
                window
                    .iter()
                    .zip(&scope.text)
                    .all(|(actual, expected)| actual.ch == *expected && actual.color == scope.color)
            })?;
    *cursor = start + scope.text.len();
    Some(&chars[start..*cursor])
}

fn line_boxes(chars: &[PositionedChar]) -> Vec<BBox> {
    let mut boxes: Vec<BBox> = Vec::new();
    let mut last_line = None;
    for ch in chars {
        if last_line == Some(ch.line) {
            let last = boxes.last_mut().unwrap();
            *last = last.union(ch.bbox);
        } else {
            boxes.push(ch.bbox);
        }
        last_line = Some(ch.line);
    }
    boxes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_links_can_follow_only_a_superscript_inside_a_formula() {
        let map = TextMap {
            regions: Vec::new(),
            inline: vec![InlineRegion {
                source: BBox::new(0., 0., 12., 10.),
                target: BBox::new(100., 200., 124., 220.),
            }],
            source_boxes: vec![BBox::new(0., 0., 5., 7.), BBox::new(10., 7., 12., 10.)],
        };
        assert_eq!(
            map.map(BBox::new(10., 7., 12., 10.)).unwrap(),
            vec![BBox::new(120., 214., 124., 220.)]
        );
        assert!(map.map(BBox::new(15., 0., 20., 10.)).is_none());
    }

    #[test]
    fn matching_a_color_scope_does_not_drop_other_linked_prose() {
        let colored = BBox::new(0., 0., 10., 10.);
        let map = TextMap {
            regions: vec![TextRegion {
                source: vec![colored],
                target: vec![BBox::new(100., 100., 120., 110.)],
            }],
            inline: Vec::new(),
            source_boxes: vec![colored, BBox::new(12., 0., 20., 10.)],
        };
        assert!(map.map(BBox::new(0., 0., 20., 10.)).is_none());
        assert!(map.map(colored).is_some());
    }
}
