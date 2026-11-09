mod cli;
mod config;
mod layout;
mod markup;
mod model;
mod output;
mod pages;
mod pdf;
mod text;
mod translate;
mod typeset;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command, InspectArgs, TranslateArgs};
use crate::layout::LayoutOptions;
use crate::model::LayoutDocument;
use crate::pages::PageSelection;
use crate::pdf::writer::{ComposeMode, ComposeOptions};
use crate::translate::TranslationOptions;
use crate::typeset::TypesetOptions;

fn main() -> ExitCode {
    // Run on a worker with a larger stack. PDF parsers and recursive
    // Form XObject graphs are a common source of deep call stacks; this makes
    // malformed input fail more gracefully instead of hitting the main-thread
    // stack limit. `join` also turns worker panics into a normal error exit.
    let worker = std::thread::Builder::new()
        .name("pdf-translate".to_string())
        .stack_size(64 * 1024 * 1024)
        .spawn(run);

    match worker {
        Ok(handle) => match handle.join() {
            Ok(Ok(())) => ExitCode::SUCCESS,
            Ok(Err(err)) => {
                eprintln!("error: {err:#}");
                ExitCode::FAILURE
            }
            Err(_) => {
                eprintln!("error: internal panic; the input PDF may be malformed");
                ExitCode::FAILURE
            }
        },
        Err(err) => {
            eprintln!("error: failed to start worker thread: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    init_logging();
    let config = config::Config::load(cli.config.as_deref())?;

    match cli.command {
        Command::Inspect(args) => inspect(args),
        Command::Translate(args) => translate(args, &config),
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .try_init();
}

fn inspect(args: InspectArgs) -> Result<()> {
    let output = args
        .output
        .as_deref()
        .map(|path| output::destinations(&args.input, path, None))
        .transpose()?;
    let (_, document) = load_layout(&args.input, &args.pages, LayoutOptions::default())?;

    let json = if args.compact {
        serde_json::to_vec(&document)?
    } else {
        serde_json::to_vec_pretty(&document)?
    };

    if let Some((output, _)) = output {
        output::write_bytes(&output, &json)?;
        eprintln!(
            "wrote {} ({} pages, {} paragraphs) to {}",
            args.input.display(),
            document.stats.pages,
            document.stats.paragraphs,
            output.display()
        );
    } else {
        println!("{}", String::from_utf8_lossy(&json));
        eprintln!(
            "{}: {} pages, {} lines, {} paragraphs ({} protected)",
            args.input.display(),
            document.stats.pages,
            document.stats.lines,
            document.stats.paragraphs,
            document.stats.protected
        );
    }

    Ok(())
}

fn translate(args: TranslateArgs, config: &config::Config) -> Result<()> {
    let target_lang = args
        .target_lang
        .or_else(|| config.translation.target_lang.clone())
        .unwrap_or_else(|| "zh-CN".to_string());
    let concurrency = args
        .concurrency
        .or(config.translation.concurrency)
        .unwrap_or(6);
    let min_scale = args.min_scale.or(config.typeset.min_scale).unwrap_or(0.72);
    let max_expand = args
        .max_expand
        .or(config.typeset.max_expand)
        .unwrap_or(1.45);
    let line_height = args
        .line_height
        .or(config.typeset.line_height)
        .unwrap_or(1.22);

    if !min_scale.is_finite() || min_scale <= 0.2 || min_scale > 1.0 {
        bail!("min scale must be in (0.2, 1.0] (--min-scale or [typeset].min_scale)");
    }
    if !max_expand.is_finite() || max_expand < 1.0 {
        bail!("max expand must be >= 1.0 (--max-expand or [typeset].max_expand)");
    }
    if !line_height.is_finite() || line_height < 1.0 {
        bail!("line height must be >= 1.0 (--line-height or [typeset].line_height)");
    }

    let output = args
        .output
        .clone()
        .unwrap_or_else(|| default_output(&args.input));
    let (output, dump) = output::destinations(&args.input, &output, args.dump_layout.as_deref())?;
    let (input, mut document) = load_layout(
        &args.input,
        &args.pages,
        LayoutOptions {
            translate_references: args.translate_references,
        },
    )?;

    eprintln!(
        "layout: {} pages, {} lines, {} paragraphs, {} protected",
        document.stats.pages,
        document.stats.lines,
        document.stats.paragraphs,
        document.stats.protected
    );
    if document.stats.translatable == 0 {
        bail!("no translatable paragraphs found; run `inspect` to see the layout");
    }

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

    let translation_options = TranslationOptions {
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
    };

    let stats = translate::translate_document(&mut document, &translation_options)?;
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

    let font_source = pdf::find_default_font(args.font.or(config.typeset.font.clone()).as_deref())?;
    eprintln!(
        "font: {} (index {})",
        font_source.path.display(),
        font_source.index
    );
    let bold_font_source = pdf::find_default_bold_font(
        args.bold_font
            .or(config.typeset.bold_font.clone())
            .as_deref(),
        &font_source,
    );
    if let Some(bold) = &bold_font_source {
        eprintln!("bold font: {} (index {})", bold.path.display(), bold.index);
    }

    let typeset_options = TypesetOptions {
        min_scale,
        line_height,
        max_expand,
    };
    let (typeset, typeset_stats) = typeset::typeset_document(
        &document,
        &font_source,
        bold_font_source.as_ref(),
        &typeset_options,
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

    let compose_options = ComposeOptions {
        mode: if args.overlay {
            ComposeMode::Overlay
        } else {
            ComposeMode::Clean
        },
        cover_original: !args.no_cover,
        cover_rgb: [1.0, 1.0, 1.0],
        debug_boxes: args.debug_boxes,
        keep_annotations: args.keep_annotations,
    };
    pdf::write_translated_pdf(input, &output, &document.pages, &typeset, &compose_options)?;
    eprintln!("wrote {}", output.display());

    Ok(())
}

fn parse_selection(raw: &str) -> Result<PageSelection> {
    let selection: PageSelection = raw
        .parse()
        .with_context(|| format!("invalid --pages value: {raw}"))?;
    Ok(selection)
}

fn default_output(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".to_string());
    input.with_file_name(format!("{stem}.zh.pdf"))
}

fn load_layout(
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
