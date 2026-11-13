use std::path::Path;

use eframe::egui;

use crate::config::{Config, TranslationConfig, TypesetConfig};
use crate::pdf::writer::{ComposeMode, ComposeOptions};
use crate::translate::TranslationOptions;
use crate::typeset::TypesetOptions;
use crate::workflow::{self, TranslateRequest};

use super::settings::ApiSettings;
use super::widgets::{
    non_empty_path, non_empty_string, option_grid, path_text, pdf_dialog, section_label,
};

pub(super) enum InputChange {
    Selected,
    Edited,
}

pub(super) struct TranslationForm {
    files: DocumentFields,
    translation: TranslationFields,
    compose: ComposeFields,
    typeset: TypesetFields,
}

#[derive(Default)]
struct DocumentFields {
    input: String,
    output: String,
    output_custom: bool,
}

struct TranslationFields {
    pages: String,
    target_lang: String,
    concurrency: usize,
    bilingual: bool,
    translate_references: bool,
    fake: bool,
}

#[derive(Default)]
struct ComposeFields {
    overlay: bool,
    keep_annotations: bool,
    no_cover: bool,
    debug_boxes: bool,
}

struct TypesetFields {
    font: String,
    bold_font: String,
    italic_font: String,
    min_scale: f64,
    max_expand: f64,
    line_height: f64,
}

impl TranslationForm {
    pub(super) fn new(config: &Config) -> Self {
        Self {
            files: DocumentFields::default(),
            translation: TranslationFields::new(&config.translation),
            compose: ComposeFields::default(),
            typeset: TypesetFields::new(&config.typeset),
        }
    }

    pub(super) fn input(&self) -> &str {
        &self.files.input
    }

    pub(super) fn set_input(&mut self, path: &Path) {
        self.files.input = path.to_string_lossy().into_owned();
        self.files.update_default_output(self.translation.bilingual);
    }

    pub(super) fn request(&self, api: &ApiSettings) -> TranslateRequest {
        TranslateRequest {
            input: self.files.input.trim().into(),
            output: non_empty_path(&self.files.output),
            pages: self.translation.pages.clone(),
            translation: self.translation.options(api),
            typeset: self.typeset.options(),
            compose: self.compose.options(),
            font: non_empty_path(&self.typeset.font),
            bold_font: non_empty_path(&self.typeset.bold_font),
            italic_font: non_empty_path(&self.typeset.italic_font),
            translate_references: self.translation.translate_references,
            bilingual: self.translation.bilingual,
            dump_layout: None,
        }
    }

    pub(super) fn write_config(&self, config: &mut Config) {
        config.translation.target_lang = non_empty_string(&self.translation.target_lang);
        config.translation.concurrency = Some(self.translation.concurrency);
        self.typeset.write_config(&mut config.typeset);
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui) -> Option<InputChange> {
        section_label(ui, "文件");
        let input_change = self.files.show(ui, self.translation.bilingual);
        ui.add_space(12.0);
        section_label(ui, "翻译");
        if self.translation.show(ui) {
            self.files.update_default_output(self.translation.bilingual);
        }
        ui.checkbox(&mut self.translation.translate_references, "翻译参考文献");
        self.compose.show(ui);
        ui.add_space(12.0);
        egui::CollapsingHeader::new("高级选项").show(ui, |ui| {
            ui.checkbox(&mut self.translation.fake, "离线模拟翻译，不调用 API");
            self.compose.show_advanced(ui);
            self.typeset.show(ui);
        });
        input_change
    }
}

impl DocumentFields {
    fn update_default_output(&mut self, bilingual: bool) {
        if !self.output_custom && !self.input.trim().is_empty() {
            self.output = workflow::default_output(Path::new(self.input.trim()), bilingual)
                .to_string_lossy()
                .into_owned();
        }
    }

    fn show(&mut self, ui: &mut egui::Ui, bilingual: bool) -> Option<InputChange> {
        let mut change = None;
        let browse = ui
            .horizontal(|ui| {
                ui.label("输入 PDF");
                ui.button("浏览").clicked()
            })
            .inner;
        if browse && let Some(path) = pdf_dialog(&self.input).pick_file() {
            self.input = path.to_string_lossy().into_owned();
            change = Some(InputChange::Selected);
        }
        let response = ui.add(
            egui::TextEdit::singleline(&mut self.input)
                .desired_width(ui.available_width())
                .hint_text("PDF 文件路径"),
        );
        if response.lost_focus() && !self.input.trim().is_empty() && change.is_none() {
            change = Some(InputChange::Edited);
        }
        if change.is_some() {
            self.update_default_output(bilingual);
        }
        self.show_output(ui);
        change
    }

    fn show_output(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("输出 PDF");
            if ui.button("浏览").clicked() {
                let mut dialog = pdf_dialog(&self.output);
                if let Some(name) = Path::new(&self.output).file_name() {
                    dialog = dialog.set_file_name(name.to_string_lossy());
                }
                if let Some(path) = dialog.save_file() {
                    self.output = path.to_string_lossy().into_owned();
                    self.output_custom = true;
                }
            }
        });
        let response = egui::TextEdit::singleline(&mut self.output)
            .desired_width(ui.available_width())
            .hint_text("默认与输入文件同目录")
            .show(ui)
            .response;
        if response.changed() {
            self.output_custom = true;
        }
    }
}

impl TranslationFields {
    fn new(config: &TranslationConfig) -> Self {
        Self {
            pages: "1-".to_string(),
            target_lang: config
                .target_lang
                .clone()
                .unwrap_or_else(|| "zh-CN".to_string()),
            concurrency: config.concurrency.unwrap_or(6),
            bilingual: true,
            translate_references: false,
            fake: false,
        }
    }

    fn options(&self, api: &ApiSettings) -> TranslationOptions {
        TranslationOptions {
            api_base: api.base_url.trim().to_string(),
            api_key: non_empty_string(&api.key),
            model: api.model.trim().to_string(),
            target_lang: self.target_lang.trim().to_string(),
            fake: self.fake,
            concurrency: self.concurrency,
            max_tokens: api.max_tokens,
            temperature: api.temperature,
            timeout_secs: api.timeout_secs,
            limit: None,
        }
    }

    fn show(&mut self, ui: &mut egui::Ui) -> bool {
        option_grid(ui, |ui| {
            ui.label("页面范围");
            ui.text_edit_singleline(&mut self.pages);
            ui.end_row();
            ui.label("目标语言");
            ui.text_edit_singleline(&mut self.target_lang);
            ui.end_row();
            ui.label("并发数");
            ui.add(egui::DragValue::new(&mut self.concurrency).range(1..=64));
            ui.end_row();
        });
        ui.checkbox(&mut self.bilingual, "生成左右双语对照 PDF")
            .changed()
    }
}

impl ComposeFields {
    fn options(&self) -> ComposeOptions {
        ComposeOptions {
            mode: if self.overlay {
                ComposeMode::Overlay
            } else {
                ComposeMode::Clean
            },
            cover_original: !self.no_cover,
            cover_rgb: [1.0, 1.0, 1.0],
            debug_boxes: self.debug_boxes,
            keep_annotations: self.keep_annotations,
        }
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(
            &mut self.keep_annotations,
            "链接注释保留原位（默认随译文移动）",
        );
        ui.checkbox(&mut self.overlay, "覆盖模式（默认删除并替换原文字形）");
    }

    fn show_advanced(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.no_cover, "覆盖模式下不绘制背景色块");
        ui.checkbox(&mut self.debug_boxes, "绘制段落调试框");
    }
}

impl TypesetFields {
    fn new(config: &TypesetConfig) -> Self {
        Self {
            font: path_text(config.font.as_deref()),
            bold_font: path_text(config.bold_font.as_deref()),
            italic_font: path_text(config.italic_font.as_deref()),
            min_scale: config.min_scale.unwrap_or(0.72),
            max_expand: config.max_expand.unwrap_or(1.45),
            line_height: config.line_height.unwrap_or(1.22),
        }
    }

    fn options(&self) -> TypesetOptions {
        TypesetOptions {
            min_scale: self.min_scale,
            max_expand: self.max_expand,
            line_height: self.line_height,
        }
    }

    fn write_config(&self, config: &mut TypesetConfig) {
        config.font = non_empty_path(&self.font);
        config.bold_font = non_empty_path(&self.bold_font);
        config.italic_font = non_empty_path(&self.italic_font);
        config.min_scale = Some(self.min_scale);
        config.max_expand = Some(self.max_expand);
        config.line_height = Some(self.line_height);
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        option_grid(ui, |ui| {
            ui.label("正文字体");
            ui.text_edit_singleline(&mut self.font);
            ui.end_row();
            ui.label("粗体字体");
            ui.text_edit_singleline(&mut self.bold_font);
            ui.end_row();
            ui.label("斜体字体");
            ui.text_edit_singleline(&mut self.italic_font);
            ui.end_row();
            ui.label("最小缩放");
            ui.add(egui::DragValue::new(&mut self.min_scale).speed(0.01));
            ui.end_row();
            ui.label("最大扩展");
            ui.add(egui::DragValue::new(&mut self.max_expand).speed(0.01));
            ui.end_row();
            ui.label("行高倍数");
            ui.add(egui::DragValue::new(&mut self.line_height).speed(0.01));
            ui.end_row();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::settings::Settings;
    use super::*;

    #[test]
    fn default_output_tracks_input_and_bilingual_until_customized() {
        let mut form = TranslationForm::new(&Config::default());
        form.set_input(Path::new("paper.pdf"));
        assert_eq!(form.files.output, "paper.bilingual.pdf");

        form.translation.bilingual = false;
        form.files.update_default_output(form.translation.bilingual);
        assert_eq!(form.files.output, "paper.zh.pdf");

        form.files.output = "custom.pdf".to_string();
        form.files.output_custom = true;
        form.set_input(Path::new("another.pdf"));
        form.translation.bilingual = true;
        form.files.update_default_output(form.translation.bilingual);
        assert_eq!(form.files.output, "custom.pdf");
    }

    #[test]
    fn request_preserves_edited_options_and_normalizes_optional_values() {
        let config: Config = toml::from_str(
            r#"
            [api]
            base_url = " http://127.0.0.1:8000/v1 "
            api_key = " test-key "
            model = " local-model "
            [typeset]
            font = "regular.otf"
            bold_font = "bold.otf"
            italic_font = "italic.otf"
            min_scale = 0.6
            max_expand = 1.3
            line_height = 1.1
        "#,
        )
        .unwrap();
        let mut form = TranslationForm::new(&config);
        let settings = Settings::new(config, None);
        form.files.input = " paper.pdf ".to_string();
        form.files.output = "  ".to_string();
        form.translation.pages = "2-5".to_string();
        form.translation.target_lang = " ja ".to_string();
        form.translation.concurrency = 3;
        form.translation.bilingual = false;
        form.translation.translate_references = true;
        form.translation.fake = true;
        form.compose.overlay = true;
        form.compose.keep_annotations = true;
        form.compose.no_cover = true;
        form.compose.debug_boxes = true;

        let request = form.request(&settings.api);
        assert_eq!(request.input, Path::new("paper.pdf"));
        assert!(request.output.is_none());
        assert_eq!(request.pages, "2-5");
        assert_eq!(request.translation.api_base, "http://127.0.0.1:8000/v1");
        assert_eq!(request.translation.api_key.as_deref(), Some("test-key"));
        assert_eq!(request.translation.model, "local-model");
        assert_eq!(request.translation.target_lang, "ja");
        assert_eq!(request.translation.concurrency, 3);
        assert!(request.translation.fake);
        assert!(!request.bilingual);
        assert!(request.translate_references);
        assert!(matches!(request.compose.mode, ComposeMode::Overlay));
        assert!(!request.compose.cover_original);
        assert!(request.compose.keep_annotations && request.compose.debug_boxes);
        assert_eq!(request.font.as_deref(), Some(Path::new("regular.otf")));
        assert_eq!(request.bold_font.as_deref(), Some(Path::new("bold.otf")));
        assert_eq!(
            request.italic_font.as_deref(),
            Some(Path::new("italic.otf"))
        );
        assert_eq!(request.typeset.min_scale, 0.6);
        assert_eq!(request.typeset.max_expand, 1.3);
        assert_eq!(request.typeset.line_height, 1.1);
    }
}
