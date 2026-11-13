use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use eframe::egui;

use crate::progress::{CancellationToken, JobStage, ProgressEvent};
use crate::workflow::{self, TranslateReport, TranslateRequest};

use super::{GuiMessage, spawn_worker};

pub(super) struct TranslationJob {
    pub(super) running: bool,
    pub(super) status: String,
    pub(super) last_output: Option<PathBuf>,
    cancel: CancellationToken,
    completed: usize,
    total: usize,
    logs: VecDeque<String>,
}

impl Default for TranslationJob {
    fn default() -> Self {
        Self {
            running: false,
            status: "选择 PDF 后开始翻译".to_string(),
            last_output: None,
            cancel: CancellationToken::default(),
            completed: 0,
            total: 0,
            logs: VecDeque::new(),
        }
    }
}

impl TranslationJob {
    pub(super) fn log(&mut self, message: impl Into<String>) {
        if self.logs.len() == 200 {
            self.logs.pop_front();
        }
        self.logs.push_back(message.into());
    }

    pub(super) fn start(&mut self, request: TranslateRequest, sender: &Sender<GuiMessage>) {
        if self.running || request.input.as_os_str().is_empty() {
            return;
        }
        self.begin();
        self.log(format!("开始处理 {}", request.input.display()));
        let sender = sender.clone();
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
            self.status = format!("无法启动翻译线程：{error}");
            self.log(self.status.clone());
        }
    }

    fn begin(&mut self) {
        self.running = true;
        self.cancel = CancellationToken::default();
        self.completed = 0;
        self.total = 0;
        self.status = "正在准备翻译".to_string();
    }

    pub(super) fn finish(&mut self, result: Result<TranslateReport, String>) {
        self.running = false;
        match result {
            Ok(report) => self.finish_success(report),
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

    fn finish_success(&mut self, report: TranslateReport) {
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
            self.log(format!("警告：{failed} 段翻译失败，请检查输出中的原文段落"));
        }
        self.log(format!(
            "排版：{} 段，{} 段溢出，{} 段缺少字形",
            report.typeset.paragraphs, report.typeset.overflow, report.typeset.missing_glyphs
        ));
        self.log(format!("输出已保存到 {}", report.output.display()));
        self.last_output = Some(report.output);
    }

    pub(super) fn handle_progress(&mut self, event: ProgressEvent) {
        match event {
            ProgressEvent::Stage(stage) => {
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

    pub(super) fn controls(&mut self, ui: &mut egui::Ui, can_start: bool) -> bool {
        if self.running {
            if ui
                .add_sized([ui.available_width(), 36.0], egui::Button::new("取消翻译"))
                .clicked()
            {
                self.cancel.cancel();
                self.status = "正在取消，等待当前请求结束".to_string();
            }
            false
        } else {
            ui.add_enabled(
                can_start,
                egui::Button::new("开始翻译").min_size(egui::vec2(ui.available_width(), 36.0)),
            )
            .clicked()
        }
    }

    pub(super) fn status_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status")
            .resizable(true)
            .default_size(128.0)
            .size_range(90.0..=240.0)
            .show(ui, |ui| {
                self.status_row(ui);
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

    fn status_row(&mut self, ui: &mut egui::Ui) {
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

fn stage_label(stage: JobStage) -> &'static str {
    match stage {
        JobStage::Extract => "解析 PDF",
        JobStage::Layout => "分析布局",
        JobStage::Translate => "翻译",
        JobStage::Typeset => "排版",
        JobStage::Compose => "写入 PDF",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_is_monotonic_and_cancellation_does_not_leak_into_next_job() {
        let mut job = TranslationJob::default();
        job.begin();
        for completed in [5, 3] {
            job.handle_progress(ProgressEvent::Translation {
                completed,
                total: 10,
            });
        }
        assert_eq!(job.completed, 5);
        let previous_cancel = job.cancel.clone();
        previous_cancel.cancel();
        job.finish(Err("cancelled".to_string()));
        assert!(!job.running);
        assert_eq!(job.status, "翻译已取消");

        job.begin();
        assert!(previous_cancel.is_cancelled());
        assert!(!job.cancel.is_cancelled());
        assert_eq!((job.completed, job.total), (0, 0));
        job.finish(Err("invalid PDF".to_string()));
        assert_eq!(job.status, "翻译失败：invalid PDF");
    }

    #[test]
    fn partial_success_keeps_output_and_failure_warning() {
        let mut job = TranslationJob::default();
        job.begin();
        job.handle_progress(ProgressEvent::Translation {
            completed: 2,
            total: 3,
        });
        job.finish(Ok(TranslateReport {
            output: PathBuf::from("translated.pdf"),
            translation: crate::translate::TranslationStats {
                translated: 2,
                failed: 1,
                ..Default::default()
            },
            typeset: crate::typeset::TypesetStats::default(),
        }));
        assert!(!job.running);
        assert_eq!(job.completed, 3);
        assert_eq!(job.last_output, Some(PathBuf::from("translated.pdf")));
        assert!(job.status.contains("1 段失败"));
        assert!(
            job.logs
                .iter()
                .any(|line| line.contains("警告：1 段翻译失败"))
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
