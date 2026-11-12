use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "pdf-translate",
    version,
    about = "Layout-preserving PDF paper translator"
)]
pub struct Cli {
    /// Configuration file. Defaults to the platform config directory.
    #[arg(short, long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// Extract text and layout information and print it as JSON.
    Inspect(InspectArgs),
    /// Translate an academic PDF and write a new PDF.
    Translate(TranslateArgs),
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    /// Input PDF.
    pub input: PathBuf,

    /// Write JSON here instead of stdout.
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Pages to inspect, e.g. 1-3,5.
    #[arg(long, default_value = "1-")]
    pub pages: String,

    /// Emit compact JSON.
    #[arg(long)]
    pub compact: bool,
}

#[derive(Debug, Args)]
pub struct TranslateArgs {
    /// Input PDF.
    pub input: PathBuf,

    /// Output PDF. Defaults to <input stem>.zh.pdf, or <input stem>.bilingual.pdf
    /// with --bilingual.
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Pages to translate, e.g. 1-3,5.
    #[arg(long, default_value = "1-")]
    pub pages: String,

    /// OpenAI-compatible base URL (without /chat/completions).
    /// Overrides [api].base_url.
    #[arg(long)]
    pub api_base: Option<String>,

    /// API key. Overrides [api].api_key, then falls back to OPENAI_API_KEY.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Model name. Overrides [api].model, then falls back to OPENAI_MODEL.
    #[arg(long)]
    pub model: Option<String>,

    /// Target language code. Overrides [translation].target_lang (default: zh-CN).
    #[arg(long)]
    pub target_lang: Option<String>,

    /// Do not call the API; prefix every paragraph with [译].
    #[arg(long)]
    pub fake: bool,

    /// Maximum concurrent translation requests.
    /// Overrides [translation].concurrency (default: 6).
    #[arg(long)]
    pub concurrency: Option<usize>,

    /// Optional CJK font file (.ttf/.otf/.ttc). Overrides [typeset].font.
    #[arg(long)]
    pub font: Option<PathBuf>,

    /// Optional bold CJK font. Overrides [typeset].bold_font.
    #[arg(long)]
    pub bold_font: Option<PathBuf>,

    /// Smallest font scale allowed when fitting translations.
    /// Overrides [typeset].min_scale (default: 0.72).
    #[arg(long)]
    pub min_scale: Option<f64>,

    /// How much vertical space a paragraph may grow into.
    /// Overrides [typeset].max_expand (default: 1.45).
    #[arg(long)]
    pub max_expand: Option<f64>,

    /// Line height multiplier for translated text.
    /// Overrides [typeset].line_height (default: 1.22).
    #[arg(long)]
    pub line_height: Option<f64>,

    /// Keep the original content stream and paint background patches over it.
    /// Explicitly retains covered source text; clean mode never selects it automatically.
    #[arg(long)]
    pub overlay: bool,

    /// Write a side-by-side bilingual PDF: original page on the left,
    /// translated page on the right.
    #[arg(long)]
    pub bilingual: bool,

    /// Keep link/annotation rectangles on translated pages.
    #[arg(long)]
    pub keep_annotations: bool,

    /// Translate the reference list as well (skipped by default).
    #[arg(long)]
    pub translate_references: bool,

    /// Do not paint a background rectangle over the original text.
    #[arg(long)]
    pub no_cover: bool,

    /// Draw paragraph bounding boxes for debugging.
    #[arg(long)]
    pub debug_boxes: bool,

    /// Translate at most this many paragraphs (useful for testing).
    #[arg(long)]
    pub limit: Option<usize>,

    /// Write the extracted layout/translation state to this JSON file.
    #[arg(long)]
    pub dump_layout: Option<PathBuf>,
}
