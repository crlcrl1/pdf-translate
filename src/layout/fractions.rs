use crate::model::{BBox, GlyphBox};

/// Bind numeric rows across a short fraction bar before baseline clustering.
/// Otherwise a denominator can join a radical on the following reading line,
/// and a fraction with no math-font glyphs can be translated as an integer.
pub(super) fn bind_numeric_fractions(mut glyphs: Vec<GlyphBox>, rules: &[BBox]) -> Vec<GlyphBox> {
    for &rule in rules {
        if rule.height() > 1.5 || rule.width() < 1.0 {
            continue;
        }
        let row = |above: bool| {
            let mut indices: Vec<_> = glyphs
                .iter()
                .enumerate()
                .filter(|(_, g)| {
                    let gap = if above {
                        g.bbox.y0 - rule.y1
                    } else {
                        rule.y0 - g.bbox.y1
                    };
                    !g.math_group
                        && g.angle.abs() < 0.1
                        && !g.text.is_empty()
                        && g.text.chars().all(|ch| ch.is_ascii_digit())
                        && rule.width() < g.font_size * 5.0
                        && g.bbox.x0 >= rule.x0 - 0.5
                        && g.bbox.x1 <= rule.x1 + 0.5
                        && gap >= -0.5
                        && gap < g.font_size * 0.65
                })
                .map(|(i, _)| i)
                .collect();
            indices.sort_by(|&a, &b| glyphs[a].bbox.x0.total_cmp(&glyphs[b].bbox.x0));
            let first = *indices.first()?;
            let size = glyphs[first].font_size;
            let bbox = indices
                .iter()
                .map(|&i| glyphs[i].bbox)
                .reduce(BBox::union)?;
            (indices.iter().all(|&i| {
                (glyphs[i].origin_y - glyphs[first].origin_y).abs() < size * 0.15
                    && (glyphs[i].font_size - size).abs() < size * 0.1
            }) && (bbox.center_x() - rule.center_x()).abs() < size * 0.2
                && bbox.width() > rule.width() * 0.45)
                .then_some(indices)
        };
        let (Some(upper), Some(lower)) = (row(true), row(false)) else {
            continue;
        };
        let text = |row: &[usize]| {
            row.iter()
                .map(|&i| glyphs[i].text.as_str())
                .collect::<String>()
        };
        let mut combined = glyphs[upper[0]].clone();
        combined.text = format!("{}/{}", text(&upper), text(&lower));
        combined.bbox = rule;
        combined.sources.clear();
        combined.decorations = vec![rule];
        combined.math_group = true;
        for &i in upper.iter().chain(&lower) {
            let g = &glyphs[i];
            combined.bbox = combined.bbox.union(g.bbox);
            combined.sources.extend_from_slice(&g.sources);
            combined.decorations.extend_from_slice(&g.decorations);
            combined.protected |= g.protected;
        }
        combined.origin_x = rule.x0;
        // TeX's math axis is about one third of a script-font em above
        // the enclosing baseline, also for fractions inside superscripts.
        combined.origin_y = (rule.y0 + rule.y1) * 0.5 - combined.font_size * 0.34;
        combined.advance = combined.bbox.width();
        let mut removed = upper;
        removed.extend(lower);
        removed.sort_unstable();
        for i in removed.into_iter().rev() {
            glyphs.remove(i);
        }
        glyphs.push(combined);
    }
    glyphs
}
