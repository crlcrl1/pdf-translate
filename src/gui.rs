mod form;
mod job;
mod preview;
mod settings;
mod widgets;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::{Result, anyhow};
use eframe::egui;

use crate::config::Config;
use crate::progress::ProgressEvent;
use crate::workflow::TranslateReport;

use form::{InputChange, TranslationForm};
use job::TranslationJob;
use preview::{Preview, PreviewSource};
use settings::Settings;

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

enum GuiMessage {
    Progress(ProgressEvent),
    Finished(Result<TranslateReport, String>),
    Preview {
        generation: u64,
        result: Result<preview::PreviewFrame, String>,
    },
}

struct GuiApp {
    form: TranslationForm,
    settings: Settings,
    job: TranslationJob,
    preview: Preview,
    sender: Sender<GuiMessage>,
    receiver: Receiver<GuiMessage>,
}

impl GuiApp {
    fn new(
        context: &eframe::CreationContext<'_>,
        config: Config,
        config_path: Option<PathBuf>,
    ) -> Self {
        install_cjk_font(&context.egui_ctx);
        let (sender, receiver) = mpsc::channel();
        let app = Self {
            form: TranslationForm::new(&config),
            settings: Settings::new(config, config_path),
            job: TranslationJob::default(),
            preview: Preview::default(),
            sender,
            receiver,
        };
        app.settings.apply_theme(&context.egui_ctx);
        app
    }

    fn set_input(&mut self, path: PathBuf) {
        self.form.set_input(&path);
        self.input_changed(InputChange::Selected);
    }

    fn input_changed(&mut self, change: InputChange) {
        if matches!(change, InputChange::Selected) {
            self.job.status = format!("已选择 {}", self.form.input());
        }
        self.job.last_output = None;
        self.preview.source = PreviewSource::Original;
        self.request_preview();
    }

    fn request_preview(&mut self) {
        if let Err(error) = self.preview.request(
            self.form.input(),
            self.job.last_output.as_deref(),
            &self.sender,
        ) {
            self.job.log(error);
        }
    }

    fn handle_messages(&mut self, context: &egui::Context) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                GuiMessage::Progress(event) => self.job.handle_progress(event),
                GuiMessage::Finished(result) => {
                    let succeeded = result.is_ok();
                    self.job.finish(result);
                    if succeeded {
                        self.preview.source = PreviewSource::Translated;
                        self.request_preview();
                    }
                }
                GuiMessage::Preview { generation, result } => {
                    if let Err(error) = self.preview.handle_result(context, generation, result) {
                        self.job.log(error);
                    }
                    if let Err(error) = self.preview.run_next(&self.sender) {
                        self.job.log(error);
                    }
                }
            }
            context.request_repaint();
        }
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
                    let change = ui
                        .add_enabled_ui(!self.job.running, |ui| self.form.show(ui))
                        .inner;
                    if let Some(change) = change {
                        self.input_changed(change);
                    }
                    ui.add_space(18.0);
                    if self.job.controls(ui, !self.form.input().trim().is_empty()) {
                        self.job
                            .start(self.form.request(&self.settings.api), &self.sender);
                    }
                    ui.add_space(8.0);
                });
            });
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        self.handle_messages(&context);
        let dropped = context.input(|input| input.raw.dropped_files.clone());
        if !self.job.running
            && let Some(path) = dropped.iter().find_map(|file| {
                (!file.path().as_os_str().is_empty()).then(|| file.path().to_path_buf())
            })
        {
            self.set_input(path);
        }
        if self.job.running {
            context.request_repaint_after(std::time::Duration::from_millis(100));
        }
        self.settings.top_bar(ui);
        self.options_panel(ui);
        self.job.status_panel(ui);
        if let Err(error) = self.preview.show(
            ui,
            self.form.input(),
            self.job.last_output.as_deref(),
            &self.sender,
        ) {
            self.job.log(error);
        }
        if let Some(result) = self.settings.window(&context, &self.form) {
            self.job.status = match result {
                Ok(path) => format!("设置已保存到 {}", path.display()),
                Err(error) => format!("保存设置失败：{error:#}"),
            };
            self.job.log(self.job.status.clone());
        }
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

fn spawn_worker(name: &str, task: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(64 * 1024 * 1024)
        .spawn(task)
        .map(|_| ())
}
