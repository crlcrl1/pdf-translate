use std::path::PathBuf;

use anyhow::Result;
use eframe::egui;

use crate::config::{ApiConfig, Config, ThemeMode};

use super::form::TranslationForm;
use super::widgets::{non_empty_string, option_grid};

pub(super) struct Settings {
    pub(super) api: ApiSettings,
    theme: ThemeMode,
    open: bool,
    config: Config,
    path: Option<PathBuf>,
}

pub(super) struct ApiSettings {
    pub(super) base_url: String,
    pub(super) key: String,
    pub(super) model: String,
    pub(super) max_tokens: u32,
    pub(super) temperature: f32,
    pub(super) timeout_secs: u64,
    key_from_env: bool,
    key_dirty: bool,
}

impl Settings {
    pub(super) fn new(config: Config, path: Option<PathBuf>) -> Self {
        Self {
            api: ApiSettings::new(&config.api),
            theme: config.gui.theme,
            open: false,
            config,
            path,
        }
    }

    pub(super) fn apply_theme(&self, context: &egui::Context) {
        context.set_theme(match self.theme {
            ThemeMode::System => egui::ThemePreference::System,
            ThemeMode::Light => egui::ThemePreference::Light,
            ThemeMode::Dark => egui::ThemePreference::Dark,
        });
    }

    pub(super) fn top_bar(&mut self, ui: &mut egui::Ui) {
        let context = ui.ctx().clone();
        egui::Panel::top("top-bar").show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("PDF Translate");
                ui.separator();
                ui.label("保留版式的论文翻译");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("设置").clicked() {
                        self.open = true;
                    }
                    egui::ComboBox::from_id_salt("theme")
                        .selected_text(theme_label(self.theme))
                        .width(110.0)
                        .show_ui(ui, |ui| {
                            for (mode, label) in [
                                (ThemeMode::System, "跟随系统"),
                                (ThemeMode::Light, "浅色模式"),
                                (ThemeMode::Dark, "深色模式"),
                            ] {
                                if ui.selectable_value(&mut self.theme, mode, label).changed() {
                                    self.apply_theme(&context);
                                }
                            }
                        });
                    ui.label("外观");
                });
            });
            ui.add_space(8.0);
        });
    }

    pub(super) fn window(
        &mut self,
        context: &egui::Context,
        form: &TranslationForm,
    ) -> Option<Result<PathBuf>> {
        if !self.open {
            return None;
        }
        let mut save = false;
        egui::Window::new("API 与排版设置")
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .show(context, |ui| {
                self.api.show(ui);
                ui.add_space(8.0);
                ui.label("API Key 仅用于请求和本地配置，不会写入 GUI 日志。");
                if self.api.key_from_env {
                    ui.label("当前 API Key 来自环境变量；未修改时不会写入配置文件。");
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    save = ui.button("保存设置").clicked();
                    if ui.button("关闭").clicked() {
                        self.open = false;
                    }
                });
            });
        save.then(|| {
            let result = self.save(form);
            if result.is_ok() {
                self.open = false;
            }
            result
        })
    }

    fn save(&mut self, form: &TranslationForm) -> Result<PathBuf> {
        self.api.write_config(&mut self.config.api);
        form.write_config(&mut self.config);
        self.config.gui.theme = self.theme;
        let path = self.config.save(self.path.as_deref())?;
        self.api.key_dirty = false;
        self.api.key_from_env = key_from_env(&self.config.api);
        Ok(path)
    }
}

impl ApiSettings {
    fn new(config: &ApiConfig) -> Self {
        Self {
            base_url: config
                .base_url
                .clone()
                .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            key: config
                .api_key
                .clone()
                .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                .unwrap_or_default(),
            model: config
                .model
                .clone()
                .or_else(|| std::env::var("OPENAI_MODEL").ok())
                .unwrap_or_else(|| "gpt-4o-mini".to_string()),
            max_tokens: config.max_tokens.unwrap_or(4096),
            temperature: config.temperature.unwrap_or(0.2),
            timeout_secs: config.timeout_secs.unwrap_or(180),
            key_from_env: key_from_env(config),
            key_dirty: false,
        }
    }

    fn write_config(&self, config: &mut ApiConfig) {
        config.base_url = non_empty_string(&self.base_url);
        if !self.key_from_env || self.key_dirty {
            config.api_key = non_empty_string(&self.key);
        }
        config.model = non_empty_string(&self.model);
        config.max_tokens = Some(self.max_tokens);
        config.temperature = Some(self.temperature);
        config.timeout_secs = Some(self.timeout_secs);
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        option_grid(ui, |ui| {
            ui.label("API Base URL");
            ui.text_edit_singleline(&mut self.base_url);
            ui.end_row();
            ui.label("API Key");
            if ui
                .add(egui::TextEdit::singleline(&mut self.key).password(true))
                .changed()
            {
                self.key_dirty = true;
            }
            ui.end_row();
            ui.label("模型");
            ui.text_edit_singleline(&mut self.model);
            ui.end_row();
            ui.label("最大 Token");
            ui.add(egui::DragValue::new(&mut self.max_tokens).range(1..=1_000_000));
            ui.end_row();
            ui.label("Temperature");
            ui.add(egui::DragValue::new(&mut self.temperature).speed(0.05));
            ui.end_row();
            ui.label("超时（秒）");
            ui.add(egui::DragValue::new(&mut self.timeout_secs).range(1..=3600));
            ui.end_row();
        });
    }
}

fn key_from_env(config: &ApiConfig) -> bool {
    config.api_key.is_none()
        && std::env::var_os("OPENAI_API_KEY").is_some_and(|value| !value.is_empty())
}

fn theme_label(theme: ThemeMode) -> &'static str {
    match theme {
        ThemeMode::System => "跟随系统",
        ThemeMode::Light => "浅色",
        ThemeMode::Dark => "深色",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_keeps_inherited_key_private_until_edited() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::default();
        config.translation.target_lang = Some("ja".to_string());
        config.typeset.italic_font = Some(PathBuf::from("italic.otf"));
        let form = TranslationForm::new(&config);
        let mut settings = Settings::new(config, Some(path.clone()));
        // Model the inherited-key state without changing process-wide environment variables.
        settings.api.key = "inherited-test-key".to_string();
        settings.api.key_from_env = true;
        settings.theme = ThemeMode::Dark;

        assert_eq!(settings.save(&form).unwrap(), path);
        let saved = Config::load(Some(&path)).unwrap();
        assert!(saved.api.api_key.is_none());
        assert_eq!(saved.translation.target_lang.as_deref(), Some("ja"));
        assert_eq!(saved.typeset.italic_font, Some(PathBuf::from("italic.otf")));
        assert_eq!(saved.gui.theme, ThemeMode::Dark);

        settings.api.key = " edited-test-key ".to_string();
        settings.api.key_dirty = true;
        settings.save(&form).unwrap();
        let saved = Config::load(Some(&path)).unwrap();
        assert_eq!(saved.api.api_key.as_deref(), Some("edited-test-key"));
        assert!(!settings.api.key_dirty);
        assert!(!settings.api.key_from_env);
    }

    #[test]
    fn failed_save_keeps_key_edits_pending() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        let form = TranslationForm::new(&config);
        let mut settings = Settings::new(config, Some(dir.path().to_path_buf()));
        settings.api.key = "edited-test-key".to_string();
        settings.api.key_dirty = true;
        assert!(settings.save(&form).is_err());
        assert!(settings.api.key_dirty);
    }
}
