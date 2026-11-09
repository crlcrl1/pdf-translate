use hayro::hayro_interpret::{DrawMode, font::GlyphRun};
use sha2::{Digest, Sha256};

/// Both interpretations count every original paint, including invisible text
/// and the second callback for fill-and-stroke. Synthetic cursor probes are
/// excluded. The signature verifies the sequence before any deletion is applied.
#[derive(Default)]
pub(super) struct GlyphSequence {
    next: usize,
    digest: Sha256,
}

impl GlyphSequence {
    pub fn observe(&mut self, run: &GlyphRun<'_, '_>, mode: &DrawMode) -> usize {
        let start = self.next;
        self.next += run.glyphs().len();
        self.digest.update([match mode {
            DrawMode::Fill(_) => 0,
            DrawMode::Stroke(_) => 1,
            DrawMode::Invisible => 2,
            DrawMode::FillAndStroke(_, _) => 3,
        }]);
        self.digest.update(run.glyphs().len().to_le_bytes());
        for glyph in run.glyphs() {
            self.digest.update(glyph.char_code().to_le_bytes());
        }
        start
    }

    pub fn signature(&self) -> [u8; 32] {
        self.digest.clone().finalize().into()
    }
}
