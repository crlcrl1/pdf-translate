use super::{PageFrame, inline::is_url_text};
use crate::model::{Align, BBox, Column, PageLayout, Paragraph, Role, TextLine};
use crate::text::{bullet_marker, is_cjk, is_section_number};
use std::cmp::Ordering;

pub(super) fn protect_references(pages: &mut [PageLayout]) {
    let mut in_references = false;
    for page in pages {
        // Full-width blocks divide the page into bands. Within each band read
        // the left column top-to-bottom, then the right column. Comparing only
        // y coordinates incorrectly protects body text opposite References.
        let mut full: Vec<_> = (0..page.paragraphs.len())
            .filter(|&i| page.paragraphs[i].column == Column::Full)
            .collect();
        full.sort_by(|&a, &b| {
            page.paragraphs[b]
                .bbox
                .y1
                .total_cmp(&page.paragraphs[a].bbox.y1)
        });
        let mut remaining: Vec<_> = (0..page.paragraphs.len())
            .filter(|&i| page.paragraphs[i].column != Column::Full)
            .collect();
        let mut order = Vec::new();
        for separator in full.into_iter().map(Some).chain(std::iter::once(None)) {
            let threshold = separator.map_or(f64::NEG_INFINITY, |i| page.paragraphs[i].bbox.y1);
            let mut band = Vec::new();
            remaining.retain(|&i| {
                if page.paragraphs[i].bbox.y1 > threshold {
                    band.push(i);
                    false
                } else {
                    true
                }
            });
            band.sort_by(|&a, &b| {
                let a = &page.paragraphs[a];
                let b = &page.paragraphs[b];
                (a.column == Column::Right)
                    .cmp(&(b.column == Column::Right))
                    .then_with(|| b.bbox.y1.total_cmp(&a.bbox.y1))
            });
            order.extend(band);
            order.extend(separator);
        }
        for index in order {
            let paragraph = &mut page.paragraphs[index];
            if paragraph.role == Role::Reference {
                in_references = true;
            } else if in_references && is_section_heading(paragraph) {
                in_references = false;
            }
            // A citation at the start of prose is not a bibliography entry.
            // Use the section boundary, including continuation pages, rather
            // than treating every paragraph beginning with `[number]` as one.
            paragraph.protected |= in_references;
        }
    }
}

fn starts_list_item(text: &str) -> bool {
    let text = text.trim_start();
    if bullet_marker(text).is_some() {
        return true;
    }
    let Some(first) = text.chars().next() else {
        return false;
    };
    if first == '(' {
        let rest = &text[1..];
        let Some(end) = rest.find(')') else {
            return false;
        };
        let marker = &rest[..end];
        return !marker.is_empty()
            && marker.len() <= 4
            && marker.chars().all(|ch| ch.is_ascii_alphanumeric())
            && rest[end + 1..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace);
    }
    let marker_len = if first.is_ascii_digit() {
        text.chars()
            .take_while(|ch| ch.is_ascii_digit())
            .map(char::len_utf8)
            .sum()
    } else if first.is_ascii_alphabetic() {
        1
    } else {
        return false;
    };
    let rest = &text[marker_len..];
    (rest.starts_with('.') || rest.starts_with(')'))
        && rest[1..].chars().next().is_some_and(char::is_whitespace)
}

fn is_section_heading(paragraph: &Paragraph) -> bool {
    if !matches!(paragraph.role, Role::Heading | Role::Title) {
        return false;
    }
    let text = paragraph.text.trim();
    text.chars().count() <= 80
        && !text.to_ascii_lowercase().contains("http")
        && text.chars().any(|ch| ch.is_alphabetic())
}

pub(super) fn build_paragraphs(
    lines: &[TextLine],
    page_index: usize,
    frame: PageFrame,
    body_size: f64,
) -> Vec<Paragraph> {
    let mut result = Vec::new();

    for column in [Column::Full, Column::Left, Column::Right] {
        let mut indices: Vec<_> = (0..lines.len())
            .filter(|&i| lines[i].column == column)
            .collect();
        indices.sort_by(|&a, &b| {
            lines[b]
                .bbox
                .y1
                .partial_cmp(&lines[a].bbox.y1)
                .unwrap_or(Ordering::Equal)
        });
        let mut leading: Vec<_> = indices
            .windows(2)
            .filter_map(|pair| {
                let a = &lines[pair[0]];
                let b = &lines[pair[1]];
                let ratio = (a.baseline - b.baseline) / a.font_size;
                (!a.formula
                    && !b.formula
                    && a.text.chars().count() >= 20
                    && b.text.chars().count() >= 20
                    && (a.font_size - b.font_size).abs() < a.font_size * 0.08
                    && (0.8..1.8).contains(&ratio))
                .then_some(ratio)
            })
            .collect();
        leading.sort_by(f64::total_cmp);
        let leading = (leading.len() >= 3).then(|| leading[leading.len() / 2]);
        let mut current: Vec<usize> = Vec::new();
        for index in indices {
            if let Some(&prev) = current.last()
                && (!can_join(&lines[prev], &lines[index], page_index, frame, body_size)
                    || (lines[prev].text.trim_end().ends_with(['.', '!', '?'])
                        && (leading.is_some_and(|leading| {
                            lines[prev].baseline - lines[index].baseline
                                > lines[prev].font_size * (leading + 0.28)
                        }) || (starts_list_item(&lines[current[0]].text)
                            // A following paragraph can have its own first-line
                            // indent, leaving only a small outdent from the list
                            // marker. Wrapped list text stays at/right of it.
                            && lines[index].bbox.x0
                                < lines[current[0]].bbox.x0 - body_size * 0.35))))
            {
                result.push(make_paragraph(
                    lines,
                    std::mem::take(&mut current),
                    page_index,
                    result.len(),
                ));
            }
            current.push(index);
        }
        if !current.is_empty() {
            result.push(make_paragraph(lines, current, page_index, result.len()));
        }
    }
    result
}

fn can_join(
    previous: &TextLine,
    line: &TextLine,
    page_index: usize,
    frame: PageFrame,
    body_size: f64,
) -> bool {
    let role = classify_line(line, page_index, frame, body_size);
    let previous_role = classify_line(previous, page_index, frame, body_size);
    if previous_role == Role::Caption
        && super::algorithms::is_caption(&previous.text)
        && super::algorithms::is_field(&line.text)
    {
        return false;
    }
    if previous_role == Role::Title
        && matches!(role, Role::Title | Role::Heading)
        && (previous.font_size - line.font_size).abs() < previous.font_size * 0.06
        && previous.bbox.y0 - line.bbox.y1 < line.font_size
        && previous.bbox.y0 > line.bbox.y0
    {
        return true;
    }
    if previous_role == Role::Heading
        && !looks_like_section_heading(&line.text)
        && (previous.font_size - line.font_size).abs() < previous.font_size * 0.06
        && previous.bbox.y0 - line.bbox.y1 < line.font_size * 0.8
        && previous.bbox.y0 > line.bbox.y0
        && line
            .spans
            .iter()
            .find(|s| !s.formula && s.text.chars().any(char::is_alphabetic))
            .is_some_and(|s| s.bold || s.font_size > body_size * 1.08)
    {
        // A wrapped heading can finish with a run-in sentence. Its first
        // ordinary body line still starts a separate paragraph.
        return true;
    }
    if looks_like_section_heading(&line.text)
        && line
            .spans
            .iter()
            .find(|span| !span.formula && span.text.chars().any(|ch| ch.is_alphabetic()))
            .is_some_and(|span| span.bold)
    {
        // Numbered run-in headings start a new paragraph even though their
        // following prose shares the same line and body-size typography.
        return false;
    }
    // Proof-ending marks close the block even when TeX uses ordinary leading
    // before the following unindented paragraph.
    if previous.text.trim_end().ends_with(['□', '∎', '■']) {
        return false;
    }
    let starts_lowercase = line.text.chars().next().is_some_and(|ch| ch.is_lowercase());
    let previous_ends_sentence = previous
        .text
        .trim_end()
        .ends_with(['.', '!', '?', ':', ';']);
    let previous_is_body_line =
        previous.text.chars().count() >= 20 && previous.bbox.width() > frame.width * 0.2;
    let role = if (role == Role::Caption || (role == Role::Heading && starts_lowercase))
        && previous_role == Role::Paragraph
        && previous_is_body_line
        && !previous_ends_sentence
    {
        // A line such as "Fig. 4. First, ..." is a sentence that
        // continues a body paragraph, not a figure caption. A short lowercase
        // continuation also stays prose when small references/code elsewhere
        // on the page make its font look comparatively large.
        Role::Paragraph
    } else {
        role
    };
    let caption_continuation = previous_role == Role::Caption
        && line.font_size <= previous.font_size * 1.06
        && (role == Role::Paragraph || starts_lowercase || previous.text.trim_end().ends_with('-'));
    if !caption_continuation && (previous_role != Role::Paragraph || role != Role::Paragraph) {
        return false;
    }
    // A body line that starts deeper than the previous line after
    // a sentence ends is a first-line indent, not a continuation.
    // Without this, whole groups of paragraphs are translated as
    // one block and one protected glyph disables all of them.
    let first_line_indent = line.bbox.x0 - previous.bbox.x0;
    let previous_ends_paragraph = previous.text.trim_end().ends_with(['.', '!', '?']);
    if !caption_continuation
        && role == Role::Paragraph
        && previous_ends_paragraph
        && first_line_indent > line.font_size.max(8.0) * 0.8
    {
        return false;
    }
    if previous.formula || line.formula {
        return false;
    }
    // A new numbered/bulleted item starts a new paragraph even when the
    // previous item's wrapped line is close and horizontally overlapping.
    if starts_list_item(&line.text) && (previous_ends_sentence || starts_list_item(&previous.text))
    {
        return false;
    }
    let gap = previous.bbox.y0 - line.bbox.y1;
    let max_gap = line.font_size.max(8.0) * 0.75;
    if gap > max_gap || gap < -line.bbox.height() * 0.8 {
        return false;
    }
    let overlap = previous.bbox.horizontal_overlap(line.bbox);
    let left_aligned = (previous.bbox.x0 - line.bbox.x0).abs() < line.font_size.max(8.0) * 1.6;
    let size_close =
        (previous.font_size - line.font_size).abs() < previous.font_size.max(line.font_size) * 0.22;
    (overlap > 0.25 || left_aligned) && size_close
}

pub(super) fn protect_attributions(paragraphs: &mut [Paragraph], lines: &[TextLine], page: usize) {
    if page != 0 {
        return;
    }
    for paragraph in paragraphs {
        if paragraph.role != Role::Paragraph || paragraph.line_indices.len() < 2 {
            continue;
        }
        // Name/affiliation grids also occur after an abstract. Require every
        // row to consist of a bold name and a smaller italic affiliation.
        let attribution = paragraph.lines(lines).all(|line| {
            let spans: Vec<_> = line
                .spans
                .iter()
                .filter(|s| s.text.chars().any(char::is_alphabetic))
                .collect();
            let Some(name) = spans.first() else {
                return false;
            };
            name.bold
                && !name.italic
                && spans
                    .iter()
                    .any(|s| s.italic && s.font_size < name.font_size * 0.95)
                && spans.iter().all(|s| s.bold || s.italic)
                && line.text.chars().count() < 100
        });
        paragraph.protected |= attribution;
    }
}

pub(super) fn make_paragraph(
    page_lines: &[TextLine],
    indices: Vec<usize>,
    page_index: usize,
    index: usize,
) -> Paragraph {
    let lines = indices.iter().map(|&i| &page_lines[i]);
    let bbox = lines.clone().fold(BBox::default(), |acc, line| {
        if acc == BBox::default() {
            line.bbox
        } else {
            acc.union(line.bbox)
        }
    });
    let mut text = String::new();
    for (line_index, line) in lines.clone().enumerate() {
        let current = line.text.trim();
        if current.is_empty() {
            continue;
        }
        if line_index > 0 {
            if text.ends_with('-') && current.chars().next().is_some_and(|c| c.is_lowercase()) {
                text.pop();
            } else if text.ends_with('-')
                && current.chars().next().is_some_and(|c| c.is_ascii_digit())
            {
                // Keep "CIFAR-" + "10" as "CIFAR-10" without a space.
            } else if !text.ends_with(char::is_whitespace)
                && !current.starts_with(char::is_whitespace)
                && !(text.ends_with(is_cjk) && current.starts_with(is_cjk))
            {
                text.push(' ');
            }
        }
        text.push_str(current);
    }

    let font_size = weighted_median(
        &lines
            .clone()
            .map(|line| (line.font_size, line.text.chars().count().max(1)))
            .collect::<Vec<_>>(),
    );
    let column = lines
        .clone()
        .next()
        .map(|line| line.column)
        .unwrap_or(Column::Full);
    let has_formula = lines.clone().any(|line| line.formula);

    Paragraph {
        id: format!("p{}-{index}", page_index + 1),
        page: page_index,
        bbox,
        text,
        line_indices: indices,
        base_style: Default::default(),
        font_size,
        column,
        role: Role::Paragraph,
        align: Align::Left,
        protected: has_formula,
        has_formula,
        formulas: Vec::new(),
        prompt_text: None,
        translation: None,
    }
}

pub(super) fn merge_caption_continuations(paragraphs: &mut Vec<Paragraph>) {
    loop {
        let mut merged = false;
        let mut index = 0;
        while index < paragraphs.len() {
            if paragraphs[index].role != Role::Caption {
                index += 1;
                continue;
            }
            let previous = &paragraphs[index];
            let mut best: Option<usize> = None;
            let mut best_gap = f64::INFINITY;
            for (candidate_index, candidate) in paragraphs.iter().enumerate() {
                if candidate_index == index
                    || !(candidate.column == previous.column
                        || candidate.column == Column::Full
                        || previous.column == Column::Full)
                    || candidate.bbox.horizontal_overlap(previous.bbox) < 0.1
                    || candidate.bbox.y1 >= previous.bbox.y0
                {
                    continue;
                }
                let gap = previous.bbox.y0 - candidate.bbox.y1;
                if gap > previous.font_size.max(8.0) * 0.8 || gap < -1.0 {
                    continue;
                }
                let starts_lower = candidate
                    .text
                    .trim()
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_lowercase());
                if !starts_lower && !previous.text.trim_end().ends_with('-') {
                    continue;
                }
                if gap < best_gap {
                    best_gap = gap;
                    best = Some(candidate_index);
                }
            }
            if let Some(candidate_index) = best {
                let next = paragraphs.remove(candidate_index);
                index -= usize::from(candidate_index < index);
                let prev = &mut paragraphs[index];
                if prev.text.ends_with('-')
                    && next.text.chars().next().is_some_and(|ch| ch.is_lowercase())
                {
                    prev.text.pop();
                } else if prev.text.ends_with('-')
                    && next
                        .text
                        .chars()
                        .next()
                        .is_some_and(|ch| ch.is_ascii_digit())
                {
                } else if !prev.text.ends_with(char::is_whitespace)
                    && !next.text.starts_with(char::is_whitespace)
                {
                    prev.text.push(' ');
                }
                prev.text.push_str(next.text.trim());
                prev.line_indices.extend(next.line_indices);
                prev.bbox = prev.bbox.union(next.bbox);
                prev.has_formula |= next.has_formula;
                merged = true;
            }
            index += 1;
        }
        if !merged {
            break;
        }
    }
}

pub(super) fn protect_footnotes(
    paragraphs: &mut [Paragraph],
    lines: &[TextLine],
    frame: PageFrame,
    body_size: f64,
) {
    let url_boxes: Vec<BBox> = paragraphs
        .iter()
        .filter(|paragraph| {
            paragraph.font_size < body_size * 0.85
                && paragraph.bbox.y1 < frame.bottom() + frame.height * 0.15
                && paragraph
                    .lines(lines)
                    .flat_map(|line| &line.spans)
                    .any(|span| is_url_text(&span.text))
        })
        .map(|paragraph| paragraph.bbox)
        .collect();

    for paragraph in paragraphs {
        if paragraph.font_size >= body_size * 0.85
            || paragraph.bbox.y1 >= frame.bottom() + frame.height * 0.15
        {
            continue;
        }
        let is_url = paragraph
            .lines(lines)
            .flat_map(|line| &line.spans)
            .any(|span| is_url_text(&span.text));
        let short_connector = paragraph.text.trim().chars().count() <= 12
            && url_boxes.iter().any(|url| {
                let vertical = paragraph.bbox.y0 < url.y1 && paragraph.bbox.y1 > url.y0;
                let gap = if paragraph.bbox.x1 < url.x0 {
                    url.x0 - paragraph.bbox.x1
                } else if url.x1 < paragraph.bbox.x0 {
                    paragraph.bbox.x0 - url.x1
                } else {
                    0.0
                };
                vertical && gap < 20.0
            });
        if is_url || short_connector {
            paragraph.protected = true;
        }
    }
}

pub(super) fn protect_front_matter(paragraphs: &mut [Paragraph], page_index: usize) {
    if page_index != 0 {
        return;
    }
    let Some(abstract_heading) = paragraphs
        .iter()
        .find(|paragraph| {
            paragraph
                .text
                .trim()
                .to_ascii_lowercase()
                .starts_with("abstract")
        })
        .or_else(|| {
            let opening = paragraphs
                .iter()
                .any(|p| p.role == Role::Title)
                .then(|| {
                    paragraphs
                        .iter()
                        .find(|p| p.role == Role::Paragraph && p.text.chars().count() > 300)
                })
                .flatten();
            // With no explicit abstract label, the first prose block may
            // still be an abstract. Use whichever starts first: that block
            // or the first section, so neither is mistaken for author data.
            let section = paragraphs
                .iter()
                .filter(|p| p.role == Role::Heading && looks_like_section_heading(&p.text))
                .max_by(|a, b| a.bbox.y1.total_cmp(&b.bbox.y1));
            opening
                .into_iter()
                .chain(section)
                .max_by(|a, b| a.bbox.y1.total_cmp(&b.bbox.y1))
        })
    else {
        return;
    };
    let threshold = abstract_heading.bbox.y1;
    let title = paragraphs
        .iter()
        .filter(|p| {
            p.bbox.y0 > threshold
                && !matches!(p.role, Role::Header | Role::Footer | Role::Formula)
                && p.text.chars().filter(|ch| ch.is_alphabetic()).count() >= 10
        })
        .max_by(|a, b| {
            a.font_size
                .total_cmp(&b.font_size)
                .then_with(|| a.bbox.y1.total_cmp(&b.bbox.y1))
        })
        .map(|p| p.id.clone());
    for paragraph in paragraphs {
        if title.as_ref() == Some(&paragraph.id) {
            paragraph.role = Role::Title;
        } else if paragraph.role != Role::Title && paragraph.bbox.y0 > threshold {
            // Title/author/affiliation front matter is kept as original artwork;
            // translating it usually breaks the author grid and email addresses.
            paragraph.protected = true;
        }
    }
}

pub(super) fn is_code_block(paragraph: &Paragraph, lines: &[TextLine]) -> bool {
    let mut total = 0;
    let mut monospace = 0;
    for span in paragraph.lines(lines).flat_map(|line| &line.spans) {
        let count = span.text.chars().filter(|ch| !ch.is_whitespace()).count();
        total += count;
        if span.mono {
            monospace += count;
        } else if !span.formula
            && span
                .text
                .chars()
                .filter(|ch| ch.is_ascii_alphabetic())
                .count()
                >= 3
        {
            // A code identifier inside an ordinary sentence stays inline.
            // Math-font symbols and subscripts inside listings are allowed.
            return false;
        }
    }
    total > 0 && monospace * 5 >= total * 4
}

pub(super) fn protect_figure_text(
    paragraphs: &mut [Paragraph],
    images: &[crate::model::ImageBox],
    lines: &[TextLine],
) {
    // Text inside a raster image is already part of the image; avoid translating
    // OCR/hidden labels that happen to sit on top of it.
    for paragraph in paragraphs.iter_mut() {
        if paragraph.role == Role::Caption {
            continue;
        }
        if let Some(image) = images
            .iter()
            .find(|image| image.bbox.intersects(paragraph.bbox))
            && paragraph.bbox.width() * paragraph.bbox.height()
                < image.bbox.width() * image.bbox.height() * 0.5
        {
            paragraph.protected = true;
        }
    }

    let mut captions: Vec<_> = paragraphs
        .iter()
        .enumerate()
        .filter(|(_, paragraph)| {
            paragraph.role == Role::Caption && !super::algorithms::is_caption(&paragraph.text)
        })
        .filter_map(|(index, paragraph)| {
            caption_kind(&paragraph.text)
                .map(|is_table| (index, paragraph.bbox, is_table, paragraph.font_size))
        })
        .collect();
    captions.sort_by(|a, b| b.1.y1.total_cmp(&a.1.y1));
    let body_width = paragraphs
        .iter()
        .filter(|paragraph| {
            matches!(
                paragraph.role,
                Role::Paragraph | Role::Heading | Role::Title
            )
        })
        .map(|paragraph| paragraph.bbox.width())
        .fold(0.0, f64::max);

    let mut table_regions = Vec::new();
    for (caption_index, caption, is_table, caption_font_size) in captions {
        // Vector diagrams have the same rows of separated labels as tables.
        // Infer that region before exempting headings: large diagram labels
        // are often classified as headings solely because of their font size.
        if let Some(region) =
            super::tables::table_region(paragraphs, lines, caption_index, &table_regions)
        {
            table_regions.push(region);
            for paragraph in paragraphs.iter_mut() {
                if paragraph.role != Role::Caption
                    && paragraph.bbox.y0 >= region.y0 - 0.5
                    && paragraph.bbox.y1 <= region.y1 + 0.5
                    && paragraph.bbox.horizontal_overlap(region) > 0.8
                {
                    paragraph.protected = true;
                }
            }
            if is_table {
                continue;
            }
        }
        let boundaries: Vec<_> = paragraphs
            .iter()
            .filter(|paragraph| {
                paragraph.bbox.horizontal_overlap(caption) > 0.5
                    && (matches!(paragraph.role, Role::Title | Role::Heading)
                        || (paragraph.role == Role::Paragraph
                            && paragraph
                                .text
                                .chars()
                                .filter(|ch| ch.is_alphabetic())
                                .count()
                                > 60
                            && paragraph.bbox.width() > caption.width() * 0.8))
            })
            .map(|p| p.bbox)
            .collect();
        for paragraph in paragraphs.iter_mut() {
            if paragraph.role == Role::Caption
                || matches!(paragraph.role, Role::Title | Role::Heading)
            {
                continue;
            }
            if paragraph.bbox.horizontal_overlap(caption) < 0.2 {
                continue;
            }
            if is_table
                && boundaries.iter().any(|boundary| {
                    (boundary.y0 > caption.y1 && boundary.y1 < paragraph.bbox.y0)
                        || (boundary.y1 < caption.y0 && boundary.y0 > paragraph.bbox.y1)
                })
            {
                continue;
            }
            // Full-width paragraphs are body text, not table cells or figure
            // labels. Without this, a table caption protects every following
            // body paragraph within 300 points and leaves it untranslated.
            if paragraph.bbox.width() > body_width.max(caption.width()) * 0.85 {
                continue;
            }
            let small_text = paragraph.font_size < caption_font_size * 0.9;
            let table_size_limit = caption_font_size
                * if paragraph.text.chars().count() <= 60 {
                    1.25
                } else {
                    1.06
                };
            let above = paragraph.bbox.y0 >= caption.y1 - 1.0;
            if above {
                if is_table {
                    if paragraph.font_size > table_size_limit {
                        continue;
                    }
                } else if paragraph.text.chars().count() >= 40 && !small_text {
                    // Figure labels above a caption are usually short.
                    continue;
                }
            } else if is_table {
                // Table cells are often set at almost the caption's font size;
                // protect them even when the row text is long.
                if paragraph.font_size > table_size_limit {
                    continue;
                }
            } else {
                continue;
            }
            let distance = if above {
                paragraph.bbox.y0 - caption.y1
            } else {
                caption.y0 - paragraph.bbox.y1
            };
            let limit = if is_table { 300.0 } else { f64::INFINITY };
            if distance >= 0.0 && distance < limit {
                paragraph.protected = true;
            }
        }
    }
}

pub(super) fn caption_kind(text: &str) -> Option<bool> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    let (rest, is_table) = if let Some(rest) = lower.strip_prefix("figure") {
        (rest, false)
    } else if let Some(rest) = lower.strip_prefix("fig.") {
        (rest, false)
    } else if let Some(rest) = lower.strip_prefix("table") {
        (rest, true)
    } else {
        let rest = lower.strip_prefix("algorithm")?;
        (rest, false)
    };

    let rest = rest.trim_start();
    let mut chars = rest.chars().peekable();
    let mut saw_label = false;
    while let Some(ch) = chars.peek().copied() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            saw_label = true;
            chars.next();
        } else {
            break;
        }
    }
    if !saw_label {
        return None;
    }
    let rest: String = chars.collect();
    let rest = rest.trim_start();
    // "Figure 1." / "Fig. 1:" / "Table 2 |" are captions.
    // "Fig. 6 (middle) shows ..." is a body sentence that merely cites a figure.
    if rest.starts_with('.')
        || rest.starts_with(':')
        || rest.starts_with('|')
        || rest.starts_with('-')
        || text[text.len() - rest.len()..]
            .chars()
            .next()
            .is_some_and(|ch| ch.is_uppercase())
    {
        Some(is_table)
    } else {
        None
    }
}

fn classify_line(line: &TextLine, page_index: usize, frame: PageFrame, body_size: f64) -> Role {
    let text = line.text.trim();
    if line.formula {
        return Role::Formula;
    }
    if is_page_number(text) {
        return Role::PageNumber;
    }
    if text.is_empty() {
        return Role::Unknown;
    }
    let top_band =
        line.bbox.y0 > frame.y0 + frame.height * 0.935 && line.font_size <= body_size * 1.15;
    let bottom_band =
        line.bbox.y1 < frame.y0 + frame.height * 0.065 && line.font_size <= body_size * 1.15;
    if (top_band || bottom_band) && text.len() < 120 {
        return if top_band { Role::Header } else { Role::Footer };
    }

    let lower = text.to_lowercase();
    if lower == "references" || lower == "bibliography" {
        return Role::Reference;
    }
    if lower == "abstract" {
        return Role::Heading;
    }
    let prefix = text.split_whitespace().next().unwrap_or_default();
    let styled_span = |span: &crate::model::TextSpan| {
        !span.formula
            && (span.bold
                || span.font_size > body_size * 1.08
                || span.font_name.as_deref().is_some_and(|font| {
                    let font = font.to_ascii_lowercase();
                    font.contains("csc") || font.contains("smallcaps")
                }))
    };
    let heading_style = line.font_size > body_size * 1.08
        || line.spans.iter().any(styled_span)
        || prefix.trim_end_matches('.').contains('.');
    let letters =
        |span: &crate::model::TextSpan| span.text.chars().filter(|ch| ch.is_alphabetic()).count();
    let total_letters: usize = line.spans.iter().map(letters).sum();
    let styled_letters: usize = line
        .spans
        .iter()
        .filter(|span| styled_span(span))
        .map(letters)
        .sum();
    // A full prose line is also geometrically centered. Require consistent
    // heading typography before treating a wide line as a standalone heading;
    // a bold run-in heading followed by ordinary prose stays in its paragraph.
    let uniform_heading = total_letters > 0 && styled_letters * 10 >= total_letters * 9;
    if lower.strip_prefix("appendix").is_some_and(|rest| {
        rest.is_empty() || rest.starts_with(':') || rest.starts_with(char::is_whitespace)
    }) && text.chars().count() < 100
        && (uniform_heading || text.split_whitespace().count() <= 4)
    {
        return Role::Heading;
    }
    let bare_letter = prefix.len() == 1 && prefix.chars().all(|ch| ch.is_ascii_uppercase());
    if looks_like_section_heading(text)
        && heading_style
        // At the top of the first page, "A ..." is commonly the paper
        // title, not an appendix marker. Let front-matter detection decide.
        && (!bare_letter || page_index != 0 || line.bbox.y0 < frame.y0 + frame.height * 0.72)
        && line.bbox.width() < frame.width * if uniform_heading { 0.88 } else { 0.5 }
    {
        return Role::Heading;
    }
    if caption_kind(text).is_some() {
        return Role::Caption;
    }

    if page_index == 0
        && !top_band
        && line.bbox.y0 > frame.y0 + frame.height * 0.72
        && line.font_size > body_size * 1.25
        && text.len() < 200
    {
        return Role::Title;
    }
    if line.bbox.width() < frame.width * 0.5
        && text.chars().count() < 100
        && (line.font_size > body_size * 1.2
            || (line.spans.iter().any(|span| span.bold) && line.font_size > body_size * 1.08))
    {
        return Role::Heading;
    }
    Role::Paragraph
}

pub(super) fn classify_paragraph(
    paragraph: &Paragraph,
    lines: &[TextLine],
    page_index: usize,
    frame: PageFrame,
    body_size: f64,
) -> Role {
    let first = paragraph
        .lines(lines)
        .next()
        .map(|line| classify_line(line, page_index, frame, body_size))
        .unwrap_or(Role::Paragraph);
    if paragraph.has_formula {
        return Role::Formula;
    }
    if first == Role::Title {
        return Role::Title;
    }
    if first == Role::Heading {
        if paragraph.text.chars().count() > 180
            || paragraph.lines(lines).skip(1).any(|line| {
                line.spans.iter().any(|span| {
                    !span.formula
                        && !span.bold
                        && span.font_size <= body_size * 1.08
                        && span.text.chars().filter(|ch| ch.is_alphabetic()).count() >= 5
                })
            })
        {
            return Role::Paragraph;
        }
        return Role::Heading;
    }
    if first == Role::Caption {
        return Role::Caption;
    }
    if first == Role::Header || first == Role::Footer || first == Role::PageNumber {
        return first;
    }
    if first == Role::Reference {
        return Role::Reference;
    }
    Role::Paragraph
}

pub(super) fn paragraph_alignment(
    paragraph: &Paragraph,
    lines: &[TextLine],
    frame: PageFrame,
) -> Align {
    if paragraph.role == Role::Title || paragraph.role == Role::Heading {
        let left = paragraph.bbox.x0;
        let right = paragraph.bbox.x1;
        let centered = ((left + right) * 0.5 - frame.center_x()).abs() < frame.width * 0.06;
        let body_left = lines
            .iter()
            .enumerate()
            .filter(|(i, line)| {
                !line.formula
                    && !paragraph.line_indices.contains(i)
                    && line.bbox.width() > frame.width * 0.3
            })
            .map(|(_, line)| line.bbox.x0)
            .min_by(f64::total_cmp);
        let left_anchored = paragraph.role == Role::Heading
            && body_left.is_some_and(|x| (left - x).abs() < paragraph.font_size * 0.6);
        if centered && !left_anchored && paragraph.bbox.width() < frame.width * 0.8 {
            return Align::Center;
        }
    }
    if matches!(paragraph.role, Role::Paragraph | Role::Caption)
        && !paragraph.has_formula
        && paragraph.line_indices.len() >= 2
    {
        // Centered quotations and captions need not be headings. Require both
        // margins to vary, so indented or justified prose is not mistaken for
        // a centered block merely because its bounding box is near the center.
        let mut left = (f64::INFINITY, f64::NEG_INFINITY);
        let mut right = left;
        let mut center = left;
        for &index in &paragraph.line_indices {
            let bbox = lines[index].bbox;
            left = (left.0.min(bbox.x0), left.1.max(bbox.x0));
            right = (right.0.min(bbox.x1), right.1.max(bbox.x1));
            let x = (bbox.x0 + bbox.x1) * 0.5;
            center = (center.0.min(x), center.1.max(x));
        }
        if center.1 - center.0 <= paragraph.font_size * 0.5
            && left.1 - left.0 >= paragraph.font_size
            && right.1 - right.0 >= paragraph.font_size
        {
            return Align::Center;
        }
    }
    Align::Left
}

pub(super) fn body_font_size(lines: &[TextLine]) -> f64 {
    let weighted: Vec<(f64, usize)> = lines
        .iter()
        .filter(|line| {
            !line.formula
                && line.text.chars().count() >= 8
                && line.bbox.width() >= 30.0
                && line.bbox.width() < 10_000.0
        })
        .map(|line| (line.font_size, line.text.chars().count().max(1)))
        .collect();
    let weighted = if weighted.is_empty() {
        lines
            .iter()
            .filter(|line| !line.formula)
            .map(|line| (line.font_size, line.text.chars().count().max(1)))
            .collect()
    } else {
        weighted
    };
    if weighted.is_empty() {
        10.0
    } else {
        weighted_median(&weighted).clamp(6.0, 30.0)
    }
}

fn weighted_median(values: &[(f64, usize)]) -> f64 {
    if values.is_empty() {
        return 10.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
    let total: usize = sorted.iter().map(|(_, weight)| *weight).sum();
    let mut running = 0usize;
    for (value, weight) in sorted {
        running += weight;
        if running * 2 >= total {
            return value;
        }
    }
    values[0].0
}

fn looks_like_section_heading(text: &str) -> bool {
    let mut words = text.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    let Some(second) = words.next() else {
        return false;
    };
    if !is_section_number(first) {
        return false;
    }
    // "3. Deep..." is a section heading; "2015 classification..." is a
    // continuation line that merely starts with a year.
    second.chars().next().is_some_and(|ch| ch.is_uppercase()) && text.chars().count() < 100
}

fn is_page_number(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() || text.len() > 12 {
        return false;
    }
    text.chars().all(|c| c.is_ascii_digit()) || text.chars().all(is_roman_numeral_digit)
}

pub(super) fn is_roman_numeral_digit(ch: char) -> bool {
    matches!(
        ch,
        'i' | 'v' | 'x' | 'l' | 'c' | 'd' | 'm' | 'I' | 'V' | 'X'
    )
}
