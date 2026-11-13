use std::path::Path;

use anyhow::{Context, Result, bail};
use eframe::egui;
use hayro::hayro_syntax::Pdf;

pub struct PreviewFrame {
    pub page_count: usize,
    pub page: usize,
    pub image: egui::ColorImage,
}

pub fn render(path: &Path, page: usize) -> Result<PreviewFrame> {
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
