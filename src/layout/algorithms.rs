use crate::model::{Paragraph, Role, TextLine};

pub(super) fn is_caption(text: &str) -> bool {
    text.trim_start()
        .to_ascii_lowercase()
        .starts_with("algorithm ")
}

pub(super) fn is_field(text: &str) -> bool {
    matches!(
        text.split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_end_matches(':'),
        "Input" | "Output" | "Require" | "Ensure" | "Parameters"
    )
}

/// Pseudocode uses indentation, line numbers and mathematical fonts even when
/// it is not monospaced. Preserve its body while translating the caption.
pub(super) fn protect_bodies(paragraphs: &mut [Paragraph], lines: &[TextLine]) {
    let captions: Vec<_> = paragraphs
        .iter()
        .enumerate()
        .filter(|(_, p)| p.role == Role::Caption && is_caption(&p.text))
        .map(|(i, _)| i)
        .collect();
    for caption in captions {
        let caption = &paragraphs[caption];
        let mut below: Vec<_> = paragraphs
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                p.column == caption.column
                    && p.bbox.y1 <= caption.bbox.y0 + 0.5
                    && p.bbox.horizontal_overlap(caption.bbox) > 0.2
            })
            .map(|(i, _)| i)
            .collect();
        below.sort_by(|&a, &b| paragraphs[b].bbox.y1.total_cmp(&paragraphs[a].bbox.y1));
        let mut edge = caption.bbox.y0;
        let mut body = Vec::new();
        let mut steps = 0;
        for index in below {
            let p = &paragraphs[index];
            if matches!(
                p.role,
                Role::Caption | Role::Heading | Role::Title | Role::Footer
            ) || edge - p.bbox.y1 > caption.font_size.max(p.font_size) * 1.5
            {
                break;
            }
            steps += p
                .lines(lines)
                .filter(|line| {
                    line.text.split_once(':').is_some_and(|(number, _)| {
                        let number = number.trim();
                        !number.is_empty() && number.chars().all(|c| c.is_ascii_digit())
                    })
                })
                .count();
            body.push(index);
            edge = edge.min(p.bbox.y0);
            if steps >= 2 && (p.text.starts_with("Output") || p.text.starts_with("Ensure")) {
                break;
            }
        }
        if steps >= 2 {
            for index in body {
                paragraphs[index].protected = true;
            }
        }
    }
}
