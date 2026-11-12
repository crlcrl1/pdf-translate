use crate::model::{BBox, Column, Paragraph, Role, TextLine};
use crate::text::is_section_number;

struct Row<'a> {
    bbox: BBox,
    baseline: f64,
    size: f64,
    lines: Vec<&'a TextLine>,
    boundary: bool,
}

impl Row<'_> {
    fn tabular(&self) -> bool {
        let mut boxes: Vec<_> = self.lines.iter().map(|line| line.bbox).collect();
        boxes.sort_by(|a, b| a.x0.total_cmp(&b.x0));
        boxes
            .windows(2)
            .any(|pair| pair[1].x0 - pair[0].x1 > self.size * 0.8)
            || self.lines.iter().any(|line| has_column_gap(line))
    }

    fn prose(&self) -> bool {
        self.lines.iter().any(|line| {
            line.text.chars().filter(|ch| ch.is_alphabetic()).count() > 45
                && !has_column_gap(line)
                && line
                    .text
                    .split_whitespace()
                    .filter(|word| word.chars().all(char::is_lowercase))
                    .take(3)
                    .count()
                    >= 3
        })
    }
}

fn has_column_gap(line: &TextLine) -> bool {
    let mut glyphs: Vec<_> = line
        .glyphs
        .iter()
        .filter(|g| !g.text.trim().is_empty())
        .collect();
    glyphs.sort_by(|a, b| a.bbox.x0.total_cmp(&b.bbox.x0));
    glyphs
        .windows(2)
        .any(|pair| pair[1].bbox.x0 - pair[0].bbox.x1 > line.font_size * 1.3)
}

/// Find the adjacent run of table rows, including single-cell group headers.
/// Row/column geometry, prose and caption boundaries determine its extent;
/// the table can span the entire page instead of a fixed caption radius.
pub(super) fn table_region(
    paragraphs: &[Paragraph],
    lines: &[TextLine],
    caption_index: usize,
    claimed: &[BBox],
) -> Option<BBox> {
    if lines.is_empty() {
        return None;
    }
    let caption = &paragraphs[caption_index];
    let mut owners = vec![None; lines.len()];
    for (index, paragraph) in paragraphs.iter().enumerate() {
        for &line in &paragraph.line_indices {
            owners[line] = Some(index);
        }
    }
    let mut rows: Vec<Row<'_>> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if owners[index] == Some(caption_index)
            || (caption.column != Column::Full && line.column != caption.column)
        {
            continue;
        }
        let owner = owners[index].map(|index| &paragraphs[index]);
        let boundary = owner.is_some_and(|p| {
            matches!(p.role, Role::Caption | Role::Reference | Role::Title)
                || (p.role == Role::Heading
                    && is_section_number(p.text.split_whitespace().next().unwrap_or_default()))
        });
        if let Some(row) = rows
            .iter_mut()
            .find(|row| (row.baseline - line.baseline).abs() < row.size.max(line.font_size) * 0.35)
        {
            row.bbox = row.bbox.union(line.bbox);
            row.size = row.size.max(line.font_size);
            row.lines.push(line);
            row.boundary |= boundary;
        } else {
            rows.push(Row {
                bbox: line.bbox,
                baseline: line.baseline,
                size: line.font_size,
                lines: vec![line],
                boundary,
            });
        }
    }
    for above in [true, false] {
        let mut candidates: Vec<_> = rows
            .iter()
            .filter(|row| {
                if above {
                    row.bbox.y0 >= caption.bbox.y1 - 0.5
                } else {
                    row.bbox.y1 <= caption.bbox.y0 + 0.5
                }
            })
            .collect();
        candidates.sort_by(|a, b| {
            if above {
                a.bbox.y0.total_cmp(&b.bbox.y0)
            } else {
                b.bbox.y1.total_cmp(&a.bbox.y1)
            }
        });
        let mut edge = if above {
            caption.bbox.y1
        } else {
            caption.bbox.y0
        };
        let mut region: Option<BBox> = None;
        let mut table_rows = 0;
        for row in candidates {
            let gap = if above {
                row.bbox.y0 - edge
            } else {
                edge - row.bbox.y1
            };
            if row.boundary || row.prose() || gap > row.size.max(caption.font_size) * 3.5 {
                break;
            }
            region = Some(region.map_or(row.bbox, |bbox| bbox.union(row.bbox)));
            table_rows += usize::from(row.tabular());
            edge = if above { row.bbox.y1 } else { row.bbox.y0 };
        }
        // In a stack of tables the caption may have rows on both sides.
        // Associate each region once in reading order; choosing the side with
        // more rows can otherwise attach every caption to the larger table.
        if table_rows >= 2
            && let Some(region) = region
            && !claimed.iter().any(|bbox| bbox.intersects(region))
        {
            return Some(region);
        }
    }
    None
}
