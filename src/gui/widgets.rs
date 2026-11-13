use std::path::{Path, PathBuf};

use eframe::egui;

pub(super) fn option_grid(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(ui.scope_id().with("grid"))
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, add);
}

pub(super) fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).strong());
    ui.add_space(4.0);
}

pub(super) fn pdf_dialog(current: &str) -> rfd::FileDialog {
    let mut dialog = rfd::FileDialog::new().add_filter("PDF", &["pdf"]);
    if let Some(parent) = Path::new(current).parent() {
        dialog = dialog.set_directory(parent);
    }
    dialog
}

pub(super) fn non_empty_string(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.trim().to_string())
}

pub(super) fn non_empty_path(value: &str) -> Option<PathBuf> {
    non_empty_string(value).map(PathBuf::from)
}

pub(super) fn path_text(path: Option<&Path>) -> String {
    path.map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
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
}
