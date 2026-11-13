use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cli::TranslateArgs;
use crate::config::Config;
use crate::layout::{self, LayoutOptions};
use crate::model::LayoutDocument;
use crate::output;
use crate::pages::PageSelection;
use crate::pdf;
use crate::pdf::writer::{ComposeMode, ComposeOptions};
use crate::progress::{CancellationToken, JobStage, ProgressEvent, ProgressSink};
use crate::translate::{self, TranslationOptions, TranslationStats};
use crate::typeset::{self, TypesetOptions, TypesetStats};

#[derive(Debug, Clone)]
pub struct TranslateRequest {
    pub input: PathBuf,
    pub output: Option<PathBuf>,
    pub pages: String,
    pub translation: TranslationOptions,
    pub typeset: TypesetOptions,
    pub compose: ComposeOptions,
    pub font: Option<PathBuf>,
    pub bold_font: Option<PathBuf>,
    pub italic_font: Option<PathBuf>,
    pub translate_references: bool,
    pub bilingual: bool,
    pub dump_layout: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct TranslateReport {
    pub output: PathBuf,
    pub translation: TranslationStats,
    pub typeset: TypesetStats,
}

impl TranslateRequest {
    pub fn from_args(args: TranslateArgs, config: &Config) -> Result<Self> {
        let target_lang = args
            .target_lang
            .or_else(|| config.translation.target_lang.clone())
            .unwrap_or_else(|| "zh-CN".to_string());
        let concurrency = args
            .concurrency
            .or(config.translation.concurrency)
            .unwrap_or(6);
        let typeset = TypesetOptions {
            min_scale: args.min_scale.or(config.typeset.min_scale).unwrap_or(0.72),
            max_expand: args
                .max_expand
                .or(config.typeset.max_expand)
                .unwrap_or(1.45),
            line_height: args
                .line_height
                .or(config.typeset.line_height)
                .unwrap_or(1.22),
        };
        validate_typeset(&typeset)?;

        let api_base = args
            .api_base
            .or_else(|| config.api.base_url.clone())
            .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let api_key = args
            .api_key
            .or_else(|| config.api.api_key.clone())
            .or_else(|| std::env::var("OPENAI_API_KEY").ok());
        let model = args
            .model
            .or_else(|| config.api.model.clone())
            .or_else(|| std::env::var("OPENAI_MODEL").ok())
            .unwrap_or_else(|| "gpt-4o-mini".to_string());
        let max_tokens = config.api.max_tokens.unwrap_or(4096);
        let temperature = config.api.temperature.unwrap_or(0.2);
        let timeout_secs = config.api.timeout_secs.unwrap_or(180);
        if max_tokens == 0 {
            bail!("[api].max_tokens must be at least 1");
        }
        if timeout_secs == 0 {
            bail!("[api].timeout_secs must be at least 1");
        }
        if !temperature.is_finite() || temperature < 0.0 {
            bail!("[api].temperature must be a non-negative number");
        }

        Ok(Self {
            input: args.input,
            output: args.output,
            pages: args.pages,
            translation: TranslationOptions {
                api_base,
                api_key,
                model,
                target_lang,
                fake: args.fake,
                concurrency,
                max_tokens,
                temperature,
                timeout_secs,
                limit: args.limit,
            },
            typeset,
            compose: ComposeOptions {
                mode: if args.overlay {
                    ComposeMode::Overlay
                } else {
                    ComposeMode::Clean
                },
                cover_original: !args.no_cover,
                cover_rgb: [1.0, 1.0, 1.0],
                debug_boxes: args.debug_boxes,
                keep_annotations: args.keep_annotations,
            },
            font: args.font.or(config.typeset.font.clone()),
            bold_font: args.bold_font.or(config.typeset.bold_font.clone()),
            italic_font: args.italic_font.or(config.typeset.italic_font.clone()),
            translate_references: args.translate_references,
            bilingual: args.bilingual,
            dump_layout: args.dump_layout,
        })
    }
}

pub fn validate_typeset(options: &TypesetOptions) -> Result<()> {
    if !options.min_scale.is_finite() || options.min_scale <= 0.2 || options.min_scale > 1.0 {
        bail!("min scale must be in (0.2, 1.0] (--min-scale or [typeset].min_scale)");
    }
    if !options.max_expand.is_finite() || options.max_expand < 1.0 {
        bail!("max expand must be >= 1.0 (--max-expand or [typeset].max_expand)");
    }
    if !options.line_height.is_finite() || options.line_height < 1.0 {
        bail!("line height must be >= 1.0 (--line-height or [typeset].line_height)");
    }
    Ok(())
}

pub fn default_output(input: &Path, bilingual: bool) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".to_string());
    let suffix = if bilingual { "bilingual" } else { "zh" };
    input.with_file_name(format!("{stem}.{suffix}.pdf"))
}

pub fn translate(args: TranslateArgs, config: &Config) -> Result<TranslateReport> {
    let request = TranslateRequest::from_args(args, config)?;
    translate_with_progress(request, &cli_progress, &CancellationToken::default())
}

fn cli_progress(event: ProgressEvent) {
    match event {
        ProgressEvent::Stage(stage) => tracing::debug!("stage: {stage:?}"),
        ProgressEvent::Layout {
            pages,
            lines,
            paragraphs,
            protected,
        } => tracing::debug!(
            pages,
            lines,
            paragraphs,
            protected,
            "layout analysis completed"
        ),
        ProgressEvent::Translation { completed, total } => {
            tracing::debug!(completed, total, "translation progress")
        }
    }
}

pub fn translate_with_progress(
    request: TranslateRequest,
    progress: &ProgressSink,
    cancel: &CancellationToken,
) -> Result<TranslateReport> {
    validate_typeset(&request.typeset)?;
    cancel.check()?;
    let output = request
        .output
        .clone()
        .unwrap_or_else(|| default_output(&request.input, request.bilingual));
    let (output, dump) =
        output::destinations(&request.input, &output, request.dump_layout.as_deref())?;

    progress(ProgressEvent::Stage(JobStage::Extract));
    let (input, mut document) = load_layout(
        &request.input,
        &request.pages,
        LayoutOptions {
            translate_references: request.translate_references,
        },
    )?;
    cancel.check()?;
    progress(ProgressEvent::Stage(JobStage::Layout));
    eprintln!(
        "layout: {} pages, {} lines, {} paragraphs, {} protected",
        document.stats.pages,
        document.stats.lines,
        document.stats.paragraphs,
        document.stats.protected
    );
    progress(ProgressEvent::Layout {
        pages: document.stats.pages,
        lines: document.stats.lines,
        paragraphs: document.stats.paragraphs,
        protected: document.stats.protected,
    });
    if document.stats.translatable == 0 {
        bail!("no translatable paragraphs found; run `inspect` to see the layout");
    }

    progress(ProgressEvent::Stage(JobStage::Translate));
    let stats = translate::translate_document_observed(
        &mut document,
        &request.translation,
        progress,
        cancel,
    )?;
    eprintln!(
        "translation: {} candidates, {} translated ({} cached), {} failed, {} protected, {} without translatable text",
        stats.candidates,
        stats.translated,
        stats.cache_hits,
        stats.failed,
        stats.skipped_protected,
        stats.skipped_nontext
    );

    if let Some(path) = dump {
        output::write_bytes(&path, &serde_json::to_vec_pretty(&document)?)?;
        eprintln!("layout written to {}", path.display());
    }

    if document
        .pages
        .iter()
        .flat_map(|page| &page.paragraphs)
        .all(|paragraph| paragraph.translation.is_none())
    {
        bail!("nothing was translated; not writing an output PDF");
    }
    cancel.check()?;

    progress(ProgressEvent::Stage(JobStage::Typeset));
    let font_source = pdf::find_default_font(request.font.as_deref())?;
    eprintln!(
        "font: {} (index {})",
        font_source.path.display(),
        font_source.index
    );
    let bold_font_source = pdf::find_default_bold_font(request.bold_font.as_deref(), &font_source);
    if let Some(bold) = &bold_font_source {
        eprintln!("bold font: {} (index {})", bold.path.display(), bold.index);
    }
    let italic_font_source =
        pdf::find_default_italic_font(request.italic_font.as_deref(), &font_source);
    if let Some(italic) = &italic_font_source {
        eprintln!(
            "italic font: {} (index {})",
            italic.path.display(),
            italic.index
        );
    }
    let symbol_font_source = pdf::find_default_symbol_font();

    let (typeset, typeset_stats) = typeset::typeset_document(
        &document,
        &font_source,
        bold_font_source.as_ref(),
        italic_font_source.as_ref(),
        symbol_font_source.as_ref(),
        &request.typeset,
    )?;
    eprintln!(
        "typeset: {} paragraphs, {} overflow, {} missing-font paragraphs, smallest scale {:.2}",
        typeset_stats.paragraphs,
        typeset_stats.overflow,
        typeset_stats.missing_glyphs,
        typeset_stats.smallest_scale
    );
    if typeset.paragraphs.is_empty() {
        bail!("no translations could be safely typeset; original PDF was preserved");
    }
    cancel.check()?;

    progress(ProgressEvent::Stage(JobStage::Compose));
    if request.bilingual {
        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let translated = tempfile::Builder::new()
            .prefix(".pdf-translate-")
            .suffix(".pdf")
            .tempfile_in(parent)?
            .into_temp_path();
        pdf::write_translated_pdf(
            input,
            translated.as_ref(),
            &document.pages,
            &typeset,
            &request.compose,
        )?;
        cancel.check()?;
        pdf::bilingual::write_bilingual_pdf(&request.input, translated.as_ref(), &output)?;
        eprintln!("wrote bilingual {}", output.display());
    } else {
        pdf::write_translated_pdf(input, &output, &document.pages, &typeset, &request.compose)?;
        eprintln!("wrote {}", output.display());
    }

    Ok(TranslateReport {
        output,
        translation: stats,
        typeset: typeset_stats,
    })
}

pub fn load_layout(
    path: &Path,
    pages: &str,
    options: LayoutOptions,
) -> Result<(lopdf::Document, LayoutDocument)> {
    let selection = parse_selection(pages)?;
    let input = pdf::preflight::load_pdf(path)?;
    let extracted = pdf::extract_pdf(input.data, &input.document, &selection)?;
    Ok((
        input.document,
        layout::analyze_document(path.display().to_string(), extracted, options),
    ))
}

fn parse_selection(raw: &str) -> Result<PageSelection> {
    raw.parse()
        .with_context(|| format!("invalid --pages value: {raw}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_name_preserves_cli_defaults() {
        assert_eq!(
            default_output(Path::new("paper.pdf"), false),
            PathBuf::from("paper.zh.pdf")
        );
        assert_eq!(
            default_output(Path::new("paper.pdf"), true),
            PathBuf::from("paper.bilingual.pdf")
        );
    }

    #[test]
    fn cancellation_is_observed_before_processing() {
        let cancel = CancellationToken::default();
        cancel.cancel();
        let request = TranslateRequest {
            input: PathBuf::from("missing.pdf"),
            output: None,
            pages: "1-".to_string(),
            translation: TranslationOptions {
                api_base: String::new(),
                api_key: None,
                model: String::new(),
                target_lang: "zh-CN".to_string(),
                fake: true,
                concurrency: 1,
                max_tokens: 4096,
                temperature: 0.2,
                timeout_secs: 180,
                limit: None,
            },
            typeset: TypesetOptions::default(),
            compose: ComposeOptions {
                mode: ComposeMode::Clean,
                cover_original: true,
                cover_rgb: [1.0, 1.0, 1.0],
                debug_boxes: false,
                keep_annotations: false,
            },
            font: None,
            bold_font: None,
            italic_font: None,
            translate_references: false,
            bilingual: false,
            dump_layout: None,
        };
        let error = translate_with_progress(request, &|_| {}, &cancel).unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    }
}
