use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use reqwest::blocking::Client;
use sha2::{Digest, Sha256};

mod cache;
mod client;

use crate::markup::{strip_style_tags, validate_translation};
use crate::model::{LayoutDocument, Role};
use crate::progress::{CancellationToken, ProgressEvent, ProgressSink};
use crate::text::is_section_number;

#[derive(Debug, Clone)]
pub struct TranslationOptions {
    pub api_base: String,
    pub api_key: Option<String>,
    pub model: String,
    pub target_lang: String,
    pub fake: bool,
    pub concurrency: usize,
    pub max_tokens: u32,
    pub temperature: f32,
    pub timeout_secs: u64,
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Clone)]
pub struct TranslationStats {
    pub candidates: usize,
    pub translated: usize,
    pub cache_hits: usize,
    pub failed: usize,
    pub skipped_protected: usize,
    pub skipped_nontext: usize,
}

struct Task {
    page: usize,
    paragraph: usize,
    text: String,
    key: String,
    formulas: Vec<String>,
    context: String,
    role: Role,
    unbound_bullet: Option<BulletCacheSource>,
}

struct BulletCacheSource {
    text: String,
    context: String,
}

pub fn translate_document_observed(
    document: &mut LayoutDocument,
    options: &TranslationOptions,
    progress: &ProgressSink,
    cancel: &CancellationToken,
) -> Result<TranslationStats> {
    cancel.check()?;
    if !options.fake && options.api_key.is_none() && !is_local_endpoint(&options.api_base) {
        bail!(
            "missing API key: set [api].api_key in the config file, set OPENAI_API_KEY, \
             pass --api-key, or use --fake for a dry run"
        );
    }
    if options.concurrency == 0 {
        bail!(
            "translation concurrency must be at least 1 (--concurrency or [translation].concurrency)"
        );
    }

    let path = cache::cache_path();
    let cache = cache::load(&path).unwrap_or_else(|e| {
        tracing::warn!("cannot read translation cache: {e}");
        HashMap::new()
    });
    let client = if options.fake {
        None
    } else {
        let mut builder = Client::builder().timeout(Duration::from_secs(options.timeout_secs));
        if is_local_endpoint(&options.api_base) {
            builder = builder.no_proxy();
        }
        Some(builder.build().context("failed to build HTTP client")?)
    };
    let (tasks, mut stats) = collect_tasks(document, options);
    let total = tasks.len();
    let counter = AtomicUsize::new(0);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(options.concurrency)
        .build()?;
    let results: Vec<_> = pool.install(|| {
        tasks
            .par_iter()
            .map(|task| {
                let result = if cancel.is_cancelled() {
                    Err(anyhow::anyhow!("translation cancelled"))
                } else {
                    translate_task(task, options, client.as_ref(), &cache)
                };
                let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                progress(ProgressEvent::Translation {
                    completed: done,
                    total,
                });
                if done == 1 || done.is_multiple_of(10) || done == total {
                    tracing::info!("translated {done}/{total}");
                }
                result
            })
            .collect()
    });
    cancel.check()?;
    let mut updates = HashMap::new();
    for (task, result) in tasks.into_iter().zip(results) {
        match result {
            Ok((mut text, hit)) => {
                let paragraph = &mut document.pages[task.page].paragraphs[task.paragraph];
                if paragraph.role == Role::Heading {
                    // A leading bound variable (for example X) is not an
                    // appendix number. Inspect the same markup the model saw.
                    preserve_heading_number(&task.text, &mut text);
                }
                if cache.get(&task.key) != Some(&text) {
                    updates.insert(task.key, text.clone());
                }
                paragraph.translation = Some(text);
                stats.translated += 1;
                stats.cache_hits += usize::from(hit);
            }
            Err(error) => {
                stats.failed += 1;
                tracing::warn!(
                    "{}: translation failed: {error}",
                    document.pages[task.page].paragraphs[task.paragraph].id
                );
            }
        }
    }
    if let Err(error) = cache::merge(&path, updates) {
        tracing::warn!("cannot write translation cache: {error}");
    }
    Ok(stats)
}

fn collect_tasks(
    document: &LayoutDocument,
    options: &TranslationOptions,
) -> (Vec<Task>, TranslationStats) {
    let mut stats = TranslationStats::default();
    let mut tasks = Vec::new();
    let title = document
        .pages
        .first()
        .into_iter()
        .flat_map(|page| &page.paragraphs)
        .filter(|p| p.role == Role::Title)
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(400)
        .collect::<String>();
    let definitions: Vec<_> = document
        .pages
        .iter()
        .flat_map(|p| &p.paragraphs)
        .filter(|p| !p.protected)
        .flat_map(|p| abbreviation_definitions(&p.text))
        .collect();
    for (page_index, page) in document.pages.iter().enumerate() {
        for (index, paragraph) in page.paragraphs.iter().enumerate() {
            let source = paragraph
                .prompt_text
                .as_deref()
                .unwrap_or(&paragraph.text)
                .trim();
            if paragraph.protected {
                stats.skipped_protected += 1;
                continue;
            }
            if !has_translatable_letters(source) {
                stats.skipped_nontext += 1;
                continue;
            }
            if options.limit.is_some_and(|limit| tasks.len() >= limit) {
                continue;
            }
            let normalized = crate::text::expand_latin_ligatures(source);
            let source = normalized.as_str();
            let context = translation_context(&title, paragraph.role, source, &definitions);
            let formulas: Vec<_> = paragraph.formulas.iter().map(|f| f.id.clone()).collect();
            let unbound_bullet = crate::text::bullet_marker(&paragraph.text)
                .filter(|marker| {
                    paragraph
                        .formulas
                        .first()
                        .is_some_and(|f| f.text == marker.to_string())
                })
                .and_then(|marker| unbound_bullet_source(source, &formulas, marker))
                .map(|text| BulletCacheSource {
                    context: translation_context(&title, paragraph.role, &text, &definitions),
                    text,
                });
            tasks.push(Task {
                page: page_index,
                paragraph: index,
                text: source.to_owned(),
                key: translation_key(options, source, &context),
                formulas,
                context,
                role: paragraph.role,
                unbound_bullet,
            });
        }
    }
    stats.candidates = tasks.len();
    (tasks, stats)
}

fn translate_task(
    task: &Task,
    options: &TranslationOptions,
    client: Option<&Client>,
    cache: &HashMap<String, String>,
) -> Result<(String, bool)> {
    // v34 binds list symbols and migrates literal-prefix replies without
    // retranslating the prose. Earlier replies still need all current checks.
    // v32 also uses a positive target-language instruction on the final retry.
    // v31 adds concise language retries and sentence retries for plain prose.
    // v30 distinguishes Chinese prose from other CJK scripts. All migrated
    // replies must pass the current language, markup and list-label checks.
    let compatible_keys = (!options.fake).then(|| {
        [b"v32\0", b"v31\0", b"v30\0", b"v29\0", b"v28\0"].map(|version| {
            translation_key_with_version(options, &task.text, &task.context, version)
        })
    });
    let previous_key = (!options.fake).then(|| {
        translation_key_with_version(
            options,
            &task.text,
            &task.context,
            previous_cache_version(&task.text),
        )
    });
    // The base prompt is unchanged from these versions. Migrate only replies
    // that pass the new validators, retaining already checked page layouts.
    let legacy_key = (!options.fake
        && !has_bound_markup(&task.text)
        && !has_small_prose_fragment(&task.text)
        && list_marker(&task.text).is_none())
    .then(|| {
        translation_key_with_version(
            options,
            &task.text,
            &task.context,
            legacy_cache_version(&task.text),
        )
    });
    if let Some(text) = cache
        .get(&task.key)
        .or_else(|| {
            compatible_keys
                .as_ref()
                .and_then(|keys| keys.iter().find_map(|key| cache.get(key)))
        })
        .or_else(|| previous_key.as_ref().and_then(|key| cache.get(key)))
        .or_else(|| legacy_key.as_ref().and_then(|key| cache.get(key)))
        && let Some(text) = validate_translation(&task.text, text, &task.formulas)
        && (options.fake
            || client::language_issue(&task.text, &text, &options.target_lang).is_none())
        && (options.fake
            || list_marker(&task.text).is_none_or(|(_, marker, _)| {
                list_marker(&text).is_some_and(|(_, output_marker, _)| output_marker == marker)
            }))
    {
        return Ok((text, true));
    }
    if !options.fake
        && let Some(text) = migrate_bullet_cache(task, options, cache)
    {
        return Ok((text, true));
    }
    let text = match client {
        Some(client) => translate_structured(
            &task.text,
            &task.formulas,
            &task.context,
            task.role,
            |text, formulas, context| {
                client::chat_completion(client, options, text, formulas, context)
            },
        )?,
        None => format!("[译] {}", task.text),
    };
    Ok((text, false))
}

fn unbound_bullet_source(text: &str, formulas: &[String], marker: char) -> Option<String> {
    let body = text.strip_prefix(&format!("<{}>", formulas.first()?))?;
    let mut source = format!("{marker}{body}");
    // IDs can skip literal <fN> text in the original document. Shift through
    // the actual ID sequence, not by subtracting one from the numeric suffix.
    for ids in formulas.windows(2) {
        source = source.replace(&format!("<{}>", ids[1]), &format!("<{}>", ids[0]));
    }
    Some(source)
}

fn migrate_bullet_cache(
    task: &Task,
    options: &TranslationOptions,
    cache: &HashMap<String, String>,
) -> Option<String> {
    let source = task.unbound_bullet.as_ref()?;
    let marker = crate::text::bullet_marker(&source.text)?;
    let previous = Task {
        page: task.page,
        paragraph: task.paragraph,
        text: source.text.clone(),
        key: translation_key(options, &source.text, &source.context),
        formulas: task.formulas[..task.formulas.len().checked_sub(1)?].to_vec(),
        context: source.context.clone(),
        role: task.role,
        unbound_bullet: None,
    };
    // This lookup cannot call a model or recurse into another bullet migration.
    let (text, hit) = translate_task(&previous, options, None, cache).ok()?;
    if !hit || crate::text::bullet_marker(&text) != Some(marker) {
        return None;
    }
    let mut body = text.strip_prefix(marker)?.trim_start().to_owned();
    for ids in task.formulas.windows(2).rev() {
        body = body.replace(&format!("<{}>", ids[0]), &format!("<{}>", ids[1]));
    }
    let output = format!("<{}> {body}", task.formulas.first()?);
    let output = validate_translation(&task.text, &output, &task.formulas)?;
    client::language_issue(&task.text, &output, &options.target_lang)
        .is_none()
        .then_some(output)
}

fn statement_label(text: &str) -> Option<(&str, &str, &str)> {
    let (label, body) = text.strip_prefix("<b>")?.split_once("</b>")?;
    let mut words = label.split_whitespace();
    let kind = words.next()?;
    let number = words.next()?;
    if words.next().is_some()
        || body.trim().is_empty()
        || !matches!(
            kind.to_ascii_lowercase().as_str(),
            "theorem"
                | "lemma"
                | "corollary"
                | "proposition"
                | "definition"
                | "remark"
                | "example"
                | "assumption"
                | "claim"
                | "conjecture"
                | "fact"
                | "observation"
        )
        || !number.chars().any(|ch| ch.is_ascii_digit())
        || !number
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '.')
    {
        return None;
    }
    Some((kind, number, body.trim_start()))
}

fn translate_structured(
    text: &str,
    formulas: &[String],
    context: &str,
    role: Role,
    mut translate: impl FnMut(&str, &[String], &str) -> Result<String>,
) -> Result<String> {
    if let Some((styles, marker, body)) = list_marker(text) {
        // Enumeration is layout metadata just like a section number. Bind it
        // locally so translating a statement cannot silently drop its item ID.
        let source = format!("{styles}{body}");
        let mut output = translate_with_statement_label(&source, formulas, context, translate)?;
        let offset = if output.starts_with(styles) {
            styles.len()
        } else {
            0
        };
        output.insert_str(offset, &format!("{marker} "));
        return validate_translation(text, &output, formulas)
            .context("numbered list item did not preserve markup");
    }
    let numbered = text
        .split_once(char::is_whitespace)
        .filter(|(number, body)| {
            is_section_number(number)
                && (role == Role::Heading || body.trim_start().starts_with("<b>"))
        });
    let Some((number, body)) = numbered else {
        return translate_with_statement_label(text, formulas, context, translate);
    };
    let body = body.trim_start();
    // A section number is layout metadata, not a quantity modifying the noun
    // that follows. Keep it out of the model input and restore it locally.
    let output = if role == Role::Heading {
        translate(body, formulas, context)?
    } else {
        // A numbered run-in heading may contain formulas between bold spans.
        // Translate it separately so a long following paragraph cannot swallow it.
        let mut bold = false;
        let mut offset = 0;
        for piece in crate::markup::pieces(body) {
            use crate::markup::Piece;
            match piece {
                Piece::Bold(on) => {
                    bold = on;
                    offset += if on { 3 } else { 4 };
                }
                Piece::Italic(on) => offset += if on { 3 } else { 4 },
                Piece::Formula(id) => offset += id.len() + 2,
                Piece::Text(part) if !bold && !part.trim().is_empty() => break,
                Piece::Text(part) => offset += part.len(),
            }
        }
        if offset == 0 || offset == body.len() {
            translate(body, formulas, context)?
        } else {
            let mut translated = Vec::new();
            for part in [&body[..offset], &body[offset..]] {
                let ids = crate::markup::pieces(part)
                    .filter_map(|piece| match piece {
                        crate::markup::Piece::Formula(id) => Some(id.to_owned()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                translated.push(translate(part.trim(), &ids, context)?);
            }
            translated.join(" ")
        }
    };
    validate_translation(text, &format!("{number} {output}"), formulas)
        .context("numbered heading did not preserve markup")
}

fn list_marker(text: &str) -> Option<(&str, &str, &str)> {
    let mut offset = 0;
    for piece in crate::markup::pieces(text) {
        match piece {
            crate::markup::Piece::Bold(true) | crate::markup::Piece::Italic(true) => offset += 3,
            _ => break,
        }
    }
    let (marker, body) = text[offset..].split_once(char::is_whitespace)?;
    let label = marker
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .or_else(|| {
            // Bare decimal labels are common in lists and section headings.
            // Exclude decimals (1.5), years without a dot, and prose initials.
            marker
                .strip_suffix(['.', ')'])
                .filter(|label| !label.is_empty() && label.chars().all(|ch| ch.is_ascii_digit()))
        })?;
    let valid = !label.is_empty()
        && label.len() <= 8
        && (label.chars().all(|ch| ch.is_ascii_digit())
            || (label.len() == 1 && label.chars().all(|ch| ch.is_ascii_alphabetic()))
            || label.chars().all(|ch| "ivxlcdmIVXLCDM".contains(ch)));
    (valid && !body.trim().is_empty()).then_some((&text[..offset], marker, body.trim_start()))
}

fn translate_with_statement_label(
    text: &str,
    formulas: &[String],
    context: &str,
    mut translate: impl FnMut(&str, &[String], &str) -> Result<String>,
) -> Result<String> {
    let Some((kind, number, body)) = statement_label(text) else {
        return translate(text, formulas, context);
    };
    // Models sometimes omit short theorem/lemma labels while faithfully
    // translating the statement. Translate the label as its own unit, keep
    // its original number, and retain the body's formula/style protocol.
    let label = strip_style_tags(&translate(kind, &[], "")?);
    let label = label.trim().trim_end_matches(['.', '。', ':', '：']);
    if !label.chars().any(char::is_alphabetic) {
        bail!("statement label translation is empty");
    }
    let body = translate(body, formulas, context)?;
    let output = format!("<b>{label} {number}</b> {body}");
    validate_translation(text, &output, formulas)
        .context("statement translation did not preserve formula or literal markers")
}

fn preserve_heading_number(source: &str, translation: &mut String) {
    let Some(prefix) = source.split_whitespace().next() else {
        return;
    };
    if !is_section_number(prefix) {
        return;
    }
    strip_duplicate_heading_number(prefix, translation);
    let visible = strip_style_tags(translation);
    if strip_heading_number(&visible, prefix).is_some() {
        return;
    }
    *translation = format!("{prefix} {translation}");
}

fn strip_heading_number<'a>(text: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    let text = text.trim_start();
    let end = text
        .find(|ch: char| !ch.is_ascii_alphanumeric() && ch != '.')
        .unwrap_or(text.len());
    let number = &text[..end];
    (number.trim_end_matches('.') == prefix.trim_end_matches('.')).then_some((number, &text[end..]))
}

fn strip_duplicate_heading_number(prefix: &str, translation: &mut String) {
    let visible = strip_style_tags(translation);
    let Some((number, rest)) = strip_heading_number(&visible, prefix) else {
        return;
    };
    if strip_heading_number(rest, prefix).is_none() {
        return;
    }
    // Remove only the first copy, and only when nothing but style tags or
    // whitespace precedes it. This also repairs stale cache entries written
    // before heading-number preservation was tag-aware.
    let Some(position) = translation.find(number) else {
        return;
    };
    let before = &translation[..position];
    if !strip_style_tags(before).trim().is_empty() {
        return;
    }
    let after = translation[position + number.len()..].trim_start();
    *translation = format!("{before}{after}");
}

fn has_translatable_letters(text: &str) -> bool {
    let letters: usize = crate::markup::pieces(text)
        .filter_map(|piece| match piece {
            crate::markup::Piece::Text(plain) => Some(plain),
            _ => None,
        })
        .map(|plain| {
            crate::markup::decode_literal(plain)
                .chars()
                .filter(|ch| ch.is_alphabetic())
                .count()
        })
        .sum();
    letters >= 2
}

fn abbreviation_definitions(text: &str) -> Vec<(String, String)> {
    let mut definitions = Vec::new();
    for (position, _) in text.match_indices('(') {
        let Some(end) = text[position + 1..].find(')') else {
            continue;
        };
        let abbreviation = text[position + 1..position + 1 + end].trim();
        if !(2..=10).contains(&abbreviation.len())
            || !abbreviation.chars().all(|ch| ch.is_ascii_alphabetic())
            || !abbreviation.chars().any(|ch| ch.is_ascii_uppercase())
        {
            continue;
        }
        let letters = abbreviation.trim_end_matches('s').to_ascii_lowercase();
        let before = text[..position].trim_end();
        let words: Vec<_> = before
            .split(|ch: char| !ch.is_ascii_alphabetic())
            .filter(|s| !s.is_empty())
            .rev()
            .take(letters.len())
            .collect();
        let initials: String = words
            .iter()
            .rev()
            .filter_map(|w| w.chars().next())
            .map(|ch| ch.to_ascii_lowercase())
            .collect();
        if initials == letters {
            let phrase = words.into_iter().rev().collect::<Vec<_>>().join(" ");
            definitions.push((abbreviation.to_owned(), phrase));
        }
    }
    definitions
}

fn relevant_context(source: &str, definitions: &[(String, String)]) -> String {
    let mut selected = std::collections::BTreeMap::new();
    for (abbreviation, phrase) in definitions {
        if source
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .any(|word| word == abbreviation)
        {
            selected.entry(abbreviation).or_insert(phrase);
        }
    }
    // Only send explicit definitions relevant to this paragraph. Long opening
    // passages can be mistaken for text to translate by small local models.
    selected
        .into_iter()
        .take(8)
        .map(|(short, long)| format!("{short} = {long}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn translation_context(
    title: &str,
    role: Role,
    source: &str,
    definitions: &[(String, String)],
) -> String {
    let mut context = relevant_context(source, definitions);
    if role == Role::Title {
        context = format!(
            "Text role: complete paper title. Preserve its scope and every prepositional phrase.\n{context}"
        );
    }
    // Short labels/connectors do not need the paper's title. A local model
    // can otherwise translate the instruction or title instead of "and".
    // Headings still receive definitions of their explicitly bound acronyms.
    if !title.is_empty() && role == Role::Paragraph && source.chars().count() >= 80 {
        context = format!("Paper title: {title}\n{context}");
    }
    context
}

fn translation_key(options: &TranslationOptions, text: &str, context: &str) -> String {
    translation_key_with_version(options, text, context, b"v34\0")
}

fn previous_cache_version(text: &str) -> &'static [u8] {
    if has_bound_markup(text) {
        b"v27\0"
    } else if has_small_prose_fragment(text) {
        b"v24\0"
    } else if list_marker(text).is_some() {
        b"v23\0"
    } else {
        b"v21\0"
    }
}

fn has_bound_markup(text: &str) -> bool {
    crate::markup::pieces(text).any(|piece| !matches!(piece, crate::markup::Piece::Text(_)))
}

fn has_small_prose_fragment(text: &str) -> bool {
    crate::markup::pieces(text).any(|piece| match piece {
        crate::markup::Piece::Text(part) => {
            crate::markup::is_article_fragment(part)
                || part.chars().filter(|ch| ch.is_alphabetic()).count() == 1
        }
        _ => false,
    })
}

fn legacy_cache_version(text: &str) -> &'static [u8] {
    if crate::markup::pieces(text).any(|piece| matches!(piece, crate::markup::Piece::Formula(_))) {
        b"v20\0"
    } else if text.contains(['<', '&']) {
        b"v19\0"
    } else {
        b"v17\0"
    }
}

fn translation_key_with_version(
    options: &TranslationOptions,
    text: &str,
    context: &str,
    version: &[u8],
) -> String {
    let mut hasher = Sha256::new();
    if options.fake {
        hasher.update(b"v4\0");
    } else {
        hasher.update(version);
        hasher.update(options.api_base.trim_end_matches('/').as_bytes());
        hasher.update([0]);
        hasher.update(context.as_bytes());
        hasher.update([0]);
    }
    hasher.update(options.model.as_bytes());
    hasher.update([0]);
    hasher.update(options.target_lang.as_bytes());
    hasher.update([0]);
    hasher.update(if options.fake { b"fake" } else { b"real" });
    hasher.update([0]);
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

fn is_local_endpoint(base: &str) -> bool {
    reqwest::Url::parse(base).ok().is_some_and(|url| {
        url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::preserve_heading_number;

    #[test]
    fn section_numbers_are_not_translated_as_quantities() {
        let mut seen = Vec::new();
        let out = super::translate_structured(
            "4 Researchers Should Evaluate Models",
            &[],
            "",
            crate::model::Role::Heading,
            |text, _, _| {
                seen.push(text.to_owned());
                Ok("研究人员应评估模型".into())
            },
        )
        .unwrap();
        assert_eq!(out, "4 研究人员应评估模型");
        assert_eq!(seen, ["Researchers Should Evaluate Models"]);
    }

    #[test]
    fn numbered_run_in_headings_keep_formulas_and_their_own_translation_unit() {
        let mut seen = Vec::new();
        let source = "5.2. <b>24 vectors in </b><f1><b>. </b>The construction uses <f2>.";
        let out = super::translate_structured(
            source,
            &["f1".into(), "f2".into()],
            "",
            crate::model::Role::Paragraph,
            |text, ids, _| {
                seen.push((text.to_owned(), ids.to_vec()));
                Ok(text.to_owned())
            },
        )
        .unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].1, ["f1"]);
        assert_eq!(seen[1].1, ["f2"]);
        assert!(out.starts_with("5.2. <b>24 vectors"));
    }

    #[test]
    fn placeholder_names_and_markup_do_not_count_as_translatable_prose() {
        for text in [
            "<f1> <f2> 0 <f34>",
            "<b><f1></b> &lt; &gt; &amp;",
            "<i>1</i>",
        ] {
            assert!(!super::has_translatable_letters(text), "{text}");
        }
        for text in [
            "<b>and</b> <f1>",
            "Text with <f1> and <f2>.",
            "&lt;literal&gt;",
        ] {
            assert!(super::has_translatable_letters(text), "{text}");
        }
    }

    #[test]
    fn numbered_statement_labels_are_translated_separately_from_the_body() {
        let source = "<b>Lemma A.2. </b><i>With the above notation, </i><f1>.";
        let formulas = vec!["f1".to_owned()];
        let mut requests = Vec::new();
        let output = super::translate_with_statement_label(
            source,
            &formulas,
            "Paper title: A study",
            |text, ids, context| {
                requests.push((text.to_owned(), ids.to_vec(), context.to_owned()));
                Ok(if text == "Lemma" {
                    "引理".to_owned()
                } else {
                    "<i>使用上述符号，</i><f1>。".to_owned()
                })
            },
        )
        .unwrap();
        assert_eq!(output, "<b>引理 A.2.</b> <i>使用上述符号，</i><f1>。");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], ("Lemma".into(), vec![], String::new()));
        assert_eq!(requests[1].1, formulas);
        assert_eq!(requests[1].2, "Paper title: A study");
        assert_eq!(requests[1].0, "<i>With the above notation, </i><f1>.");
    }

    #[test]
    fn statement_label_detection_does_not_split_bold_prose_or_titles() {
        for kind in [
            "Theorem",
            "Corollary",
            "Proposition",
            "Definition",
            "Example",
            "Claim",
        ] {
            let source = format!("<b>{kind} 2.1 </b>Body text.");
            assert_eq!(
                super::statement_label(&source),
                Some((kind, "2.1", "Body text."))
            );
        }
        for source in [
            "<b>Lemma 1.</b>",
            "<b>Lemma 1. Full statement.</b>",
            "<b>Section 3.</b> Text.",
            "<b>Theorem proofs</b> are important.",
            "<b>Example <f1></b> text.",
        ] {
            assert!(super::statement_label(source).is_none(), "{source}");
        }
    }

    fn cache_options() -> super::TranslationOptions {
        super::TranslationOptions {
            api_base: "http://localhost/v1".into(),
            api_key: None,
            model: "cache-test".into(),
            target_lang: "zh-CN".into(),
            fake: false,
            concurrency: 1,
            max_tokens: 100,
            temperature: 0.0,
            timeout_secs: 1,
            limit: None,
        }
    }

    #[test]
    fn real_translation_cache_is_scoped_to_document_context() {
        let mut options = cache_options();
        let key = |options: &super::TranslationOptions, context| {
            super::translation_key(options, "3.1 ABC", context)
        };
        assert_ne!(
            key(&options, "Paper about algebra"),
            key(&options, "Paper about biology")
        );
        assert_ne!(key(&options, "Paper about algebra"), key(&options, ""));
        options.fake = true;
        assert_eq!(
            key(&options, "Paper about algebra"),
            key(&options, "Paper about biology")
        );
    }

    #[test]
    fn legacy_cache_migration_revalidates_language_and_bound_markers() {
        let options = cache_options();
        let source = "Use this instead.";
        let legacy_key = super::translation_key_with_version(
            &options,
            source,
            "",
            super::legacy_cache_version(source),
        );
        let task = super::Task {
            page: 0,
            paragraph: 0,
            text: source.into(),
            key: super::translation_key(&options, source, ""),
            formulas: vec![],
            context: String::new(),
            role: super::Role::Paragraph,
            unbound_bullet: None,
        };
        assert_ne!(task.key, legacy_key);
        for (output, valid) in [
            ("改用此项。", true),
            ("使用此项 instead。", false),
            ("使用此项 หนึ่ง。", false),
            ("此项による。", false),
            ("改用<f2>。", false),
        ] {
            // No client keeps this cache-hit test entirely offline.
            let cache = [(legacy_key.clone(), output.into())].into_iter().collect();
            let (_, hit) = super::translate_task(&task, &options, None, &cache).unwrap();
            assert_eq!(hit, valid, "{output}");
        }
        let mut task = task;
        let source = "Use <f1> instead.";
        task.text = source.into();
        task.formulas = vec!["f1".into()];
        task.context = "Paper title: A different subject".into();
        task.key = super::translation_key(&options, source, &task.context);
        for version in [
            b"v20\0", b"v21\0", b"v22\0", b"v24\0", b"v25\0", b"v26\0", b"v27\0", b"v28\0",
            b"v29\0", b"v30\0", b"v31\0", b"v32\0", b"v34\0",
        ] {
            let key = super::translation_key_with_version(&options, source, &task.context, version);
            let cache = [(key, "改用<f1>。".into())].into_iter().collect();
            let (_, hit) = super::translate_task(&task, &options, None, &cache).unwrap();
            assert_eq!(
                hit,
                matches!(
                    version,
                    b"v27\0" | b"v28\0" | b"v29\0" | b"v30\0" | b"v31\0" | b"v32\0" | b"v34\0"
                ),
                "{version:?}"
            );
        }
        let key = super::translation_key_with_version(&options, source, &task.context, b"v27\0");
        let cache = [(key, "only使用<f1>。".into())].into_iter().collect();
        let (_, hit) = super::translate_task(&task, &options, None, &cache).unwrap();
        assert!(
            !hit,
            "v27 migration must reject newly detected untranslated words"
        );
        for version in [b"v28\0", b"v29\0", b"v30\0", b"v31\0", b"v32\0"] {
            let key = super::translation_key_with_version(&options, source, &task.context, version);
            let cache = [(key, "<f1>による。".into())].into_iter().collect();
            let (_, hit) = super::translate_task(&task, &options, None, &cache).unwrap();
            assert!(!hit, "{version:?} must reject introduced kana");
        }
        task.text = "1. Use <f1> instead.".into();
        task.key = super::translation_key(&options, &task.text, &task.context);
        for (output, valid) in [("改用<f1>。", false), ("1. 改用<f1>。", true)] {
            let key =
                super::translation_key_with_version(&options, &task.text, &task.context, b"v28\0");
            let cache = [(key, output.into())].into_iter().collect();
            let (_, hit) = super::translate_task(&task, &options, None, &cache).unwrap();
            assert_eq!(hit, valid, "{output}");
        }
    }

    #[test]
    fn bullet_cache_migration_changes_only_the_bound_object_ids() {
        let options = cache_options();
        let source = "<f1> <b>Read <f3>.</b> Use &lt;f2&gt; and <f4> instead.";
        let formulas = vec!["f1".into(), "f3".into(), "f4".into()];
        let previous = super::unbound_bullet_source(source, &formulas, '•').unwrap();
        assert_eq!(
            previous,
            "• <b>Read <f1>.</b> Use &lt;f2&gt; and <f3> instead."
        );
        let old_key = super::translation_key_with_version(&options, &previous, "", b"v32\0");
        let task = super::Task {
            page: 0,
            paragraph: 0,
            text: source.into(),
            key: super::translation_key(&options, source, ""),
            formulas,
            context: String::new(),
            role: super::Role::Paragraph,
            unbound_bullet: Some(super::BulletCacheSource {
                text: previous,
                context: String::new(),
            }),
        };
        for (reply, valid) in [
            ("• <b>读取<f1>。</b>改用&lt;f2&gt;和<f3>。", true),
            ("· <b>读取<f1>。</b>改用&lt;f2&gt;和<f3>。", false),
            ("<b>读取<f1>。</b>改用&lt;f2&gt;和<f3>。", false),
            ("• <b>读取<f1>。</b>改用&lt;f2&gt;和<f1>。", false),
            ("• <b>读取<f1>。</b>改用&lt;f2&gt;和<f3> instead。", false),
        ] {
            let cache = [(old_key.clone(), reply.into())].into_iter().collect();
            let (output, hit) = super::translate_task(&task, &options, None, &cache).unwrap();
            assert_eq!(hit, valid, "{reply}");
            if valid {
                assert_eq!(output, "<f1> <b>读取<f3>。</b>改用&lt;f2&gt;和<f4>。");
            }
        }
    }

    #[test]
    fn enumeration_markers_are_restored_locally_in_their_original_style() {
        for (source, expected) in [
            ("(1) <i>Using <f1>.</i>", "(1) <i>使用<f1>。</i>"),
            ("<i>(a) Using <f1>.</i>", "<i>(a) 使用<f1>。</i>"),
            ("(iv) <i>Using <f1>.</i>", "(iv) <i>使用<f1>。</i>"),
            ("1. <i>Using <f1>.</i>", "1. <i>使用<f1>。</i>"),
            ("2) <i>Using <f1>.</i>", "2) <i>使用<f1>。</i>"),
        ] {
            let output = super::translate_structured(
                source,
                &["f1".into()],
                "",
                super::Role::Paragraph,
                |text, formulas, _| {
                    assert_eq!(text, "<i>Using <f1>.</i>");
                    assert_eq!(formulas, ["f1"]);
                    Ok("<i>使用<f1>。</i>".into())
                },
            )
            .unwrap();
            assert_eq!(output, expected);
        }
        assert!(super::list_marker("(see above) The next statement.").is_none());
        assert!(super::list_marker("(<f1>) The next statement.").is_none());
        assert!(super::list_marker("1.5 The next value.").is_none());
        assert!(super::list_marker("A. Person wrote this.").is_none());
    }

    #[test]
    fn context_uses_only_explicit_relevant_abbreviation_definitions() {
        let definitions = super::abbreviation_definitions(
            "We study Alpha Beta Calculus (ABC) with Large Language Models (LLMs). \
             We use chain-of-thought (CoT), not a made up expansion (XYZ).",
        );
        assert_eq!(
            super::relevant_context("3.1 ABC results", &definitions),
            "ABC = Alpha Beta Calculus"
        );
        assert_eq!(
            super::relevant_context("CoT and LLMs", &definitions),
            "CoT = chain of thought\nLLMs = Large Language Models"
        );
        assert!(super::relevant_context("Abstract", &definitions).is_empty());
        assert!(super::relevant_context("XYZ", &definitions).is_empty());
    }

    #[test]
    fn short_labels_do_not_receive_the_paper_title_as_content() {
        let definitions = vec![("ABC".into(), "Alpha Beta Calculus".into())];
        for text in ["and", "Abstract", "Core contributors."] {
            assert!(
                super::translation_context("A title", super::Role::Paragraph, text, &definitions)
                    .is_empty()
            );
        }
        assert_eq!(
            super::translation_context("A title", super::Role::Heading, "3.1 ABC", &definitions),
            "ABC = Alpha Beta Calculus"
        );
        let prose = "A sufficiently long paragraph about the systems used in the paper should receive its domain context.";
        assert!(
            super::translation_context("A title", super::Role::Paragraph, prose, &definitions)
                .starts_with("Paper title: A title")
        );
    }

    #[test]
    fn only_loopback_hosts_bypass_the_proxy_and_key_requirement() {
        for base in [
            "http://127.0.0.1:8000/v1",
            "http://localhost/v1",
            "http://[::1]:8000/v1",
        ] {
            assert!(super::is_local_endpoint(base));
        }
        for base in [
            "https://localhost.example.com/v1",
            "https://example.com/127.0.0.1",
            "https://example.com/?host=localhost",
        ] {
            assert!(!super::is_local_endpoint(base));
        }
    }

    #[test]
    fn styled_heading_number_is_not_duplicated() {
        let mut translated = "<b>3. 深度残差学习</b>".to_string();
        preserve_heading_number("3. Deep Residual Learning", &mut translated);
        assert_eq!(translated, "<b>3. 深度残差学习</b>");
        let mut translated = "残差学习".to_string();
        preserve_heading_number("3.1. Residual Learning", &mut translated);
        assert_eq!(translated, "3.1. 残差学习");
    }

    #[test]
    fn heading_numbers_without_a_final_dot_are_preserved() {
        for (source, input, expected) in [
            ("A Ablations", "消融实验", "A 消融实验"),
            ("B Evaluation", "<b>B. 评估</b>", "<b>B. 评估</b>"),
            ("3.1 Results", "<b>结果</b>", "3.1 <b>结果</b>"),
            ("3 Results", "30次实验结果", "3 30次实验结果"),
            ("A Appendix", "Abstract results", "A Abstract results"),
            ("Abstract.", "摘要", "摘要"),
            ("A Appendix", "A <b>A. 附录</b>", "<b>A. 附录</b>"),
        ] {
            let mut translated = input.to_owned();
            preserve_heading_number(source, &mut translated);
            assert_eq!(translated, expected, "{source}");
        }
    }

    #[test]
    fn duplicated_cached_heading_number_is_removed() {
        let mut translated = "A. <b>A. 目标检测基线模型</b>".to_string();
        preserve_heading_number("A. Object Detection Baselines", &mut translated);
        assert_eq!(translated, "<b>A. 目标检测基线模型</b>");

        let mut translated = "B. B. 目标检测技术的改进".to_string();
        preserve_heading_number("B. Object Detection Improvements", &mut translated);
        assert_eq!(translated, "B. 目标检测技术的改进");
    }
}
