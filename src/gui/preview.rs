use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use anyhow::{Context, Result, bail};
use eframe::egui;
use hayro::hayro_syntax::Pdf;

use super::{GuiMessage, spawn_worker};

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum PreviewSource {
    #[default]
    Original,
    Translated,
}

#[derive(Default)]
pub(super) struct Preview {
    pub(super) source: PreviewSource,
    displayed: Option<DisplayedPage>,
    loading: bool,
    generation: u64,
    pending: Option<PreviewRequest>,
    worker_running: bool,
}

struct DisplayedPage {
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

pub(super) struct PreviewFrame {
    page_count: usize,
    page: usize,
    image: egui::ColorImage,
}

impl Preview {
    pub(super) fn request(
        &mut self,
        input: &str,
        output: Option<&Path>,
        sender: &Sender<GuiMessage>,
    ) -> Result<(), String> {
        let path = match self.source {
            PreviewSource::Original => Path::new(input.trim()),
            PreviewSource::Translated => match output {
                Some(path) => path,
                None => return Ok(()),
            },
        };
        if path.as_os_str().is_empty() {
            return Ok(());
        }
        self.enqueue(path.to_path_buf());
        self.run_next(sender)
    }

    fn enqueue(&mut self, path: PathBuf) {
        self.generation += 1;
        self.pending = Some(PreviewRequest {
            generation: self.generation,
            path,
            page: self.displayed.as_ref().map_or(1, |preview| preview.page),
        });
        self.loading = true;
    }

    fn next_request(&mut self) -> Option<PreviewRequest> {
        if self.worker_running {
            return None;
        }
        let request = self.pending.take()?;
        self.worker_running = true;
        Some(request)
    }

    pub(super) fn run_next(&mut self, sender: &Sender<GuiMessage>) -> Result<(), String> {
        let Some(request) = self.next_request() else {
            return Ok(());
        };
        let sender = sender.clone();
        if let Err(error) = spawn_worker("pdf-preview", move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                render(&request.path, request.page)
            }))
            .map_err(|_| "预览渲染发生内部错误".to_string())
            .and_then(|result| result.map_err(|error| format!("{error:#}")));
            let _ = sender.send(GuiMessage::Preview {
                generation: request.generation,
                result,
            });
        }) {
            self.worker_running = false;
            self.loading = false;
            return Err(format!("无法启动预览线程：{error}"));
        }
        Ok(())
    }

    pub(super) fn handle_result(
        &mut self,
        context: &egui::Context,
        generation: u64,
        result: Result<PreviewFrame, String>,
    ) -> Result<(), String> {
        self.worker_running = false;
        if generation != self.generation {
            return Ok(());
        }
        self.loading = false;
        match result {
            Ok(frame) => {
                let texture = context.load_texture(
                    format!("preview-{generation}"),
                    frame.image,
                    egui::TextureOptions::LINEAR,
                );
                self.displayed = Some(DisplayedPage {
                    source: self.source,
                    page: frame.page,
                    page_count: frame.page_count,
                    texture,
                });
                Ok(())
            }
            Err(error) => {
                self.displayed = None;
                Err(format!("预览失败：{error}"))
            }
        }
    }

    pub(super) fn show(
        &mut self,
        ui: &mut egui::Ui,
        input: &str,
        output: Option<&Path>,
        sender: &Sender<GuiMessage>,
    ) -> Result<(), String> {
        egui::CentralPanel::default()
            .show(ui, |ui| {
                let result = self.toolbar(ui, input, output, sender);
                ui.add_space(8.0);
                self.show_page(ui, input.trim().is_empty());
                result
            })
            .inner
    }

    fn toolbar(
        &mut self,
        ui: &mut egui::Ui,
        input: &str,
        output: Option<&Path>,
        sender: &Sender<GuiMessage>,
    ) -> Result<(), String> {
        ui.horizontal(|ui| {
            ui.heading("页面预览");
            ui.separator();
            let mut changed = ui
                .selectable_value(&mut self.source, PreviewSource::Original, "原文")
                .changed();
            changed |= ui
                .add_enabled_ui(output.is_some(), |ui| {
                    ui.selectable_value(&mut self.source, PreviewSource::Translated, "译文")
                        .changed()
                })
                .inner;
            changed |= self.page_controls(ui);
            let result = if changed {
                self.request(input, output, sender)
            } else {
                Ok(())
            };
            if self.loading {
                ui.spinner();
            }
            result
        })
        .inner
    }

    fn page_controls(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(preview) = &mut self.displayed else {
            return false;
        };
        ui.separator();
        let mut target = None;
        if ui.button("上一页").clicked() && preview.page > 1 {
            target = Some(preview.page - 1);
        }
        ui.label(format!("{} / {}", preview.page, preview.page_count));
        if ui.button("下一页").clicked() && preview.page < preview.page_count {
            target = Some(preview.page + 1);
        }
        if let Some(page) = target {
            preview.page = page;
            true
        } else {
            false
        }
    }

    fn show_page(&self, ui: &mut egui::Ui, input_empty: bool) {
        let preview = self
            .displayed
            .as_ref()
            .filter(|preview| preview.source == self.source);
        if let Some(preview) = preview {
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
                if input_empty {
                    ui.label(
                        egui::RichText::new("选择或拖入 PDF 后显示页面预览")
                            .color(ui.visuals().weak_text_color()),
                    );
                } else if self.loading {
                    ui.spinner();
                } else {
                    ui.label("当前没有可用预览");
                }
            });
        }
    }
}

fn render(path: &Path, page: usize) -> Result<PreviewFrame> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("cannot read preview PDF {}", path.display()))?;
    let pdf =
        Pdf::new(bytes).map_err(|error| anyhow::anyhow!("cannot parse preview PDF: {error:?}"))?;
    let page_count = pdf.pages().len();
    if page_count == 0 {
        bail!("the PDF has no pages");
    }
    let page = page.clamp(1, page_count);
    let options = hayro::PixmapSettings {
        x_scale: 1.5,
        y_scale: 1.5,
        bg_color: hayro::vello_cpu::color::palette::css::WHITE,
    };
    let pixmap = hayro::render(
        &pdf.pages()[page - 1],
        &hayro::RenderCache::new(),
        &Default::default(),
        &Default::default(),
        &options,
    );
    let size = [pixmap.width() as usize, pixmap.height() as usize];
    let image = egui::ColorImage::from_rgba_unmultiplied(size, pixmap.data_as_u8_slice());
    Ok(PreviewFrame {
        page_count,
        page,
        image,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Document, Object, dictionary};

    fn frame(page: usize) -> PreviewFrame {
        PreviewFrame {
            page,
            page_count: 5,
            image: egui::ColorImage::from_rgba_unmultiplied([1, 1], &[255; 4]),
        }
    }

    #[test]
    fn pending_requests_coalesce_and_stale_results_leave_display_unchanged() {
        for stale_result in [Ok(frame(1)), Err("old failure".to_string())] {
            let context = egui::Context::default();
            let mut preview = Preview::default();
            preview.handle_result(&context, 0, Ok(frame(3))).unwrap();
            let old_texture = preview.displayed.as_ref().unwrap().texture.id();
            preview.enqueue(PathBuf::from("first.pdf"));
            let first = preview.next_request().unwrap();
            assert_eq!(first.page, 3);

            preview.source = PreviewSource::Translated;
            preview.enqueue(PathBuf::from("skipped.pdf"));
            preview.enqueue(PathBuf::from("latest.pdf"));
            assert!(preview.next_request().is_none());
            preview
                .handle_result(&context, first.generation, stale_result)
                .unwrap();
            assert!(preview.loading);
            assert_eq!(
                preview.displayed.as_ref().unwrap().texture.id(),
                old_texture
            );

            let latest = preview.next_request().unwrap();
            assert_eq!(latest.path, Path::new("latest.pdf"));
            preview
                .handle_result(&context, latest.generation, Ok(frame(2)))
                .unwrap();
            let displayed = preview.displayed.as_ref().unwrap();
            assert!(displayed.source == PreviewSource::Translated);
            assert_eq!(displayed.page, 2);
            assert!(!preview.loading);
            assert!(preview.next_request().is_none());
        }
    }

    #[test]
    fn latest_failure_clears_display_and_allows_another_request() {
        let context = egui::Context::default();
        let mut preview = Preview::default();
        preview.handle_result(&context, 0, Ok(frame(1))).unwrap();
        preview.enqueue(PathBuf::from("broken.pdf"));
        let request = preview.next_request().unwrap();
        let result =
            preview.handle_result(&context, request.generation, Err("invalid PDF".to_string()));
        assert_eq!(result.unwrap_err(), "预览失败：invalid PDF");
        assert!(preview.displayed.is_none());
        assert!(!preview.loading);
        preview.enqueue(PathBuf::from("valid.pdf"));
        assert!(preview.next_request().is_some());
    }

    #[test]
    fn preview_has_an_opaque_white_page() {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 12.into(), 18.into()],
        });
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blank.pdf");
        document.save(&path).unwrap();

        let frame = render(&path, 1).unwrap();
        assert_eq!(frame.page_count, 1);
        assert_eq!(frame.image.size, [18, 27]);
        assert!(
            frame
                .image
                .pixels
                .iter()
                .all(|pixel| *pixel == egui::Color32::WHITE)
        );
    }
}
