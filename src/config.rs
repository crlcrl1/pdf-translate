use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Environment variable that overrides the default configuration path.
pub const CONFIG_PATH_ENV: &str = "PDF_TRANSLATE_CONFIG";

/// Settings that are independent of the document being translated.
///
/// Document-specific choices (input/output paths, page ranges, overlay mode,
/// annotation handling, ...) are supplied per job by the CLI or GUI. Command
/// line flags also override the corresponding values from this file.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub api: ApiConfig,
    pub translation: TranslationConfig,
    pub typeset: TypesetConfig,
    pub gui: GuiConfig,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuiConfig {
    pub theme: ThemeMode,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    /// OpenAI-compatible base URL, without `/chat/completions`.
    pub base_url: Option<String>,
    /// API key. Falls back to `OPENAI_API_KEY` when unset.
    pub api_key: Option<String>,
    /// Model name. Falls back to `OPENAI_MODEL` when unset.
    pub model: Option<String>,
    /// Maximum number of tokens requested per translation.
    pub max_tokens: Option<u32>,
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// HTTP request timeout in seconds.
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TranslationConfig {
    /// Target language code.
    pub target_lang: Option<String>,
    /// Maximum number of concurrent translation requests.
    pub concurrency: Option<usize>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TypesetConfig {
    /// Optional CJK font file (.ttf/.otf/.ttc).
    pub font: Option<PathBuf>,
    /// Optional bold CJK font.
    pub bold_font: Option<PathBuf>,
    /// Optional italic CJK font (a Kaiti face works well for Chinese).
    pub italic_font: Option<PathBuf>,
    /// Smallest font scale allowed when fitting translations.
    pub min_scale: Option<f64>,
    /// How much vertical space a paragraph may grow into.
    pub max_expand: Option<f64>,
    /// Line height multiplier for translated text.
    pub line_height: Option<f64>,
}

impl Config {
    /// Load the configuration file.
    ///
    /// `explicit` (from `--config`) wins over `PDF_TRANSLATE_CONFIG`, which
    /// wins over the platform configuration directory. An explicitly selected
    /// file must exist; a missing default file is treated as an empty config.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let explicit_path = explicit
            .map(Path::to_path_buf)
            .or_else(|| env::var_os(CONFIG_PATH_ENV).map(PathBuf::from));
        let required = explicit_path.is_some();
        let Some(path) = explicit_path.or_else(default_path) else {
            return Ok(Self::default());
        };

        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound && !required => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot read configuration file {}", path.display()));
            }
        };

        let config = toml::from_str(&text)
            .with_context(|| format!("invalid configuration file {}", path.display()))?;
        tracing::debug!("loaded configuration from {}", path.display());
        Ok(config)
    }

    #[cfg(feature = "gui")]
    pub fn save(&self, explicit: Option<&Path>) -> Result<PathBuf> {
        let path = explicit
            .map(Path::to_path_buf)
            .or_else(|| env::var_os(CONFIG_PATH_ENV).map(PathBuf::from))
            .or_else(default_path)
            .context("cannot determine the configuration directory")?;
        let text = toml::to_string_pretty(self).context("cannot serialize configuration")?;
        crate::output::write_private_bytes(&path, text.as_bytes())?;
        Ok(path)
    }
}

/// Default configuration path: `$XDG_CONFIG_HOME/pdf-translate/config.toml`
/// (or `~/.config/pdf-translate/config.toml`); on Windows,
/// `%APPDATA%\pdf-translate\config.toml`.
pub fn default_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        env::var_os("APPDATA").map(|base| {
            PathBuf::from(base)
                .join("pdf-translate")
                .join("config.toml")
        })
    }

    #[cfg(not(windows))]
    {
        let base = env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("pdf-translate").join("config.toml"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_document_independent_settings() {
        let config: Config = toml::from_str(
            r#"
            [api]
            base_url = "http://127.0.0.1:8000/v1"
            api_key = "secret"
            model = "hy-mt2"
            max_tokens = 8192
            temperature = 0.1
            timeout_secs = 300

            [translation]
            target_lang = "ja"
            concurrency = 3

            [typeset]
            font = "/tmp/font.ttf"
            bold_font = "/tmp/bold.ttf"
            italic_font = "/tmp/italic.ttf"
            min_scale = 0.6
            max_expand = 1.3
            line_height = 1.1
            "#,
        )
        .unwrap();

        assert_eq!(
            config.api.base_url.as_deref(),
            Some("http://127.0.0.1:8000/v1")
        );
        assert_eq!(config.api.api_key.as_deref(), Some("secret"));
        assert_eq!(config.api.model.as_deref(), Some("hy-mt2"));
        assert_eq!(config.api.max_tokens, Some(8192));
        assert_eq!(config.api.temperature, Some(0.1));
        assert_eq!(config.api.timeout_secs, Some(300));
        assert_eq!(config.translation.target_lang.as_deref(), Some("ja"));
        assert_eq!(config.translation.concurrency, Some(3));
        assert_eq!(
            config.typeset.font.as_deref(),
            Some(Path::new("/tmp/font.ttf"))
        );
        assert_eq!(
            config.typeset.bold_font.as_deref(),
            Some(Path::new("/tmp/bold.ttf"))
        );
        assert_eq!(
            config.typeset.italic_font.as_deref(),
            Some(Path::new("/tmp/italic.ttf"))
        );
        assert_eq!(config.typeset.min_scale, Some(0.6));
        assert_eq!(config.typeset.max_expand, Some(1.3));
        assert_eq!(config.typeset.line_height, Some(1.1));
    }

    #[test]
    fn empty_config_sets_no_overrides() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.api.base_url.is_none());
        assert!(config.translation.concurrency.is_none());
        assert!(config.typeset.font.is_none());
    }

    #[test]
    fn gui_theme_round_trips_through_toml() {
        let config = Config {
            gui: GuiConfig {
                theme: ThemeMode::Dark,
            },
            ..Config::default()
        };
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("theme = \"dark\""));
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.gui.theme, ThemeMode::Dark);
    }

    #[cfg(all(unix, feature = "gui"))]
    #[test]
    fn saved_config_is_private_even_when_replacing_a_readable_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = Config {
            api: ApiConfig {
                api_key: Some("secret".to_string()),
                ..ApiConfig::default()
            },
            ..Config::default()
        };
        config.save(Some(&path)).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        config.save(Some(&path)).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let error = toml::from_str::<Config>("[api]\nunknown = 1\n").unwrap_err();
        assert!(
            error.to_string().contains("unknown field"),
            "unexpected error: {error}"
        );
    }
}
