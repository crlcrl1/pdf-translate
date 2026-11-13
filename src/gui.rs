mod preview;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::{Result, anyhow};
use eframe::egui;

use crate::config::{Config, ThemeMode};
use crate::pdf::writer::{ComposeMode, ComposeOptions};
use crate::progress::{CancellationToken, JobStage, ProgressEvent};
use crate::translate::TranslationOptions;
use crate::typeset::TypesetOptions;
use crate::workflow::{self, TranslateReport, TranslateRequest};

pub fn run(config: Config, input: Option<PathBuf>, config_path: Option<PathBuf>) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("PDF Translate")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([960.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "PDF Translate",
        options,
        Box::new(move |context| {
            let mut app = GuiApp::new(context, config, config_path);
            if let Some(input) = input {
                app.set_input(input);
            }
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| anyhow!(error.to_string()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PreviewSource {
    Original,
    Translated,
}

struct PreviewState {
    source: PreviewSource,
    page: usize,
    page_count: usize,
    texture: egui::TextureHandle,
}

struct PreviewRequest {
    generation: u64,
    path: PathBuf,
    page: usize,
}

enum GuiMessage {
    Progress(ProgressEvent),
    Finished(Result<TranslateReport, String>),
    Preview {
        generation: u64,
        result: Result<preview::PreviewFrame, String>,
    },
}

struct GuiApp {
    config: Config,
    config_path: Option<PathBuf>,
    sender: Sender<GuiMessage>,
    receiver: Receiver<GuiMessage>,
    input: String,
    output: String,
    output_custom: bool,
    pages: String,
    target_lang: String,
    concurrency: usize,
    bilingual: bool,
    overlay: bool,
    translate_references: bool,
    keep_annotations: bool,
    fake: bool,
    no_cover: bool,
    debug_boxes: bool,
    font: String,
    bold_font: String,
    min_scale: f64,
    max_expand: f64,
    line_height: f64,
    api_base: String,
    api_key: String,
    api_key_from_env: bool,
    api_key_dirty: bool,
    model: String,
    max_tokens: u32,
    temperature: f32,
    timeout_secs: u64,
    theme: ThemeMode,
    settings_open: bool,
    running: bool,
    cancel: CancellationToken,
    stage: Option<JobStage>,
    completed: usize,
    total: usize,
    status: String,
    logs: VecDeque<String>,
    last_output: Option<PathBuf>,
    preview_source: PreviewSource,
    preview: Option<PreviewState>,
    preview_loading: bool,
    preview_generation: u64,
    preview_pending: Option<PreviewRequest>,
    preview_worker_running: bool,
}

impl GuiApp {
    fn new(
        context: &eframe::CreationContext<'_>,
        config: Config,
        config_path: Option<PathBuf>,
    ) -> Self {
        install_cjk_font(&context.egui_ctx);
        let (sender, receiver) = mpsc::channel();
        let theme = config.gui.theme;
        let api_key_from_env = config.api.api_key.is_none()
            && std::env::var_os("OPENAI_API_KEY").is_some_and(|value| !value.is_empty());
        let app = Self {
            input: String::new(),
            output: String::new(),
            output_custom: false,
            pages: "1-".to_string(),
            target_lang: config
                .translation
                .target_lang
                .clone()
                .unwrap_or_else(|| "zh-CN".to_string()),
            concurrency: config.translation.concurrency.unwrap_or(6),
            bilingual: true,
            overlay: false,
            translate_references: false,
            keep_annotations: false,
            fake: false,
            no_cover: false,
            debug_boxes: false,
            font: path_text(config.typeset.font.as_deref()),
            bold_font: path_text(config.typeset.bold_font.as_deref()),
            min_scale: config.typeset.min_scale.unwrap_or(0.72),
            max_expand: config.typeset.max_expand.unwrap_or(1.45),
            line_height: config.typeset.line_height.unwrap_or(1.22),
            api_base: config
                .api
                .base_url
                .clone()
                .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            api_key: config
                .api
                .api_key
                .clone()
                .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                .unwrap_or_default(),
            api_key_from_env,
            api_key_dirty: false,
            model: config
                .api
                .model
                .clone()
                .or_else(|| std::env::var("OPENAI_MODEL").ok())
                .unwrap_or_else(|| "gpt-4o-mini".to_string()),
            max_tokens: config.api.max_tokens.unwrap_or(4096),
            temperature: config.api.temperature.unwrap_or(0.2),
            timeout_secs: config.api.timeout_secs.unwrap_or(180),
            theme,
            settings_open: false,
            running: false,
            cancel: CancellationToken::default(),
            stage: None,
            completed: 0,
            total: 0,
            status: "选择 PDF 后开始翻译".to_string(),
            logs: VecDeque::new(),
            last_output: None,
            preview_source: PreviewSource::Original,
            preview: None,
            preview_loading: false,
            preview_generation: 0,
            preview_pending: None,
            preview_worker_running: false,
            config,
            config_path,
            sender,
            receiver,
        };
        app.apply_theme(&context.egui_ctx);
        app
    }

    fn apply_theme(&self, context: &egui::Context) {
        context.set_theme(match self.theme {
            ThemeMode::System => egui::ThemePreference::System,
            ThemeMode::Light => egui::ThemePreference::Light,
            ThemeMode::Dark => egui::ThemePreference::Dark,
        });
    }

    fn log(&mut self, message: impl Into<String>) {
        if self.logs.len() == 200 {
            self.logs.pop_front();
        }
        self.logs.push_back(message.into());
    }

    fn set_input(&mut self, path: PathBuf) {
        self.input = path.to_string_lossy().into_owned();
        if !self.output_custom {
            self.output = workflow::default_output(&path, self.bilingual)
                .to_string_lossy()
                .into_owned();
        }
        self.last_output = None;
        self.preview_source = PreviewSource::Original;
        self.status = format!("已选择 {}", path.display());
        self.request_preview();
    }

    fn request_preview(&mut self) {
        let path = match self.preview_source {
            PreviewSource::Original => PathBuf::from(self.input.trim()),
            PreviewSource::Translated => match &self.last_output {
                Some(path) => path.clone(),
                None => return,
            },
        };
        if path.as_os_str().is_empty() {
            return;
        }
        let page = self.preview.as_ref().map_or(1, |preview| preview.page);
        self.preview_generation += 1;
        self.preview_pending = Some(PreviewRequest {
            generation: self.preview_generation,
            path,
            page,
        });
        self.preview_loading = true;
        self.run_next_preview();
    }

    fn run_next_preview(&mut self) {
        if self.preview_worker_running {
            return;
        }
        let Some(request) = self.preview_pending.take() else {
            return;
        };
        self.preview_worker_running = true;
        let sender = self.sender.clone();
        if let Err(error) = spawn_worker("pdf-preview", move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                preview::render(&request.path, request.page)
            }))
            .map_err(|_| "预览渲染发生内部错误".to_string())
            .and_then(|result| result.map_err(|error| format!("{error:#}")));
            let _ = sender.send(GuiMessage::Preview {
                generation: request.generation,
                result,
            });
        }) {
            self.preview_worker_running = false;
            self.preview_loading = false;
            self.log(format!("无法启动预览线程：{error}"));
        }
    }

    fn start_translation(&mut self) {
        if self.running || self.input.trim().is_empty() {
            return;
        }
        self.running = true;
        self.cancel = CancellationToken::default();
        self.stage = None;
        self.completed = 0;
        self.total = 0;
        self.status = "正在准备翻译".to_string();
        self.log(format!("开始处理 {}", self.input.trim()));
        let request = TranslateRequest {
            input: PathBuf::from(self.input.trim()),
            output: non_empty_path(&self.output),
            pages: self.pages.clone(),
            translation: TranslationOptions {
                api_base: self.api_base.trim().to_string(),
                api_key: non_empty_string(&self.api_key),
                model: self.model.trim().to_string(),
                target_lang: self.target_lang.trim().to_string(),
                fake: self.fake,
                concurrency: self.concurrency,
                max_tokens: self.max_tokens,
                temperature: self.temperature,
                timeout_secs: self.timeout_secs,
                limit: None,
            },
            typeset: TypesetOptions {
                min_scale: self.min_scale,
                max_expand: self.max_expand,
                line_height: self.line_height,
            },
            compose: ComposeOptions {
                mode: if self.overlay {
                    ComposeMode::Overlay
                } else {
                    ComposeMode::Clean
                },
                cover_original: !self.no_cover,
                cover_rgb: [1.0, 1.0, 1.0],
                debug_boxes: self.debug_boxes,
                keep_annotations: self.keep_annotations,
            },
            font: non_empty_path(&self.font),
            bold_font: non_empty_path(&self.bold_font),
            translate_references: self.translate_references,
            bilingual: self.bilingual,
            dump_layout: None,
        };
        let sender = self.sender.clone();
        let progress_sender = sender.clone();
        let worker_cancel = self.cancel.clone();
        if let Err(error) = spawn_worker("pdf-gui-translate", move || {
            let progress = move |event| {
                let _ = progress_sender.send(GuiMessage::Progress(event));
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                workflow::translate_with_progress(request, &progress, &worker_cancel)
            }))
            .map_err(|_| "翻译线程发生内部错误".to_string())
            .and_then(|result| result.map_err(|error| format!("{error:#}")));
            let _ = sender.send(GuiMessage::Finished(result));
        }) {
            self.running = false;
            self.stage = None;
            self.status = format!("无法启动翻译线程：{error}");
            self.log(self.status.clone());
        }
    }

    fn handle_messages(&mut self, context: &egui::Context) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                GuiMessage::Progress(event) => self.handle_progress(event),
                GuiMessage::Finished(result) => {
                    self.running = false;
                    self.stage = None;
                    match result {
                        Ok(report) => {
                            self.completed = self.total;
                            let failed = report.translation.failed;
                            self.status = if failed == 0 {
                                format!(
                                    "完成：{} 段已翻译，{} 段命中缓存",
                                    report.translation.translated, report.translation.cache_hits
                                )
                            } else {
                                format!(
                                    "完成（{failed} 段失败）：{} 段已翻译，{} 段命中缓存",
                                    report.translation.translated, report.translation.cache_hits
                                )
                            };
                            if failed > 0 {
                                self.log(format!(
                                    "警告：{failed} 段翻译失败，请检查输出中的原文段落"
                                ));
                            }
                            self.log(format!(
                                "排版：{} 段，{} 段溢出，{} 段缺少字形",
                                report.typeset.paragraphs,
                                report.typeset.overflow,
                                report.typeset.missing_glyphs
                            ));
                            self.log(format!("输出已保存到 {}", report.output.display()));
                            self.last_output = Some(report.output);
                            self.preview_source = PreviewSource::Translated;
                            self.request_preview();
                        }
                        Err(error) => {
                            self.status = if self.cancel.is_cancelled() {
                                "翻译已取消".to_string()
                            } else {
                                format!("翻译失败：{error}")
                            };
                            self.log(self.status.clone());
                        }
                    }
                }
                GuiMessage::Preview { generation, result } => {
                    self.preview_worker_running = false;
                    if generation == self.preview_generation {
                        self.preview_loading = false;
                        match result {
                            Ok(frame) => {
                                let texture = context.load_texture(
                                    format!("preview-{generation}"),
                                    frame.image,
                                    egui::TextureOptions::LINEAR,
                                );
                                self.preview = Some(PreviewState {
                                    source: self.preview_source,
                                    page: frame.page,
                                    page_count: frame.page_count,
                                    texture,
                                });
                            }
                            Err(error) => {
                                self.preview = None;
                                self.log(format!("预览失败：{error}"));
                            }
                        }
                    }
                    self.run_next_preview();
                }
            }
            context.request_repaint();
        }
    }

    fn handle_progress(&mut self, event: ProgressEvent) {
        match event {
            ProgressEvent::Stage(stage) => {
                self.stage = Some(stage);
                self.status = format!("正在{}", stage_label(stage));
                self.log(self.status.clone());
            }
            ProgressEvent::Layout {
                pages,
                lines,
                paragraphs,
                protected,
            } => {
                self.log(format!(
                    "布局：{pages} 页，{lines} 行，{paragraphs} 段，{protected} 段受保护"
                ));
            }
            ProgressEvent::Translation { completed, total } => {
                self.completed = self.completed.max(completed);
                self.total = total;
                self.status = format!("正在翻译：{}/{}", self.completed, self.total);
            }
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let context = ui.ctx().clone();
        egui::Panel::top("top-bar").show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("PDF Translate");
                ui.separator();
                ui.label("保留版式的论文翻译");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("设置").clicked() {
                        self.settings_open = true;
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

    fn options_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("options")
            .resizable(true)
            .default_size(330.0)
            .size_range(290.0..=430.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(6.0);
                    ui.heading("翻译任务");
                    ui.add_space(10.0);
                    ui.add_enabled_ui(!self.running, |ui| {
                        section_label(ui, "文件");
                        if let Some(path) = path_row(ui, "输入 PDF", &self.input) {
                            self.set_input(path);
                        }
                        let input_response = ui.add(
                            egui::TextEdit::singleline(&mut self.input)
                                .desired_width(ui.available_width())
                                .hint_text("PDF 文件路径"),
                        );
                        if input_response.lost_focus() && !self.input.trim().is_empty() {
                            let path = PathBuf::from(self.input.trim());
                            if !self.output_custom {
                                self.output = workflow::default_output(&path, self.bilingual)
                                    .to_string_lossy()
                                    .into_owned();
                            }
                            self.last_output = None;
                            self.preview_source = PreviewSource::Original;
                            self.request_preview();
                        }
                        ui.horizontal(|ui| {
                            ui.label("输出 PDF");
                            if ui.button("浏览").clicked()
                                && let Some(path) = save_pdf_dialog(&self.output)
                            {
                                self.output = path.to_string_lossy().into_owned();
                                self.output_custom = true;
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

                        ui.add_space(12.0);
                        section_label(ui, "翻译");
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
                        if ui
                            .checkbox(&mut self.bilingual, "生成左右双语对照 PDF")
                            .changed()
                            && !self.output_custom
                            && !self.input.trim().is_empty()
                        {
                            self.output = workflow::default_output(
                                Path::new(self.input.trim()),
                                self.bilingual,
                            )
                            .to_string_lossy()
                            .into_owned();
                        }
                        ui.checkbox(&mut self.translate_references, "翻译参考文献");
                        ui.checkbox(&mut self.keep_annotations, "保留链接注释");
                        ui.checkbox(&mut self.overlay, "覆盖模式（默认删除并替换原文字形）");

                        ui.add_space(12.0);
                        egui::CollapsingHeader::new("高级选项").show(ui, |ui| {
                            ui.checkbox(&mut self.fake, "离线模拟翻译，不调用 API");
                            ui.checkbox(&mut self.no_cover, "覆盖模式下不绘制背景色块");
                            ui.checkbox(&mut self.debug_boxes, "绘制段落调试框");
                            option_grid(ui, |ui| {
                                ui.label("正文字体");
                                ui.text_edit_singleline(&mut self.font);
                                ui.end_row();
                                ui.label("粗体字体");
                                ui.text_edit_singleline(&mut self.bold_font);
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
                        });
                    });

                    ui.add_space(18.0);
                    if self.running {
                        if ui
                            .add_sized([ui.available_width(), 36.0], egui::Button::new("取消翻译"))
                            .clicked()
                        {
                            self.cancel.cancel();
                            self.status = "正在取消，等待当前请求结束".to_string();
                        }
                    } else {
                        let enabled = !self.input.trim().is_empty();
                        if ui
                            .add_enabled(
                                enabled,
                                egui::Button::new("开始翻译")
                                    .min_size(egui::vec2(ui.available_width(), 36.0)),
                            )
                            .clicked()
                        {
                            self.start_translation();
                        }
                    }
                    ui.add_space(8.0);
                });
            });
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("页面预览");
                ui.separator();
                if ui
                    .selectable_value(&mut self.preview_source, PreviewSource::Original, "原文")
                    .changed()
                {
                    self.request_preview();
                }
                ui.add_enabled_ui(self.last_output.is_some(), |ui| {
                    if ui
                        .selectable_value(
                            &mut self.preview_source,
                            PreviewSource::Translated,
                            "译文",
                        )
                        .changed()
                    {
                        self.request_preview();
                    }
                });
                if let Some((page, page_count)) = self
                    .preview
                    .as_ref()
                    .map(|preview| (preview.page, preview.page_count))
                {
                    ui.separator();
                    let mut target = None;
                    if ui.button("上一页").clicked() && page > 1 {
                        target = Some(page - 1);
                    }
                    ui.label(format!("{page} / {page_count}"));
                    if ui.button("下一页").clicked() && page < page_count {
                        target = Some(page + 1);
                    }
                    if let Some(page) = target {
                        self.preview.as_mut().unwrap().page = page;
                        self.request_preview();
                    }
                }
                if self.preview_loading {
                    ui.spinner();
                }
            });
            ui.add_space(8.0);

            let valid_preview = self
                .preview
                .as_ref()
                .is_some_and(|preview| preview.source == self.preview_source);
            if valid_preview {
                let preview = self.preview.as_ref().unwrap();
                let texture_size = preview.texture.size_vec2();
                let available = ui.available_size() - egui::vec2(16.0, 16.0);
                let scale = (available.x / texture_size.x)
                    .min(available.y / texture_size.y)
                    .max(0.05);
                let size = texture_size * scale;
                ui.centered_and_justified(|ui| {
                    ui.add(
                        egui::Image::new(&preview.texture)
                            .fit_to_exact_size(size)
                            .corner_radius(2.0),
                    );
                });
            } else {
                ui.centered_and_justified(|ui| {
                    if self.input.trim().is_empty() {
                        ui.label(
                            egui::RichText::new("选择或拖入 PDF 后显示页面预览")
                                .color(ui.visuals().weak_text_color()),
                        );
                    } else if self.preview_loading {
                        ui.spinner();
                    } else {
                        ui.label("当前没有可用预览");
                    }
                });
            }
        });
    }

    fn status_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status")
            .resizable(true)
            .default_size(128.0)
            .size_range(90.0..=240.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.running {
                        ui.spinner();
                    }
                    ui.label(&self.status);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(self.last_output.is_some(), egui::Button::new("打开结果"))
                            .clicked()
                        {
                            self.open_output(false);
                        }
                        if ui
                            .add_enabled(
                                self.last_output.is_some(),
                                egui::Button::new("打开所在目录"),
                            )
                            .clicked()
                        {
                            self.open_output(true);
                        }
                    });
                });
                if self.total > 0 {
                    let fraction = self.completed as f32 / self.total.max(1) as f32;
                    ui.add(
                        egui::ProgressBar::new(fraction)
                            .text(format!("{} / {}", self.completed, self.total)),
                    );
                }
                ui.separator();
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &self.logs {
                            ui.label(egui::RichText::new(line).monospace().small());
                        }
                    });
            });
    }

    fn settings_window(&mut self, context: &egui::Context) {
        if !self.settings_open {
            return;
        }
        egui::Window::new("API 与排版设置")
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .show(context, |ui| {
                option_grid(ui, |ui| {
                    ui.label("API Base URL");
                    ui.text_edit_singleline(&mut self.api_base);
                    ui.end_row();
                    ui.label("API Key");
                    let response =
                        ui.add(egui::TextEdit::singleline(&mut self.api_key).password(true));
                    if response.changed() {
                        self.api_key_dirty = true;
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
                ui.add_space(8.0);
                ui.label("API Key 仅用于请求和本地配置，不会写入 GUI 日志。");
                if self.api_key_from_env {
                    ui.label("当前 API Key 来自环境变量；未修改时不会写入配置文件。");
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("保存设置").clicked() {
                        match self.save_settings() {
                            Ok(path) => {
                                self.status = format!("设置已保存到 {}", path.display());
                                self.log(self.status.clone());
                                self.settings_open = false;
                            }
                            Err(error) => {
                                self.status = format!("保存设置失败：{error:#}");
                                self.log(self.status.clone());
                            }
                        }
                    }
                    if ui.button("关闭").clicked() {
                        self.settings_open = false;
                    }
                });
            });
    }

    fn save_settings(&mut self) -> Result<PathBuf> {
        self.config.api.base_url = non_empty_string(&self.api_base);
        if !self.api_key_from_env || self.api_key_dirty {
            self.config.api.api_key = non_empty_string(&self.api_key);
        }
        self.config.api.model = non_empty_string(&self.model);
        self.config.api.max_tokens = Some(self.max_tokens);
        self.config.api.temperature = Some(self.temperature);
        self.config.api.timeout_secs = Some(self.timeout_secs);
        self.config.translation.target_lang = non_empty_string(&self.target_lang);
        self.config.translation.concurrency = Some(self.concurrency);
        self.config.typeset.font = non_empty_path(&self.font);
        self.config.typeset.bold_font = non_empty_path(&self.bold_font);
        self.config.typeset.min_scale = Some(self.min_scale);
        self.config.typeset.max_expand = Some(self.max_expand);
        self.config.typeset.line_height = Some(self.line_height);
        self.config.gui.theme = self.theme;
        let path = self.config.save(self.config_path.as_deref())?;
        self.api_key_dirty = false;
        self.api_key_from_env = self.config.api.api_key.is_none()
            && std::env::var_os("OPENAI_API_KEY").is_some_and(|value| !value.is_empty());
        Ok(path)
    }

    fn open_output(&mut self, parent: bool) {
        let Some(output) = &self.last_output else {
            return;
        };
        let path = if parent {
            output.parent().unwrap_or(output)
        } else {
            output
        };
        if let Err(error) = open::that_detached(path) {
            self.status = format!("无法打开 {}：{error}", path.display());
            self.log(self.status.clone());
        }
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        self.handle_messages(&context);
        let dropped = context.input(|input| input.raw.dropped_files.clone());
        if !self.running
            && let Some(path) = dropped.iter().find_map(|file| {
                (!file.path().as_os_str().is_empty()).then(|| file.path().to_path_buf())
            })
        {
            self.set_input(path);
        }
        if self.running {
            context.request_repaint_after(std::time::Duration::from_millis(100));
        }
        self.top_bar(ui);
        self.options_panel(ui);
        self.status_panel(ui);
        self.preview_panel(ui);
        self.settings_window(&context);
    }
}

fn install_cjk_font(_context: &egui::Context) {
    #[cfg(feature = "embedded-fonts")]
    {
        let compressed = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/fonts/NotoSerifCJKsc-Regular.otf.zst"
        ));
        if let Ok(mut decoder) = ruzstd::decoding::StreamingDecoder::new(compressed.as_slice()) {
            let mut data = Vec::new();
            if std::io::Read::read_to_end(&mut decoder, &mut data).is_ok() {
                let mut fonts = egui::FontDefinitions::default();
                fonts.font_data.insert(
                    "noto-cjk".to_string(),
                    egui::FontData::from_owned(data).into(),
                );
                for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                    if let Some(fonts) = fonts.families.get_mut(&family) {
                        fonts.push("noto-cjk".to_string());
                    }
                }
                _context.set_fonts(fonts);
            }
        }
    }
}

fn path_row(ui: &mut egui::Ui, label: &str, current: &str) -> Option<PathBuf> {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.button("浏览").clicked()
    })
    .inner
    .then(|| open_pdf_dialog(current))
    .flatten()
}

fn option_grid(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(ui.scope_id().with("grid"))
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, add);
}

fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).strong());
    ui.add_space(4.0);
}

fn open_pdf_dialog(current: &str) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().add_filter("PDF", &["pdf"]);
    if let Some(parent) = Path::new(current).parent() {
        dialog = dialog.set_directory(parent);
    }
    dialog.pick_file()
}

fn save_pdf_dialog(current: &str) -> Option<PathBuf> {
    let path = Path::new(current);
    let mut dialog = rfd::FileDialog::new().add_filter("PDF", &["pdf"]);
    if let Some(parent) = path.parent() {
        dialog = dialog.set_directory(parent);
    }
    if let Some(name) = path.file_name() {
        dialog = dialog.set_file_name(name.to_string_lossy());
    }
    dialog.save_file()
}

fn non_empty_string(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.trim().to_string())
}

fn non_empty_path(value: &str) -> Option<PathBuf> {
    non_empty_string(value).map(PathBuf::from)
}

fn path_text(path: Option<&Path>) -> String {
    path.map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn stage_label(stage: JobStage) -> &'static str {
    match stage {
        JobStage::Extract => "解析 PDF",
        JobStage::Layout => "分析布局",
        JobStage::Translate => "翻译",
        JobStage::Typeset => "排版",
        JobStage::Compose => "写入 PDF",
    }
}

fn theme_label(theme: ThemeMode) -> &'static str {
    match theme {
        ThemeMode::System => "跟随系统",
        ThemeMode::Light => "浅色",
        ThemeMode::Dark => "深色",
    }
}

fn spawn_worker(name: &str, task: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(64 * 1024 * 1024)
        .spawn(task)
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_optional_paths_are_ignored() {
        assert_eq!(non_empty_path("  "), None);
        assert_eq!(
            non_empty_path(" /tmp/out.pdf "),
            Some(PathBuf::from("/tmp/out.pdf"))
        );
    }

    #[test]
    fn stage_labels_cover_every_stage() {
        for stage in [
            JobStage::Extract,
            JobStage::Layout,
            JobStage::Translate,
            JobStage::Typeset,
            JobStage::Compose,
        ] {
            assert!(!stage_label(stage).is_empty());
        }
    }
}
