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
}

pub fn translate_document(
    document: &mut LayoutDocument,
    options: &TranslationOptions,
) -> Result<TranslationStats> {
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
        Some(
            Client::builder()
                .timeout(Duration::from_secs(options.timeout_secs))
                .build()
                .context("failed to build HTTP client")?,
        )
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
                let result = translate_task(task, options, client.as_ref(), &cache);
                let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
                if done == 1 || done.is_multiple_of(10) || done == total {
                    tracing::info!("translated {done}/{total}");
                }
                result
            })
            .collect()
    });
    let mut updates = HashMap::new();
    for (task, result) in tasks.into_iter().zip(results) {
        match result {
            Ok((mut text, hit)) => {
                let paragraph = &mut document.pages[task.page].paragraphs[task.paragraph];
                if paragraph.role == Role::Heading {
                    preserve_heading_number(&paragraph.text, &mut text);
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
                tracing::warn!("translation failed: {error}");
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
            tasks.push(Task {
                page: page_index,
                paragraph: index,
                text: source.to_owned(),
                key: translation_key(options, source),
                formulas: paragraph.formulas.iter().map(|f| f.id.clone()).collect(),
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
    if let Some(text) = cache.get(&task.key)
        && let Some(text) = validate_translation(&task.text, text, &task.formulas)
    {
        return Ok((text, true));
    }
    let text = match client {
        Some(client) => client::chat_completion(client, options, &task.text, &task.formulas)?,
        None => format!("[译] {}", task.text),
    };
    Ok((text, false))
}

fn preserve_heading_number(source: &str, translation: &mut String) {
    let Some(prefix) = source.split_whitespace().next() else {
        return;
    };
    let numbered = prefix
        .trim_end_matches('.')
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '.')
        && prefix.ends_with('.');
    if !numbered {
        return;
    }
    strip_duplicate_heading_number(prefix, translation);
    let visible = strip_style_tags(translation);
    if visible.trim_start().starts_with(prefix) {
        return;
    }
    *translation = format!("{prefix} {translation}");
}

fn strip_duplicate_heading_number(prefix: &str, translation: &mut String) {
    let visible = strip_style_tags(translation);
    let Some(rest) = visible.trim_start().strip_prefix(prefix) else {
        return;
    };
    if !rest.trim_start().starts_with(prefix) {
        return;
    }
    // Remove only the first copy, and only when nothing but style tags or
    // whitespace precedes it. This also repairs stale cache entries written
    // before heading-number preservation was tag-aware.
    let Some(position) = translation.find(prefix) else {
        return;
    };
    let before = &translation[..position];
    if !strip_style_tags(before).trim().is_empty() {
        return;
    }
    let after = translation[position + prefix.len()..].trim_start();
    *translation = format!("{before}{after}");
}

fn has_translatable_letters(text: &str) -> bool {
    let letters = text.chars().filter(|ch| ch.is_alphabetic()).count();
    letters >= 2
}

fn translation_key(options: &TranslationOptions, text: &str) -> String {
    let mut hasher = Sha256::new();
    if options.fake {
        hasher.update(b"v4\0");
    } else {
        // Older live entries may contain a response truncated by the model.
        hasher.update(b"v5\0");
        hasher.update(options.api_base.trim_end_matches('/').as_bytes());
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
    let lower = base.to_ascii_lowercase();
    lower.contains("127.0.0.1")
        || lower.contains("localhost")
        || lower.contains("[::1]")
        || lower.contains("0.0.0.0")
}

#[cfg(test)]
mod tests {
    use super::preserve_heading_number;

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
    fn duplicated_cached_heading_number_is_removed() {
        let mut translated = "A. <b>A. 目标检测基线模型</b>".to_string();
        preserve_heading_number("A. Object Detection Baselines", &mut translated);
        assert_eq!(translated, "<b>A. 目标检测基线模型</b>");

        let mut translated = "B. B. 目标检测技术的改进".to_string();
        preserve_heading_number("B. Object Detection Improvements", &mut translated);
        assert_eq!(translated, "B. 目标检测技术的改进");
    }
}
