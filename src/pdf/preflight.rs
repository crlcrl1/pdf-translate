//! Bound decoding before passing input to the rendering interpreter.
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use lopdf::{Document, LoadOptions, Object};

use super::content::MAX_CONTENT_STREAM_BYTES;

const MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_DECODED_BYTES: usize = 256 * 1024 * 1024;

pub struct ValidatedPdf {
    pub data: Vec<u8>,
    pub document: Document,
}

pub fn load_pdf(path: &Path) -> Result<ValidatedPdf> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    if file.metadata()?.len() > MAX_INPUT_BYTES {
        bail!("PDF exceeds the {MAX_INPUT_BYTES}-byte input limit");
    }
    // A growing file must obey the same limit as the initial metadata.
    let mut data = Vec::new();
    file.take(MAX_INPUT_BYTES + 1).read_to_end(&mut data)?;
    if data.len() as u64 > MAX_INPUT_BYTES {
        bail!("PDF exceeds the {MAX_INPUT_BYTES}-byte input limit");
    }
    let document = Document::load_mem_with_options(
        &data,
        LoadOptions::with_max_decompressed_size(MAX_CONTENT_STREAM_BYTES),
    )
    .with_context(|| format!("failed to load {}", path.display()))?;
    let mut decoded_total = 0usize;
    for (id, object) in &document.objects {
        let Object::Stream(stream) = object else {
            continue;
        };
        let filters = stream.filters().unwrap_or_default();
        let image_codec = filters.iter().any(|filter| {
            matches!(
                *filter,
                b"DCTDecode" | b"JPXDecode" | b"JBIG2Decode" | b"CCITTFaxDecode"
            )
        });
        let is_image = stream
            .dict
            .get(b"Subtype")
            .and_then(Object::as_name)
            .is_ok_and(|s| s == b"Image");
        if !is_image && image_codec {
            bail!("non-image stream {id:?} uses an image codec");
        }
        // Image streams are decoded by the renderer; decompressing a
        // Flate-compressed RGB image here would cost width * height * channels
        // bytes and reject legitimate high-resolution figures. All other
        // streams must pass the bounded decoder before hayro sees the file.
        let decoded_len = if is_image || image_codec {
            stream.content.len()
        } else {
            stream
                .get_plain_content_with_limit(MAX_CONTENT_STREAM_BYTES)
                .with_context(|| {
                    format!("stream {id:?} exceeds decoding limits or has an unsupported filter")
                })?
                .len()
        };
        if decoded_len > MAX_CONTENT_STREAM_BYTES {
            bail!("stream {id:?} exceeds the {MAX_CONTENT_STREAM_BYTES}-byte limit");
        }
        decoded_total = decoded_total.saturating_add(decoded_len);
        if decoded_total > MAX_DECODED_BYTES {
            bail!("PDF exceeds the {MAX_DECODED_BYTES}-byte aggregate stream limit");
        }
    }
    Ok(ValidatedPdf { data, document })
}
