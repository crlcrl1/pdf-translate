use serde::{Deserialize, Serialize};

/// Axis-aligned rectangle in PDF user space (points, y-up).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct BBox {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl BBox {
    pub const fn new(x0: f64, y0: f64, x1: f64, y1: f64) -> Self {
        Self { x0, y0, x1, y1 }
    }

    pub fn from_kurbo(rect: hayro::kurbo::Rect) -> Self {
        Self {
            x0: rect.x0,
            y0: rect.y0,
            x1: rect.x1,
            y1: rect.y1,
        }
    }

    pub fn width(self) -> f64 {
        (self.x1 - self.x0).max(0.0)
    }

    pub fn height(self) -> f64 {
        (self.y1 - self.y0).max(0.0)
    }

    pub fn center_x(self) -> f64 {
        (self.x0 + self.x1) * 0.5
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    pub fn inflate(self, dx: f64, dy: f64) -> Self {
        Self {
            x0: self.x0 - dx,
            y0: self.y0 - dy,
            x1: self.x1 + dx,
            y1: self.y1 + dy,
        }
    }

    pub fn intersects(self, other: Self) -> bool {
        self.x0 < other.x1 && self.x1 > other.x0 && self.y0 < other.y1 && self.y1 > other.y0
    }

    /// Horizontal overlap ratio relative to the narrower rectangle.
    pub fn horizontal_overlap(self, other: Self) -> f64 {
        let overlap = (self.x1.min(other.x1) - self.x0.max(other.x0)).max(0.0);
        let min_width = self.width().min(other.width());
        if min_width <= 0.0 {
            0.0
        } else {
            overlap / min_width
        }
    }
}

/// Position in the original page's ordered glyph paint callbacks. A repeated
/// Form invocation receives distinct positions even when its transforms match.
#[derive(Clone, Copy, Debug)]
pub struct SourceGlyph {
    pub ordinal: usize,
    pub code: u32,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlyphBox {
    pub text: String,
    pub bbox: BBox,
    pub origin_x: f64,
    pub origin_y: f64,
    pub advance: f64,
    pub font_size: f64,
    pub angle: f64,
    pub font_name: Option<String>,
    pub bold: bool,
    pub italic: bool,
    pub serif: bool,
    pub mono: bool,
    pub color: [u8; 4],
    pub invisible: bool,
    /// Visible content that cannot safely be translated independently.
    #[serde(default)]
    pub protected: bool,
    /// Short vector rules (for example TeX's `_` in `conv2_x`) that belong
    /// to this glyph. They are covered when the glyph is translated.
    #[serde(default)]
    pub decorations: Vec<BBox>,
    pub char_code: u32,
    pub glyph_id: u32,
    /// Per-page paint positions, including all glyphs represented by ActualText.
    #[serde(skip)]
    pub sources: smallvec::SmallVec<[SourceGlyph; 1]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageBox {
    pub bbox: BBox,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedPage {
    pub index: usize,
    pub x0: f64,
    pub y0: f64,
    pub width: f64,
    pub height: f64,
    pub rotation: i32,
    pub layout_transform: [f64; 6],
    #[serde(skip)]
    pub glyph_signature: [u8; 32],
    pub glyphs: Vec<GlyphBox>,
    pub images: Vec<ImageBox>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Column {
    Full,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Title,
    Heading,
    Paragraph,
    Caption,
    Formula,
    Header,
    Footer,
    PageNumber,
    Reference,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TextStyle {
    pub bold: bool,
    pub italic: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextSpan {
    pub text: String,
    pub bbox: BBox,
    pub font_size: f64,
    pub font_name: Option<String>,
    pub bold: bool,
    pub italic: bool,
    pub serif: bool,
    pub mono: bool,
    pub color: [u8; 4],
    pub formula: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextLine {
    pub bbox: BBox,
    pub baseline: f64,
    pub text: String,
    pub spans: Vec<TextSpan>,
    pub font_size: f64,
    pub column: Column,
    pub formula: bool,
    pub math_ratio: f64,
    #[serde(skip)]
    pub glyphs: Vec<GlyphBox>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormulaRef {
    #[serde(skip)]
    pub sources: Vec<SourceGlyph>,
    pub id: String,
    pub bbox: BBox,
    pub baseline: f64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paragraph {
    pub id: String,
    pub page: usize,
    pub bbox: BBox,
    pub text: String,
    #[serde(skip)]
    pub line_indices: Vec<usize>,
    #[serde(skip)]
    pub base_style: TextStyle,
    pub font_size: f64,
    pub column: Column,
    pub role: Role,
    pub align: Align,
    pub protected: bool,
    pub has_formula: bool,
    #[serde(default)]
    pub formulas: Vec<FormulaRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub translation: Option<String>,
}

impl Paragraph {
    pub fn lines<'a>(&'a self, lines: &'a [TextLine]) -> impl Iterator<Item = &'a TextLine> {
        self.line_indices.iter().map(|&index| &lines[index])
    }

    pub fn finalize_style(&mut self, lines: &[TextLine]) {
        let mut total = 0;
        let mut italic = 0;
        for span in self.lines(lines).flat_map(|line| &line.spans) {
            let count = span.text.chars().filter(|ch| !ch.is_whitespace()).count();
            total += count;
            if span.italic {
                italic += count;
            }
        }
        self.base_style = TextStyle {
            bold: matches!(self.role, Role::Title | Role::Heading),
            italic: total > 0 && italic * 10 >= total * 8,
        };
    }
}

#[derive(Debug, Clone)]
pub struct PageLayout {
    pub index: usize,
    pub x0: f64,
    pub y0: f64,
    pub width: f64,
    pub height: f64,
    pub rotation: i32,
    /// Maps original PDF user space to the upright layout coordinate system.
    pub layout_transform: [f64; 6],
    pub glyph_signature: [u8; 32],
    pub lines: Vec<TextLine>,
    pub paragraphs: Vec<Paragraph>,
    pub images: Vec<ImageBox>,
}

impl Serialize for PageLayout {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        #[derive(Serialize)]
        struct ParagraphView<'a> {
            #[serde(flatten)]
            paragraph: &'a Paragraph,
            lines: Vec<&'a TextLine>,
        }
        let paragraphs: Vec<_> = self
            .paragraphs
            .iter()
            .map(|paragraph| ParagraphView {
                paragraph,
                lines: paragraph.lines(&self.lines).collect(),
            })
            .collect();
        let mut page = serializer.serialize_struct("PageLayout", 10)?;
        page.serialize_field("index", &self.index)?;
        page.serialize_field("x0", &self.x0)?;
        page.serialize_field("y0", &self.y0)?;
        page.serialize_field("width", &self.width)?;
        page.serialize_field("height", &self.height)?;
        page.serialize_field("rotation", &self.rotation)?;
        page.serialize_field("layout_transform", &self.layout_transform)?;
        page.serialize_field("lines", &self.lines)?;
        page.serialize_field("paragraphs", &paragraphs)?;
        page.serialize_field("images", &self.images)?;
        page.end()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LayoutDocument {
    pub source: String,
    pub pages: Vec<PageLayout>,
    pub stats: LayoutStats,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LayoutStats {
    pub pages: usize,
    pub lines: usize,
    pub paragraphs: usize,
    pub translatable: usize,
    pub protected: usize,
    pub formula_paragraphs: usize,
}

impl LayoutDocument {
    pub fn recompute_stats(&mut self) {
        let pages = self.pages.len();
        let lines = self.pages.iter().map(|p| p.lines.len()).sum();
        let paragraphs = self.pages.iter().map(|p| p.paragraphs.len()).sum();
        let protected = self
            .pages
            .iter()
            .flat_map(|p| &p.paragraphs)
            .filter(|p| p.protected)
            .count();
        let formula_paragraphs = self
            .pages
            .iter()
            .flat_map(|p| &p.paragraphs)
            .filter(|p| p.has_formula)
            .count();
        self.stats = LayoutStats {
            pages,
            lines,
            paragraphs,
            translatable: paragraphs.saturating_sub(protected),
            protected,
            formula_paragraphs,
        };
    }
}
