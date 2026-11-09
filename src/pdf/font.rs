use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
#[cfg(not(feature = "embedded-fonts"))]
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use subsetter::GlyphRemapper;
use ttf_parser::{Face, GlyphId, fonts_in_collection};

use crate::text::is_cjk;

#[derive(Debug, Clone)]
pub struct FontSource {
    pub path: PathBuf,
    pub index: u32,
    pub family: String,
    pub data: Cow<'static, [u8]>,
    pub embedded: bool,
}

#[derive(Debug, Clone)]
pub struct FontMetrics {
    pub units_per_em: f64,
    pub ascender: f64,
    pub descender: f64,
    pub cap_height: f64,
    pub bbox: [f64; 4],
    pub italic_angle: f64,
    pub char_to_gid: HashMap<char, u16>,
    pub gid_advance: Vec<u16>,
}

#[derive(Debug, Clone)]
pub struct EmbeddedFont {
    pub pdf_name: String,
    pub subset: Vec<u8>,
    pub char_to_code: HashMap<char, u16>,
    pub unicode_maps: Vec<BTreeMap<u16, char>>,
    pub char_variants: HashMap<char, usize>,
    pub new_gid_to_width: BTreeMap<u16, u16>,
    pub metrics: FontMetrics,
    pub is_cff: bool,
}

#[cfg(feature = "embedded-fonts")]
mod embedded_fonts {
    pub const REGULAR_NAME: &str = "Noto Serif CJK SC Regular";
    #[cfg(feature = "embedded-bold-font")]
    pub const BOLD_NAME: &str = "Noto Serif CJK SC Bold";
    pub const REGULAR: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/fonts/NotoSerifCJKsc-Regular.otf.zst"
    ));
    #[cfg(feature = "embedded-bold-font")]
    pub const BOLD: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/fonts/NotoSerifCJKsc-Bold.otf.zst"
    ));
}

#[cfg(feature = "embedded-fonts")]
fn embedded_regular_font() -> Result<FontSource> {
    let font = FontSource::from_embedded(embedded_fonts::REGULAR_NAME, embedded_fonts::REGULAR, 0)?;
    if !font.supports_cjk() {
        bail!("embedded regular font has no CJK glyphs");
    }
    Ok(font)
}

#[cfg(feature = "embedded-bold-font")]
fn embedded_bold_font() -> Result<FontSource> {
    let font = FontSource::from_embedded(embedded_fonts::BOLD_NAME, embedded_fonts::BOLD, 0)?;
    if !font.supports_cjk() {
        bail!("embedded bold font has no CJK glyphs");
    }
    Ok(font)
}

impl FontSource {
    pub fn load(path: impl AsRef<Path>, index: Option<u32>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let data = std::fs::read(&path)
            .with_context(|| format!("failed to read font: {}", path.display()))?;
        Self::from_data(path, Cow::Owned(data), index, false)
    }

    fn from_data(
        path: PathBuf,
        data: Cow<'static, [u8]>,
        index: Option<u32>,
        embedded: bool,
    ) -> Result<Self> {
        let collection_count = fonts_in_collection(data.as_ref());
        let index = match index {
            Some(index) => {
                if collection_count.is_some_and(|count| index >= count) {
                    bail!(
                        "font index {index} is out of range (collection has {} faces)",
                        collection_count.unwrap_or(0)
                    );
                }
                index
            }
            None => pick_sc_face(data.as_ref()).unwrap_or(0),
        };

        let face = Face::parse(data.as_ref(), index)
            .map_err(|err| anyhow!("failed to parse font ({}): {err}", path.display()))?;
        let family = face_family(&face).unwrap_or_else(|| "CJK".to_string());

        Ok(Self {
            path,
            index,
            family,
            data,
            embedded,
        })
    }

    #[cfg(feature = "embedded-fonts")]
    fn from_embedded(name: &str, data: &'static [u8], index: u32) -> Result<Self> {
        let mut decoder = ruzstd::decoding::StreamingDecoder::new(data)
            .with_context(|| format!("failed to open embedded font {name}"))?;
        let mut font_data = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut font_data)
            .with_context(|| format!("failed to decompress embedded font {name}"))?;
        Self::from_data(
            PathBuf::from(format!("<embedded:{name}>")),
            Cow::Owned(font_data),
            Some(index),
            true,
        )
    }

    pub fn supports_cjk(&self) -> bool {
        Face::parse(self.data.as_ref(), self.index)
            .map(|face| {
                ['中', '文', '。', '，']
                    .into_iter()
                    .all(|ch| face.glyph_index(ch).is_some())
            })
            .unwrap_or(false)
    }

    pub fn metrics(&self, chars: impl IntoIterator<Item = char>) -> Result<FontMetrics> {
        let face = Face::parse(self.data.as_ref(), self.index)
            .map_err(|err| anyhow!("failed to parse font: {err}"))?;
        let units_per_em = f64::from(face.units_per_em());
        if units_per_em <= 0.0 {
            bail!("font has invalid units-per-em");
        }

        let mut char_to_gid = HashMap::new();
        let mut unique = BTreeSet::new();
        unique.insert(' ');
        unique.insert('?');
        unique.extend(chars);
        for ch in unique {
            if let Some(gid) = face.glyph_index(ch) {
                char_to_gid.insert(ch, gid.0);
            }
        }

        let glyph_count = usize::from(face.number_of_glyphs());
        let mut gid_advance = vec![0u16; glyph_count];
        for (gid, slot) in gid_advance.iter_mut().enumerate() {
            if let Some(advance) = face.glyph_hor_advance(GlyphId(gid as u16)) {
                *slot = advance;
            }
        }

        let bbox = face.global_bounding_box();
        let scale = 1000.0 / units_per_em;
        Ok(FontMetrics {
            units_per_em,
            ascender: f64::from(face.ascender()),
            descender: f64::from(face.descender()),
            cap_height: face
                .capital_height()
                .map(f64::from)
                .unwrap_or(f64::from(face.ascender()) * 0.7),
            bbox: [
                f64::from(bbox.x_min) * scale,
                f64::from(bbox.y_min) * scale,
                f64::from(bbox.x_max) * scale,
                f64::from(bbox.y_max) * scale,
            ],
            italic_angle: f64::from(face.italic_angle()),
            char_to_gid,
            gid_advance,
        })
    }

    pub fn embed(&self, metrics: &FontMetrics) -> Result<EmbeddedFont> {
        let mut remapper = GlyphRemapper::new();
        let mut char_to_code = HashMap::new();
        let mut unicode_maps: Vec<BTreeMap<u16, char>> = vec![BTreeMap::new()];
        let mut char_variants = HashMap::new();
        let mut aliases: HashMap<u16, usize> = HashMap::new();
        let mut new_gid_to_width = BTreeMap::new();
        let mut chars: Vec<_> = metrics.char_to_gid.iter().collect();
        chars.sort_by_key(|(ch, _)| **ch);
        if chars.is_empty() || chars.len() > u16::MAX as usize {
            bail!("font requires between 1 and 65535 distinct character codes");
        }
        // Identity-H works in both simple extractors and renderers. Distinct
        // Unicode values sharing a GID get separate Type0 font dictionaries,
        // sharing the font program but using different ToUnicode maps.
        for (&ch, &old_gid) in chars {
            let gid = remapper.remap(old_gid);
            let variant = aliases.entry(gid).or_default();
            while unicode_maps.len() <= *variant {
                unicode_maps.push(BTreeMap::new());
            }
            unicode_maps[*variant].insert(gid, ch);
            char_to_code.insert(ch, gid);
            char_variants.insert(ch, *variant);
            *variant += 1;
            let advance = metrics.gid_advance[usize::from(old_gid)];
            let width = (f64::from(advance) / metrics.units_per_em * 1000.0).round() as u16;
            new_gid_to_width.insert(gid, width);
        }
        let subset = subsetter::subset(self.data.as_ref(), self.index, &remapper)
            .map_err(|err| anyhow!("failed to subset font: {err}"))?;
        use sha2::{Digest, Sha256};
        let tag: String = Sha256::digest(&subset)[..6]
            .iter()
            .map(|byte| char::from(b'A' + byte % 26))
            .collect();
        let family = sanitize_name(&self.family);
        let pdf_name = format!("{tag}+{family}");
        let is_cff = Face::parse(self.data.as_ref(), self.index)
            .map(|face| face.tables().cff.is_some())
            .unwrap_or(true);

        Ok(EmbeddedFont {
            pdf_name,
            subset,
            char_to_code,
            unicode_maps,
            char_variants,
            new_gid_to_width,
            metrics: metrics.clone(),
            is_cff,
        })
    }
}

impl FontMetrics {
    pub fn char_advance(&self, ch: char, size: f64) -> f64 {
        if let Some(&gid) = self.char_to_gid.get(&ch)
            && let Some(&advance) = self.gid_advance.get(usize::from(gid))
            && advance > 0
        {
            return f64::from(advance) / self.units_per_em * size;
        }

        if ch.is_whitespace() {
            size * 0.28
        } else if is_cjk(ch) {
            size
        } else {
            size * 0.52
        }
    }

    pub fn text_width(&self, text: &str, size: f64, letter_spacing: f64) -> f64 {
        text.chars()
            .map(|ch| self.char_advance(ch, size))
            .sum::<f64>()
            + text.chars().count().saturating_sub(1) as f64 * letter_spacing
    }

    pub fn ascent_ratio(&self) -> f64 {
        (self.ascender / self.units_per_em).clamp(0.6, 1.0)
    }
}

impl EmbeddedFont {
    pub fn encode_text(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.chars().count() * 4);
        for ch in text.chars() {
            let gid = self
                .char_to_code
                .get(&ch)
                .copied()
                .expect("typesetting must reject characters absent from the font");
            let _ = write!(out, "{gid:04X}");
        }
        out
    }

    pub fn variant_runs<'a>(&self, text: &'a str) -> Vec<(usize, &'a str)> {
        let mut runs = Vec::new();
        let mut start = 0;
        let mut previous = None;
        for (offset, ch) in text.char_indices() {
            let variant = self.char_variants[&ch];
            if let Some(active) = previous
                && active != variant
            {
                runs.push((active, &text[start..offset]));
                start = offset;
            }
            previous = Some(variant);
        }
        if let Some(active) = previous {
            runs.push((active, &text[start..]));
        }
        runs
    }

    pub fn to_unicode_cmap(&self, variant: usize) -> Vec<u8> {
        let mut out = String::new();
        out.push_str(
            "/CIDInit /ProcSet findresource begin\n\
             12 dict begin\n\
             begincmap\n\
             /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
             /CMapName /Adobe-Identity-UCS def\n\
             /CMapType 2 def\n\
             1 begincodespacerange\n\
             <0000> <FFFF>\n\
             endcodespacerange\n",
        );

        let entries: Vec<(&u16, &char)> = self.unicode_maps[variant].iter().collect();
        for chunk in entries.chunks(100) {
            let _ = writeln!(out, "{} beginbfchar", chunk.len());
            for (gid, ch) in chunk {
                let mut utf16 = [0u16; 2];
                let encoded = ch.encode_utf16(&mut utf16);
                let hex: String = encoded.iter().map(|unit| format!("{unit:04X}")).collect();
                let _ = writeln!(out, "<{gid:04X}> <{hex}>");
            }
            out.push_str("endbfchar\n");
        }

        out.push_str(
            "endcmap\n\
             CMapName currentdict /CMap defineresource pop\n\
             end\n\
             end\n",
        );
        out.into_bytes()
    }

    pub fn widths_array(&self) -> Vec<lopdf::Object> {
        let mut result = Vec::new();
        let entries: Vec<(&u16, &u16)> = self.new_gid_to_width.iter().collect();
        let mut index = 0;
        while index < entries.len() {
            let start = *entries[index].0;
            let mut widths = Vec::new();
            let mut current = start;
            while index < entries.len() && *entries[index].0 == current {
                widths.push((*entries[index].1).into());
                index += 1;
                current = current.saturating_add(1);
            }
            result.push(start.into());
            result.push(lopdf::Object::Array(widths));
        }
        result
    }
}

fn pick_sc_face(data: &[u8]) -> Option<u32> {
    let count = fonts_in_collection(data)?;
    for index in 0..count {
        let Ok(face) = Face::parse(data, index) else {
            continue;
        };
        let Some(family) = face_family(&face) else {
            continue;
        };
        let upper = family.to_ascii_uppercase();
        if upper.contains(" SC")
            || upper.ends_with("SC")
            || upper.contains("SIMPLIFIED")
            || upper.contains("GB")
        {
            return Some(index);
        }
    }
    Some(0)
}

fn face_family(face: &Face<'_>) -> Option<String> {
    let mut fallback = None;
    for name in face.names() {
        if matches!(name.name_id, 1 | 4 | 16)
            && let Some(value) = name.to_string()
            && !value.is_empty()
        {
            if name.name_id == 1 {
                return Some(value);
            }
            fallback.get_or_insert(value);
        }
    }
    fallback
}

fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect();
    if cleaned.is_empty() {
        "CJK".to_string()
    } else {
        cleaned
    }
}

#[cfg(not(feature = "embedded-fonts"))]
fn system_font_database() -> &'static fontdb::Database {
    static DATABASE: OnceLock<fontdb::Database> = OnceLock::new();
    DATABASE.get_or_init(|| {
        let mut database = fontdb::Database::new();
        database.load_system_fonts();
        database
    })
}

#[cfg(not(feature = "embedded-fonts"))]
fn system_face_source(face: &fontdb::FaceInfo) -> Option<(PathBuf, u32)> {
    let path = match &face.source {
        fontdb::Source::File(path) => path.clone(),
        fontdb::Source::SharedFile(path, _) => path.clone(),
        fontdb::Source::Binary(_) => return None,
    };
    Some((path, face.index))
}

#[cfg(not(feature = "embedded-fonts"))]
fn find_system_font(weight: fontdb::Weight) -> Option<FontSource> {
    // Generic families honor the platform's font settings first; explicit
    // families are a fallback for systems whose generic serif/sans does not
    // cover CJK. Every candidate is checked for actual CJK glyphs.
    const FAMILIES: [fontdb::Family<'static>; 10] = [
        fontdb::Family::Serif,
        fontdb::Family::SansSerif,
        fontdb::Family::Name("Noto Serif CJK SC"),
        fontdb::Family::Name("Source Han Serif SC"),
        fontdb::Family::Name("Noto Sans CJK SC"),
        fontdb::Family::Name("Source Han Sans SC"),
        fontdb::Family::Name("Songti SC"),
        fontdb::Family::Name("PingFang SC"),
        fontdb::Family::Name("Microsoft YaHei"),
        fontdb::Family::Name("SimSun"),
    ];

    let database = system_font_database();
    let mut tried = Vec::new();
    for family in FAMILIES {
        let query = fontdb::Query {
            families: &[family],
            weight,
            stretch: fontdb::Stretch::Normal,
            style: fontdb::Style::Normal,
        };
        let Some(id) = database.query(&query) else {
            continue;
        };
        let Some(face) = database.face(id) else {
            continue;
        };
        let Some((path, index)) = system_face_source(face) else {
            continue;
        };
        if tried
            .iter()
            .any(|(tried_path, tried_index)| tried_path == &path && *tried_index == index)
        {
            continue;
        }
        tried.push((path.clone(), index));
        if let Ok(font) = FontSource::load(&path, Some(index))
            && font.supports_cjk()
        {
            return Some(font);
        }
    }
    None
}

pub fn find_default_font(explicit: Option<&Path>) -> Result<FontSource> {
    if let Some(path) = explicit {
        return FontSource::load(path, None);
    }

    #[cfg(feature = "embedded-fonts")]
    {
        embedded_regular_font()
    }

    #[cfg(not(feature = "embedded-fonts"))]
    {
        find_system_font(fontdb::Weight::NORMAL).ok_or_else(|| {
            anyhow!(
                "no usable CJK font found; set [typeset].font in the config file or pass \
                 --font /path/to/font.ttf (or .ttc)"
            )
        })
    }
}

pub fn find_default_bold_font(explicit: Option<&Path>, regular: &FontSource) -> Option<FontSource> {
    if let Some(path) = explicit {
        return FontSource::load(path, None).ok();
    }

    if regular.embedded {
        #[cfg(feature = "embedded-bold-font")]
        {
            return embedded_bold_font().ok();
        }
        #[cfg(not(feature = "embedded-bold-font"))]
        {
            return None;
        }
    }

    let path = regular.path.to_string_lossy();
    for marker in ["Regular", "Regu", "regular"] {
        if path.contains(marker) {
            let candidate = path.replace(marker, "Bold");
            if candidate != path && Path::new(&candidate).exists() {
                return FontSource::load(candidate, None).ok();
            }
        }
    }
    None
}

#[cfg(all(test, feature = "embedded-fonts"))]
mod tests {
    use super::*;

    #[test]
    fn embedded_regular_font_is_available() {
        let regular = find_default_font(None).unwrap();
        assert!(regular.embedded);
        assert!(regular.family.contains("Noto Serif CJK SC"));
        assert!(regular.supports_cjk());
        assert!(
            regular
                .metrics(['中', '文'])
                .unwrap()
                .char_to_gid
                .contains_key(&'中')
        );
    }

    #[cfg(feature = "embedded-bold-font")]
    #[test]
    fn embedded_bold_font_is_available() {
        let regular = find_default_font(None).unwrap();
        let bold = find_default_bold_font(None, &regular).unwrap();
        assert!(bold.embedded);
        assert!(bold.supports_cjk());
        assert!(
            bold.metrics(['中'])
                .unwrap()
                .char_to_gid
                .contains_key(&'中')
        );
    }
}
