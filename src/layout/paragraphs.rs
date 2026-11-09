use super::{PageFrame, inline::is_url_text};
use crate::model::{Align, BBox, Column, PageLayout, Paragraph, Role, TextLine};
use crate::text::is_cjk;
use std::cmp::Ordering;

pub(super) fn protect_references(pages: &mut [PageLayout]) {
    let mut in_references = false;
    for page in pages {
        let heading_y = page
            .paragraphs
            .iter()
            .filter(|paragraph| paragraph.role == Role::Reference)
            .map(|paragraph| paragraph.bbox.y0)
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
        let has_reference_entry = page.paragraphs.iter().any(is_reference_entry);

        if let Some(heading_y) = heading_y {
            in_references = true;
            // Reference paragraphs may have been split into continuation
            // chunks that no longer start with "[n]". Protect everything
            // below the section heading on this page, but stop at a section
            // heading that starts content after the reference list.
            let stop_y = page
                .paragraphs
                .iter()
                .filter(|paragraph| {
                    matches!(paragraph.role, Role::Heading | Role::Title)
                        && paragraph.bbox.y1 < heading_y
                })
                .map(|paragraph| paragraph.bbox.y1)
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
            for paragraph in &mut page.paragraphs {
                let in_reference_region = paragraph.bbox.y0 <= heading_y + 1.0
                    && stop_y.is_none_or(|stop| paragraph.bbox.y0 > stop + 1.0);
                if paragraph.role == Role::Reference || in_reference_region {
                    paragraph.protected = true;
                }
            }
            continue;
        }

        if in_references {
            let has_heading = page.paragraphs.iter().any(|paragraph| {
                matches!(paragraph.role, Role::Heading | Role::Title)
                    && paragraph.bbox.y0 > page.y0 + page.height * 0.5
            });
            if has_heading {
                in_references = false;
            } else if has_reference_entry {
                // The reference list continues on this page.
                for paragraph in &mut page.paragraphs {
                    paragraph.protected = true;
                }
                continue;
            } else {
                in_references = false;
            }
        }

        for paragraph in &mut page.paragraphs {
            if paragraph.role == Role::Reference || is_reference_entry(paragraph) {
                paragraph.protected = true;
            }
        }
    }
}

fn is_reference_entry(paragraph: &Paragraph) -> bool {
    let text = paragraph.text.trim_start();
    text.starts_with('[') && text.chars().nth(1).is_some_and(|ch| ch.is_ascii_digit())
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
        let mut current: Vec<usize> = Vec::new();
        for index in indices {
            if let Some(&prev) = current.last()
                && !can_join(&lines[prev], &lines[index], page_index, frame, body_size)
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
    let starts_lowercase = line.text.chars().next().is_some_and(|ch| ch.is_lowercase());
    let previous_ends_sentence = previous
        .text
        .trim_end()
        .ends_with(['.', '!', '?', ':', ';']);
    let previous_is_body_line =
        previous.text.chars().count() >= 20 && previous.bbox.width() > frame.width * 0.2;
    let role = if role == Role::Caption
        && previous_role == Role::Paragraph
        && previous_is_body_line
        && !previous_ends_sentence
    {
        // A line such as "Fig. 4. First, ..." is a sentence that
        // continues a body paragraph, not a figure caption.
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
    let Some(abstract_heading) = paragraphs.iter().find(|paragraph| {
        paragraph
            .text
            .trim()
            .to_ascii_lowercase()
            .starts_with("abstract")
            && paragraph.role == Role::Heading
    }) else {
        return;
    };
    let threshold = abstract_heading.bbox.y1;
    for paragraph in paragraphs {
        if paragraph.role != Role::Title && paragraph.bbox.y0 > threshold {
            // Title/author/affiliation front matter is kept as original artwork;
            // translating it usually breaks the author grid and email addresses.
            paragraph.protected = true;
        }
    }
}

pub(super) fn protect_figure_text(paragraphs: &mut [Paragraph], images: &[crate::model::ImageBox]) {
    // Text inside a raster image is already part of the image; avoid translating
    // OCR/hidden labels that happen to sit on top of it.
    for paragraph in paragraphs.iter_mut() {
        if let Some(image) = images
            .iter()
            .find(|image| image.bbox.intersects(paragraph.bbox))
            && paragraph.bbox.width() * paragraph.bbox.height()
                < image.bbox.width() * image.bbox.height() * 0.5
        {
            paragraph.protected = true;
        }
    }

    let captions: Vec<(BBox, bool, f64)> = paragraphs
        .iter()
        .filter(|paragraph| paragraph.role == Role::Caption)
        .filter_map(|paragraph| {
            caption_kind(&paragraph.text)
                .map(|is_table| (paragraph.bbox, is_table, paragraph.font_size))
        })
        .collect();

    for (caption, is_table, caption_font_size) in captions {
        for paragraph in paragraphs.iter_mut() {
            if paragraph.role == Role::Caption
                || matches!(paragraph.role, Role::Title | Role::Heading)
            {
                continue;
            }
            let small_text = paragraph.font_size < caption_font_size * 0.9;
            let above = paragraph.bbox.y0 >= caption.y1 - 1.0;
            if above {
                if is_table {
                    if paragraph.font_size > caption_font_size * 1.06 {
                        continue;
                    }
                } else if paragraph.text.chars().count() >= 40 && !small_text {
                    // Figure labels above a caption are usually short.
                    continue;
                }
            } else if is_table {
                // Table cells are often set at almost the caption's font size;
                // protect them even when the row text is long.
                if paragraph.font_size > caption_font_size * 1.06 {
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
    if looks_like_section_heading(text) && line.bbox.width() < frame.width * 0.5 {
        return Role::Heading;
    }
    if caption_kind(text).is_some() {
        return Role::Caption;
    }

    let centered = (line.bbox.center_x() - frame.center_x()).abs() < frame.width * 0.06;
    if page_index == 0
        && !top_band
        && line.bbox.y0 > frame.y0 + frame.height * 0.72
        && centered
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
        if paragraph.text.chars().count() > 180 {
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

pub(super) fn paragraph_alignment(paragraph: &Paragraph, frame: PageFrame) -> Align {
    if paragraph.role == Role::Title || paragraph.role == Role::Heading {
        let left = paragraph.bbox.x0;
        let right = paragraph.bbox.x1;
        let centered = ((left + right) * 0.5 - frame.center_x()).abs() < frame.width * 0.06;
        if centered && paragraph.bbox.width() < frame.width * 0.8 {
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
    if first.is_empty() || first.len() > 12 {
        return false;
    }
    if !first
        .chars()
        .all(|ch| ch.is_ascii_digit() || ch == '.' || ch == '-')
    {
        return false;
    }
    if !first.chars().any(|ch| ch.is_ascii_digit()) {
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
